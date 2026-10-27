use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use agent_sidekick::backends::{Backend, BackendKind, Cost};
use agent_sidekick::data::{write_events, DataDir, ModelRow, SessionLog};
use agent_sidekick::dispatch::{sha256_hex, DispatchRequest, Dispatcher, Identity, Status};
use agent_sidekick::redact::Secrets;
use agent_sidekick::runner::claude_launch;
use agent_sidekick::trust::{Dimension, Event, ModelKey, Outcome, Scope, Sperre};

const SRC: &str = "fn a() { assert!(x); let decided_by = 1; }\n";
const TEST: &str = "assert!(1 == 1);\n";

struct Env {
    dir: tempfile::TempDir,
    repo: PathBuf,
    dispatcher: Dispatcher,
    log: SessionLog,
}

fn sh(dir: &Path, args: &[&str]) {
    let s = Command::new("git").arg("-C").arg(dir).args(args).status().unwrap();
    assert!(s.success());
}

fn backend(model: &str) -> Backend {
    Backend {
        name: "fake".into(),
        description: "t".into(),
        cost: Cost::Free,
        is_default: false,
        max_concurrent: None,
        model: Some(model.into()),
        models: None,
        env: BTreeMap::new(),
        pass_env: Vec::new(),
        implicit: false,
        kind: BackendKind::ClaudeOauth { config_dir: PathBuf::from("/cfg"), permissions: Default::default() },
        foreign_secret_env: Vec::new(),
    }
}

fn key(model: &str) -> ModelKey {
    Identity::default().key(&backend(model), Some(model))
}

fn env(model: &str, register: bool, accepted: usize, action: &str) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::create_dir_all(repo.join("tests")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), SRC).unwrap();
    std::fs::write(repo.join("tests/t.rs"), TEST).unwrap();
    sh(&repo, &["init", "-q"]);
    sh(&repo, &["add", "-A"]);
    sh(&repo, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "init"]);
    let data_root = dir.path().join("data");
    let local = dir.path().join("local");
    std::fs::create_dir_all(&data_root).unwrap();
    let k = key(model);
    if register {
        let row = ModelRow {
            key: k.clone(),
            context_tokens: None,
            cost: "free".into(),
            max_steps: None,
            tool_output_lines: None,
            context_budget_tokens: None,
            sperre: Sperre::Technisch,
            added_at: 0,
            added_by: "t".into(),
        };
        ModelRow::write_all(&data_root.join("models.parquet"), &[row]).unwrap();
    }
    let now = chrono::Utc::now().timestamp_millis();
    let events: Vec<Event> =
        (0..accepted).map(|_| Event::new(&k.model_id(), Dimension::WriteCode, Scope::Global, Outcome::Accepted, now)).collect();
    let tests_events: Vec<Event> =
        (0..accepted).map(|_| Event::new(&k.model_id(), Dimension::WriteTests, Scope::Global, Outcome::Accepted, now)).collect();
    if accepted > 0 {
        write_events(&data_root.join("experiences/a/s.parquet"), &[events, tests_events].concat()).unwrap();
    }
    let args_file = dir.path().join("args.txt");
    let action_file = dir.path().join("action.sh");
    std::fs::write(&action_file, action).unwrap();
    let claude = dir.path().join("claude");
    std::fs::write(&claude, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n. {}\n", args_file.display(), action_file.display())).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let dispatcher = Dispatcher {
        data: DataDir::new(&data_root),
        local_root: local.clone(),
        work_dir: dir.path().join("work"),
        logs_dir: dir.path().join("logs"),
        machine_id: "m".into(),
        user_id: "u".into(),
        launcher: Box::new(move |b, req| {
            Ok(claude_launch(&claude, req, vec![], Secrets::new::<[&str; 0], &str>([]), vec![], b.claude_permissions().unwrap()))
        }),
    };
    let log = SessionLog::open(&data_root, &local, "me", "sess").unwrap();
    Env { dir, repo, dispatcher, log }
}

fn request(e: &Env, dimension: Dimension, job: &str) -> DispatchRequest {
    DispatchRequest {
        prompt: "do it".into(),
        repo: e.repo.clone(),
        dimension,
        scope: Scope::Global,
        job_id: job.into(),
        model: None,
        max_turns: 5,
        timeout: Duration::from_secs(30),
        identity: Identity::default(),
    }
}

fn branches(repo: &Path) -> String {
    String::from_utf8(Command::new("git").arg("-C").arg(repo).args(["branch", "--list", "sidekick/*"]).output().unwrap().stdout).unwrap()
}

async fn run(e: &mut Env, model: &str, dim: Dimension, job: &str) -> agent_sidekick::dispatch::DispatchResult {
    let req = request(e, dim, job);
    e.dispatcher.dispatch(&backend(model), &req, &mut e.log).await.unwrap()
}

