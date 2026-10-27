use std::collections::{HashMap, HashSet};
use std::fmt;

use anyhow::{anyhow, bail, Result};
use sha2::{Digest, Sha256};

pub const MS_PER_DAY: i64 = 86_400_000;
pub const HALF_WEIGHT_AFTER_MS: i64 = 90 * MS_PER_DAY;
pub const ACCEPT_RATE: f64 = 0.05;
pub const REJECT_RATE: f64 = 0.15;
pub const BLOCK_RATE: f64 = 0.50;
pub const ACCEPT_CAP: f64 = 0.95;
pub const WRITE_RIGHT_MIN: f64 = 0.50;
pub const DECISIONS_RIGHT_MIN: f64 = 0.85;
pub const NUR_DIFF_WRITE_CAP: f64 = 0.60;
pub const MODEL_ID_LEN: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ModelKey {
    pub provider: String,
    pub model: String,
    pub checkpoint: String,
    pub quant: String,
    pub harness: String,
}

impl ModelKey {
    pub fn new(provider: &str, model: &str, checkpoint: &str, quant: &str, harness: &str) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            checkpoint: checkpoint.into(),
            quant: quant.into(),
            harness: harness.into(),
        }
    }

    pub fn model_id(&self) -> String {
        let mut hasher = Sha256::new();
        for part in [&self.provider, &self.model, &self.checkpoint, &self.quant, &self.harness] {
            hasher.update((part.len() as u64).to_be_bytes());
            hasher.update(part.as_bytes());
        }
        let hex: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        hex[..MODEL_ID_LEN].to_string()
    }

    pub fn is_gold(&self) -> bool {
        self.provider == "anthropic" && self.model == "claude-opus-5-5" && self.harness == "claude-code"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Dimension {
    Search,
    Inventory,
    WriteTests,
    WriteCode,
    Decisions,
}

impl Dimension {
    pub const ALL: [Dimension; 5] = [
        Dimension::Search,
        Dimension::Inventory,
        Dimension::WriteTests,
        Dimension::WriteCode,
        Dimension::Decisions,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Dimension::Search => "search",
            Dimension::Inventory => "inventory",
            Dimension::WriteTests => "write-tests",
            Dimension::WriteCode => "write-code",
            Dimension::Decisions => "decisions",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|d| d.as_str() == s)
            .ok_or_else(|| anyhow!("unknown dimension `{s}`"))
    }

    pub fn start(self) -> f64 {
        match self {
            Dimension::Search | Dimension::Inventory => 0.30,
            Dimension::WriteTests | Dimension::WriteCode => 0.20,
            Dimension::Decisions => 0.10,
        }
    }

    pub fn is_write(self) -> bool {
        matches!(self, Dimension::WriteTests | Dimension::WriteCode)
    }

    pub fn seedable(self) -> bool {
        matches!(self, Dimension::Search | Dimension::Inventory)
    }
}

impl fmt::Display for Dimension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scope {
    Global,
    Repo(String),
    Module(String, String),
}

impl Scope {
    pub fn parse(s: &str) -> Result<Self> {
        if s == "global" {
            return Ok(Scope::Global);
        }
        if let Some(name) = s.strip_prefix("repo:") {
            if name.is_empty() || name.contains('/') {
                bail!("invalid scope `{s}`");
            }
            return Ok(Scope::Repo(name.into()));
        }
        if let Some(rest) = s.strip_prefix("module:") {
            if let Some((repo, path)) = rest.split_once('/') {
                if !repo.is_empty() && !path.is_empty() {
                    return Ok(Scope::Module(repo.into(), path.into()));
                }
            }
        }
        bail!("invalid scope `{s}` (expected global, repo:<name> or module:<repo>/<path>)")
    }

    pub fn parent(&self) -> Option<Scope> {
        match self {
            Scope::Global => None,
            Scope::Repo(_) => Some(Scope::Global),
            Scope::Module(repo, _) => Some(Scope::Repo(repo.clone())),
        }
    }

