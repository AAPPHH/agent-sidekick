use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use arrow::array::{
    Array, ArrayRef, AsArray, Float64Array, Int32Array, Int64Array, RecordBatch, StringArray, TimestampMillisecondArray,
};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit, TimestampMillisecondType};
use arrow::util::pretty::pretty_format_batches;
use datafusion::datasource::MemTable;
use datafusion::execution::context::SQLOptions;
use datafusion::prelude::SessionContext;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use parquet::file::properties::WriterProperties;
use parquet::file::metadata::KeyValue;

use crate::trust::{
    dedup_sorted, Counters, Dimension, Event, LastBlock, ModelKey, Outcome, Scope, Sperre, Standing, Visibility,
};

pub const SCHEMA_VERSION: i32 = 1;
pub const RESET_SCHEMA_VERSION: i32 = 2;
pub const READ_VERSIONS: [i32; 2] = [SCHEMA_VERSION, RESET_SCHEMA_VERSION];
const VERSION_KEY: &str = "schema_version";
const NEWEST_CONCLUSIONS: usize = 3;

fn version_error(path: &Path, found: &str) -> anyhow::Error {
    anyhow!("{}: unsupported schema_version `{found}` (this build reads {READ_VERSIONS:?})", path.display())
}

fn ts_type() -> DataType {
    DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into()))
}

fn schema_of(fields: &[(&str, DataType, bool)]) -> SchemaRef {
    Arc::new(Schema::new(fields.iter().map(|(n, t, nullable)| Field::new(*n, t.clone(), *nullable)).collect::<Vec<_>>()))
}

fn strs<T: Into<String>>(v: impl IntoIterator<Item = Option<T>>) -> ArrayRef {
    Arc::new(StringArray::from(v.into_iter().map(|s| s.map(Into::into)).collect::<Vec<Option<String>>>()))
}

fn ts_array(v: Vec<i64>) -> ArrayRef {
    Arc::new(TimestampMillisecondArray::from(v).with_timezone("UTC"))
}

fn write_batch(path: &Path, batch: &RecordBatch, version: i32) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let tmp = path.with_extension("parquet.tmp");
    let props = WriterProperties::builder()
        .set_key_value_metadata(Some(vec![KeyValue::new(VERSION_KEY.to_string(), version.to_string())]))
        .build();
    let file = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    let mut writer = ArrowWriter::try_new(file, batch.schema(), Some(props))?;
    writer.write(batch)?;
    writer.close()?;
    fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

fn read_batches(path: &Path) -> Result<Vec<RecordBatch>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).with_context(|| format!("read {}", path.display()))?;
    let found = builder
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .and_then(|kv| kv.iter().find(|k| k.key == VERSION_KEY))
        .and_then(|k| k.value.clone())
        .ok_or_else(|| anyhow!("{}: missing {VERSION_KEY}", path.display()))?;
    if !READ_VERSIONS.iter().any(|v| v.to_string() == found) {
        return Err(version_error(path, &found));
    }
    builder.build()?.map(|b| b.map_err(Into::into)).collect()
}

pub fn read_schema(path: &Path) -> Result<SchemaRef> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    Ok(ParquetRecordBatchReaderBuilder::try_new(file).with_context(|| format!("read {}", path.display()))?.schema().clone())
}

struct Row<'a> {
    batch: &'a RecordBatch,
    i: usize,
}

impl Row<'_> {
    fn col(&self, name: &str) -> Option<&ArrayRef> {
        self.batch.column_by_name(name).filter(|c| !c.is_null(self.i))
    }

    fn str(&self, name: &str) -> Option<String> {
        self.col(name).and_then(|c| c.as_string_opt::<i32>()).map(|a| a.value(self.i).to_string())
    }

    fn req(&self, name: &str) -> Result<String> {
        self.str(name).ok_or_else(|| anyhow!("row {}: missing `{name}`", self.i))
    }

    fn f64(&self, name: &str) -> Option<f64> {
        self.col(name).and_then(|c| c.as_primitive_opt::<arrow::datatypes::Float64Type>()).map(|a| a.value(self.i))
    }

    fn i64(&self, name: &str) -> Option<i64> {
        self.col(name).and_then(|c| c.as_primitive_opt::<arrow::datatypes::Int64Type>()).map(|a| a.value(self.i))
    }

    fn ts(&self, name: &str) -> Result<i64> {
        self.col(name)
            .and_then(|c| c.as_primitive_opt::<TimestampMillisecondType>())
            .map(|a| a.value(self.i))
            .ok_or_else(|| anyhow!("row {}: missing `{name}`", self.i))
    }

    fn version(&self) -> Result<()> {
        let v = self.col(VERSION_KEY).and_then(|c| c.as_primitive_opt::<arrow::datatypes::Int32Type>()).map(|a| a.value(self.i));
        match v {
            Some(v) if READ_VERSIONS.contains(&v) => Ok(()),
            other => bail!("row {}: unsupported schema_version {other:?}", self.i),
        }
    }
}

