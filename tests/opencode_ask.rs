#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_sidekick::ask::{wire_launch, AskWire, OPENCODE_TOOL_NAME};
use agent_sidekick::backends::{build_launch, resolve_with, Credential, ResolveDefaults};
use agent_sidekick::config::Config;
use agent_sidekick::harness::HarnessKind;
use agent_sidekick::runner::{run_launch, RunRequest};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

const QUESTION: &str = "Which database?";
const ANSWER: &str = "Use Postgres";
const WAIT: Duration = Duration::from_secs(70);

fn read_request(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let path = line.split_whitespace().nth(1)?.to_owned();
    let mut length = 0usize;
    loop {
        let mut h = String::new();
        reader.read_line(&mut h).ok()?;
        if h == "\r\n" || h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                length = v.trim().parse().ok()?;
            }
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    Some((path, body))
}

fn chunk(delta: Value, finish: Value) -> String {
    let c = json!({"id": "c1", "object": "chat.completion.chunk", "created": 0, "model": "m", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]});
    format!("data: {c}\n\n")
}

struct Fake {
    port: u16,
    tools_offered: Arc<Mutex<Vec<String>>>,
    tool_results: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (offered, results) = (
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Mutex::new(Vec::new())),
        );
        let (o, r) = (offered.clone(), results.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (o, r) = (o.clone(), r.clone());
                std::thread::spawn(move || Self::serve(stream, o, r));
            }
        });
        Fake {
            port,
            tools_offered: offered,
            tool_results: results,
        }
    }

    fn serve(
        mut stream: TcpStream,
        offered: Arc<Mutex<Vec<String>>>,
        results: Arc<Mutex<Vec<String>>>,
    ) {
        let Some((path, body)) = read_request(&mut stream) else {
            return;
        };
        if !path.ends_with("/chat/completions") {
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"data\":[]}\n");
            return;
        }
        let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let messages = req["messages"].as_array().cloned().unwrap_or_default();
        let tool_msgs: Vec<String> = messages
            .iter()
            .filter(|m| m["role"] == "tool")
            .map(|m| m["content"].to_string())
            .collect();
        if let Some(tools) = req["tools"].as_array() {
            *offered.lock().unwrap() = tools
                .iter()
                .filter_map(|t| t["function"]["name"].as_str().map(String::from))
                .collect();
        }
        let delta = if tool_msgs.is_empty() {
            json!({"role": "assistant", "tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": OPENCODE_TOOL_NAME, "arguments": json!({"question": QUESTION}).to_string()}}]})
        } else {
            *results.lock().unwrap() = tool_msgs.clone();
            json!({"role": "assistant", "content": format!("FAKE-DONE {}", tool_msgs.join(" "))})
        };
        let finish = if tool_msgs.is_empty() {
            "tool_calls"
        } else {
            "stop"
        };
        let usage = json!({"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15});
        let body = if req["stream"] == true {
            let u = json!({"id": "c1", "object": "chat.completion.chunk", "created": 0, "model": "m", "choices": [], "usage": usage});
            format!(
                "{}{}data: {u}\n\ndata: [DONE]\n\n",
                chunk(delta, Value::Null),
                chunk(json!({}), json!(finish))
            )
        } else {
            json!({"id": "c1", "object": "chat.completion", "created": 0, "model": "m", "choices": [{"index": 0, "message": delta, "finish_reason": finish}], "usage": usage}).to_string()
        };
        let ctype = if req["stream"] == true {
            "text/event-stream"
        } else {
            "application/json"
        };
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nConnection: close\r\n\r\n{body}"
        );
    }
}

fn opencode_binary() -> Option<PathBuf> {
    agent_sidekick::harness::opencode::find_opencode(None, &|k| std::env::var_os(k), &|p| {
        p.is_file()
    })
    .ok()
}

fn runnable(path: &Path) -> bool {
    std::process::Command::new(path)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[tokio::test(flavor = "multi_thread")]
async fn real_opencode_blocks_in_ask_orchestrator_for_over_a_minute_and_continues() {
    let Some(program) = opencode_binary().filter(|p| runnable(p)) else {
        eprintln!("opencode not installed; skipping");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let fake = Fake::start();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let text = format!(
        "[backends.dgx]\nharness = \"opencode\"\nprovider = \"ollama\"\nbase_url = \"http://127.0.0.1:{}\"\nmodel = \"fake-model\"\ndescription = \"d\"\ndefault = true\n",
        fake.port
    );
    let cfg: Config = toml::from_str(&text).unwrap();
    let defaults = ResolveDefaults {
        home: Some(tmp.path().join("home")),
        legacy_agent_config_dir: None,
        config_dir: Some(tmp.path().join("cfg")),
    };
    let backends = resolve_with(&cfg, &defaults).unwrap();
    let backend = backends.get("dgx").unwrap();
    let req = RunRequest {
        prompt: "decide something".into(),
        cwd: work,
        max_turns: 20,
        model: None,
        allowed_tools: None,
        timeout: Duration::from_secs(300),
    };
    let base = vec![
        ("PATH".into(), std::env::var_os("PATH").unwrap_or_default()),
        ("SHELL".into(), "/bin/sh".into()),
    ];
    let mut launch = build_launch(backend, &req, &Credential::None, &program, base, &[]).unwrap();
    let sock = tmp.path().join("ask.sock");
    std::fs::write(tmp.path().join("t"), "tok").unwrap();
    let listener = UnixListener::bind(&sock).unwrap();
    let asked = Arc::new(Mutex::new(None::<(String, Instant)>));
    let seen = asked.clone();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        let q = serde_json::from_str::<Value>(&line).unwrap()["question"]
            .as_str()
            .unwrap()
            .to_string();
        let at = Instant::now();
        *seen.lock().unwrap() = Some((q, at));
        std::thread::sleep(WAIT);
        let mut stream = stream;
        writeln!(stream, "{}", json!({"answer": ANSWER})).unwrap();
    });
    let wire = AskWire {
        exe: PathBuf::from(env!("CARGO_BIN_EXE_agent-sidekick")),
        job_id: "j-test".into(),
        endpoint: sock.to_string_lossy().into_owned(),
    };
    wire_launch(HarnessKind::OpenCode, &mut launch, &wire).unwrap();
    let started = Instant::now();
    let outcome = run_launch(
        launch,
        req,
        &tmp.path().join("logs"),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    server.join().unwrap();
    let (q, _) = asked
        .lock()
        .unwrap()
        .clone()
        .expect("the bridge forwarded a question");
    assert_eq!(q, QUESTION);
    assert!(started.elapsed() >= WAIT, "{:?}", started.elapsed());
    assert!(
        fake.tools_offered
            .lock()
            .unwrap()
            .iter()
            .any(|t| t == OPENCODE_TOOL_NAME),
        "{:?}",
        fake.tools_offered.lock().unwrap()
    );
    let results = fake.tool_results.lock().unwrap().join(" ");
    assert!(
        results.contains(ANSWER),
        "tool result: {results}; tail: {}",
        outcome.tail
    );
    assert_eq!(outcome.exit_code, Some(0), "{}", outcome.tail);
    assert!(!outcome.timed_out);
}
