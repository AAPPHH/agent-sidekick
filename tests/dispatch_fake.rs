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
            account: None,
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

fn reports(model: &str) -> String {
    format!("echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\",\"modelUsage\":{{\"{model}\":{{\"inputTokens\":10,\"outputTokens\":5,\"costUSD\":0.5}},\"claude-haiku-5-5\":{{\"inputTokens\":1,\"outputTokens\":1,\"costUSD\":0.001}}}}}}'\n")
}

fn max_turns_report() -> String {
    "echo '{\"type\":\"result\",\"subtype\":\"error_max_turns\",\"is_error\":true,\"errors\":[\"Reached maximum number of turns (5)\"]}'\nexit 1\n".into()
}

fn mentioning_report(text: &str) -> String {
    format!("echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"{text}\"}}'\n")
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
    assert_eq!(e.log.events().len(), 1);
    let note = &e.log.events()[0];
    assert_eq!(note.outcome, Outcome::Note);
    assert_eq!((note.job_id.as_deref(), note.repo.as_deref(), note.model_id.as_str()), (Some("j4"), None, key("m-c").model_id().as_str()));
    assert!(note.duration_s.is_some() && note.ts_ms > 0);
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
    let mut e = env("claude-opus-5-5", true, 0, &format!("printf 'fn a() {{ let decided_by = 1; }}\\n' > src/lib.rs\n{}", reports("claude-opus-5-5-20261001")));
    let r = run(&mut e, "claude-opus-5-5", Dimension::WriteCode, "j9").await;
    assert_eq!(r.status, Status::Completed);
    assert!(r.applied && r.branch.is_none());
    assert!(r.warnings.iter().any(|w| w.contains("assert")));
    assert!(r.violations.is_empty());
    assert_eq!(std::fs::read_to_string(e.repo.join("src/lib.rs")).unwrap(), "fn a() { let decided_by = 1; }\n");
    assert!(branches(&e.repo).is_empty());
    assert_eq!(e.log.events().len(), 1);
    assert_eq!(e.log.events()[0].outcome, Outcome::Note);
}

#[tokio::test]
async fn step_limit_abort_is_a_result() {
    let mut e = env("m-f", true, 40, &format!("printf 'partial\\n' > src/p.rs\n{}", max_turns_report()));
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
        &format!("printf 'fn a() {{ new }}\\n' > src/lib.rs\nprintf 'fn a() {{ other }}\\n' > ../../../repo/src/lib.rs\n{}", reports("claude-opus-5-5")),
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

#[tokio::test]
async fn max_turns_abort_cannot_launder_a_violation() {
    let mut e = env("m-i", true, 40, &format!("printf 'x\\n' > tests/new.rs\n{}", max_turns_report()));
    let r = run(&mut e, "m-i", Dimension::WriteCode, "j16").await;
    assert_eq!(r.status, Status::Blocked);
    assert!(r.violations.iter().any(|v| v.contains("protected path tests/new.rs")));
    assert_eq!(e.log.events()[0].outcome, Outcome::Blocked);

    let mut e = env("m-j", true, 0, &format!("printf 'fn b() {{}}\\n' > src/b.rs\n{}", max_turns_report()));
    let r = run(&mut e, "m-j", Dimension::WriteCode, "j17").await;
    assert_eq!(r.status, Status::Blocked);
    assert!(r.diff_discarded);
}

#[tokio::test]
async fn protected_glob_matches_ancestor_directories() {
    let mut e = env("m-k", true, 40, "mkdir -p src/validate\nprintf 'fn v() {}\\n' > src/validate/mod.rs\n");
    let r = run(&mut e, "m-k", Dimension::WriteCode, "j18").await;
    assert_eq!(r.status, Status::Blocked);
    assert!(r.violations.iter().any(|v| v.contains("protected path src/validate/mod.rs")));
}

#[tokio::test]
async fn term_moved_between_non_ascii_files_is_a_removal() {
    let mut e = env("m-l", true, 40, "printf 'fn a() {}\\n' > src/\u{e4}.rs\nprintf 'fn a() { assert!(x); assert!(y); }\\n' > src/\u{f6}.rs\n");
    std::fs::write(e.repo.join("src/\u{e4}.rs"), SRC).unwrap();
    std::fs::write(e.repo.join("src/\u{f6}.rs"), SRC).unwrap();
    sh(&e.repo, &["add", "-A"]);
    sh(&e.repo, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "umlauts"]);
    let r = run(&mut e, "m-l", Dimension::WriteCode, "j19").await;
    assert_eq!(r.status, Status::Blocked);
    assert!(r.violations.iter().any(|v| v.contains("removed protected term `assert` in src/\u{e4}.rs")), "{:?}", r.violations);
}

