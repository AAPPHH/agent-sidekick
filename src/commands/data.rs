use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{anyhow, Context, Result};
use arrow::datatypes::SchemaRef;

use crate::data::{experience_schema, read_events, read_schema, strip_private, write_events, DataDir, ModelRow, ProtectedRow};
use crate::dispatch::git;

pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
pub const BOT: &str = "github-actions[bot]";
pub const EXEMPT: [&str; 3] = ["data/models.parquet", "data/protected.parquet", "data/peers.toml"];
const PEERS: &str = "data/peers.toml";
const ZERO_REV: &str = "0000000000000000000000000000000000000000";

#[derive(Debug, clap::Args)]
pub struct DataArgs {
    #[command(subcommand)]
    pub command: DataCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum DataCommand {
    /// Check the data/ changes between two revisions against the pusher's prefixes and the data rules.
    Validate {
        #[arg(long)]
        base: String,
        #[arg(long)]
        head: String,
        #[arg(long)]
        pusher: String,
        #[arg(long)]
        root: PathBuf,
    },
    /// Union and dedup all experiences into data/aggregate/latest.parquet.
    Aggregate {
        #[arg(long)]
        root: PathBuf,
    },
}

pub fn run(args: DataArgs) -> Result<ExitCode> {
    match args.command {
        DataCommand::Validate { base, head, pusher, root } => {
            let violations = Validator { root: &root, pusher: &pusher }.validate(&base, &head)?;
            if violations.is_empty() {
                return Ok(ExitCode::SUCCESS);
            }
            println!("{} violation(s):", violations.len());
            violations.iter().for_each(|v| println!("- {v}"));
            Ok(ExitCode::FAILURE)
        }
        DataCommand::Aggregate { root } => {
            let n = aggregate(&root)?;
            println!("{n} events");
            Ok(ExitCode::SUCCESS)
        }
    }
}

pub fn aggregate(root: &Path) -> Result<usize> {
    let data = DataDir::new(root.join("data"));
    let events: Vec<_> = data.events(None)?.iter().map(strip_private).collect();
    let path = data.aggregate_path();
    if !path.exists() || read_events(&path)? != events {
        write_events(&path, &events)?;
    }
    Ok(events.len())
}

fn is_pseudonym(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub struct Validator<'a> {
    pub root: &'a Path,
    pub pusher: &'a str,
}

impl Validator<'_> {
    pub fn validate(&self, base: &str, head: &str) -> Result<Vec<String>> {
        let listing = if base == ZERO_REV {
            git(self.root, &["-c", "core.quotepath=off", "ls-tree", "-r", "--name-only", head])?
        } else {
            git(self.root, &["-c", "core.quotepath=off", "diff", "--name-only", "--no-renames", base, head])?
        };
        let changed: Vec<String> = listing
            .lines()
            .filter(|p| p.starts_with("data/"))
            .map(String::from)
            .collect();
        let prefixes = self.prefixes(base)?;
        let mut out = Vec::new();
        for path in &changed {
            if let Some(v) = self.path_violation(path, &prefixes) {
                out.push(v);
            }
            let full = self.root.join(path);
            if full.is_file() {
                out.extend(self.content_violations(path, &full));
            }
        }
        Ok(out)
    }

    fn prefixes(&self, base: &str) -> Result<Vec<String>> {
        let text = if base == ZERO_REV {
            std::fs::read_to_string(self.root.join(PEERS)).unwrap_or_default()
        } else {
            git(self.root, &["show", &format!("{base}:{PEERS}")]).unwrap_or_default()
        };
        let table: toml::Table = text.parse().with_context(|| format!("parse {PEERS}"))?;
        Ok(table
            .iter()
            .filter(|(_, owner)| owner.as_str().is_some_and(|o| o.eq_ignore_ascii_case(self.pusher)))
            .map(|(prefix, _)| format!("data/experiences/{prefix}/"))
            .collect())
    }

    fn path_violation(&self, path: &str, prefixes: &[String]) -> Option<String> {
        if EXEMPT.contains(&path) || prefixes.iter().any(|p| path.starts_with(p.as_str()) && !path[p.len()..].is_empty()) {
            return None;
        }
        let owned = if prefixes.is_empty() { "no prefix".to_string() } else { prefixes.join(", ") };
        Some(format!("{path}: outside the prefixes of `{}` ({owned})", self.pusher))
    }

    fn content_violations(&self, rel: &str, full: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let size = std::fs::metadata(full).map(|m| m.len()).unwrap_or(0);
        if size > MAX_FILE_BYTES {
            out.push(format!("{rel}: {size} bytes exceeds the limit of {MAX_FILE_BYTES}"));
        }
        if full.extension().is_none_or(|e| e != "parquet") {
            return out;
        }
        let name = rel.trim_start_matches("data/");
        let result = if name == "models.parquet" {
            check_schema(full, ModelRow::schema()).and_then(|_| ModelRow::read_all(full).map(|_| Vec::new()))
        } else if name == "protected.parquet" {
            check_schema(full, ProtectedRow::schema()).and_then(|_| ProtectedRow::read_all(full).map(|_| Vec::new()))
        } else if name.starts_with("experiences/") || name.starts_with("aggregate/") {
            check_schema(full, experience_schema()).and_then(|_| event_violations(full))
        } else {
            Ok(vec!["unknown parquet location".to_string()])
        };
        match result {
            Ok(v) => out.extend(v.into_iter().map(|m| format!("{rel}: {m}"))),
            Err(e) => out.push(format!("{rel}: invalid ({e:#})")),
        }
        out
    }
}

fn check_schema(path: &Path, expected: SchemaRef) -> Result<()> {
    let found = read_schema(path)?;
    let sig = |s: &SchemaRef| s.fields().iter().map(|f| (f.name().clone(), f.data_type().clone())).collect::<Vec<_>>();
    if sig(&found) != sig(&expected) {
        return Err(anyhow!("schema does not match the expected columns"));
    }
    Ok(())
}

fn event_violations(path: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for e in read_events(path)? {
        let id = &e.event_id;
        if e.visibility == crate::trust::Visibility::Private && (e.summary.is_some() || e.conclusion.is_some()) {
            out.push(format!("event {id}: summary/conclusion text on a private row"));
        }
        for (field, value) in [("user_id", &e.user_id), ("machine_id", &e.machine_id)] {
            if !is_pseudonym(value) {
                out.push(format!("event {id}: {field} is not a 16-hex pseudonym"));
            }
        }
        if e.outcome == crate::trust::Outcome::Seed && !e.dimension.seedable() {
            out.push(format!("event {id}: seed for dimension {} (only search/inventory)", e.dimension));
        }
    }
    Ok(out)
}
