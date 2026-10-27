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
*ENVME*) echo "SOCKET-ENV: $AGENT_SIDEKICK_ASK_SOCKET"; env | grep -i token ;;
*SLEEPME*) sleep 4; echo slept ;;
*) echo plain ;;
esac
"#;

fn sh(dir: &Path, args: &[&str]) {
    assert!(Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .unwrap()
        .success());
}

fn fixture() -> (
    tempfile::TempDir,
    Server,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    fixture_with(&[])
}

fn fixture_with(
    env: &[(&str, &str)],
) -> (
    tempfile::TempDir,
    Server,
    std::path::PathBuf,
    std::path::PathBuf,
) {
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
    sh(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "i",
        ],
    );
    let data = dir.path().join("data");
    let mut envs = vec![("AGENT_SIDEKICK_DATA_DIR", data.to_str().unwrap())];
    envs.extend_from_slice(env);
    let mut server = Server::start(&cfg, &envs);
    server.initialize();
    (dir, server, repo, cfg)
}

fn field<'a>(text: &'a str, key: &str) -> &'a str {
    text.lines()
        .find_map(|l| l.strip_prefix(&format!("{key}: ")))
        .unwrap_or_else(|| panic!("no {key} in {text}"))
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
    let qid: u64 = field(&text, "question_id").parse().unwrap();
    std::thread::sleep(Duration::from_secs(5));
    let done = s.call_tool_within(
        "sidekick_answer",
        json!({"job_id": job, "answer": "Postgres", "question_id": qid}),
        Duration::from_secs(30),
    );
    let text = text_of(&done);
    assert!(done["isError"] != json!(true), "{text}");
    assert!(text.contains("status: NoChange"), "{text}");
    assert!(
        text.contains("WORKER-GOT:") && text.contains("Postgres"),
        "{text}"
    );
    assert!(
        text.contains("ask_orchestrator") && text.contains("PROMPT: ASKME please"),
        "{text}"
    );
    assert!(text.contains("questions asked: 1"), "{text}");
    let model_id = field(&text, "model_id").to_string();
    let brief = text_of(&s.call_tool(
        "sidekick_steckbrief",
        json!({"model_id": model_id, "detail": true}),
    ));
    assert!(brief.contains("Rückfragen der Worker: 1"), "{brief}");
    let rows = text_of(&s.call_tool(
        "sidekick_query",
        json!({"sql": "select questions, outcome from experiences where questions is not null"}),
    ));
    assert!(rows.contains('1') && rows.contains("note"), "{rows}");
    let extra = text_of(&s.call_tool(
        "sidekick_query",
        json!({"sql": "select count(*) as n from experiences where summary like 'questions%'"}),
    ));
    assert!(extra.contains('0'), "{extra}");
    assert!(
        !brief.contains("note 3") && !brief.contains("note +3"),
        "{brief}"
    );
    let short = text_of(&s.call_tool("sidekick_steckbrief", json!({"model_id": model_id})));
    assert!(short.contains("Rückfragen erwünscht"), "{short}");
}

#[test]
fn unanswered_question_tells_the_worker_to_deliver_what_it_has() {
    let (_d, mut s, repo, _cfg) = fixture_with(&[("AGENT_SIDEKICK_ASK_TIMEOUT_S", "1")]);
    let cwd = repo.to_string_lossy().to_string();
    let first = s.call_tool_within(
        "sidekick_dispatch",
        json!({"prompt": "ASKME", "cwd": cwd, "dimension": "search"}),
        Duration::from_secs(30),
    );
    let job = field(&text_of(&first), "job_id").to_string();
    let qid: u64 = field(&text_of(&first), "question_id").parse().unwrap();
    std::thread::sleep(Duration::from_secs(3));
    let done = s.call_tool_within(
        "sidekick_answer",
        json!({"job_id": job, "answer": "too late", "question_id": qid}),
        Duration::from_secs(30),
    );
    let text = text_of(&done);
    assert!(text.contains("your answer was not delivered"), "{text}");
    assert!(text.contains("Keine Antwort vom Orchestrator: nichts entscheiden, aktuellen Stand abliefern und offene Punkte nennen."), "{text}");
    assert!(
        text.contains("status: NoChange") && !text.contains("too late\""),
        "{text}"
    );
}

#[test]
fn answer_for_an_unknown_job_is_an_error() {
    let (_d, mut s, _repo, _cfg) = fixture();
    let res = s.call_tool(
        "sidekick_answer",
        json!({"job_id": "nope", "answer": "x", "question_id": 1}),
    );
    assert_eq!(res["isError"], json!(true), "{res}");
}

fn ask(s: &mut Server, repo: &Path, extra: serde_json::Value) -> serde_json::Value {
    let mut args = json!({"prompt": "ASKME please", "cwd": repo.to_string_lossy(), "dimension": "search", "timeout_s": 3});
    args.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    s.call_tool_within("sidekick_dispatch", args, Duration::from_secs(30))
}

