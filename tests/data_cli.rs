use std::path::{Path, PathBuf};
use std::process::Command;

use agent_sidekick::commands::data::{aggregate, Validator};
use agent_sidekick::data::{read_events, write_events, ModelRow, ProtectedRow};
use agent_sidekick::trust::{Dimension, Event, ModelKey, Outcome, Scope, Sperre, Visibility};

fn sh(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn commit(dir: &Path, msg: &str) -> String {
    sh(dir, &["add", "-A"]);
    sh(
        dir,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            msg,
        ],
    );
    sh(dir, &["rev-parse", "HEAD"])
}

fn event(id: &str, outcome: Outcome, dimension: Dimension) -> Event {
    let mut e = Event::new("0123456789abcdef", dimension, Scope::Global, outcome, 1_000);
    e.event_id = id.into();
    e.user_id = "0123456789abcdef".into();
    e.machine_id = "fedcba9876543210".into();
    e
}

fn repo() -> (tempfile::TempDir, PathBuf, String) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    sh(&root, &["init", "-q"]);
    std::fs::write(
        root.join("peers.toml"),
        "owners = [\"Owen\"]\nalice = \"Alice\"\nbob = \"bob\"\nowen = \"owen\"\n",
    )
    .unwrap();
    let base = commit(&root, "base");
    (dir, root, base)
}

fn violations(root: &Path, base: &str, pusher: &str) -> Vec<String> {
    let head = sh(root, &["rev-parse", "HEAD"]);
    Validator { root, pusher }.validate(base, &head).unwrap()
}

#[test]
fn valid_push_in_own_prefix_passes() {
    let (_d, root, base) = repo();
    write_events(
        &root.join("experiences/alice/s1.parquet"),
        &[event("a", Outcome::Seed, Dimension::Search)],
    )
    .unwrap();
    commit(&root, "x");
    assert!(violations(&root, &base, "alice").is_empty());
}

#[test]
fn foreign_prefix_aggregate_and_unknown_paths_fail() {
    let (_d, root, base) = repo();
    let e = [event("a", Outcome::Accepted, Dimension::Search)];
    write_events(&root.join("experiences/bob/s1.parquet"), &e).unwrap();
    write_events(&root.join("aggregate/latest.parquet"), &e).unwrap();
    std::fs::write(root.join("notes.txt"), "x").unwrap();
    commit(&root, "x");
    let v = violations(&root, &base, "alice");
    assert_eq!(
        v.iter()
            .filter(|m| m.contains("outside the prefixes"))
            .count(),
        3,
        "{v:?}"
    );
}

#[test]
fn shared_rule_files_need_valid_schema_and_an_owner() {
    let (_d, root, base) = repo();
    ProtectedRow::write_all(&root.join("protected.parquet"), &ProtectedRow::defaults()).unwrap();
    let key = ModelKey::new("p", "m", "c", "q", "h");
    let row = ModelRow {
        key,
        context_tokens: None,
        cost: "free".into(),
        max_steps: None,
        tool_output_lines: None,
        context_budget_tokens: None,
        sperre: Sperre::Technisch,
        added_at: 0,
        added_by: "x".into(),
    };
    ModelRow::write_all(&root.join("models.parquet"), &[row]).unwrap();
    commit(&root, "x");
    assert!(violations(&root, &base, "owen").is_empty());
    std::fs::write(root.join("models.parquet"), b"garbage").unwrap();
    commit(&root, "y");
    let head = sh(&root, &["rev-parse", "HEAD"]);
    let v = Validator {
        root: &root,
        pusher: "OWEN",
    }
    .validate(&base, &head)
    .unwrap();
    assert!(
        v.iter()
            .any(|m| m.contains("models.parquet") && m.contains("invalid")),
        "{v:?}"
    );
}

#[test]
fn content_rules_are_enforced() {
    let (_d, root, base) = repo();
    let mut private = event("p", Outcome::Note, Dimension::Search);
    private.summary = Some("secret".into());
    private.visibility = Visibility::Private;
    let mut raw = event("r", Outcome::Note, Dimension::Search);
    raw.user_id = "john@example.com".into();
    let bad_seed = event("s", Outcome::Seed, Dimension::WriteCode);
    let mut shared = event("ok", Outcome::Note, Dimension::Search);
    shared.visibility = Visibility::Shared;
    shared.summary = Some("fine".into());
    write_events(
        &root.join("experiences/alice/s1.parquet"),
        &[private, raw, bad_seed, shared],
    )
    .unwrap();
    commit(&root, "x");
    let v = violations(&root, &base, "alice");
    assert_eq!(v.len(), 3, "{v:?}");
    assert!(v.iter().any(|m| m.contains("private")));
    assert!(v.iter().any(|m| m.contains("user_id")));
    assert!(v.iter().any(|m| m.contains("seed")));
}

