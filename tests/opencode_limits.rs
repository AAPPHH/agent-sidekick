use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_sidekick::backends::{build_launch, resolve_with, Credential, ResolveDefaults};
use agent_sidekick::config::Config;
use agent_sidekick::data::ModelRow;
use agent_sidekick::dispatch::with_registry_limits;
use agent_sidekick::runner::{run_launch, RunRequest};
use agent_sidekick::trust::{ModelKey, Sperre};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

const BUDGET: i64 = 40_000;
const TOOL_STEPS: usize = 30;
const LINE_CAP: i64 = 200;

#[derive(Debug, Default, Clone)]
struct Seen {
    max_tokens: Option<i64>,
    max_completion_tokens: Option<i64>,
    tools: bool,
    tool_message_bytes: Vec<usize>,
    prompt_tokens: usize,
}

struct Fake {
    port: u16,
    seen: Arc<Mutex<Vec<Seen>>>,
}

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

fn tool_command(n: usize) -> String {
    format!("yes 'line {n} xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx' | head -n 400")
}

impl Fake {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let issued = Arc::new(Mutex::new(0usize));
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (log, issued) = (log.clone(), issued.clone());
                std::thread::spawn(move || Self::serve(stream, log, issued));
            }
        });
        Fake { port, seen }
    }

    fn serve(mut stream: TcpStream, log: Arc<Mutex<Vec<Seen>>>, issued: Arc<Mutex<usize>>) {
        let Some((path, body)) = read_request(&mut stream) else {
            return;
        };
        if !path.ends_with("/chat/completions") {
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"data\":[]}\n");
            return;
        }
        let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let messages = req["messages"].as_array().cloned().unwrap_or_default();
        let tools = req["tools"].as_array().is_some_and(|t| !t.is_empty());
        let prompt_tokens = body.len() / 4;
        log.lock().unwrap().push(Seen {
            max_tokens: req["max_tokens"].as_i64(),
            max_completion_tokens: req["max_completion_tokens"].as_i64(),
            tools,
            tool_message_bytes: messages
                .iter()
                .filter(|m| m["role"] == "tool")
                .map(|m| m["content"].to_string().len())
                .collect(),
            prompt_tokens,
        });
        let call = tools && {
            let mut n = issued.lock().unwrap();
            *n += 1;
            *n <= TOOL_STEPS
        };
        let step = *issued.lock().unwrap();
        let usage = json!({"prompt_tokens": prompt_tokens, "completion_tokens": 20, "total_tokens": prompt_tokens + 20});
        let delta = if call {
            json!({"role": "assistant", "tool_calls": [{"index": 0, "id": format!("call_{step}"), "type": "function", "function": {"name": "bash", "arguments": json!({"command": tool_command(step), "description": "print"}).to_string()}}]})
        } else {
            json!({"role": "assistant", "content": "FAKE-DONE"})
        };
        let finish = if call { "tool_calls" } else { "stop" };
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

fn row() -> ModelRow {
    ModelRow {
        key: ModelKey::new("p", "m", "c", "q", "h"),
        context_tokens: Some(262_144),
        cost: "free".into(),
        max_steps: Some(100),
        tool_output_lines: Some(LINE_CAP),
        context_budget_tokens: Some(BUDGET),
        sperre: Sperre::Technisch,
        added_at: 0,
        added_by: "t".into(),
    }
}

struct Outcome {
    seen: Vec<Seen>,
    exit: Option<i32>,
    tail: String,
}

async fn session(program: &Path, limited: bool) -> Outcome {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Fake::start();
    let cfg_dir = tmp.path().join("cfg");
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
        config_dir: Some(cfg_dir),
    };
    let backends = resolve_with(&cfg, &defaults).unwrap();
    let backend = backends.get("dgx").unwrap();
    let backend = if limited {
        with_registry_limits(backend, &row())
    } else {
        backend.clone()
    };
    let req = RunRequest {
        prompt: "run the tools".into(),
        cwd: work,
        max_turns: 100,
        model: None,
        allowed_tools: None,
        timeout: Duration::from_secs(240),
    };
    let base = vec![
        ("PATH".into(), std::env::var_os("PATH").unwrap_or_default()),
        ("SHELL".into(), "/bin/sh".into()),
    ];
    let mut launch = build_launch(&backend, &req, &Credential::None, program, base, &[]).unwrap();
    if !limited {
        let file = launch
            .env
            .iter()
            .find(|(k, _)| k == "XDG_CONFIG_HOME")
            .map(|(_, v)| PathBuf::from(v).join("opencode/opencode.json"))
            .unwrap();
        let mut config: Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        for m in config["provider"]["sidekick"]["models"]
            .as_object_mut()
            .unwrap()
            .values_mut()
        {
            m.as_object_mut().unwrap().remove("limit");
        }
        std::fs::write(&file, config.to_string()).unwrap();
        for (k, v) in launch.env.iter_mut() {
            if k == "OPENCODE_CONFIG_CONTENT" {
                *v = json!({"agent": {"build": {"steps": 100}}})
                    .to_string()
                    .into();
            }
        }
    }
    let outcome = run_launch(
        launch,
        req,
        &tmp.path().join("logs"),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let seen = fake.seen.lock().unwrap().clone();
    Outcome {
        seen,
        exit: outcome.exit_code,
        tail: outcome.tail,
    }
}