    pub fn chain(&self) -> Vec<Scope> {
        let mut out = vec![self.clone()];
        while let Some(p) = out.last().and_then(Scope::parent) {
            out.push(p);
        }
        out
    }

    pub fn repo(&self) -> Option<&str> {
        match self {
            Scope::Global => None,
            Scope::Repo(r) | Scope::Module(r, _) => Some(r),
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scope::Global => f.write_str("global"),
            Scope::Repo(r) => write!(f, "repo:{r}"),
            Scope::Module(r, p) => write!(f, "module:{r}/{p}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Outcome {
    Accepted,
    Rejected,
    Aborted,
    Blocked,
    Note,
    Seed,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Accepted => "accepted",
            Outcome::Rejected => "rejected",
            Outcome::Aborted => "aborted",
            Outcome::Blocked => "blocked",
            Outcome::Note => "note",
            Outcome::Seed => "seed",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        [
            Outcome::Accepted,
            Outcome::Rejected,
            Outcome::Aborted,
            Outcome::Blocked,
            Outcome::Note,
            Outcome::Seed,
        ]
        .into_iter()
        .find(|o| o.as_str() == s)
        .ok_or_else(|| anyhow!("unknown outcome `{s}`"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sperre {
    Technisch,
    NurDiff,
}

impl Sperre {
    pub fn as_str(self) -> &'static str {
        match self {
            Sperre::Technisch => "technisch",
            Sperre::NurDiff => "nur-diff",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "technisch" => Ok(Sperre::Technisch),
            "nur-diff" => Ok(Sperre::NurDiff),
            _ => bail!("unknown sperre `{s}`"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    Private,
    Shared,
}

impl Visibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Visibility::Private => "private",
            Visibility::Shared => "shared",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "private" => Ok(Visibility::Private),
            "shared" => Ok(Visibility::Shared),
            _ => bail!("unknown visibility `{s}`"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub event_id: String,
    pub ts_ms: i64,
    pub machine_id: String,
    pub user_id: String,
    pub model_id: String,
    pub dimension: Dimension,
    pub scope: Scope,
    pub repo: Option<String>,
    pub job_id: Option<String>,
    pub steps: Option<i64>,
    pub duration_s: Option<f64>,
    pub review_s: Option<f64>,
    pub outcome: Outcome,
    pub seed_value: Option<f64>,
    pub block_reason: Option<String>,
    pub evidence_hash: Option<String>,
    pub summary: Option<String>,
    pub conclusion: Option<String>,
    pub visibility: Visibility,
}

impl Event {
    pub fn new(model_id: &str, dimension: Dimension, scope: Scope, outcome: Outcome, ts_ms: i64) -> Self {
        Self {
            event_id: uuid::Uuid::new_v4().to_string(),
            ts_ms,
            machine_id: String::new(),
            user_id: String::new(),
            model_id: model_id.into(),
            dimension,
            repo: scope.repo().map(str::to_string),
            scope,
            job_id: None,
            steps: None,
            duration_s: None,
            review_s: None,
            outcome,
            seed_value: None,
            block_reason: None,
            evidence_hash: None,
            summary: None,
            conclusion: None,
            visibility: Visibility::Shared,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub accepted: u32,
    pub rejected: u32,
    pub aborted: u32,
    pub blocked: u32,
    pub note: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LastBlock {
    pub ts_ms: i64,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    pub value: f64,
    pub counters: Counters,
    pub last_block: Option<LastBlock>,
    pub manual: bool,
}

impl Cell {
    fn new(value: f64) -> Self {
        Self { value, counters: Counters::default(), last_block: None, manual: false }
    }
}

pub fn dedup_sorted(events: impl IntoIterator<Item = Event>) -> Vec<Event> {
    let mut seen = HashSet::new();
    let mut out: Vec<Event> = events.into_iter().filter(|e| seen.insert(e.event_id.clone())).collect();
    out.sort_by(|a, b| a.ts_ms.cmp(&b.ts_ms).then_with(|| a.event_id.cmp(&b.event_id)));
    out
}

#[derive(Clone, Debug, Default)]
pub struct TrustBook {
    cells: HashMap<(Dimension, Scope), Cell>,
}

impl TrustBook {
    pub fn replay(events: &[Event], model_id: &str, now_ms: i64) -> Self {
        let mut book = Self::default();
        let own = events.iter().filter(|e| e.model_id == model_id).cloned();
        for e in dedup_sorted(own) {
            book.apply(&e, now_ms);
        }
        book
    }

    pub fn value(&self, dimension: Dimension, scope: &Scope) -> f64 {
        self.resolve(dimension, scope).map_or(dimension.start(), |c| c.value)
    }

    pub fn resolve(&self, dimension: Dimension, scope: &Scope) -> Option<&Cell> {
        scope.chain().into_iter().find_map(|s| self.cells.get(&(dimension, s)))
    }

    pub fn cell(&self, dimension: Dimension, scope: &Scope) -> Option<&Cell> {
        self.cells.get(&(dimension, scope.clone()))
    }

    pub fn cells(&self) -> impl Iterator<Item = (&(Dimension, Scope), &Cell)> {
        self.cells.iter()
    }

    fn entry(&mut self, dimension: Dimension, scope: &Scope) -> &mut Cell {
        if !self.cells.contains_key(&(dimension, scope.clone())) {
            let inherited = self.value(dimension, scope);
            self.cells.insert((dimension, scope.clone()), Cell::new(inherited));
        }
        self.cells.get_mut(&(dimension, scope.clone())).expect("inserted above")
    }

    fn apply(&mut self, e: &Event, now_ms: i64) {
        let weight = if now_ms - e.ts_ms > HALF_WEIGHT_AFTER_MS { 0.5 } else { 1.0 };
        let cell = self.entry(e.dimension, &e.scope);
        match e.outcome {
            Outcome::Accepted => {
                cell.counters.accepted += 1;
                if cell.value < ACCEPT_CAP {
                    cell.value = (cell.value + (1.0 - cell.value) * ACCEPT_RATE * weight).min(ACCEPT_CAP);
                }
            }
            Outcome::Rejected => {
                cell.counters.rejected += 1;
                cell.value -= cell.value * REJECT_RATE * weight;
            }
            Outcome::Aborted => cell.counters.aborted += 1,
            Outcome::Note => cell.counters.note += 1,
            Outcome::Blocked => {
                cell.counters.blocked += 1;
                cell.value -= cell.value * BLOCK_RATE * weight;
                cell.last_block = Some(LastBlock { ts_ms: e.ts_ms, reason: e.block_reason.clone().unwrap_or_default() });
                if e.scope != Scope::Global {
                    let global = self.entry(e.dimension, &Scope::Global);
                    global.value -= global.value * BLOCK_RATE * weight;
                    global.last_block = Some(LastBlock { ts_ms: e.ts_ms, reason: e.block_reason.clone().unwrap_or_default() });
                }
            }
            Outcome::Seed => {
                if let (true, Some(v)) = (e.dimension.seedable(), e.seed_value) {
                    cell.value = v.clamp(0.0, 1.0);
                    cell.manual = true;
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Standing {
    pub model_id: String,
    pub gold: bool,
    pub sperre: Sperre,
    pub book: TrustBook,
}

impl Standing {
    pub fn compute(key: &ModelKey, sperre: Sperre, events: &[Event], now_ms: i64) -> Self {
        let model_id = key.model_id();
        let book = TrustBook::replay(events, &model_id, now_ms);
        Self { model_id, gold: key.is_gold(), sperre, book }
    }

    pub fn trust(&self, dimension: Dimension, scope: &Scope) -> f64 {
        if self.gold {
            1.0
        } else {
            self.book.value(dimension, scope)
        }
    }

    pub fn effective_trust(&self, dimension: Dimension, scope: &Scope) -> f64 {
        let t = self.trust(dimension, scope);
        if !self.gold && dimension.is_write() && self.sperre == Sperre::NurDiff {
            t.min(NUR_DIFF_WRITE_CAP)
        } else {
            t
        }
    }

    pub fn pruefanteil(&self, dimension: Dimension, scope: &Scope) -> f64 {
        1.0 - self.effective_trust(dimension, scope)
    }

    pub fn write_right(&self, dimension: Dimension, scope: &Scope) -> bool {
        dimension.is_write() && self.effective_trust(dimension, scope) >= WRITE_RIGHT_MIN
    }

    pub fn decisions_right(&self, scope: &Scope) -> bool {
        self.trust(Dimension::Decisions, scope) >= DECISIONS_RIGHT_MIN
    }

    pub fn manual(&self, dimension: Dimension, scope: &Scope) -> bool {
        !self.gold && self.book.resolve(dimension, scope).is_some_and(|c| c.manual)
    }

    pub fn protected_locked(&self) -> bool {
        !self.gold
    }

    pub fn apply_without_review(&self) -> bool {
        self.gold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 10_000 * MS_PER_DAY;
    const BASE: i64 = NOW - 1_000_000;

    fn ev(model: &str, d: Dimension, scope: &str, o: Outcome, ts: i64) -> Event {
        Event::new(model, d, Scope::parse(scope).unwrap(), o, BASE + ts)
    }

    fn seed(model: &str, d: Dimension, v: f64, ts: i64) -> Event {
        let mut e = ev(model, d, "global", Outcome::Seed, ts);
        e.seed_value = Some(v);
        e
    }

    fn qwen() -> ModelKey {
        ModelKey::new("dgx", "qwen3-coder-next", "qwen3-coder-next", "q8", "opencode")
    }

    fn near(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn model_id_is_stable_and_checkpoint_sensitive() {
        let a = qwen();
        assert_eq!(a.model_id(), a.clone().model_id());
        assert_eq!(a.model_id().len(), MODEL_ID_LEN);
        let mut b = a.clone();
        b.checkpoint = "other".into();
        assert_ne!(a.model_id(), b.model_id());
        assert_ne!(ModelKey::new("ab", "c", "", "", "").model_id(), ModelKey::new("a", "bc", "", "", "").model_id());
    }

    #[test]
    fn gold_matches_only_opus_in_claude_code() {
        assert!(ModelKey::new("anthropic", "claude-opus-5-5", "claude-opus-5-5-20261001", "fp8", "claude-code").is_gold());
        assert!(!ModelKey::new("anthropic", "claude-opus-5-5", "x", "", "opencode").is_gold());
        assert!(!ModelKey::new("anthropic", "claude-sonnet-5-5", "x", "", "claude-code").is_gold());
        assert!(!ModelKey::new("other", "claude-opus-5-5", "x", "", "claude-code").is_gold());
    }

    #[test]
    fn scope_parse_and_fallback() {
        let m = Scope::parse("module:r/src/a").unwrap();
        assert_eq!(m, Scope::Module("r".into(), "src/a".into()));
        assert_eq!(m.to_string(), "module:r/src/a");
        assert_eq!(m.chain(), vec![m.clone(), Scope::Repo("r".into()), Scope::Global]);
        assert!(Scope::parse("repo:").is_err());
        assert!(Scope::parse("module:r").is_err());
        assert!(Scope::parse("x").is_err());
    }

    #[test]
    fn qwen_start_data() {
        let id = qwen().model_id();
        let mut blocked = ev(&id, Dimension::WriteCode, "global", Outcome::Blocked, 1000);
        blocked.block_reason = Some("Validierung decided_by entfernt".into());
        let events = vec![seed(&id, Dimension::Search, 0.55, 3), seed(&id, Dimension::Inventory, 0.35, 2), blocked];
        let s = Standing::compute(&qwen(), Sperre::Technisch, &events, NOW);
        let g = Scope::Global;
        near(s.trust(Dimension::Search, &g), 0.55);
        near(s.trust(Dimension::Inventory, &g), 0.35);
        near(s.trust(Dimension::WriteCode, &g), 0.10);
        near(s.trust(Dimension::WriteTests, &g), 0.20);
        near(s.trust(Dimension::Decisions, &g), 0.10);
        assert!(s.manual(Dimension::Search, &g));
        assert!(!s.manual(Dimension::WriteCode, &g));
        assert!(!s.write_right(Dimension::WriteCode, &g));
        near(s.pruefanteil(Dimension::Search, &g), 0.45);
        assert_eq!(s.book.cell(Dimension::WriteCode, &g).unwrap().last_block.as_ref().unwrap().reason, "Validierung decided_by entfernt");
    }

    #[test]
    fn seed_ignored_for_write_and_decisions() {
        let mut e = seed("m", Dimension::WriteCode, 0.9, 1);
        let mut d = seed("m", Dimension::Decisions, 0.9, 2);
        e.seed_value = Some(0.9);
        d.seed_value = Some(0.9);
        let book = TrustBook::replay(&[e, d], "m", NOW);
        near(book.value(Dimension::WriteCode, &Scope::Global), 0.20);
        near(book.value(Dimension::Decisions, &Scope::Global), 0.10);
    }

    #[test]
    fn accepted_rejected_and_cap() {
        let g = Scope::Global;
        let acc = |ts| ev("m", Dimension::Search, "global", Outcome::Accepted, ts);
        near(TrustBook::replay(&[acc(1)], "m", NOW).value(Dimension::Search, &g), 0.30 + 0.70 * 0.05);
        let rej = ev("m", Dimension::Search, "global", Outcome::Rejected, 1);
        near(TrustBook::replay(&[rej], "m", NOW).value(Dimension::Search, &g), 0.30 * 0.85);
        let many: Vec<Event> = (0..200).map(acc).collect();
        near(TrustBook::replay(&many, "m", NOW).value(Dimension::Search, &g), 0.95);
        let mut high = vec![seed("m", Dimension::Search, 0.97, 0)];
        high.push(acc(1));
        near(TrustBook::replay(&high, "m", NOW).value(Dimension::Search, &g), 0.97);
    }

    #[test]
    fn aborted_and_note_are_noops_but_counted() {
        let events = vec![ev("m", Dimension::Search, "global", Outcome::Aborted, 1), ev("m", Dimension::Search, "global", Outcome::Note, 2)];
        let book = TrustBook::replay(&events, "m", NOW);
        near(book.value(Dimension::Search, &Scope::Global), 0.30);
        let c = &book.cell(Dimension::Search, &Scope::Global).unwrap().counters;
        assert_eq!((c.aborted, c.note), (1, 1));
    }

    #[test]
    fn old_events_count_half_but_seed_does_not() {
        let old = -91 * MS_PER_DAY;
        let rej = ev("m", Dimension::Search, "global", Outcome::Rejected, old);
        near(TrustBook::replay(&[rej], "m", NOW).value(Dimension::Search, &Scope::Global), 0.30 * (1.0 - 0.075));
        let acc = ev("m", Dimension::Search, "global", Outcome::Accepted, old);
        near(TrustBook::replay(&[acc], "m", NOW).value(Dimension::Search, &Scope::Global), 0.30 + 0.70 * 0.025);
        let s = seed("m", Dimension::Search, 0.55, old);
        near(TrustBook::replay(&[s], "m", NOW).value(Dimension::Search, &Scope::Global), 0.55);
    }

    #[test]
    fn dedup_by_event_id_and_ts_order() {
        let a = ev("m", Dimension::Search, "global", Outcome::Rejected, 5);
        let mut dup = a.clone();
        dup.ts_ms = 9;
        let book = TrustBook::replay(&[a.clone(), dup, a], "m", NOW);
        near(book.value(Dimension::Search, &Scope::Global), 0.30 * 0.85);
        let s = seed("m", Dimension::Search, 0.8, 2);
        let r = ev("m", Dimension::Search, "global", Outcome::Rejected, 3);
        near(TrustBook::replay(&[r.clone(), s.clone()], "m", NOW).value(Dimension::Search, &Scope::Global), 0.8 * 0.85);
        let r_early = ev("m", Dimension::Search, "global", Outcome::Rejected, 1);
        near(TrustBook::replay(&[s, r_early], "m", NOW).value(Dimension::Search, &Scope::Global), 0.8);
    }

    #[test]
    fn module_can_exceed_global_and_falls_back() {
        let mut events: Vec<Event> = (0..10).map(|i| ev("m", Dimension::WriteCode, "module:r/src", Outcome::Accepted, i)).collect();
        events.push(ev("m", Dimension::WriteCode, "global", Outcome::Rejected, 100));
        let book = TrustBook::replay(&events, "m", NOW);
        let module = book.value(Dimension::WriteCode, &Scope::parse("module:r/src").unwrap());
        let global = book.value(Dimension::WriteCode, &Scope::Global);
        assert!(module > global);
        near(book.value(Dimension::WriteCode, &Scope::parse("module:r/other").unwrap()), global);
        near(book.value(Dimension::WriteCode, &Scope::parse("repo:r").unwrap()), global);
    }

    #[test]
    fn blocked_in_module_also_lowers_global() {
        let mut b = ev("m", Dimension::WriteCode, "module:r/src", Outcome::Blocked, 1);
        b.block_reason = Some("x".into());
        let book = TrustBook::replay(&[b], "m", NOW);
        near(book.value(Dimension::WriteCode, &Scope::parse("module:r/src").unwrap()), 0.10);
        near(book.value(Dimension::WriteCode, &Scope::Global), 0.10);
        assert_eq!(book.cell(Dimension::WriteCode, &Scope::Global).unwrap().counters.blocked, 0);
    }

    #[test]
    fn events_of_other_models_are_ignored() {
        let e = ev("other", Dimension::Search, "global", Outcome::Rejected, 1);
        near(TrustBook::replay(&[e], "m", NOW).value(Dimension::Search, &Scope::Global), 0.30);
    }

    #[test]
    fn gold_is_one_everywhere() {
        let key = ModelKey::new("anthropic", "claude-opus-5-5", "c", "", "claude-code");
        let mut b = ev(&key.model_id(), Dimension::WriteCode, "global", Outcome::Blocked, 1);
        b.block_reason = Some("x".into());
        let s = Standing::compute(&key, Sperre::Technisch, &[b], NOW);
        let scope = Scope::parse("module:r/x").unwrap();
        for d in Dimension::ALL {
            near(s.trust(d, &scope), 1.0);
        }
        assert!(s.write_right(Dimension::WriteCode, &scope) && s.decisions_right(&scope));
        assert!(s.apply_without_review() && !s.protected_locked());
        near(s.pruefanteil(Dimension::Search, &scope), 0.0);
        assert!(!Standing::compute(&qwen(), Sperre::Technisch, &[], NOW).apply_without_review());
    }

    #[test]
    fn rights_and_nur_diff_cap() {
        let key = qwen();
        let id = key.model_id();
        let mut events = vec![];
        for i in 0..100 {
            events.push(ev(&id, Dimension::WriteCode, "global", Outcome::Accepted, i));
            events.push(ev(&id, Dimension::Decisions, "global", Outcome::Accepted, i));
        }
        let g = Scope::Global;
        let tech = Standing::compute(&key, Sperre::Technisch, &events, NOW);
        near(tech.trust(Dimension::WriteCode, &g), 0.95);
        assert!(tech.write_right(Dimension::WriteCode, &g));
        assert!(!tech.write_right(Dimension::Search, &g));
        assert!(tech.decisions_right(&g));
        let nd = Standing::compute(&key, Sperre::NurDiff, &events, NOW);
        near(nd.effective_trust(Dimension::WriteCode, &g), 0.60);
        near(nd.pruefanteil(Dimension::WriteCode, &g), 0.40);
        near(nd.effective_trust(Dimension::Decisions, &g), 0.95);
        let low = Standing::compute(&key, Sperre::Technisch, &[], NOW);
        assert!(!low.write_right(Dimension::WriteCode, &g) && !low.decisions_right(&g));
    }
}