#[test]
fn a_second_dispatch_with_a_running_job_id_is_rejected_and_the_first_job_still_works() {
    let (_d, mut s, repo, _cfg) = fixture();
    let first = text_of(&ask(&mut s, &repo, json!({"job_id": "dup"})));
    assert_eq!(field(&first, "status"), "question", "{first}");
    let second = ask(&mut s, &repo, json!({"job_id": "dup"}));
    assert_eq!(second["isError"], json!(true), "{second}");
    assert!(text_of(&second).contains("already running"), "{second}");
    let done = text_of(&s.call_tool_within("sidekick_answer", json!({"job_id": "dup", "answer": "Postgres", "question_id": field(&first, "question_id").parse::<u64>().unwrap()}), Duration::from_secs(30)));
    assert!(
        done.contains("WORKER-GOT:") && done.contains("Postgres"),
        "{done}"
    );
}

#[test]
fn job_ids_are_validated_before_any_socket_is_created() {
    let (d, mut s, repo, _cfg) = fixture();
    let victim = d.path().join("victim.sock");
    std::fs::write(&victim, "x").unwrap();
    for bad in [
        d.path().join("victim").to_string_lossy().to_string(),
        "../../x".to_string(),
        "a b".to_string(),
        ".hidden".to_string(),
        "x".repeat(90),
    ] {
        let res = ask(&mut s, &repo, json!({"job_id": bad}));
        assert_eq!(res["isError"], json!(true), "{bad}: {res}");
        assert!(text_of(&res).contains("job_id"), "{res}");
    }
    assert!(victim.exists());
}

#[test]
fn an_answer_for_another_question_is_not_delivered_and_names_the_open_one() {
    let (_d, mut s, repo, _cfg) = fixture();
    let first = text_of(&ask(&mut s, &repo, json!({"job_id": "stale"})));
    let qid: u64 = field(&first, "question_id").parse().unwrap();
    let wrong = s.call_tool_within(
        "sidekick_answer",
        json!({"job_id": "stale", "answer": "x", "question_id": qid + 100}),
        Duration::from_secs(10),
    );
    let text = text_of(&wrong);
    assert!(
        text.contains("not delivered")
            && text.contains(&format!("question_id: {qid}"))
            && text.contains("Which database?"),
        "{text}"
    );
    let done = text_of(&s.call_tool_within(
        "sidekick_answer",
        json!({"job_id": "stale", "answer": "Right", "question_id": qid}),
        Duration::from_secs(30),
    ));
    assert!(done.contains("Right") && !done.contains("\"x\""), "{done}");
}

#[test]
fn an_answer_without_a_question_id_is_rejected_and_the_question_stays_open() {
    let (_d, mut s, repo, _cfg) = fixture();
    let first = text_of(&ask(&mut s, &repo, json!({"job_id": "noid"})));
    let qid: u64 = field(&first, "question_id").parse().unwrap();
    let bare = s.call_tool_within(
        "sidekick_answer",
        json!({"job_id": "noid", "answer": "wrong"}),
        Duration::from_secs(10),
    );
    assert_eq!(bare["isError"], json!(true), "{bare}");
    let done = text_of(&s.call_tool_within(
        "sidekick_answer",
        json!({"job_id": "noid", "answer": "Right", "question_id": qid}),
        Duration::from_secs(30),
    ));
    assert!(done.contains("Right") && !done.contains("wrong"), "{done}");
}

#[test]
fn the_ask_token_is_not_in_the_environment_the_worker_sees() {
    let (_d, mut s, repo, _cfg) = fixture();
    let first = text_of(&ask(
        &mut s,
        &repo,
        json!({"job_id": "envtok", "prompt": "ENVME"}),
    ));
    assert!(
        first.contains("SOCKET-ENV: /") && !first.contains('#'),
        "{first}"
    );
}

#[test]
fn a_finished_job_nobody_collects_is_dropped_from_the_broker() {
    let (_d, mut s, repo, _cfg) = fixture_with(&[
        ("AGENT_SIDEKICK_ASK_TIMEOUT_S", "1"),
        ("AGENT_SIDEKICK_RESULT_GRACE_S", "1"),
    ]);
    let first = text_of(&ask(&mut s, &repo, json!({"job_id": "leak"})));
    assert_eq!(field(&first, "status"), "question", "{first}");
    std::thread::sleep(Duration::from_secs(6));
    let res = s.call_tool(
        "sidekick_answer",
        json!({"job_id": "leak", "answer": "late", "question_id": 1}),
    );
    assert_eq!(res["isError"], json!(true), "{res}");
    let again = ask(&mut s, &repo, json!({"job_id": "leak"}));
    assert!(text_of(&again).contains("status: question"), "{again}");
}

#[test]
fn dispatch_progress_reports_running_once_the_job_has_a_slot() {
    let (_d, mut s, repo, _cfg) = fixture_with(&[("AGENT_SIDEKICK_PROGRESS_INTERVAL_S", "1")]);
    let args = json!({"prompt": "SLEEPME", "cwd": repo.to_string_lossy(), "dimension": "search", "timeout_s": 30});
    let id = s.send_request(
        "tools/call",
        json!({"name": "sidekick_dispatch", "arguments": args, "_meta": {"progressToken": "p1"}}),
    );
    let (_res, notes) = s.wait_response(id, Duration::from_secs(30));
    let messages: Vec<String> = notes
        .iter()
        .filter_map(|n| n["params"]["message"].as_str().map(String::from))
        .collect();
    assert!(
        !messages.is_empty()
            && messages.iter().all(|m| !m.contains("queued"))
            && messages.iter().any(|m| m.contains("running for")),
        "{messages:?}"
    );
}