fn proposal(body: &str) -> String {
    format!("cat <<'PATCHEOF'\nI propose this.\n<<<SIDEKICK-PATCH\n{body}SIDEKICK-PATCH>>>\nPATCHEOF\n")
}

const NEW_FILE: &str = "diff --git a/src/b.rs b/src/b.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/b.rs\n@@ -0,0 +1 @@\n+fn b() {}\n";
const NEW_TEST: &str = "diff --git a/tests/new.rs b/tests/new.rs\nnew file mode 100644\n--- /dev/null\n+++ b/tests/new.rs\n@@ -0,0 +1 @@\n+fn t() {}\n";
const DROP_ASSERT: &str = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-fn a() { assert!(x); let decided_by = 1; }\n+fn a() { let decided_by = 1; }\n";

#[tokio::test]
async fn proposal_mode_returns_patch_without_applying_to_the_target() {
    let mut e = env("m-p1", true, 0, &proposal(NEW_FILE));
    let r = run(&mut e, "m-p1", Dimension::WriteCode, "p1").await;
    assert!(!r.write_right && r.write_tools_withheld && r.proposal_mode);
    assert_eq!(r.status, Status::Completed);
    assert!(!r.applied);
    assert_eq!(r.branch.as_deref(), Some("sidekick/p1"));
    assert!(std::fs::read_to_string(r.patch_path.as_ref().unwrap()).unwrap().contains("+fn b() {}"));
    assert!(!e.repo.join("src/b.rs").exists());
    assert_eq!(e.log.events().len(), 1);
    assert_eq!(e.log.events()[0].outcome, Outcome::Note);
    let args = std::fs::read_to_string(e.dir.path().join("args.txt")).unwrap();
    assert!(args.contains("<<<SIDEKICK-PATCH"));
    assert!(args.lines().any(|l| l == "--disallowedTools"));
}

#[tokio::test]
async fn proposal_touching_protected_paths_or_terms_is_blocked() {
    let mut e = env("m-p2", true, 0, &proposal(NEW_TEST));
    let r = run(&mut e, "m-p2", Dimension::WriteCode, "p2").await;
    assert_eq!(r.status, Status::Blocked);
    assert!(r.violations.iter().any(|v| v.contains("protected path tests/new.rs")));
    assert!(r.branch.is_none() && branches(&e.repo).is_empty());
    assert_eq!(e.log.events()[0].outcome, Outcome::Blocked);

    let mut e = env("m-p2", true, 0, &proposal(DROP_ASSERT));
    let r = run(&mut e, "m-p2", Dimension::WriteCode, "p3").await;
    assert_eq!(r.status, Status::Blocked);
    assert!(r.violations.iter().any(|v| v.contains("removed protected term `assert`")));
    assert_eq!(std::fs::read_to_string(e.repo.join("src/lib.rs")).unwrap(), SRC);
}

#[tokio::test]
async fn proposal_garbage_is_no_patch_without_trust_change() {
    for (i, action) in ["echo 'no patch here'\n".to_string(), proposal("this is not a diff\n"), proposal("--- a/x\n+++ b/x\n@@ -1 +1 @@\n-nope\n+nope2\n")]
        .iter()
        .enumerate()
    {
        let mut e = env("m-p4", true, 0, action);
        let r = run(&mut e, "m-p4", Dimension::WriteCode, &format!("g{i}")).await;
        assert_eq!(r.status, Status::NoPatch, "{i}");
        assert!(r.reason.is_some() && r.branch.is_none());
        assert!(branches(&e.repo).is_empty());
        assert_eq!(e.log.events().len(), 1);
        assert_eq!(e.log.events()[0].outcome, Outcome::Note);
    }
}

#[tokio::test]
async fn real_diff_without_write_right_stays_blocked_in_proposal_mode() {
    let mut e = env("m-p5", true, 0, &format!("printf 'fn b() {{}}\\n' > src/b.rs\n{}", proposal(NEW_FILE)));
    let r = run(&mut e, "m-p5", Dimension::WriteCode, "p5").await;
    assert_eq!(r.status, Status::Blocked);
    assert!(r.diff_discarded);
}

const GOLD_EDIT: &str = "printf 'fn b() {}\\n' > src/b.rs\n";