fn rows(batches: &[RecordBatch]) -> impl Iterator<Item = Row<'_>> {
    batches.iter().flat_map(|batch| (0..batch.num_rows()).map(move |i| Row { batch, i }))
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelRow {
    pub key: ModelKey,
    pub context_tokens: Option<i64>,
    pub cost: String,
    pub max_steps: Option<i64>,
    pub tool_output_lines: Option<i64>,
    pub context_budget_tokens: Option<i64>,
    pub sperre: Sperre,
    pub added_at: i64,
    pub added_by: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Profile {
    pub strengths: Option<String>,
    pub weaknesses: Option<String>,
    pub task_scope: Option<String>,
    pub dims_prior: Option<String>,
    pub benchmarks: Option<String>,
    pub sources: Option<String>,
    pub profile_as_of: Option<String>,
}

impl Profile {
    pub const COLUMNS: [&'static str; 7] = ["strengths", "weaknesses", "task_scope", "dims_prior", "benchmarks", "sources", "profile_as_of"];

    pub fn fields(&self) -> [&Option<String>; 7] {
        [&self.strengths, &self.weaknesses, &self.task_scope, &self.dims_prior, &self.benchmarks, &self.sources, &self.profile_as_of]
    }

    fn read(r: &Row) -> Self {
        let [strengths, weaknesses, task_scope, dims_prior, benchmarks, sources, profile_as_of] = Self::COLUMNS.map(|c| r.str(c));
        Self { strengths, weaknesses, task_scope, dims_prior, benchmarks, sources, profile_as_of }
    }

    pub fn render(&self) -> String {
        let rows = [
            ("Stärken", &self.strengths),
            ("Schwächen", &self.weaknesses),
            ("Umfang", &self.task_scope),
            ("Einschätzung je Dimension (abgeleitet aus öffentlichen Daten, nicht gemessen)", &self.dims_prior),
            ("Benchmarks", &self.benchmarks),
            ("Quellen", &self.sources),
        ];
        if rows.iter().all(|(_, v)| v.is_none()) {
            return String::new();
        }
        let mut out = format!(
            "Profil (öffentliche Daten, Stand {}; kein Einfluss auf Vertrauen)\n",
            self.profile_as_of.as_deref().unwrap_or("unbekannt")
        );
        for (label, value) in rows {
            if let Some(v) = value {
                out.push_str(&format!("- {label}: {v}\n"));
            }
        }
        out
    }
}

impl ModelRow {
    pub fn model_id(&self) -> String {
        self.key.model_id()
    }

    pub fn schema() -> SchemaRef {
        schema_of(&[
            ("model_id", DataType::Utf8, false),
            ("provider", DataType::Utf8, false),
            ("model", DataType::Utf8, false),
            ("checkpoint", DataType::Utf8, false),
            ("quant", DataType::Utf8, false),
            ("harness", DataType::Utf8, false),
            ("context_tokens", DataType::Int64, true),
            ("cost", DataType::Utf8, false),
            ("max_steps", DataType::Int64, true),
            ("tool_output_lines", DataType::Int64, true),
            ("context_budget_tokens", DataType::Int64, true),
            ("sperre", DataType::Utf8, false),
            ("added_at", ts_type(), false),
            ("added_by", DataType::Utf8, false),
            ("strengths", DataType::Utf8, true),
            ("weaknesses", DataType::Utf8, true),
            ("task_scope", DataType::Utf8, true),
            ("dims_prior", DataType::Utf8, true),
            ("benchmarks", DataType::Utf8, true),
            ("sources", DataType::Utf8, true),
            ("profile_as_of", DataType::Utf8, true),
        ])
    }

    pub fn to_batch(models: &[ModelRow]) -> Result<RecordBatch> {
        Self::to_batch_with(models, &HashMap::new())
    }

    pub fn to_batch_with(models: &[ModelRow], profiles: &HashMap<String, Profile>) -> Result<RecordBatch> {
        let s = |f: fn(&ModelRow) -> String| strs(models.iter().map(|m| Some(f(m))));
        let i = |f: fn(&ModelRow) -> Option<i64>| -> ArrayRef { Arc::new(Int64Array::from(models.iter().map(f).collect::<Vec<_>>())) };
        let cols = vec![
            s(|m| m.model_id()),
            s(|m| m.key.provider.clone()),
            s(|m| m.key.model.clone()),
            s(|m| m.key.checkpoint.clone()),
            s(|m| m.key.quant.clone()),
            s(|m| m.key.harness.clone()),
            i(|m| m.context_tokens),
            s(|m| m.cost.clone()),
            i(|m| m.max_steps),
            i(|m| m.tool_output_lines),
            i(|m| m.context_budget_tokens),
            s(|m| m.sperre.as_str().into()),
            ts_array(models.iter().map(|m| m.added_at).collect()),
            s(|m| m.added_by.clone()),
        ];
        let empty = Profile::default();
        let profile_cols = (0..Profile::COLUMNS.len()).map(|i| {
            strs(models.iter().map(|m| profiles.get(&m.model_id()).unwrap_or(&empty).fields()[i].clone()))
        });
        Ok(RecordBatch::try_new(Self::schema(), cols.into_iter().chain(profile_cols).collect())?)
    }

    pub fn write_all(path: &Path, models: &[ModelRow]) -> Result<()> {
        Self::write_all_with(path, models, &HashMap::new())
    }

    pub fn write_all_with(path: &Path, models: &[ModelRow], profiles: &HashMap<String, Profile>) -> Result<()> {
        write_batch(path, &Self::to_batch_with(models, profiles)?, SCHEMA_VERSION)
    }

    pub fn read_profiles(path: &Path) -> Result<HashMap<String, Profile>> {
        let batches = read_batches(path)?;
        rows(&batches).map(|r| Ok((r.req("model_id")?, Profile::read(&r)))).collect()
    }

    pub fn read_all(path: &Path) -> Result<Vec<ModelRow>> {
        let batches = read_batches(path)?;
        rows(&batches)
            .map(|r| {
                let key = ModelKey::new(&r.req("provider")?, &r.req("model")?, &r.req("checkpoint")?, &r.req("quant")?, &r.req("harness")?);
                let stored = r.req("model_id")?;
                if stored != key.model_id() {
                    bail!("{}: model_id {stored} does not match its quintuple", path.display());
                }
                Ok(ModelRow {
                    key,
                    context_tokens: r.i64("context_tokens"),
                    cost: r.req("cost")?,
                    max_steps: r.i64("max_steps"),
                    tool_output_lines: r.i64("tool_output_lines"),
                    context_budget_tokens: r.i64("context_budget_tokens"),
                    sperre: Sperre::parse(&r.req("sperre")?)?,
                    added_at: r.ts("added_at")?,
                    added_by: r.req("added_by")?,
                })
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtectedKind {
    PathGlob,
    RemovedTerm,
}

impl ProtectedKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProtectedKind::PathGlob => "path_glob",
            ProtectedKind::RemovedTerm => "removed_term",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "path_glob" => Ok(ProtectedKind::PathGlob),
            "removed_term" => Ok(ProtectedKind::RemovedTerm),
            _ => bail!("unknown protected kind `{s}`"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtectedRow {
    pub kind: ProtectedKind,
    pub value: String,
}

impl ProtectedRow {
    pub fn defaults() -> Vec<ProtectedRow> {
        let globs = ["tests/**", "spec/**", "**/validate*"].map(|v| (ProtectedKind::PathGlob, v));
        let terms = ["decided_by", "assert"].map(|v| (ProtectedKind::RemovedTerm, v));
        globs.into_iter().chain(terms).map(|(kind, v)| ProtectedRow { kind, value: v.into() }).collect()
    }

    pub fn schema() -> SchemaRef {
        schema_of(&[("kind", DataType::Utf8, false), ("value", DataType::Utf8, false)])
    }

    pub fn to_batch(protected: &[ProtectedRow]) -> Result<RecordBatch> {
        let cols = vec![
            strs(protected.iter().map(|p| Some(p.kind.as_str()))),
            strs(protected.iter().map(|p| Some(p.value.as_str()))),
        ];
        Ok(RecordBatch::try_new(Self::schema(), cols)?)
    }

    pub fn write_all(path: &Path, protected: &[ProtectedRow]) -> Result<()> {
        write_batch(path, &Self::to_batch(protected)?, SCHEMA_VERSION)
    }

    pub fn read_all(path: &Path) -> Result<Vec<ProtectedRow>> {
        let batches = read_batches(path)?;
        rows(&batches)
            .map(|r| Ok(ProtectedRow { kind: ProtectedKind::parse(&r.req("kind")?)?, value: r.req("value")? }))
            .collect()
    }
}

pub fn experience_schema() -> SchemaRef {
    schema_of(&[
        ("event_id", DataType::Utf8, false),
        ("ts", ts_type(), false),
        ("machine_id", DataType::Utf8, false),
        ("user_id", DataType::Utf8, false),
        ("model_id", DataType::Utf8, false),
        ("dimension", DataType::Utf8, false),
        ("scope", DataType::Utf8, false),
        ("repo", DataType::Utf8, true),
        ("job_id", DataType::Utf8, true),
        ("steps", DataType::Int64, true),
        ("duration_s", DataType::Float64, true),
        ("review_s", DataType::Float64, true),
        ("outcome", DataType::Utf8, false),
        ("seed_value", DataType::Float64, true),
        ("block_reason", DataType::Utf8, true),
        ("evidence_hash", DataType::Utf8, true),
        ("summary", DataType::Utf8, true),
        ("conclusion", DataType::Utf8, true),
        ("visibility", DataType::Utf8, false),
        (VERSION_KEY, DataType::Int32, false),
    ])
}

pub fn events_version(events: &[Event]) -> i32 {
    if events.iter().any(|e| e.outcome == Outcome::Reset) {
        RESET_SCHEMA_VERSION
    } else {
        SCHEMA_VERSION
    }
}

pub fn events_to_batch(events: &[Event]) -> Result<RecordBatch> {
    let s = |f: fn(&Event) -> String| strs(events.iter().map(|e| Some(f(e))));
    let o = |f: fn(&Event) -> Option<String>| strs(events.iter().map(f));
    let cols: Vec<ArrayRef> = vec![
        s(|e| e.event_id.clone()),
        ts_array(events.iter().map(|e| e.ts_ms).collect()),
        s(|e| e.machine_id.clone()),
        s(|e| e.user_id.clone()),
        s(|e| e.model_id.clone()),
        s(|e| e.dimension.as_str().into()),
        s(|e| e.scope.to_string()),
        o(|e| e.repo.clone()),
        o(|e| e.job_id.clone()),
        Arc::new(Int64Array::from(events.iter().map(|e| e.steps).collect::<Vec<_>>())),
        Arc::new(Float64Array::from(events.iter().map(|e| e.duration_s).collect::<Vec<_>>())),
        Arc::new(Float64Array::from(events.iter().map(|e| e.review_s).collect::<Vec<_>>())),
        s(|e| e.outcome.as_str().into()),
        Arc::new(Float64Array::from(events.iter().map(|e| e.seed_value).collect::<Vec<_>>())),
        o(|e| e.block_reason.clone()),
        o(|e| e.evidence_hash.clone()),
        o(|e| e.summary.clone()),
        o(|e| e.conclusion.clone()),
        s(|e| e.visibility.as_str().into()),
        Arc::new(Int32Array::from(vec![events_version(events); events.len()])),
    ];
    Ok(RecordBatch::try_new(experience_schema(), cols)?)
}

pub fn write_events(path: &Path, events: &[Event]) -> Result<()> {
    write_batch(path, &events_to_batch(events)?, events_version(events))
}

pub fn read_events(path: &Path) -> Result<Vec<Event>> {
    let batches = read_batches(path)?;
    rows(&batches)
        .map(|r| {
            r.version()?;
            Ok(Event {
                event_id: r.req("event_id")?,
                ts_ms: r.ts("ts")?,
                machine_id: r.str("machine_id").unwrap_or_default(),
                user_id: r.str("user_id").unwrap_or_default(),
                model_id: r.req("model_id")?,
                dimension: Dimension::parse(&r.req("dimension")?)?,
                scope: Scope::parse(&r.req("scope")?)?,
                repo: r.str("repo"),
                job_id: r.str("job_id"),
                steps: r.i64("steps"),
                duration_s: r.f64("duration_s"),
                review_s: r.f64("review_s"),
                outcome: Outcome::parse(&r.req("outcome")?)?,
                seed_value: r.f64("seed_value"),
                block_reason: r.str("block_reason"),
                evidence_hash: r.str("evidence_hash"),
                summary: r.str("summary"),
                conclusion: r.str("conclusion"),
                visibility: Visibility::parse(&r.req("visibility")?)?,
            })
        })
        .collect()
}

pub fn strip_private(event: &Event) -> Event {
    let mut shared = event.clone();
    if event.visibility == Visibility::Private {
        shared.summary = None;
        shared.conclusion = None;
        shared.block_reason = shared.block_reason.as_deref().map(redact_block_reason);
    }
    shared
}

pub fn redact_block_reason(reason: &str) -> String {
    reason
        .split("; ")
        .map(|part| {
            if part.starts_with("protected path ") {
                "protected path".to_string()
            } else if let Some(rest) = part.strip_prefix("removed protected term `") {
                format!("removed term {}", rest.split('`').next().unwrap_or_default())
            } else if part.starts_with("path ") && part.contains(" outside module path ") {
                "path outside module path".to_string()
            } else if part.starts_with("write-tests touches non-test path ") {
                "write-tests touches non-test path".to_string()
            } else if let Some(rest) = part.strip_prefix("diff without write right (") {
                format!("diff without write right ({})", rest.split("):").next().unwrap_or_default())
            } else {
                part.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |b: u8| block.map(|k| k ^ b);
    let inner = Sha256::new().chain_update(pad(0x36)).chain_update(msg).finalize();
    Sha256::new().chain_update(pad(0x5c)).chain_update(inner).finalize().into()
}

pub fn pseudonym(secret: &[u8], raw: &str) -> String {
    hmac_sha256(secret, raw.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

pub fn shared_copy(event: &Event, secret: Option<&[u8]>) -> Event {
    let mut shared = strip_private(event);
    if let Some(secret) = secret {
        shared.user_id = pseudonym(secret, &event.user_id);
        shared.machine_id = pseudonym(secret, &event.machine_id);
    }
    shared
}

#[derive(Debug)]
pub struct SessionLog {
    pub shared_path: PathBuf,
    pub local_path: PathBuf,
    events: Vec<Event>,
    secret: Option<Vec<u8>>,
}

impl SessionLog {
    pub fn open(shared_root: &Path, local_root: &Path, name: &str, session: &str) -> Result<Self> {
        if [name, session].iter().any(|p| p.is_empty() || p.contains(['/', '\\']) || p.starts_with('.')) {
            bail!("invalid experience name or session id");
        }
        let rel = Path::new("experiences").join(name).join(format!("{session}.parquet"));
        let local_path = local_root.join(&rel);
        let events = if local_path.exists() { read_events(&local_path)? } else { Vec::new() };
        Ok(Self { shared_path: shared_root.join(rel), local_path, events, secret: None })
    }

    pub fn with_secret(mut self, secret: Vec<u8>) -> Self {
        self.secret = Some(secret);
        self
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    pub fn append(&mut self, event: Event) -> Result<()> {
        if self.events.iter().any(|e| e.event_id == event.event_id) {
            bail!("duplicate event_id {}", event.event_id);
        }
        let mut next = self.events.clone();
        next.push(event);
        write_events(&self.local_path, &next)?;
        let shared: Vec<Event> = next.iter().map(|e| shared_copy(e, self.secret.as_deref())).collect();
        write_events(&self.shared_path, &shared)?;
        self.events = next;
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct DataDir {
    pub root: PathBuf,
}

impl DataDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn models_path(&self) -> PathBuf {
        self.root.join("models.parquet")
    }

    pub fn protected_path(&self) -> PathBuf {
        self.root.join("protected.parquet")
    }

    pub fn aggregate_path(&self) -> PathBuf {
        self.root.join("aggregate").join("latest.parquet")
    }

    pub fn models(&self) -> Result<Vec<ModelRow>> {
        if self.models_path().exists() {
            ModelRow::read_all(&self.models_path())
        } else {
            Ok(Vec::new())
        }
    }

    pub fn model(&self, model_id: &str) -> Result<ModelRow> {
        self.models()?
            .into_iter()
            .find(|m| m.model_id() == model_id)
            .ok_or_else(|| anyhow!("no registry row for model_id `{model_id}`"))
    }

    pub fn profiles(&self) -> Result<HashMap<String, Profile>> {
        if self.models_path().exists() {
            ModelRow::read_profiles(&self.models_path())
        } else {
            Ok(HashMap::new())
        }
    }

    pub fn protected(&self) -> Result<Vec<ProtectedRow>> {
        if self.protected_path().exists() {
            ProtectedRow::read_all(&self.protected_path())
        } else {
            Ok(ProtectedRow::defaults())
        }
    }

    pub fn experience_files(root: &Path) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        let mut stack = vec![root.join("experiences")];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            for entry in entries {
                let path = entry?.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "parquet") {
                    out.push(path);
                }
            }
        }
        out.sort();
        Ok(out)
    }

    pub fn events(&self, local_root: Option<&Path>) -> Result<Vec<Event>> {
        self.collect_events(local_root, true)
    }

    pub fn experience_events(&self) -> Result<Vec<Event>> {
        self.collect_events(None, false)
    }

    fn collect_events(&self, local_root: Option<&Path>, with_aggregate: bool) -> Result<Vec<Event>> {
        let mut files = Vec::new();
        if let Some(local) = local_root {
            files.extend(Self::experience_files(local)?);
        }
        files.extend(Self::experience_files(&self.root)?);
        if with_aggregate && self.aggregate_path().exists() {
            files.push(self.aggregate_path());
        }
        let mut all = Vec::new();
        for file in files {
            all.extend(read_events(&file)?);
        }
        Ok(dedup_sorted(all))
    }

    pub fn event_ids(&self, local_root: Option<&Path>) -> Result<HashSet<String>> {
        Ok(self.events(local_root)?.into_iter().map(|e| e.event_id).collect())
    }

    pub fn steckbrief(&self, model_id: &str, scope: &Scope, local_root: Option<&Path>, now_ms: i64) -> Result<Steckbrief> {
        let model = self.model(model_id)?;
        Ok(Steckbrief::build(&model, &self.events(local_root)?, scope, now_ms).with_profile(self.profiles()?.remove(model_id)))
    }

    pub async fn query(&self, local_root: Option<&Path>, sql: &str) -> Result<String> {
        let ctx = SessionContext::new();
        let tables = [
            ("models", ModelRow::to_batch_with(&self.models()?, &self.profiles()?)?),
            ("protected", ProtectedRow::to_batch(&self.protected()?)?),
            ("experiences", events_to_batch(&self.events(local_root)?)?),
        ];
        for (name, batch) in tables {
            ctx.register_table(name, Arc::new(MemTable::try_new(batch.schema(), vec![vec![batch]])?))?;
        }
        let options = SQLOptions::new().with_allow_ddl(false).with_allow_dml(false).with_allow_statements(false);
        let batches = ctx.sql_with_options(sql, options).await?.collect().await?;
        Ok(pretty_format_batches(&batches)?.to_string())
    }
}

#[derive(Clone, Debug)]
pub struct SteckbriefRow {
    pub dimension: Dimension,
    pub scope: Scope,
    pub trust: f64,
    pub pruefanteil: f64,
    pub counters: Counters,
    pub last_block: Option<LastBlock>,
    pub manual: bool,
}

#[derive(Clone, Debug)]
pub struct Conclusion {
    pub ts_ms: i64,
    pub scope: Scope,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct Steckbrief {
    pub model: ModelRow,
    pub model_id: String,
    pub gold: bool,
    pub scope: Scope,
    pub rows: Vec<SteckbriefRow>,
    pub write_right_code: bool,
    pub write_right_tests: bool,
    pub decisions_right: bool,
    pub duration_s: f64,
    pub review_s: f64,
    pub conclusions: Vec<Conclusion>,
    pub reset_at: Option<i64>,
    pub profile: Option<Profile>,
}

fn utc(ts_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ts_ms).map_or_else(|| ts_ms.to_string(), |t| t.format("%Y-%m-%d %H:%M UTC").to_string())
}

impl Steckbrief {
    pub fn build(model: &ModelRow, events: &[Event], scope: &Scope, now_ms: i64) -> Self {
        let standing = Standing::compute(&model.key, model.sperre, events, now_ms);
        let chain = scope.chain();
        let mut rows = Vec::new();
        for dimension in Dimension::ALL {
            let cells: Vec<_> = chain.iter().rev().filter_map(|s| standing.book.cell(dimension, s).map(|c| (s, c))).collect();
            let shown: Vec<_> = if standing.gold || cells.is_empty() {
                let cell = standing.book.resolve(dimension, scope);
                vec![(if standing.gold { scope } else { &Scope::Global }, cell)]
            } else {
                cells.into_iter().map(|(s, c)| (s, Some(c))).collect()
            };
            for (s, cell) in shown {
                rows.push(SteckbriefRow {
                    dimension,
                    scope: s.clone(),
                    trust: standing.trust(dimension, s),
                    pruefanteil: standing.pruefanteil(dimension, s),
                    counters: cell.map(|c| c.counters.clone()).unwrap_or_default(),
                    last_block: cell.and_then(|c| c.last_block.clone()),
                    manual: standing.manual(dimension, s),
                });
            }
        }
        let own: Vec<&Event> = events.iter().filter(|e| e.model_id == standing.model_id).collect();
        let mut conclusions: Vec<Conclusion> = own
            .iter()
            .filter_map(|e| e.conclusion.as_ref().map(|t| Conclusion { ts_ms: e.ts_ms, scope: e.scope.clone(), text: t.clone() }))
            .collect();
        conclusions.sort_by_key(|c| std::cmp::Reverse(c.ts_ms));
        conclusions.truncate(NEWEST_CONCLUSIONS);
        Self {
            model: model.clone(),
            model_id: standing.model_id.clone(),
            gold: standing.gold,
            scope: scope.clone(),
            rows,
            write_right_code: standing.write_right(Dimension::WriteCode, scope),
            write_right_tests: standing.write_right(Dimension::WriteTests, scope),
            decisions_right: standing.decisions_right(scope),
            duration_s: {
                let mut jobs = std::collections::HashSet::new();
                own.iter().filter(|e| e.duration_s.is_some() && e.job_id.as_ref().is_none_or(|j| jobs.insert(j.clone()))).filter_map(|e| e.duration_s).sum()
            },
            review_s: own.iter().filter_map(|e| e.review_s).sum(),
            conclusions,
            reset_at: standing.book.reset_at(),
            profile: None,
        }
    }

    pub fn with_profile(mut self, profile: Option<Profile>) -> Self {
        self.profile = profile;
        self
    }

    pub fn render(&self) -> String {
        let k = &self.model.key;
        let yes = |b: bool| if b { "ja" } else { "nein" };
        let write = |b: bool| if b { "ja" } else { "nein (Vorschlagsmodus)" };
        let num = |v: Option<i64>| v.map_or("-".to_string(), |v| v.to_string());
        let mut out = format!(
            "Steckbrief {} ({}/{}, checkpoint {}, quant {}, harness {}){}\nScope: {}\nKosten: {}, Sperre: {}, max_steps {}, tool_output_lines {}, context_budget_tokens {}, context_tokens {}\n",
            self.model_id,
            k.provider,
            k.model,
            k.checkpoint,
            k.quant,
            k.harness,
            if self.gold { " GOLD" } else { "" },
            self.scope,
            self.model.cost,
            self.model.sperre.as_str(),
            num(self.model.max_steps),
            num(self.model.tool_output_lines),
            num(self.model.context_budget_tokens),
            num(self.model.context_tokens),
        );
        if let Some(p) = &self.profile {
            out.push_str(&p.render());
        }
        for r in &self.rows {
            let c = &r.counters;
            out.push_str(&format!(
                "- {} @ {}: {:.2} (Prüfanteil {:.2}) acc {} rej {} abo {} blk {} note {}",
                r.dimension, r.scope, r.trust, r.pruefanteil, c.accepted, c.rejected, c.aborted, c.blocked, c.note
            ));
            if r.manual {
                out.push_str(" [manuell gesetzt]");
            }
            if let Some(b) = &r.last_block {
                out.push_str(&format!(" [letzter Block {} \"{}\"]", utc(b.ts_ms), b.reason));
            }
            out.push('\n');
        }
        out.push_str(&format!(
            "Schreibrecht write-code: {}, write-tests: {}, Entscheidungsrecht: {}\nKostenbilanz: Laufzeit {}s, Review {}s\n",
            write(self.write_right_code),
            write(self.write_right_tests),
            yes(self.decisions_right),
            self.duration_s.round() as i64,
            self.review_s.round() as i64
        ));
        if let Some(t) = self.reset_at {
            out.push_str(&format!("zurückgesetzt am {}\n", utc(t).split(' ').next().unwrap_or_default()));
        }
        for c in &self.conclusions {
            out.push_str(&format!("Fazit ({}, ts {}): {}\n", c.scope, c.ts_ms, c.text));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust::MS_PER_DAY;

    const NOW: i64 = 10_000 * MS_PER_DAY;

    fn qwen_row() -> ModelRow {
        ModelRow {
            key: ModelKey::new("dgx", "qwen3-coder-next", "qwen3-coder-next", "q8", "opencode"),
            context_tokens: Some(128_000),
            cost: "shared-dgx".into(),
            max_steps: Some(80),
            tool_output_lines: None,
            context_budget_tokens: Some(64_000),
            sperre: Sperre::NurDiff,
            added_at: NOW,
            added_by: "john".into(),
        }
    }

    fn ev(id: &str, d: Dimension, scope: &str, o: Outcome, ts: i64) -> Event {
        let mut e = Event::new(id, d, Scope::parse(scope).unwrap(), o, NOW - 1000 + ts);
        e.machine_id = "m1".into();
        e.user_id = "u1".into();
        e
    }

    #[test]
    fn models_and_protected_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("models.parquet");
        ModelRow::write_all(&p, &[qwen_row()]).unwrap();
        assert_eq!(ModelRow::read_all(&p).unwrap(), vec![qwen_row()]);
        let q = dir.path().join("protected.parquet");
        ProtectedRow::write_all(&q, &ProtectedRow::defaults()).unwrap();
        assert_eq!(ProtectedRow::read_all(&q).unwrap(), ProtectedRow::defaults());
        assert_eq!(ProtectedRow::defaults().len(), 5);
    }

    #[test]
    fn unknown_schema_version_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("e.parquet");
        write_events(&p, &[ev("m", Dimension::Search, "global", Outcome::Note, 0)]).unwrap();
        let batch = read_batches(&p).unwrap().remove(0);
        let props = WriterProperties::builder().set_key_value_metadata(Some(vec![KeyValue::new(VERSION_KEY.to_string(), "99".to_string())])).build();
        let bad = dir.path().join("bad.parquet");
        let mut w = ArrowWriter::try_new(File::create(&bad).unwrap(), batch.schema(), Some(props)).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
        let err = read_events(&bad).unwrap_err().to_string();
        assert!(err.contains("unsupported schema_version"), "{err}");
    }

    #[test]
    fn events_roundtrip_with_nulls() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("e.parquet");
        let mut a = ev("m", Dimension::WriteCode, "module:r/src", Outcome::Blocked, 1);
        a.block_reason = Some("x".into());
        a.steps = Some(80);
        a.duration_s = Some(142.0);
        a.seed_value = Some(0.5);
        a.summary = Some("s".into());
        let b = ev("m", Dimension::Search, "global", Outcome::Note, 2);
        write_events(&p, &[a.clone(), b.clone()]).unwrap();
        assert_eq!(read_events(&p).unwrap(), vec![a, b]);
    }

    #[test]
    fn session_log_appends_and_strips_private_text_in_shared_copy() {
        let shared = tempfile::tempdir().unwrap();
        let local = tempfile::tempdir().unwrap();
        let mut log = SessionLog::open(shared.path(), local.path(), "john", "sess-1").unwrap();
        let mut private = ev("m", Dimension::Search, "global", Outcome::Accepted, 1);
        private.visibility = Visibility::Private;
        private.summary = Some("geheim".into());
        private.conclusion = Some("auch geheim".into());
        let mut public = ev("m", Dimension::Search, "global", Outcome::Accepted, 2);
        public.summary = Some("offen".into());
        log.append(private.clone()).unwrap();
        log.append(public.clone()).unwrap();
        assert!(log.append(public.clone()).is_err());
        let s = read_events(&log.shared_path).unwrap();
        assert_eq!(s[0].summary, None);
        assert_eq!(s[0].conclusion, None);
        assert_eq!(s[1].summary.as_deref(), Some("offen"));
        assert_eq!(read_events(&log.local_path).unwrap()[0].summary.as_deref(), Some("geheim"));
        let reopened = SessionLog::open(shared.path(), local.path(), "john", "sess-1").unwrap();
        assert_eq!(reopened.events().len(), 2);
        assert!(SessionLog::open(shared.path(), local.path(), "../x", "s").is_err());
    }

    #[test]
    fn union_dedups_by_event_id_and_prefers_local_full_copy() {
        let shared = tempfile::tempdir().unwrap();
        let local = tempfile::tempdir().unwrap();
        let mut log = SessionLog::open(shared.path(), local.path(), "john", "s1").unwrap();
        let mut e = ev("m", Dimension::Search, "global", Outcome::Accepted, 1);
        e.visibility = Visibility::Private;
        e.conclusion = Some("c".into());
        log.append(e.clone()).unwrap();
        let mut other = SessionLog::open(shared.path(), local.path(), "anna", "s2").unwrap();
        other.append(ev("m", Dimension::Search, "global", Outcome::Rejected, 2)).unwrap();
        write_events(&shared.path().join("aggregate/latest.parquet"), &[strip_private(&e)]).unwrap();
        let dd = DataDir::new(shared.path());
        let union = dd.events(Some(local.path())).unwrap();
        assert_eq!(union.len(), 2);
        assert_eq!(union[0].conclusion.as_deref(), Some("c"));
        assert_eq!(dd.events(None).unwrap().len(), 2);
        assert_eq!(dd.events(None).unwrap()[0].conclusion, None);
    }

    #[test]
    fn steckbrief_shows_trust_counters_and_rights() {
        let row = qwen_row();
        let id = row.model_id();
        let mut seed = ev(&id, Dimension::Search, "global", Outcome::Seed, 1);
        seed.seed_value = Some(0.55);
        let mut blocked = ev(&id, Dimension::WriteCode, "global", Outcome::Blocked, 2);
        blocked.block_reason = Some("Validierung decided_by entfernt".into());
        blocked.duration_s = Some(142.0);
        blocked.conclusion = Some("brach aus".into());
        let sb = Steckbrief::build(&row, &[seed, blocked], &Scope::parse("module:r/src").unwrap(), NOW);
        let text = sb.render();
        assert!(text.contains("search @ global: 0.55"), "{text}");
        assert!(text.contains("[manuell gesetzt]"));
        assert!(text.contains("write-code @ global: 0.10"));
        assert!(text.contains("blk 1"));
        assert!(text.contains("Validierung decided_by entfernt"));
        assert!(text.contains("Schreibrecht write-code: nein"));
        assert!(text.contains("Fazit (global"));
        assert!(text.contains("write-tests: nein (Vorschlagsmodus)"));
        assert!(text.contains(" UTC \"Validierung"), "{text}");
        assert!(!text.contains("-0s"));
        assert_eq!(sb.duration_s, 142.0);
        assert!(!sb.write_right_code && !sb.decisions_right);
    }

    #[test]
    fn steckbrief_gold_and_registry_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let mut gold = qwen_row();
        gold.key = ModelKey::new("anthropic", "claude-opus-5-5", "c", "", "claude-code");
        gold.sperre = Sperre::Technisch;
        ModelRow::write_all(&dir.path().join("models.parquet"), &[gold.clone(), qwen_row()]).unwrap();
        let dd = DataDir::new(dir.path());
        let sb = dd.steckbrief(&gold.model_id(), &Scope::Global, None, NOW).unwrap();
        assert!(sb.gold && sb.write_right_code && sb.decisions_right);
        assert!(sb.render().contains("GOLD"));
        assert!(dd.steckbrief("nope", &Scope::Global, None, NOW).is_err());
        assert_eq!(dd.protected().unwrap(), ProtectedRow::defaults());
    }

    #[tokio::test]
    async fn sql_query_over_registered_tables_and_blocks_ddl() {
        let shared = tempfile::tempdir().unwrap();
        let local = tempfile::tempdir().unwrap();
        ModelRow::write_all(&shared.path().join("models.parquet"), &[qwen_row()]).unwrap();
        let mut log = SessionLog::open(shared.path(), local.path(), "john", "s1").unwrap();
        log.append(ev(&qwen_row().model_id(), Dimension::Search, "global", Outcome::Accepted, 1)).unwrap();
        let dd = DataDir::new(shared.path());
        let out = dd.query(Some(local.path()), "select outcome, count(*) as n from experiences group by outcome").await.unwrap();
        assert!(out.contains("accepted") && out.contains('1'), "{out}");
        let out = dd.query(None, "select m.model, e.dimension from models m join experiences e on e.model_id = m.model_id").await.unwrap();
        assert!(out.contains("qwen3-coder-next") && out.contains("search"), "{out}");
        assert!(dd.query(None, "select value from protected where kind = 'removed_term'").await.unwrap().contains("assert"));
        assert!(dd.query(None, "create table x as select 1").await.is_err());
        assert!(dd.query(None, "copy (select 1) to '/tmp/x.csv'").await.is_err());
    }

    fn profile() -> Profile {
        Profile {
            strengths: Some("stark".into()),
            weaknesses: Some("schwach".into()),
            task_scope: Some("klein".into()),
            dims_prior: Some("search=mittel;decisions=schwach".into()),
            benchmarks: Some("X 1 (src, 2026-01-01, vendor)".into()),
            sources: Some("https://a ; https://b".into()),
            profile_as_of: Some("2026-10-27".into()),
        }
    }

    #[test]
    fn profile_columns_roundtrip_and_old_files_still_read() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("models.parquet");
        let id = qwen_row().model_id();
        ModelRow::write_all_with(&p, &[qwen_row()], &HashMap::from([(id.clone(), profile())])).unwrap();
        assert_eq!(ModelRow::read_all(&p).unwrap(), vec![qwen_row()]);
        assert_eq!(ModelRow::read_profiles(&p).unwrap()[&id], profile());
        let full = ModelRow::to_batch(&[qwen_row()]).unwrap();
        let old_cols = ModelRow::schema().fields().len() - Profile::COLUMNS.len();
        let old = full.project(&(0..old_cols).collect::<Vec<_>>()).unwrap();
        let q = dir.path().join("old.parquet");
        write_batch(&q, &old, SCHEMA_VERSION).unwrap();
        assert_eq!(ModelRow::read_all(&q).unwrap(), vec![qwen_row()]);
        assert_eq!(ModelRow::read_profiles(&q).unwrap()[&id], Profile::default());
    }

    #[test]
    fn profile_renders_after_the_fixed_part_and_never_moves_trust() {
        let dir = tempfile::tempdir().unwrap();
        let row = qwen_row();
        let id = row.model_id();
        let mut blocked = ev(&id, Dimension::WriteCode, "global", Outcome::Blocked, 2);
        blocked.block_reason = Some("x".into());
        let mut accepted = ev(&id, Dimension::Search, "global", Outcome::Accepted, 3);
        accepted.conclusion = Some("ok".into());
        write_events(&dir.path().join("experiences/a/s.parquet"), &[blocked, accepted]).unwrap();
        ModelRow::write_all(&dir.path().join("models.parquet"), &[row.clone()]).unwrap();
        let dd = DataDir::new(dir.path());
        let plain = dd.steckbrief(&id, &Scope::Global, None, NOW).unwrap();
        assert!(!plain.render().contains("Profil"));
        ModelRow::write_all_with(&dir.path().join("models.parquet"), &[row], &HashMap::from([(id.clone(), profile())])).unwrap();
        let with = dd.steckbrief(&id, &Scope::Global, None, NOW).unwrap();
        let values = |sb: &Steckbrief| sb.rows.iter().map(|r| (r.dimension, r.scope.clone(), r.trust.to_bits(), r.pruefanteil.to_bits())).collect::<Vec<_>>();
        assert_eq!(values(&plain), values(&with));
        assert_eq!((plain.write_right_code, plain.decisions_right), (with.write_right_code, with.decisions_right));
        let text = with.render();
        let (fixed, rest) = text.split_once("Profil (öffentliche Daten, Stand 2026-10-27; kein Einfluss auf Vertrauen)\n").unwrap();
        assert!(fixed.contains("Kosten:") && !fixed.contains("search @"), "{text}");
        assert!(rest.contains("- Stärken: stark") && rest.contains("- Schwächen: schwach") && rest.contains("- Umfang: klein"));
        assert!(rest.contains("- Einschätzung je Dimension (abgeleitet aus öffentlichen Daten, nicht gemessen): search=mittel"));
        assert!(rest.contains("- Benchmarks: X 1") && rest.contains("- Quellen: https://a ; https://b"));
        assert_eq!(text.replace(&with.profile.as_ref().unwrap().render(), ""), plain.render());
    }
}
