//! Speaks raw newline-delimited JSON-RPC to `agent-sidekick serve` over stdio.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const TIMEOUT: Duration = Duration::from_secs(20);

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    next_id: u64,
}

impl Server {
    fn start(config_dir: &Path, extra_env: &[(&str, &str)]) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent-sidekick"));
        cmd.arg("serve")
            .env("AGENT_SIDEKICK_CONFIG_DIR", config_dir)
            .env(
                "AGENT_SIDEKICK_CLAUDE_CONFIG_DIR",
                config_dir.join("claude"),
            )
            .env("AGENT_SIDEKICK_DISABLE_KEYRING", "1")
            .env_remove("AGENT_SIDEKICK_TOKEN")
            .env_remove("AGENT_SIDEKICK_CLAUDE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("spawn agent-sidekick serve");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin: Some(stdin),
            lines,
            next_id: 1,
        }
    }

    fn send(&mut self, msg: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
    }

    /// Send a request without waiting for its response; returns its id.
    fn send_request(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    /// Wait for the response to `id`; returns it and every notification seen before it.
    fn wait_response(&mut self, id: u64, within: Duration) -> (Value, Vec<Value>) {
        let deadline = Instant::now() + within;
        let mut notifications = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self
                .lines
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no response to request {id} within {within:?}"));
            let msg: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("non-JSON line on stdout ({e}): {line}"));
            if msg.get("id") == Some(&json!(id)) {
                assert!(msg.get("error").is_none(), "request {id} failed: {msg}");
                return (msg["result"].clone(), notifications);
            }
            if msg.get("id").is_none() {
                notifications.push(msg);
            }
        }
    }

    fn cancel(&mut self, id: u64) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": id, "reason": "test"}
        }));
    }

    /// `(running, queued)` from `sidekick_status`.
    fn counts(&mut self) -> (u64, u64) {
        let text = text_of(&self.call_tool("sidekick_status", json!({})));
        let field = |name: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(name))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or_else(|| panic!("no '{name}' in status: {text}"))
        };
        (field("running:"), field("queued:"))
    }

    /// Poll `sidekick_status` until it reports `want` = (running, queued).
    fn wait_counts(&mut self, want: (u64, u64), within: Duration) {
        let deadline = Instant::now() + within;
        loop {
            let got = self.counts();
            if got == want {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "status stuck at (running, queued) = {got:?}, wanted {want:?}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Wait until the server process exits on its own.
    fn wait_exit(&mut self, within: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + within;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "server still running after {within:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Send a request and wait (with a deadline) for the response with the same id.
    /// Every stdout line must be JSON: anything else means logs leak into the protocol.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self
                .lines
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no response to {method} within {TIMEOUT:?}"));
            let msg: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("non-JSON line on stdout ({e}): {line}"));
            if msg.get("id") == Some(&json!(id)) {
                assert!(msg.get("error").is_none(), "{method} failed: {msg}");
                return msg["result"].clone();
            }
        }
    }

    fn initialize(&mut self) -> Value {
        let result = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "handshake-test", "version": "0.0.0"}
            }),
        );
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        result
    }

    fn call_tool(&mut self, name: &str, args: Value) -> Value {
        self.request("tools/call", json!({"name": name, "arguments": args}))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn text_of(result: &Value) -> String {
    result["content"]
        .as_array()
        .expect("content array")
        .iter()
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn handshake_lists_tools_and_validates_params() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = Server::start(dir.path(), &[]);

    let init = server.initialize();
    assert_eq!(init["serverInfo"]["name"], "agent-sidekick", "{init}");
    assert_eq!(init["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(init["capabilities"]["tools"].is_object(), "{init}");

    let list = server.request("tools/list", json!({}));
    let tools = list["tools"].as_array().expect("tools array");
    let find = |name: &str| {
        tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("tool {name} missing: {list}"))
            .clone()
    };
    find("sidekick_status");
    let run = find("sidekick_run");
    assert!(run["description"]
        .as_str()
        .unwrap()
        .contains("headless Claude Code"));
    let schema = &run["inputSchema"];
    let props = schema["properties"].as_object().expect("properties");
    for key in [
        "prompt",
        "cwd",
        "max_turns",
        "model",
        "allowed_tools",
        "timeout_s",
    ] {
        assert!(props.contains_key(key), "missing property {key}: {schema}");
    }
    assert_eq!(schema["required"], json!(["prompt"]), "{schema}");
    let at = &props["allowed_tools"];
    let at_type = &at["type"];
    assert!(
        at_type == "array"
            || at_type
                .as_array()
                .is_some_and(|a| a.contains(&json!("array"))),
        "{at}"
    );
    assert_eq!(at["items"]["type"], "string", "{at}");

    // Status must succeed and never contain anything token-like.
    let status = server.call_tool("sidekick_status", json!({}));
    assert_ne!(status["isError"], json!(true), "{status}");
    let text = text_of(&status);
    assert!(text.contains("max_concurrent"), "{text}");
    assert!(!text.contains("sk-ant-"), "{text}");

    // Validation happens before locating claude or loading the token.
    let missing = dir.path().join("definitely-missing-dir");
    let res = server.call_tool(
        "sidekick_run",
        json!({"prompt": "hello", "cwd": missing.to_string_lossy()}),
    );
    assert_eq!(res["isError"], json!(true), "{res}");
    let text = text_of(&res);
    assert!(text.contains("does not exist"), "{text}");
    assert!(text.contains("cwd"), "{text}");

    let res = server.call_tool("sidekick_run", json!({"prompt": "hello", "max_turns": 201}));
    assert_eq!(res["isError"], json!(true), "{res}");
    assert!(text_of(&res).contains("max is 200"), "{res}");
}

/// Needs the real `token` and `locate` modules (stubs panic in the server branch).
#[test]
fn status_reports_token_source_without_leaking() {
    let dir = tempfile::tempdir().unwrap();
    let missing_claude = dir.path().join("no-claude.exe");

    let mut server = Server::start(
        dir.path(),
        &[("AGENT_SIDEKICK_CLAUDE", &missing_claude.to_string_lossy())],
    );
    server.initialize();
    let status = server.call_tool("sidekick_status", json!({}));
    assert_ne!(status["isError"], json!(true), "{status}");
    let text = text_of(&status);
    assert!(text.contains("token: missing"), "{text}");
    assert!(text.contains("claude: NOT FOUND"), "{text}");
    drop(server);

    let secret = "sk-ant-oat01-TESTTOKENabc123_-xyz";
    let mut server = Server::start(dir.path(), &[("AGENT_SIDEKICK_TOKEN", secret)]);
    server.initialize();
    let status = server.call_tool("sidekick_status", json!({}));
    let text = text_of(&status);
    assert!(text.contains("token: present (environment"), "{text}");
    assert!(!text.contains("sk-ant-"), "{text}");
    assert!(!text.contains("TESTTOKEN"), "{text}");
}

const DUMMY_TOKEN: &str = "sk-ant-oat01-DUMMYTOKENforTests_0123456789";

/// Writes a fake `claude` that appends a line to `marker` and then hangs for ~60 s.
#[cfg(unix)]
fn hanging_claude(dir: &Path, marker: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("claude");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\necho started >> '{}'\nexec sleep 60\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[cfg(windows)]
fn hanging_claude(dir: &Path, marker: &Path) -> std::path::PathBuf {
    let path = dir.join("claude.cmd");
    std::fs::write(
        &path,
        format!(
            "@echo off\r\necho started>>\"{}\"\r\nping -n 60 127.0.0.1 >nul\r\n",
            marker.display()
        ),
    )
    .unwrap();
    path
}

/// A server whose `claude` is the hanging fake, with a dummy token and optional config.toml.
struct HangingSetup {
    dir: tempfile::TempDir,
    marker: std::path::PathBuf,
}

impl HangingSetup {
    fn new(config_toml: Option<&str>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        if let Some(cfg) = config_toml {
            std::fs::write(dir.path().join("config.toml"), cfg).unwrap();
        }
        let marker = dir.path().join("started.txt");
        Self { dir, marker }
    }

    fn start(&self, extra_env: &[(&str, &str)]) -> Server {
        let claude = hanging_claude(self.dir.path(), &self.marker);
        let claude = claude.to_string_lossy().into_owned();
        let mut env = vec![
            ("AGENT_SIDEKICK_CLAUDE", claude.as_str()),
            ("AGENT_SIDEKICK_TOKEN", DUMMY_TOKEN),
        ];
        env.extend_from_slice(extra_env);
        let mut server = Server::start(self.dir.path(), &env);
        server.initialize();
        server
    }

    fn starts(&self) -> usize {
        std::fs::read_to_string(&self.marker)
            .map(|s| s.lines().filter(|l| l.trim() == "started").count())
            .unwrap_or(0)
    }

    fn run_args(&self, timeout_s: u64) -> Value {
        json!({
            "name": "sidekick_run",
            "arguments": {
                "prompt": "hang",
                "cwd": self.dir.path().to_string_lossy(),
                "timeout_s": timeout_s
            }
        })
    }
}

#[test]
fn progress_notifications_sent_while_running() {
    let setup = HangingSetup::new(None);
    let mut server = setup.start(&[("AGENT_SIDEKICK_PROGRESS_INTERVAL_S", "1")]);

    let mut params = setup.run_args(4);
    params["_meta"] = json!({"progressToken": "tok-42"});
    let id = server.send_request("tools/call", params);
    let (result, notifications) = server.wait_response(id, Duration::from_secs(30));

    let progress: Vec<&Value> = notifications
        .iter()
        .filter(|n| n["method"] == "notifications/progress")
        .collect();
    assert!(
        !progress.is_empty(),
        "no progress before the result: {notifications:?}"
    );
    for p in &progress {
        assert_eq!(p["params"]["progressToken"], "tok-42", "{p}");
        let msg = p["params"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("running for") || msg.contains("queued"), "{p}");
        assert!(!msg.contains("sk-ant-"), "{p}");
    }
    let values: Vec<f64> = progress
        .iter()
        .map(|p| p["params"]["progress"].as_f64().unwrap())
        .collect();
    assert!(values.windows(2).all(|w| w[1] > w[0]), "{values:?}");

    assert_eq!(result["isError"], json!(true), "{result}");
    assert!(text_of(&result).contains("timed out after 4s"), "{result}");

    // Without a progress token there are no progress notifications.
    let id = server.send_request("tools/call", setup.run_args(2));
    let (_, notifications) = server.wait_response(id, Duration::from_secs(30));
    assert!(
        !notifications
            .iter()
            .any(|n| n["method"] == "notifications/progress"),
        "{notifications:?}"
    );
}

#[test]
fn cancelled_while_queued_never_starts() {
    let setup = HangingSetup::new(Some("max_concurrent = 1\n"));
    let mut server = setup.start(&[]);

    let first = server.send_request("tools/call", setup.run_args(8));
    server.wait_counts((1, 0), Duration::from_secs(15));
    let second = server.send_request("tools/call", setup.run_args(8));
    server.wait_counts((1, 1), Duration::from_secs(15));

    server.cancel(second);
    server.wait_counts((1, 0), Duration::from_secs(10));

    // The first run ends by timeout; the cancelled one must not take over the slot.
    let (result, _) = server.wait_response(first, Duration::from_secs(30));
    assert!(text_of(&result).contains("timed out"), "{result}");
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(server.counts(), (0, 0));
    assert_eq!(setup.starts(), 1, "cancelled queued run was started");
}

#[test]
#[ignore = "needs runner::run to honour the CancellationToken (runner review fix); enable after merge"]
fn cancelled_running_run_is_killed() {
    let setup = HangingSetup::new(None);
    let mut server = setup.start(&[]);

    let id = server.send_request("tools/call", setup.run_args(120));
    server.wait_counts((1, 0), Duration::from_secs(15));
    server.cancel(id);
    server.wait_counts((0, 0), Duration::from_secs(15));
    assert_eq!(setup.starts(), 1);
}

#[test]
fn stdin_close_shuts_down_with_run_in_flight() {
    let setup = HangingSetup::new(None);
    let mut server = setup.start(&[]);

    server.send_request("tools/call", setup.run_args(120));
    server.wait_counts((1, 0), Duration::from_secs(15));
    drop(server.stdin.take());
    // rmcp drains for up to 5 s, then runs get cancelled (<= 3 s) and the runtime stops (<= 2 s).
    server.wait_exit(Duration::from_secs(20));
}

#[cfg(unix)]
#[test]
fn sigterm_shuts_down_with_run_in_flight() {
    let setup = HangingSetup::new(None);
    let mut server = setup.start(&[]);

    server.send_request("tools/call", setup.run_args(120));
    server.wait_counts((1, 0), Duration::from_secs(15));
    let pid = server.child.id().to_string();
    let status = Command::new("kill").args(["-TERM", &pid]).status().unwrap();
    assert!(status.success());
    // Signal path: runs cancelled (<= 3 s), service stopped (<= 2 s), runtime stopped (<= 2 s).
    server.wait_exit(Duration::from_secs(10));
}
