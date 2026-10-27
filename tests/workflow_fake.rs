mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use agent_sidekick::data::ModelRow;
use agent_sidekick::trust::{ModelKey, Sperre};
use serde_json::{json, Value};
use support::{text_of, Server};

const WORKER: &str = r#"#!/bin/sh
patch() { printf '%s\n' 'Proposal:' '<<<SIDEKICK-PATCH'; cat; printf '%s\n' 'SIDEKICK-PATCH>>>'; }
case "$1" in
*STEP-SEARCH*) echo "found: src/lib.rs" ;;
*STEP-EXTRA*) echo "extra done" ;;
*STEP-ASK*)
  out=$(mktemp -d)
  {
    printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"f","version":"1"}}}'
    printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/initialized"}'
    printf '%s\n' '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"ask_orchestrator","arguments":{"question":"Which flavor?"}}}'
    while [ ! -e "$out/done" ]; do sleep 0.05; done
  } | "$AGENT_SIDEKICK_ASK_EXE" ask-bridge --job "$AGENT_SIDEKICK_ASK_JOB" --socket "$AGENT_SIDEKICK_ASK_SOCKET" | while IFS= read -r line; do
    case "$line" in *'"id":2'*) printf '%s\n' "$line" > "$out/ans"; : > "$out/done"; break;; esac
  done
  echo "ASK-ANSWER: $(cat "$out/ans")"
  ;;
*STEP-CODE*)
  case "$1" in *"found: src/lib.rs"*) echo "SAW-DEPENDENCY" ;; esac
  patch <<'EOP'
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1 +1,2 @@
 fn a() { assert!(x); }
+fn b() {}
EOP
  ;;
*STEP-BAD*)
  patch <<'EOP'
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1 +1 @@
-fn a() { assert!(x); }
+fn a() { x; }
EOP
  ;;
*STEP-TESTS*)
  patch <<'EOP'
diff --git a/tests/t.rs b/tests/t.rs
--- a/tests/t.rs
+++ b/tests/t.rs
@@ -1 +1,2 @@
 // t
+// more
EOP
  ;;
*) echo "unknown step" ;;
esac
"#;

const CLAUDE: &str = r#"#!/bin/sh
for last; do :; done
case "$last" in
*GOLD-CODE*) echo "fn g() {}" >> src/lib.rs ;;
*GOLD-TESTS*) echo "// gold" >> tests/t.rs ;;
esac
echo '{"type":"result","subtype":"success","is_error":false,"result":"gold done","modelUsage":{"claude-opus-5-5":{"inputTokens":10,"outputTokens":5,"costUSD":0.5}}}'
"#;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn script(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn row(provider: &str, model: &str, harness: &str, sperre: Sperre) -> ModelRow {
    ModelRow {
        key: ModelKey::new(provider, model, model, "none", harness),
        context_tokens: None,
        cost: "free".into(),
        max_steps: None,
        tool_output_lines: None,
        context_budget_tokens: None,
        sperre,
        added_at: 0,
        added_by: "t".into(),
    }
}

struct Fx {
    _dir: tempfile::TempDir,
    server: Server,
    repo: PathBuf,
}

