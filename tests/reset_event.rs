use std::path::Path;
use std::process::Command;

use agent_sidekick::commands::data::Validator;
use agent_sidekick::data::{write_events, DataDir, ModelRow};
use agent_sidekick::sync::{DataStore, ExperienceInput};
use agent_sidekick::trust::{Dimension, Event, ModelKey, Outcome, Scope, Sperre, Visibility};

fn sh(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn commit(dir: &Path) -> String {
    sh(dir, &["add", "-A"]);
    sh(dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "c"]);
    sh(dir, &["rev-parse", "HEAD"])
}

fn qwen() -> ModelRow {
    ModelRow {
        key: ModelKey::new("dgx", "Qwen3-Coder-Next", "Qwen3-Coder-Next", "none", "opencode"),
        context_tokens: None,
        cost: "shared-dgx".into(),
        max_steps: None,
        tool_output_lines: None,
        context_budget_tokens: None,
        sperre: Sperre::NurDiff,
        added_at: 0,
        added_by: "t".into(),
    }
}

fn reset_event(id: &str) -> Event {
    let mut e = Event::new("0123456789abcdef", Dimension::Search, Scope::Global, Outcome::Reset, 1_000);
    e.event_id = id.into();
    e.user_id = "0123456789abcdef".into();
    e.machine_id = "fedcba9876543210".into();
    e
}

const PEERS: &str = "owners = [\"Owen\"]\nalice = \"Alice\"\nowen = \"owen\"\n";

#[test]
fn validate_rejects_reset_rows_from_non_owners() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    sh(root, &["init", "-q"]);
    std::fs::write(root.join("peers.toml"), PEERS).unwrap();
    let base = commit(root);
    write_events(&root.join("experiences/owen/s.parquet"), &[reset_event("a")]).unwrap();
    let head = commit(root);
    assert!(Validator { root, pusher: "owen" }.validate(&base, &head).unwrap().is_empty());
    write_events(&root.join("experiences/alice/s.parquet"), &[reset_event("b")]).unwrap();
    let head = commit(root);
    let v = Validator { root, pusher: "alice" }.validate(&base, &head).unwrap();
    assert!(v.iter().any(|m| m.contains("alice") && m.contains("reset row")), "{v:?}");
}

fn store(name: &str, peers: &str) -> (tempfile::TempDir, DataStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = DataStore::open(dir.path(), &dir.path().join("none.git").to_string_lossy(), Some(name)).unwrap();
    ModelRow::write_all(&store.checkout.join("models.parquet"), &[qwen()]).unwrap();
    std::fs::write(store.checkout.join("peers.toml"), peers).unwrap();
    (dir, store)
}

fn input(model_id: &str, outcome: Outcome) -> ExperienceInput {
    ExperienceInput {
        model_id: model_id.into(),
        dimension: Dimension::Search,
        scope: Scope::parse("repo:x").unwrap(),
        outcome,
        job_id: None,
        steps: None,
        duration_s: None,
        review_s: None,
        seed_value: None,
        summary: None,
        conclusion: None,
        visibility: Visibility::Private,
    }
}

#[test]
fn record_reset_needs_an_owner_prefix_and_resets_the_steckbrief() {
    let id = qwen().model_id();
    let (_d, owner) = store("owen", PEERS);
    let mut rejected = input(&id, Outcome::Rejected);
    rejected.conclusion = Some("Datei nennen".into());
    owner.record(rejected).unwrap();
    let mut with_job = input(&id, Outcome::Reset);
    with_job.job_id = Some("j".into());
    assert!(owner.record(with_job).is_err());
    owner.record(input(&id, Outcome::Reset)).unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let sb = DataDir::new(&owner.checkout).steckbrief(&id, &Scope::Global, Some(&owner.local_root), now).unwrap();
    let text = sb.render();
    assert!(text.contains("zurückgesetzt am "), "{text}");
    assert!(!text.contains("rej 1"), "{text}");
    assert!(sb.rows.iter().all(|r| r.counters.rejected == 0));
    let (_d2, other) = store("alice", PEERS);
    let err = other.record(input(&id, Outcome::Reset)).unwrap_err().to_string();
    assert!(err.contains("only allowed for owners"), "{err}");
    let (_d3, none) = store("owen", "");
    assert!(none.record(input(&id, Outcome::Reset)).is_err());
}

#[test]
fn peers_prefix_is_exact_and_case_colliding_keys_are_rejected() {
    let table: toml::Table = "owners = [\"mallory\"]\nzed = \"mallory\"\n".parse().unwrap();
    assert!(agent_sidekick::sync::peers_owner(&table, "zed"));
    assert!(!agent_sidekick::sync::peers_owner(&table, "Zed"));
    assert!(!agent_sidekick::sync::peers_owner(&table, "owners"));
    let clash: toml::Table = "owners = [\"owen\"]\nZed = \"owen\"\nzed = \"mallory\"\n".parse().unwrap();
    assert!(agent_sidekick::sync::peers_check(&clash).is_err());
    assert!(!agent_sidekick::sync::peers_owner(&clash, "Zed"));
    assert!(!agent_sidekick::sync::peers_owner(&clash, "zed"));

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    sh(root, &["init", "-q"]);
    std::fs::write(root.join("peers.toml"), "owners = [\"owen\"]\nZed = \"owen\"\nzed = \"mallory\"\n").unwrap();
    let base = commit(root);
    std::fs::write(root.join("README"), "x").unwrap();
    let head = commit(root);
    assert!(Validator { root, pusher: "mallory" }.validate(&base, &head).is_err());
}

#[test]
fn reset_files_carry_schema_version_2_and_both_versions_read() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain.parquet");
    let with_reset = dir.path().join("reset.parquet");
    let note = Event::new("0123456789abcdef", Dimension::Search, Scope::Global, Outcome::Note, 1);
    write_events(&plain, &[note.clone()]).unwrap();
    write_events(&with_reset, &[note, reset_event("a")]).unwrap();
    let version = |p: &Path| {
        let f = std::fs::File::open(p).unwrap();
        let b = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(f).unwrap();
        b.metadata().file_metadata().key_value_metadata().unwrap().iter().find(|k| k.key == "schema_version").unwrap().value.clone().unwrap()
    };
    assert_eq!(version(&plain), "1");
    assert_eq!(version(&with_reset), "2");
    assert_eq!(agent_sidekick::data::read_events(&plain).unwrap().len(), 1);
    assert_eq!(agent_sidekick::data::read_events(&with_reset).unwrap().len(), 2);
}