#[tokio::test]
async fn gold_with_the_wrong_model_is_returned_as_branch() {
    let mut e = env("claude-opus-5-5", true, 0, &format!("{GOLD_EDIT}{}", reports("claude-sonnet-5-5")));
    let r = run(&mut e, "claude-opus-5-5", Dimension::WriteCode, "g1").await;
    assert_eq!(r.status, Status::Completed);
    assert!(!r.applied && r.branch.as_deref() == Some("sidekick/g1"));
    assert!(r.warnings.iter().any(|w| w.contains("model mismatch") && w.contains("claude-sonnet-5-5")), "{:?}", r.warnings);
    assert!(!e.repo.join("src/b.rs").exists());
    assert_eq!(r.tail, "done");
}

#[tokio::test]
async fn gold_without_a_reported_model_is_unverified_and_not_applied() {
    let mut e = env("claude-opus-5-5", true, 0, GOLD_EDIT);
    let r = run(&mut e, "claude-opus-5-5", Dimension::WriteCode, "g2").await;
    assert!(!r.applied && r.branch.is_some());
    assert!(r.warnings.iter().any(|w| w.contains("model unverified")), "{:?}", r.warnings);
    assert!(!e.repo.join("src/b.rs").exists());
}

#[tokio::test]
async fn gold_unverified_still_enforces_protected_paths() {
    let mut e = env("claude-opus-5-5", true, 0, "printf 'x\\n' > tests/new.rs\n");
    let r = run(&mut e, "claude-opus-5-5", Dimension::WriteCode, "g3").await;
    assert_eq!(r.status, Status::Blocked);
}

#[tokio::test]
async fn aliases_resolve_to_registry_ids_and_the_cli_gets_the_full_id() {
    let mut e = env("claude-opus-5-5", true, 0, &format!("{GOLD_EDIT}{}", reports("claude-opus-5-5")));
    let r = run(&mut e, "opus", Dimension::WriteCode, "g4").await;
    assert_eq!(r.model_id, key("claude-opus-5-5").model_id());
    assert!(r.applied, "{:?}", r.warnings);
    let args = std::fs::read_to_string(e.dir.path().join("args.txt")).unwrap();
    let lines: Vec<&str> = args.lines().collect();
    let at = lines.iter().position(|l| *l == "--model").unwrap();
    assert_eq!(lines[at + 1], "claude-opus-5-5");
    assert!(lines.windows(2).any(|w| w == ["--output-format", "json"]));
    assert_eq!(lines[lines.len() - 2], "--");
}

#[test]
fn alias_table_and_default_model_resolution() {
    use agent_sidekick::dispatch::resolve_claude_alias;
    for (a, id) in [("opus", "claude-opus-5-5"), ("sonnet", "claude-sonnet-5-5"), ("haiku", "claude-haiku-5-5"), ("fable", "claude-fable-5-1")] {
        assert_eq!(resolve_claude_alias(a), id);
        assert_eq!(key(a).model, id);
        assert_eq!(key(id).model_id(), key(a).model_id());
    }
    assert_eq!(resolve_claude_alias("claude-opus-5-5-20261001"), "claude-opus-5-5-20261001");
    for variant in ["Claude-Sonnet-5-5", "claude-sonnet-5-5-20260901", "sonnet[1m]", "claude-sonnet-5-5-20260901[1m]", " SONNET ", "Sonnet"] {
        assert_eq!(key(variant).model_id(), key("claude-sonnet-5-5").model_id(), "{variant}");
    }
    assert_ne!(key("claude-sonnet-5-5-evil").model_id(), key("claude-sonnet-5-5").model_id());
    let mut b = backend("opus");
    b.model = Some("sonnet".into());
    assert_eq!(Identity::default().key(&b, None).model, "claude-sonnet-5-5");
    b.model = None;
    assert_eq!(Identity::default().key(&b, None).model, "default");
    assert!(Identity::default().key(&backend("opus"), Some("opus")).is_gold());
}

#[tokio::test]
async fn worker_text_mentioning_the_max_turns_token_is_not_an_abort() {
    let mut e = env("m-t1", true, 40, &format!("printf 'fn b() {{}}\\n' > src/b.rs\n{}", mentioning_report("handled error_max_turns and reached max turns")));
    let r = run(&mut e, "m-t1", Dimension::WriteCode, "t1").await;
    assert_eq!(r.status, Status::Completed);
    assert_eq!(r.branch.as_deref(), Some("sidekick/t1"));
    assert_eq!(e.log.events()[0].outcome, Outcome::Note);
    assert_eq!(e.log.events()[0].summary.as_deref(), Some("status: completed"));

    let mut e = env("m-t1", true, 40, "echo 'error_max_turns, Reached max turns (5)'\nexit 1\n");
    let r = run(&mut e, "m-t1", Dimension::WriteCode, "t2").await;
    assert_eq!(r.status, Status::NoChange);
}