fn fixture() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("cfg");
    let checkout = dir.path().join("data/checkout");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::create_dir_all(&checkout).unwrap();
    git(&checkout, &["init", "-q", "-b", "main"]);
    let worker = dir.path().join("fakecmd.sh");
    let other = dir.path().join("othercmd.sh");
    let claude = dir.path().join("fakeclaude");
    script(&worker, WORKER);
    script(&other, WORKER);
    script(&claude, CLAUDE);
    let caller = dir.path().join("caller");
    std::fs::create_dir_all(&caller).unwrap();
    std::fs::write(caller.join(".claude.json"), r#"{"oauthAccount":{"emailAddress":"caller@example.com"}}"#).unwrap();
    std::fs::write(
        cfg.join("config.toml"),
        format!(
            "claude_path = '{}'\n[backends.cmd]\ntype = \"command\"\ndescription = \"t\"\ndefault = true\ncommand = ['{}', '{{prompt}}']\n[backends.unreg]\ntype = \"command\"\ndescription = \"t\"\ncommand = ['{}', '{{prompt}}']\n[backends.gold]\ntype = \"claude-oauth\"\ndescription = \"t\"\naccount = \"gold@example.com\"\nmodel = \"claude-opus-5-5\"\n",
            claude.display(),
            worker.display(),
            other.display()
        ),
    )
    .unwrap();
    std::fs::write(cfg.join("token-gold"), "sk-ant-oat01-WORKFLOWFAKETOKEN-0123456789abcdef\n").unwrap();
    ModelRow::write_all(
        &checkout.join("models.parquet"),
        &[row("fakecmd.sh", "default", "command", Sperre::NurDiff), row("anthropic", "claude-opus-5-5", "claude-code", Sperre::Technisch)],
    )
    .unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::create_dir_all(repo.join("tests")).unwrap();
    git(dir.path(), &["init", "-q", repo.to_str().unwrap()]);
    std::fs::write(repo.join("src/lib.rs"), "fn a() { assert!(x); }\n").unwrap();
    std::fs::write(repo.join("tests/t.rs"), "// t\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "i"]);
    let data = dir.path().join("data");
    let mut server = Server::start(
        &cfg,
        &[
            ("AGENT_SIDEKICK_DATA_DIR", data.to_str().unwrap()),
            ("CLAUDE_CONFIG_DIR", caller.to_str().unwrap()),
        ],
    );
    server.initialize();
    Fx { _dir: dir, server, repo }
}

fn step(id: &str, prompt: &str, dimension: &str, deps: &[&str]) -> Value {
    json!({"id": id, "prompt": prompt, "dimension": dimension, "depends_on": deps})
}

fn field<'a>(text: &'a str, key: &str) -> &'a str {
    text.lines().find_map(|l| l.strip_prefix(&format!("{key}: "))).unwrap_or_else(|| panic!("no {key} in {text}"))
}

fn run(fx: &mut Fx, steps: Vec<Value>) -> (Value, String) {
    let res = fx.server.call_tool_within("sidekick_workflow", json!({"steps": steps, "cwd": fx.repo.to_string_lossy()}), Duration::from_secs(60));
    let text = text_of(&res);
    (res, text)
}

fn branches(repo: &Path) -> String {
    git(repo, &["branch", "--list", "sidekick/*"])
}

fn step_line<'a>(text: &'a str, id: &str) -> &'a str {
    text.lines().find(|l| l.starts_with(&format!("- {id} "))).unwrap_or_else(|| panic!("no step {id} in {text}"))
}

#[test]
fn search_then_proposal_then_tests_merge_into_one_branch_and_return_it() {
    let mut fx = fixture();
    let (res, text) = run(
        &mut fx,
        vec![
            step("search", "STEP-SEARCH find it", "search", &[]),
            step("code", "STEP-CODE implement", "write-code", &["search"]),
            step("tests", "STEP-TESTS cover it", "write-tests", &["code"]),
        ],
    );
    assert!(res["isError"] != json!(true), "{text}");
    assert_eq!(field(&text, "status"), "workflow completed", "{text}");
    assert_eq!(field(&text, "applied"), "false", "{text}");
    let branch = field(&text, "branch").to_string();
    assert!(step_line(&text, "code").contains("merged=true") && step_line(&text, "tests").contains("merged=true"), "{text}");
    assert!(step_line(&text, "search").contains("status=NoChange"), "{text}");
    assert!(text.contains("SAW-DEPENDENCY"), "{text}");
    assert!(git(&fx.repo, &["show", &format!("{branch}:src/lib.rs")]).contains("fn b()"));
    assert!(git(&fx.repo, &["show", &format!("{branch}:tests/t.rs")]).contains("// more"));
    assert_eq!(std::fs::read_to_string(fx.repo.join("src/lib.rs")).unwrap(), "fn a() { assert!(x); }\n");
    assert!(git(&fx.repo, &["status", "--porcelain"]).trim().is_empty());
    let patch = std::fs::read_to_string(field(&text, "patch")).unwrap();
    assert!(patch.contains("src/lib.rs") && patch.contains("tests/t.rs"), "{patch}");
    assert_eq!(branches(&fx.repo).lines().count(), 1, "step branches are folded into the workflow branch: {}", branches(&fx.repo));
    assert_eq!(git(&fx.repo, &["worktree", "list"]).lines().count(), 1);
    let job = step_line(&text, "code").split("job_id=").nth(1).unwrap().split(' ').next().unwrap().to_string();
    let verdict = fx.server.call_tool("sidekick_record_experience", json!({"job_id": job, "outcome": "accepted"}));
    assert!(verdict["isError"] != json!(true), "{}", text_of(&verdict));
}

