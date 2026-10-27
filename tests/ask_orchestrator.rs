mod support;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use agent_sidekick::data::ModelRow;
use agent_sidekick::trust::{ModelKey, Sperre};
use serde_json::json;
use support::{text_of, Server};

const WORKER: &str = r#"#!/bin/sh
case "$1" in
*ASKME*)
  out=$(mktemp -d)
  {
    printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"f","version":"1"}}}'
    printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/initialized"}'
    printf '%s\n' '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"ask_orchestrator","arguments":{"question":"Which database?"}}}'
    while [ ! -e "$out/done" ]; do sleep 0.05; done
  } | "$AGENT_SIDEKICK_ASK_EXE" ask-bridge --job "$AGENT_SIDEKICK_ASK_JOB" --socket "$AGENT_SIDEKICK_ASK_SOCKET" | while IFS= read -r line; do
    case "$line" in *'"id":2'*) printf '%s\n' "$line" > "$out/ans"; : > "$out/done"; break;; esac
  done
  echo "WORKER-GOT: $(cat "$out/ans")"
  echo "PROMPT: $1"
  ;;
*) echo plain ;;
esac
"#;

fn sh(dir: &Path, args: &[&str]) {
    assert!(Command::new("git").arg("-C").arg(dir).args(args).status().unwrap().success());
}

fn fixture() -> (tempfile::TempDir, Server, std::path::PathBuf, std::path::PathBuf) {
    fixture_with(&[])
}

fn fixture_with(env: &[(&str, &str)]) -> (tempfile::TempDir, Server, std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("cfg");
    let checkout = dir.path().join("data/checkout");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::create_dir_all(&checkout).unwrap();
    sh(&checkout, &["init", "-q", "-b", "main"]);
    let script = dir.path().join("fakecmd.sh");
    std::fs::write(&script, WORKER).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        cfg.join("config.toml"),
        format!("[backends.cmd]\ntype = \"command\"\ndescription = \"t\"\ndefault = true\ncommand = ['{}', '{{prompt}}']\n", script.display()),
    )
    .unwrap();
    let row = ModelRow {
        key: ModelKey::new("fakecmd.sh", "default", "default", "none", "command"),
        context_tokens: None,
        cost: "free".into(),
        max_steps: None,
        tool_output_lines: None,
        context_budget_tokens: None,
        sperre: Sperre::NurDiff,
        added_at: 0,
        added_by: "t".into(),
    };
    ModelRow::write_all(&checkout.join("models.parquet"), &[row]).unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    sh(&repo, &["init", "-q"]);
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    sh(&repo, &["add", "-A"]);
    sh(&repo, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "i"]);
    let data = dir.path().join("data");
    let mut envs = vec![("AGENT_SIDEKICK_DATA_DIR", data.to_str().unwrap())];
    envs.extend_from_slice(env);
    let mut server = Server::start(&cfg, &envs);
    server.initialize();
    (dir, server, repo, cfg)
}

fn field<'a>(text: &'a str, key: &str) -> &'a str {
    text.lines().find_map(|l| l.strip_prefix(&format!("{key}: "))).unwrap_or_else(|| panic!("no {key} in {text}"))
}

#[test]
fn question_blocks_the_worker_and_the_answer_resumes_it() {
    let (_d, mut s, repo, _cfg) = fixture();
    let cwd = repo.to_string_lossy().to_string();
    let args = json!({"prompt": "ASKME please", "cwd": cwd, "dimension": "search", "timeout_s": 3});
    let first = s.call_tool_within("sidekick_dispatch", args, Duration::from_secs(30));
    let text = text_of(&first);
    assert!(first["isError"] != json!(true), "{text}");
    assert_eq!(field(&text, "status"), "question", "{text}");
    assert_eq!(field(&text, "question"), "Which database?", "{text}");
    let job = field(&text, "job_id").to_string();
    std::thread::sleep(Duration::from_secs(5));
    let done = s.call_tool_within("sidekick_answer", json!({"job_id": job, "answer": "Postgres"}), Duration::from_secs(30));
    let text = text_of(&done);
    assert!(done["isError"] != json!(true), "{text}");
    assert!(text.contains("status: NoChange"), "{text}");
    assert!(text.contains("WORKER-GOT:") && text.contains("Postgres"), "{text}");
    assert!(text.contains("ask_orchestrator") && text.contains("PROMPT: ASKME please"), "{text}");
    assert!(text.contains("questions asked: 1"), "{text}");
    let model_id = field(&text, "model_id").to_string();
    let brief = text_of(&s.call_tool("sidekick_steckbrief", json!({"model_id": model_id, "detail": true})));
    assert!(brief.contains("Rückfragen der Worker: 1"), "{brief}");
    let sql = "select summary, visibility from experiences where summary like 'questions%' order by visibility";
    let rows = text_of(&s.call_tool("sidekick_query", json!({"sql": sql})));
    assert!(rows.contains("questions: 1") && rows.contains("1. Which database?"), "{rows}");
    let short = text_of(&s.call_tool("sidekick_steckbrief", json!({"model_id": model_id})));
    assert!(short.contains("Rückfragen erwünscht"), "{short}");
}

#[test]
fn unanswered_question_tells_the_worker_to_deliver_what_it_has() {
    let (_d, mut s, repo, _cfg) = fixture_with(&[("AGENT_SIDEKICK_ASK_TIMEOUT_S", "1")]);
    let cwd = repo.to_string_lossy().to_string();
    let first = s.call_tool_within("sidekick_dispatch", json!({"prompt": "ASKME", "cwd": cwd, "dimension": "search"}), Duration::from_secs(30));
    let job = field(&text_of(&first), "job_id").to_string();
    std::thread::sleep(Duration::from_secs(3));
    let done = s.call_tool_within("sidekick_answer", json!({"job_id": job, "answer": "too late"}), Duration::from_secs(30));
    let text = text_of(&done);
    assert!(text.contains("your answer was not delivered"), "{text}");
    assert!(text.contains("Keine Antwort vom Orchestrator: nichts entscheiden, aktuellen Stand abliefern und offene Punkte nennen."), "{text}");
    assert!(text.contains("status: NoChange") && !text.contains("too late\""), "{text}");
}

#[test]
fn answer_for_an_unknown_job_is_an_error() {
    let (_d, mut s, _repo, _cfg) = fixture();
    let res = s.call_tool("sidekick_answer", json!({"job_id": "nope", "answer": "x"}));
    assert_eq!(res["isError"], json!(true), "{res}");
}