#[tokio::test]
async fn unregistered_is_rejected() {
    let mut e = env("m-unreg", false, 0, "");
    let req = request(&e, Dimension::WriteCode, "j1");
    let err = e.dispatcher.dispatch(&backend("m-unreg"), &req, &mut e.log).await.unwrap_err();
    assert!(err.to_string().contains("not registered"));
}

#[tokio::test]
async fn write_code_blocked_on_removed_term_and_discarded() {
    let mut e = env("m-a", true, 40, "printf 'fn a() { let decided_by = 1; }\\n' > src/lib.rs\n");
    let r = run(&mut e, "m-a", Dimension::WriteCode, "j2").await;
    assert!(r.write_right);
    assert_eq!(r.status, Status::Blocked);
    assert!(r.block_reason.as_ref().unwrap().contains("assert"));
    assert_eq!(r.evidence_hash.as_ref().unwrap().len(), 64);
    assert!(branches(&e.repo).is_empty());
    assert!(!e.dir.path().join("work/worktrees/j2").exists());
    assert_eq!(std::fs::read_to_string(e.repo.join("src/lib.rs")).unwrap(), SRC);
    assert_eq!(e.log.events().len(), 1);
    assert_eq!(e.log.events()[0].outcome, Outcome::Blocked);
    assert_eq!(e.log.events()[0].evidence_hash, r.evidence_hash);
    assert!(r.evidence_hash.as_ref().unwrap() != &sha256_hex(b""));
}

#[tokio::test]
async fn write_code_blocked_on_protected_path() {
    let mut e = env("m-b", true, 40, "printf 'x\\n' > tests/new.rs\n");
    let r = run(&mut e, "m-b", Dimension::WriteCode, "j3").await;
    assert_eq!(r.status, Status::Blocked);
    assert!(r.violations.iter().any(|v| v.contains("protected path tests/new.rs")));
}

#[tokio::test]
async fn write_code_clean_diff_returns_branch_and_patch() {
    let mut e = env("m-c", true, 40, "printf 'fn b() {}\\n' > src/b.rs\n");
    let r = run(&mut e, "m-c", Dimension::WriteCode, "j4").await;
    assert_eq!(r.status, Status::Completed);
    assert!(!r.applied);
    assert_eq!(r.branch.as_deref(), Some("sidekick/j4"));
    assert!(r.patch_path.as_ref().unwrap().exists());
    assert!(!e.repo.join("src/b.rs").exists());
    assert!(branches(&e.repo).contains("sidekick/j4"));
    assert!(e.log.events().is_empty());
}

#[tokio::test]
async fn write_tests_inverted_path_rule() {
    let mut e = env("m-d", true, 40, "printf 'assert!(2 == 2);\\n' > tests/new.rs\n");
    let r = run(&mut e, "m-d", Dimension::WriteTests, "j5").await;
    assert_eq!(r.status, Status::Completed);
    assert_eq!(r.branch.as_deref(), Some("sidekick/j5"));

    let mut e = env("m-d", true, 40, "printf 'fn b() {}\\n' > src/b.rs\n");
    let r = run(&mut e, "m-d", Dimension::WriteTests, "j6").await;
    assert_eq!(r.status, Status::Blocked);
    assert!(r.violations[0].contains("non-test path"));

    let mut e = env("m-d", true, 40, "printf '// gone\\n' > tests/t.rs\n");
    let r = run(&mut e, "m-d", Dimension::WriteTests, "j7").await;
    assert_eq!(r.status, Status::Blocked);
    assert!(r.violations.iter().any(|v| v.contains("removed protected term `assert`")));
}

#[tokio::test]
async fn no_write_right_withholds_tools_and_discards_diff() {
    let mut e = env("m-e", true, 0, "printf 'fn b() {}\\n' > src/b.rs\n");
    let r = run(&mut e, "m-e", Dimension::WriteCode, "j8").await;
    assert!(!r.write_right && r.write_tools_withheld);
    assert!(r.diff_discarded);
    assert_eq!(r.status, Status::Blocked);
    assert_eq!(r.evidence_hash.as_ref().unwrap().len(), 64);
    assert!(r.block_reason.as_ref().unwrap().contains("without write right"));
    assert_eq!(e.log.events()[0].outcome, Outcome::Blocked);
    assert!(r.branch.is_none() && branches(&e.repo).is_empty());
    let args = std::fs::read_to_string(e.dir.path().join("args.txt")).unwrap();
    let lines: Vec<&str> = args.lines().collect();
    let at = lines.iter().position(|l| *l == "--disallowedTools").unwrap();
    for t in ["Edit", "Write", "MultiEdit", "NotebookEdit", "Bash"] {
        assert!(lines[at + 1..].contains(&t), "{t}");
    }
    assert!(r.unsupported_limits.contains(&"tool_output_lines"));
}