#[test]
fn a_blocked_step_skips_its_dependents_and_independent_steps_continue() {
    let mut fx = fixture();
    let (res, text) = run(
        &mut fx,
        vec![
            step("search", "STEP-SEARCH", "search", &[]),
            step("code", "STEP-BAD remove the assert", "write-code", &["search"]),
            step("tests", "STEP-TESTS", "write-tests", &["code"]),
            step("extra", "STEP-EXTRA", "inventory", &[]),
        ],
    );
    assert_eq!(res["isError"], json!(true), "{text}");
    assert_eq!(field(&text, "status"), "workflow partial", "{text}");
    assert!(step_line(&text, "code").contains("status=Blocked"), "{text}");
    assert!(text.contains("removed protected term"), "{text}");
    assert!(step_line(&text, "tests").contains("status=Skipped"), "{text}");
    assert!(text.contains("dependency `code` blocked"), "{text}");
    assert!(step_line(&text, "extra").contains("status=NoChange") && step_line(&text, "search").contains("status=NoChange"), "{text}");
    assert!(text.contains("no step changed files") && !text.contains("branch:"), "{text}");
    assert!(branches(&fx.repo).trim().is_empty(), "{}", branches(&fx.repo));
    assert_eq!(std::fs::read_to_string(fx.repo.join("src/lib.rs")).unwrap(), "fn a() { assert!(x); }\n");
    let job = step_line(&text, "code").split("job_id=").nth(1).unwrap().split(' ').next().unwrap().to_string();
    let again = fx.server.call_tool("sidekick_record_experience", json!({"job_id": job, "outcome": "accepted"}));
    assert_eq!(again["isError"], json!(true), "blocked jobs get no verdict");
    let model_id = step_line(&text, "code").split("model_id=").nth(1).unwrap().split(' ').next().unwrap().to_string();
    let brief = text_of(&fx.server.call_tool("sidekick_steckbrief", json!({"model_id": model_id, "detail": true})));
    assert!(brief.contains("blk 1"), "{brief}");
}

#[test]
fn a_workflow_with_an_unrunnable_step_is_rejected_up_front_with_all_reasons() {
    let mut fx = fixture();
    let (res, text) = run(
        &mut fx,
        vec![step("search", "STEP-SEARCH", "search", &[]), json!({"id": "x", "prompt": "STEP-EXTRA", "dimension": "search", "backend": "unreg"}), json!({"id": "y", "prompt": "p", "dimension": "bogus"})],
    );
    assert_eq!(res["isError"], json!(true), "{text}");
    assert!(text.contains("nothing ran") && text.contains("step `x`") && text.contains("not registered") && text.contains("step `y`"), "{text}");
    assert!(branches(&fx.repo).trim().is_empty());
    assert!(!fx.repo.parent().unwrap().join("data/work").exists());
    for (steps, needle) in [
        (vec![step("a", "p", "search", &["b"]), step("b", "p", "search", &["a"])], "dependency cycle"),
        (vec![step("a", "p", "search", &[]), step("a", "p", "search", &[])], "duplicate step id"),
        (vec![step("a", "p", "search", &["zz"])], "unknown step `zz`"),
        (vec![step("a/b", "p", "search", &[])], "must be 1-40"),
        (vec![], "at least one step"),
    ] {
        let (res, text) = run(&mut fx, steps);
        assert_eq!(res["isError"], json!(true), "{text}");
        assert!(text.contains(needle), "{needle}: {text}");
    }
}

