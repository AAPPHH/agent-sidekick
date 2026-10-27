use std::path::{Path, PathBuf};
use std::process::Command;

use agent_sidekick::data::{pseudonym, read_events, write_events, DataDir, ModelRow};
use agent_sidekick::sync::{DataStore, ExperienceInput};
use agent_sidekick::trust::{Dimension, Event, ModelKey, Outcome, Scope, Sperre, Visibility};

fn sh(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn commit_all(dir: &Path, msg: &str) {
    sh(dir, &["add", "-A"]);
    sh(dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", msg]);
}

fn model() -> ModelRow {
    ModelRow {
        key: ModelKey::new("p", "m", "c", "none", "claude-code"),
        context_tokens: None,
        cost: "free".into(),
        max_steps: None,
        tool_output_lines: None,
        context_budget_tokens: None,
        sperre: Sperre::Technisch,
        added_at: 0,
        added_by: "t".into(),
    }
}

fn init_remote(bare: &Path, work: &Path) {
    std::fs::create_dir_all(bare).unwrap();
    sh(bare, &["init", "-q", "--bare", "-b", "main"]);
    std::fs::create_dir_all(work).unwrap();
    sh(work, &["init", "-q", "-b", "main"]);
    sh(work, &["remote", "add", "origin", &bare.to_string_lossy()]);
    ModelRow::write_all(&work.join("data/models.parquet"), &[model()]).unwrap();
    std::fs::write(work.join("README.md"), "code\n").unwrap();
    commit_all(work, "init");
    sh(work, &["push", "-q", "origin", "main"]);
}

struct Fx {
    _dir: tempfile::TempDir,
    bare: PathBuf,
    peer: PathBuf,
    base: PathBuf,
}

fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let bare = dir.path().join("remote.git");
    let peer = dir.path().join("peer");
    init_remote(&bare, &peer);
    let base = dir.path().join("app");
    Fx { _dir: dir, bare, peer, base }
}

fn open(f: &Fx) -> DataStore {
    let s = DataStore::open(&f.base, &f.bare.to_string_lossy(), Some("me")).unwrap();
    s.pull().unwrap();
    s
}

fn input(outcome: Outcome, dimension: Dimension) -> ExperienceInput {
    ExperienceInput {
        model_id: model().model_id(),
        dimension,
        scope: Scope::Global,
        outcome,
        job_id: None,
        steps: Some(3),
        duration_s: Some(10.0),
        review_s: Some(2.0),
        seed_value: None,
        summary: Some("secret summary".into()),
        conclusion: Some("secret conclusion".into()),
        visibility: Visibility::Private,
    }
}

fn remote_files(f: &Fx, rev: &str) -> Vec<String> {
    sh(&f.bare, &["show", "--name-only", "--pretty=format:", rev]).lines().filter(|l| !l.is_empty()).map(String::from).collect()
}

#[test]
fn only_own_path_is_committed_and_pushed() {
    let f = fx();
    let s = open(&f);
    assert!(s.checkout.join("data/models.parquet").exists());
    assert!(!s.checkout.join("README.md").exists());
    std::fs::write(s.checkout.join("data/stray.txt"), "x").unwrap();
    let (_, pushed) = s.record(input(Outcome::Accepted, Dimension::WriteCode)).unwrap();
    assert!(pushed);
    let files = remote_files(&f, "main");
    assert_eq!(files.len(), 1);
    assert!(files[0].starts_with("data/experiences/me/") && files[0].ends_with(".parquet"));
    assert!(sh(&f.bare, &["log", "-1", "--format=%s", "main"]).starts_with("data: "));
    assert!(sh(&s.checkout, &["status", "--porcelain"]).contains("stray.txt"));
    assert!(s.local_root.join("experiences/me").read_dir().unwrap().count() == 1);
}

#[test]
fn rebase_retry_after_concurrent_push() {
    let f = fx();
    let s = open(&f);
    s.record(input(Outcome::Accepted, Dimension::Search)).unwrap();
    let other = f.peer.join("data/experiences/other/o.parquet");
    write_events(&other, &[Event::new(&model().model_id(), Dimension::Search, Scope::Global, Outcome::Accepted, 1)]).unwrap();
    commit_all(&f.peer, "data: other");
    sh(&f.peer, &["pull", "-q", "--rebase", "origin", "main"]);
    sh(&f.peer, &["push", "-q", "origin", "main"]);
    let (_, pushed) = s.record(input(Outcome::Rejected, Dimension::Search)).unwrap();
    assert!(pushed);
    let log = sh(&f.bare, &["log", "--format=%s", "main"]);
    assert!(log.contains("data: other") && log.matches("data: me").count() == 2);
    assert!(!s.pending());
    assert_eq!(DataDir::experience_files(&s.data.root).unwrap().len(), 2);
}

#[test]
fn offline_keeps_pending_and_pushes_on_sync() {
    let f = fx();
    let gone = f.bare.with_file_name("gone.git");
    let s = open(&f);
    std::fs::rename(&f.bare, &gone).unwrap();
    let (_, pushed) = s.record(input(Outcome::Accepted, Dimension::Search)).unwrap();
    assert!(!pushed && s.pending());
    assert!(s.sync().unwrap().contains("pending"));
    std::fs::rename(&gone, &f.bare).unwrap();
    let report = s.sync().unwrap();
    assert!(report.contains("pushed"), "{report}");
    assert!(!s.pending());
    assert!(remote_files(&f, "main").iter().any(|p| p.starts_with("data/experiences/me/")));
}

#[test]
fn pseudonyms_are_stable_and_raw_values_stay_local() {
    let f = fx();
    let s = open(&f);
    s.record(input(Outcome::Accepted, Dimension::Search)).unwrap();
    s.record(input(Outcome::Accepted, Dimension::Search)).unwrap();
    let clone = f.peer.parent().unwrap().join("check");
    sh(f.peer.parent().unwrap(), &["clone", "-q", &f.bare.to_string_lossy(), &clone.to_string_lossy()]);
    let file = DataDir::experience_files(&clone.join("data")).unwrap().remove(0);
    let events = read_events(&file).unwrap();
    assert_eq!(events.len(), 2);
    let secret = std::fs::read_to_string(f.base.join("secret")).unwrap();
    let uid = pseudonym(secret.as_bytes(), &s.user_id);
    let mid = pseudonym(secret.as_bytes(), &s.machine_id);
    for e in &events {
        assert_eq!(e.user_id, uid);
        assert_eq!(e.machine_id, mid);
        assert!(e.summary.is_none() && e.conclusion.is_none());
    }
    assert_eq!(uid.len(), 16);
    let bytes = std::fs::read(&file).unwrap();
    let has = |needle: &str| bytes.windows(needle.len()).any(|w| w == needle.as_bytes());
    assert!(!has(&s.machine_id) && !has("secret summary") && !has(&secret));
    assert!(!has(&s.user_id) || s.user_id.len() < 4);
    let local = read_events(&DataDir::experience_files(&s.local_root).unwrap()[0]).unwrap();
    assert_eq!(local[0].user_id, s.user_id);
    assert_eq!(local[0].summary.as_deref(), Some("secret summary"));
    assert!(!clone.join("secret").exists());
}

#[test]
fn record_experience_validation() {
    let f = fx();
    let s = open(&f);
    let mut seed = input(Outcome::Seed, Dimension::WriteCode);
    seed.seed_value = Some(0.9);
    assert!(s.record(seed.clone()).unwrap_err().to_string().contains("only allowed for search and inventory"));
    seed.dimension = Dimension::Decisions;
    assert!(s.record(seed.clone()).is_err());
    seed.dimension = Dimension::Search;
    seed.seed_value = None;
    assert!(s.record(seed.clone()).is_err());
    seed.seed_value = Some(0.55);
    assert!(s.record(seed).is_ok());
    assert!(s.record(input(Outcome::Blocked, Dimension::WriteCode)).is_err());
    let mut unknown = input(Outcome::Note, Dimension::Search);
    unknown.model_id = "nope".into();
    assert!(s.record(unknown).is_err());
}

#[test]
fn hmac_matches_rfc4231() {
    let mac = agent_sidekick::data::hmac_sha256(b"Jefe", b"what do ya want for nothing?");
    let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
}

#[test]
fn pushed_copy_of_a_private_block_reason_has_no_file_paths() {
    use agent_sidekick::data::shared_copy;
    let mut e = Event::new("0123456789abcdef", Dimension::WriteCode, Scope::Global, Outcome::Blocked, 1);
    e.visibility = Visibility::Private;
    let full = "protected path src/secret/validate.rs (matches **/validate*); removed protected term `decided_by` in src/secret/a.rs; 20 changed lines exceed limit 10";
    e.block_reason = Some(full.into());
    let pushed = shared_copy(&e, None);
    assert_eq!(pushed.block_reason.as_deref(), Some("protected path; removed term decided_by; 20 changed lines exceed limit 10"));
    assert_eq!(e.block_reason.as_deref(), Some(full));
    e.visibility = Visibility::Shared;
    assert_eq!(shared_copy(&e, None).block_reason.as_deref(), Some(full));
}