#[test]
fn oversized_file_fails() {
    let (_d, root, base) = repo();
    let mut big = event("big", Outcome::Note, Dimension::Search);
    big.visibility = Visibility::Shared;
    big.summary = Some(
        "x".repeat(3 * 1024 * 1024)
            .chars()
            .enumerate()
            .map(|(i, _)| char::from(b'a' + ((i * 7919 + i / 13) % 26) as u8))
            .collect(),
    );
    write_events(&root.join("experiences/alice/s1.parquet"), &[big]).unwrap();
    commit(&root, "x");
    let v = violations(&root, &base, "alice");
    assert!(v.iter().any(|m| m.contains("exceeds the limit")), "{v:?}");
}

#[test]
fn cli_exit_codes_and_aggregate() {
    let (_d, root, base) = repo();
    let mut shared = event("a", Outcome::Accepted, Dimension::WriteCode);
    shared.visibility = Visibility::Shared;
    write_events(
        &root.join("experiences/alice/s1.parquet"),
        &[shared.clone()],
    )
    .unwrap();
    write_events(
        &root.join("experiences/bob/s1.parquet"),
        &[
            shared.clone(),
            event("b", Outcome::Rejected, Dimension::WriteCode),
        ],
    )
    .unwrap();
    let head = commit(&root, "x");
    let bin = env!("CARGO_BIN_EXE_agent-sidekick");
    let run = |pusher: &str| {
        Command::new(bin)
            .args([
                "data", "validate", "--base", &base, "--head", &head, "--pusher", pusher, "--root",
            ])
            .arg(&root)
            .output()
            .unwrap()
    };
    assert!(run("bob").status.code() == Some(1));
    assert!(String::from_utf8_lossy(&run("bob").stdout).contains("outside the prefixes"));
    let ok = Command::new(bin)
        .args(["data", "aggregate", "--root"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(ok.status.success());
    let events = read_events(&root.join("aggregate/latest.parquet")).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(aggregate(&root).unwrap(), 2);
}

#[test]
fn initial_push_with_zero_base_is_checked_against_head() {
    let (_d, root, _base) = repo();
    write_events(
        &root.join("experiences/owen/s1.parquet"),
        &[event("a", Outcome::Seed, Dimension::Search)],
    )
    .unwrap();
    let head = commit(&root, "x");
    let zero = "0".repeat(40);
    assert!(Validator {
        root: &root,
        pusher: "owen"
    }
    .validate(&zero, &head)
    .unwrap()
    .is_empty());
    assert!(!Validator {
        root: &root,
        pusher: "alice"
    }
    .validate(&zero, &head)
    .unwrap()
    .is_empty());
}

#[test]
fn non_owner_cannot_change_shared_rules_or_workflows() {
    let (_d, root, base) = repo();
    ProtectedRow::write_all(&root.join("protected.parquet"), &ProtectedRow::defaults()).unwrap();
    std::fs::write(
        root.join("peers.toml"),
        "owners = [\"alice\"]\nalice = \"alice\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join(".github/workflows")).unwrap();
    std::fs::write(
        root.join(".github/workflows/data-validate.yml"),
        "name: x\n",
    )
    .unwrap();
    std::fs::write(root.join(".github/CODEOWNERS"), "* @alice\n").unwrap();
    commit(&root, "x");
    let v = violations(&root, &base, "alice");
    for p in [
        "protected.parquet",
        "peers.toml",
        ".github/workflows/data-validate.yml",
        ".github/CODEOWNERS",
    ] {
        assert!(
            v.iter()
                .any(|m| m.starts_with(p) && m.contains("only the owners")),
            "{p}: {v:?}"
        );
    }
    assert!(violations(&root, &base, "owen").is_empty());
}

#[test]
fn event_id_must_not_exist_under_another_prefix() {
    let (_d, root, _base) = repo();
    let mut shared = event("dup", Outcome::Accepted, Dimension::Search);
    shared.visibility = Visibility::Shared;
    write_events(&root.join("experiences/bob/s1.parquet"), &[shared.clone()]).unwrap();
    let base = commit(&root, "bob");
    write_events(
        &root.join("experiences/alice/s1.parquet"),
        &[shared, event("fresh", Outcome::Seed, Dimension::Search)],
    )
    .unwrap();
    commit(&root, "alice");
    let v = violations(&root, &base, "alice");
    assert_eq!(v.len(), 1, "{v:?}");
    assert!(v[0].contains("event dup") && v[0].contains("`bob`"));
}

#[test]
fn non_ascii_paths_are_keyed_correctly_and_paths_out_lists_them() {
    let (_d, root, base) = repo();
    let name = if cfg!(windows) {
        "ünï'x.txt"
    } else {
        "ünï\"x.txt"
    };
    std::fs::write(root.join(name), "x").unwrap();
    commit(&root, "x");
    let head = sh(&root, &["rev-parse", "HEAD"]);
    let found = Validator {
        root: &root,
        pusher: "alice",
    }
    .violations(&base, &head)
    .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, name);
    let out = root.join("paths.txt");
    let status = Command::new(env!("CARGO_BIN_EXE_agent-sidekick"))
        .args([
            "data", "validate", "--base", &base, "--head", &head, "--pusher", "alice", "--root",
        ])
        .arg(&root)
        .arg("--paths-out")
        .arg(&out)
        .output()
        .unwrap();
    assert_eq!(status.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(out).unwrap(), format!("{name}\n"));
}

#[test]
fn aggregate_ignores_the_previous_aggregate() {
    let (_d, root, _base) = repo();
    let mut stale = event("stale", Outcome::Accepted, Dimension::Search);
    stale.visibility = Visibility::Shared;
    write_events(&root.join("aggregate/latest.parquet"), &[stale]).unwrap();
    let mut live = event("live", Outcome::Accepted, Dimension::Search);
    live.visibility = Visibility::Shared;
    write_events(&root.join("experiences/alice/s1.parquet"), &[live]).unwrap();
    assert_eq!(aggregate(&root).unwrap(), 1);
    let ids: Vec<_> = read_events(&root.join("aggregate/latest.parquet"))
        .unwrap()
        .into_iter()
        .map(|e| e.event_id)
        .collect();
    assert_eq!(ids, ["live"]);
}

#[test]
fn owners_may_change_root_files_but_not_foreign_prefixes() {
    let (_d, root, base) = repo();
    std::fs::write(root.join("README.md"), "x").unwrap();
    commit(&root, "x");
    assert!(violations(&root, &base, "owen").is_empty());
    let v = violations(&root, &base, "alice");
    assert!(
        v.iter()
            .any(|m| m.starts_with("README.md") && m.contains("outside the prefixes")),
        "{v:?}"
    );
    write_events(
        &root.join("experiences/bob/s1.parquet"),
        &[event("a", Outcome::Seed, Dimension::Search)],
    )
    .unwrap();
    commit(&root, "y");
    let v = violations(&root, &base, "owen");
    assert!(
        v.iter()
            .any(|m| m.starts_with("experiences/bob/") && m.contains("outside the prefixes")),
        "{v:?}"
    );
}

#[test]
fn validate_accepts_the_questions_column_and_files_without_it() {
    use agent_sidekick::data::events_to_batch;
    use parquet::arrow::ArrowWriter;
    use parquet::file::metadata::KeyValue;
    use parquet::file::properties::WriterProperties;
    let (_d, root, base) = repo();
    let mut with = event("a", Outcome::Accepted, Dimension::Search);
    with.questions = Some(2);
    write_events(&root.join("experiences/alice/new.parquet"), &[with]).unwrap();
    let batch = events_to_batch(&[event("b", Outcome::Accepted, Dimension::Search)]).unwrap();
    let old = batch
        .project(&(0..batch.num_columns() - 1).collect::<Vec<_>>())
        .unwrap();
    let props = WriterProperties::builder()
        .set_key_value_metadata(Some(vec![KeyValue::new(
            "schema_version".to_string(),
            "1".to_string(),
        )]))
        .build();
    let mut w = ArrowWriter::try_new(
        std::fs::File::create(root.join("experiences/alice/old.parquet")).unwrap(),
        old.schema(),
        Some(props),
    )
    .unwrap();
    w.write(&old).unwrap();
    w.close().unwrap();
    commit(&root, "x");
    assert!(violations(&root, &base, "alice").is_empty());
}