#[test]
fn gold_only_writing_steps_are_applied_directly() {
    let mut fx = fixture();
    let gold = |id: &str, prompt: &str, dim: &str, deps: &[&str]| {
        let mut s = step(id, prompt, dim, deps);
        s["backend"] = json!("gold");
        s
    };
    let (res, text) = run(&mut fx, vec![gold("code", "GOLD-CODE", "write-code", &[]), gold("tests", "GOLD-TESTS", "write-tests", &["code"])]);
    assert!(res["isError"] != json!(true), "{text}");
    assert_eq!(field(&text, "applied"), "true", "{text}");
    assert!(!text.contains("branch:"), "{text}");
    assert!(std::fs::read_to_string(fx.repo.join("src/lib.rs")).unwrap().contains("fn g()"));
    assert!(std::fs::read_to_string(fx.repo.join("tests/t.rs")).unwrap().contains("// gold"));
    assert!(branches(&fx.repo).trim().is_empty(), "{}", branches(&fx.repo));
    assert!(step_line(&text, "code").contains("gold=true"), "{text}");
}

#[test]
fn a_non_gold_writing_step_makes_the_whole_workflow_return_the_branch() {
    let mut fx = fixture();
    let mut gold = step("code", "GOLD-CODE", "write-code", &[]);
    gold["backend"] = json!("gold");
    let (res, text) = run(&mut fx, vec![gold, step("tests", "STEP-TESTS", "write-tests", &["code"])]);
    assert!(res["isError"] != json!(true), "{text}");
    assert_eq!(field(&text, "applied"), "false", "{text}");
    let branch = field(&text, "branch").to_string();
    assert!(git(&fx.repo, &["show", &format!("{branch}:src/lib.rs")]).contains("fn g()"));
    assert!(git(&fx.repo, &["show", &format!("{branch}:tests/t.rs")]).contains("// more"));
    assert_eq!(std::fs::read_to_string(fx.repo.join("src/lib.rs")).unwrap(), "fn a() { assert!(x); }\n");
}

#[test]
fn a_question_from_a_step_surfaces_and_the_answer_resumes_the_workflow() {
    let mut fx = fixture();
    let (res, text) = run(&mut fx, vec![step("search", "STEP-SEARCH", "search", &[]), step("ask", "STEP-ASK", "search", &["search"])]);
    assert!(res["isError"] != json!(true), "{text}");
    assert_eq!(field(&text, "status"), "question", "{text}");
    assert_eq!((field(&text, "step"), field(&text, "question")), ("ask", "Which flavor?"), "{text}");
    let job = field(&text, "job_id").to_string();
    assert!(job.ends_with("-ask"), "{job}");
    let done = fx.server.call_tool_within("sidekick_answer", json!({"job_id": job, "answer": "vanilla"}), Duration::from_secs(60));
    let text = text_of(&done);
    assert_eq!(field(&text, "status"), "workflow completed", "{text}");
    assert!(text.contains("ASK-ANSWER") && text.contains("vanilla"), "{text}");
}

#[test]
fn independent_writing_steps_run_side_by_side_and_both_land_on_the_branch() {
    let mut fx = fixture();
    let (res, text) = run(&mut fx, vec![step("code", "STEP-CODE", "write-code", &[]), step("tests", "STEP-TESTS", "write-tests", &[])]);
    assert!(res["isError"] != json!(true), "{text}");
    let branch = field(&text, "branch").to_string();
    assert!(git(&fx.repo, &["show", &format!("{branch}:src/lib.rs")]).contains("fn b()"));
    assert!(git(&fx.repo, &["show", &format!("{branch}:tests/t.rs")]).contains("// more"));
    assert_eq!(git(&fx.repo, &["rev-list", "--count", &branch]).trim(), "3");
}
