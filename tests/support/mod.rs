//! Shared helpers for the black-box MCP tests: speaks raw newline-delimited JSON-RPC to
//! `agent-sidekick serve` over stdio.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

pub const TIMEOUT: Duration = Duration::from_secs(20);

pub struct Server {
    pub child: Child,
    pub stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    next_id: u64,
    /// Responses that arrived while waiting for another request.
    stashed: HashMap<u64, Value>,
    _data: tempfile::TempDir,
}

impl Server {
    /// Start `agent-sidekick serve` with `config_dir` as its config dir (keyring disabled,
    /// no inherited token or claude override).
    pub fn start(config_dir: &Path, extra_env: &[(&str, &str)]) -> Self {
        let data = tempfile::tempdir().expect("data dir");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent-sidekick"));
        cmd.arg("serve")
            .env("AGENT_SIDEKICK_CONFIG_DIR", config_dir)
            .env(
                "AGENT_SIDEKICK_CLAUDE_CONFIG_DIR",
                config_dir.join("claude"),
            )
            .env("AGENT_SIDEKICK_DISABLE_KEYRING", "1")
            .env("AGENT_SIDEKICK_DATA_DIR", data.path())
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
            stashed: HashMap::new(),
            _data: data,
        }
    }

    pub fn send(&mut self, msg: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
    }

    /// Send a request without waiting for its response; returns its id.
    pub fn send_request(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    /// Wait for the response to `id`; returns it and every notification seen before it.
    /// Responses to other requests that arrive meanwhile are kept for their own wait.
    pub fn wait_response(&mut self, id: u64, within: Duration) -> (Value, Vec<Value>) {
        let mut notifications = Vec::new();
        if let Some(msg) = self.stashed.remove(&id) {
            assert!(msg.get("error").is_none(), "request {id} failed: {msg}");
            return (msg["result"].clone(), notifications);
        }
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self
                .lines
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no response to request {id} within {within:?}"));
            let msg: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("non-JSON line on stdout ({e}): {line}"));
            match msg.get("id").and_then(Value::as_u64) {
                Some(got) if got == id => {
                    assert!(msg.get("error").is_none(), "request {id} failed: {msg}");
                    return (msg["result"].clone(), notifications);
                }
                Some(other) => {
                    self.stashed.insert(other, msg);
                }
                None => notifications.push(msg),
            }
        }
    }

    pub fn cancel(&mut self, id: u64) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": id, "reason": "test"}
        }));
    }

    /// `(running, queued)` from `sidekick_status` (the global counters).
    pub fn counts(&mut self) -> (u64, u64) {
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
    pub fn wait_counts(&mut self, want: (u64, u64), within: Duration) {
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
    pub fn wait_exit(&mut self, within: Duration) -> std::process::ExitStatus {
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
    pub fn request(&mut self, method: &str, params: Value) -> Value {
        self.request_within(method, params, TIMEOUT)
    }

    pub fn request_within(&mut self, method: &str, params: Value, within: Duration) -> Value {
        let id = self.send_request(method, params);
        self.wait_response(id, within).0
    }

    pub fn initialize(&mut self) -> Value {
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

    pub fn call_tool(&mut self, name: &str, args: Value) -> Value {
        self.request("tools/call", json!({"name": name, "arguments": args}))
    }

    /// `tools/call` with a longer deadline (for runs).
    pub fn call_tool_within(&mut self, name: &str, args: Value, within: Duration) -> Value {
        self.request_within(
            "tools/call",
            json!({"name": name, "arguments": args}),
            within,
        )
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn text_of(result: &Value) -> String {
    result["content"]
        .as_array()
        .expect("content array")
        .iter()
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}