#[tokio::test]
async fn proposal_with_a_violation_is_blocked_even_when_the_worker_text_claims_max_turns() {
    let text = format!("<<<SIDEKICK-PATCH\n{DROP_ASSERT}SIDEKICK-PATCH>>>\nerror_max_turns");
    let json = serde_json::json!({"type": "result", "subtype": "success", "is_error": false, "result": text}).to_string();
    let mut e = env("m-t2", true, 0, &format!("cat <<'JSONEOF'\n{json}\nJSONEOF\n"));
    let r = run(&mut e, "m-t2", Dimension::WriteCode, "t3").await;
    assert_eq!(r.status, Status::Blocked, "{:?}", r.reason);
    assert!(r.violations.iter().any(|v| v.contains("removed protected term `assert`")));
    assert_eq!(e.log.events()[0].outcome, Outcome::Blocked);
}

#[tokio::test]
async fn proposal_that_hits_the_turn_limit_is_still_checked_before_it_counts_as_aborted() {
    let json = format!("{{\"type\":\"result\",\"subtype\":\"error_max_turns\",\"is_error\":true,\"result\":{}}}", serde_json::json!(format!("<<<SIDEKICK-PATCH\n{NEW_TEST}SIDEKICK-PATCH>>>")));
    let mut e = env("m-t3", true, 0, &format!("cat <<'JSONEOF'\n{json}\nJSONEOF\n"));
    let r = run(&mut e, "m-t3", Dimension::WriteCode, "t4").await;
    assert_eq!(r.status, Status::Blocked);
    let mut e = env("m-t3", true, 0, &max_turns_report());
    let r = run(&mut e, "m-t3", Dimension::WriteCode, "t5").await;
    assert_eq!(r.status, Status::Aborted);
}

#[tokio::test]
async fn claude_error_result_is_a_failure_with_the_cli_message() {
    let err = "echo '{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"errors\":[\"API Error: 500\"]}'\nexit 1\n";
    for (i, dim, accepted) in [(1, Dimension::WriteCode, 0), (2, Dimension::WriteCode, 40)] {
        let mut e = env("m-t4", true, accepted, err);
        let r = run(&mut e, "m-t4", dim, &format!("f{i}")).await;
        assert_eq!(r.status, Status::Failed);
        assert!(r.tail.contains("API Error: 500") && r.tail.contains("error_during_execution"), "{}", r.tail);
        assert!(r.reason.as_deref().unwrap().contains("API Error: 500"));
        assert!(r.event_id.is_none() && e.log.events().is_empty());
        assert!(branches(&e.repo).is_empty());
    }
}

#[tokio::test]
async fn dispatch_event_never_carries_the_checkout_directory_name() {
    let mut e = env("m-t5", true, 40, "printf 'fn b() {}\\n' > src/b.rs\n");
    run(&mut e, "m-t5", Dimension::WriteCode, "r1").await;
    assert_eq!(e.log.events()[0].repo, None);
    let mut req = request(&e, Dimension::WriteCode, "r2");
    req.scope = Scope::Repo("named".into());
    e.dispatcher.dispatch(&backend("m-t5"), &req, &mut e.log).await.unwrap();
    assert_eq!(e.log.events()[1].repo.as_deref(), Some("named"));
}

#[tokio::test]
async fn blocked_events_carry_a_mechanical_lesson_as_conclusion() {
    let mut e = env("m-l1", true, 40, "printf 'fn a() { let decided_by = 1; }\\n' > src/lib.rs\n");
    run(&mut e, "m-l1", Dimension::WriteCode, "jl1").await;
    assert_eq!(e.log.events()[0].conclusion.as_deref(), Some("Begriff assert darf nicht entfernt werden – im Auftrag nennen"));
    let mut e = env("m-l2", true, 40, "printf 'x\\n' > tests/new.rs\n");
    run(&mut e, "m-l2", Dimension::WriteCode, "jl2").await;
    assert_eq!(e.log.events()[0].conclusion.as_deref(), Some("geschützte Pfade im Auftrag nennen; Tests sind unveränderlich"));
    let mut e = env("m-l3", true, 0, "printf 'fn b() {}\\n' > src/b.rs\n");
    run(&mut e, "m-l3", Dimension::WriteCode, "jl3").await;
    assert_eq!(e.log.events()[0].conclusion.as_deref(), Some("ohne Schreibrecht nur Vorschlag verlangen"));
}