fn agent_steps(o: &Outcome) -> Vec<&Seen> {
    o.seen.iter().filter(|s| s.tools).collect()
}

fn report(label: &str, o: &Outcome) {
    let steps = agent_steps(o);
    let biggest = steps.iter().map(|s| s.prompt_tokens).max().unwrap_or(0);
    let tool = steps
        .iter()
        .flat_map(|s| s.tool_message_bytes.iter())
        .max()
        .copied()
        .unwrap_or(0);
    eprintln!(
        "{label}: requests {} (agent steps {}), max est. tokens/request {biggest}, largest tool message {tool} bytes, max_tokens {:?}",
        o.seen.len(),
        steps.len(),
        steps.iter().map(|s| s.max_tokens.or(s.max_completion_tokens)).collect::<Vec<_>>().first()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn registry_limits_bound_every_request_of_a_long_opencode_session() {
    let Some(program) = opencode_binary().filter(|p| runnable(p)) else {
        eprintln!("[skip] opencode binary not found (install opencode-ai)");
        return;
    };
    let after = session(&program, true).await;
    report("after", &after);
    assert_eq!(after.exit, Some(0), "{}", after.tail);
    let steps = agent_steps(&after);
    assert!(
        steps.len() > TOOL_STEPS,
        "session too short: {}",
        steps.len()
    );
    for s in &steps {
        assert!(
            s.max_tokens
                .or(s.max_completion_tokens)
                .is_some_and(|t| t > 0 && t <= BUDGET / 4),
            "max output tokens missing: {s:?}"
        );
        assert!(
            s.prompt_tokens < BUDGET as usize,
            "request of {} est. tokens exceeds the budget of {BUDGET}",
            s.prompt_tokens
        );
        for t in &s.tool_message_bytes {
            assert!(
                *t <= (BUDGET / 4) as usize + 1_000,
                "tool output of {t} bytes not capped"
            );
        }
    }
    let drops = after
        .seen
        .windows(2)
        .filter(|w| w[1].prompt_tokens + 5_000 < w[0].prompt_tokens)
        .count();
    assert!(drops > 0, "no compaction: the request size never dropped");
}

#[tokio::test(flavor = "multi_thread")]
async fn without_limits_the_session_outgrows_the_budget() {
    let Some(program) = opencode_binary().filter(|p| runnable(p)) else {
        eprintln!("[skip] opencode binary not found (install opencode-ai)");
        return;
    };
    let before = session(&program, false).await;
    report("before", &before);
    let steps = agent_steps(&before);
    assert!(
        steps.iter().any(|s| s.prompt_tokens > BUDGET as usize),
        "baseline stayed within the budget"
    );
}

#[test]
fn registry_limits_reach_the_opencode_backend_and_are_reported_as_enforced() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg: Config = toml::from_str("[backends.dgx]\nharness = \"opencode\"\nprovider = \"ollama\"\nbase_url = \"http://127.0.0.1:1\"\nmodel = \"m\"\ndescription = \"d\"\ndefault = true\ncontext_tokens = 100000\nmax_output_tokens = 2000\n").unwrap();
    let defaults = ResolveDefaults {
        home: Some(tmp.path().join("home")),
        legacy_agent_config_dir: None,
        config_dir: Some(tmp.path().join("cfg")),
    };
    let backends = resolve_with(&cfg, &defaults).unwrap();
    let backend = backends.get("dgx").unwrap();
    let limited = with_registry_limits(backend, &row());
    let agent_sidekick::backends::BackendKind::OpenCode { limits, .. } = &limited.kind else {
        panic!()
    };
    assert_eq!(
        (limits.context_tokens, limits.max_output_tokens),
        (Some(100_000), Some(2_000))
    );
    assert_eq!(
        (limits.tool_output_lines, limits.context_budget_tokens),
        (Some(LINE_CAP as u32), Some(BUDGET as u32))
    );
    assert_eq!(
        agent_sidekick::harness::opencode::effective_context(limits),
        BUDGET as u32
    );
    assert_eq!(
        agent_sidekick::harness::opencode::effective_output(limits),
        2_000
    );
    assert!(agent_sidekick::dispatch::unsupported_limits(&limited).is_empty());
}