#[tokio::test]
async fn gold_applies_directly_and_only_warns() {
    let mut e = env("claude-opus-5-5", true, 0, "printf 'fn a() { let decided_by = 1; }\\n' > src/lib.rs\n");
    let r = run(&mut e, "claude-opus-5-5", Dimension::WriteCode, "j9").await;
    assert_eq!(r.status, Status::Completed);
    assert!(r.applied && r.branch.is_none());
    assert!(r.warnings.iter().any(|w| w.contains("assert")));
    assert!(r.violations.is_empty());
    assert_eq!(std::fs::read_to_string(e.repo.join("src/lib.rs")).unwrap(), "fn a() { let decided_by = 1; }\n");
    assert!(branches(&e.repo).is_empty());
    assert!(e.log.events().is_empty());
}

#[tokio::test]
async fn step_limit_abort_is_a_result() {
    let mut e = env("m-f", true, 40, "printf 'partial\\n' > src/p.rs\necho 'Error: Reached max turns (5)'\nexit 1\n");
    let r = run(&mut e, "m-f", Dimension::WriteCode, "j10").await;
    assert_eq!(r.status, Status::Aborted);
    assert!(branches(&e.repo).is_empty());
    assert_eq!(e.log.events()[0].outcome, Outcome::Aborted);
}

#[tokio::test]
async fn search_job_producing_a_diff_is_blocked() {
    let mut e = env("m-g", true, 40, "printf 'fn b() {}\\n' > src/b.rs\n");
    let r = run(&mut e, "m-g", Dimension::Search, "j11").await;
    assert_eq!(r.status, Status::Blocked);
    assert!(r.diff_discarded && r.branch.is_none());
    assert_eq!(e.log.events()[0].outcome, Outcome::Blocked);
    assert_eq!(e.log.events()[0].evidence_hash, r.evidence_hash);
    assert!(branches(&e.repo).is_empty());
}

#[tokio::test]
async fn module_scope_bounds_paths() {
    let mut e = env("m-h", true, 40, "printf 'fn b() {}\\n' > src/b.rs\nprintf 'fn c() {}\\n' > lib2.rs\n");
    let mut req = request(&e, Dimension::WriteCode, "j12");
    req.scope = Scope::Module("repo".into(), "src".into());
    let r = e.dispatcher.dispatch(&backend("m-h"), &req, &mut e.log).await.unwrap();
    assert_eq!(r.status, Status::Blocked);
    assert_eq!(r.violations.len(), 1);
    assert!(r.violations[0].contains("lib2.rs outside module path src"));

    let mut e = env("m-h", true, 40, "printf 'fn b() {}\\n' > src/b.rs\n");
    let mut req = request(&e, Dimension::WriteCode, "j13");
    req.scope = Scope::Module("repo".into(), "src".into());
    let r = e.dispatcher.dispatch(&backend("m-h"), &req, &mut e.log).await.unwrap();
    assert_eq!(r.status, Status::Completed);
    assert!(r.branch.is_some());
}

#[tokio::test]
async fn gold_apply_conflict_keeps_branch_and_patch() {
    let mut e = env(
        "claude-opus-5-5",
        true,
        0,
        "printf 'fn a() { new }\\n' > src/lib.rs\nprintf 'fn a() { other }\\n' > ../../../repo/src/lib.rs\n",
    );
    let r = run(&mut e, "claude-opus-5-5", Dimension::WriteCode, "j14").await;
    assert_eq!(r.status, Status::Completed);
    assert!(!r.applied);
    assert_eq!(r.branch.as_deref(), Some("sidekick/j14"));
    assert!(r.patch_path.as_ref().unwrap().exists());
    assert!(r.warnings.iter().any(|w| w.contains("gold apply failed")));
    assert!(branches(&e.repo).contains("sidekick/j14"));
}

#[tokio::test]
async fn gold_key_on_foreign_endpoint_never_applies() {
    let mut e = env("claude-opus-5-5", true, 0, "printf 'fn b() {}\\n' > src/b.rs\n");
    let mut b = backend("claude-opus-5-5");
    b.kind = BackendKind::ClaudeEndpoint {
        base_url: "http://anthropic/v1".into(),
        api_key_env: None,
        auth: Default::default(),
        config_dir: PathBuf::from("/cfg"),
        permissions: Default::default(),
    };
    assert!(Identity::default().key(&b, Some("claude-opus-5-5")).is_gold());
    let req = request(&e, Dimension::WriteCode, "j15");
    let r = e.dispatcher.dispatch(&b, &req, &mut e.log).await.unwrap();
    assert!(!r.applied);
    assert!(!e.repo.join("src/b.rs").exists());
    assert_eq!(r.branch.as_deref(), Some("sidekick/j15"));
}
