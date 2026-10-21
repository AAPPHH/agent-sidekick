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
    stdin: ChildStdin,
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
            stdin,
            lines,
            next_id: 1,
        }
    }

    fn send(&mut self, msg: &Value) {
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
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
#[ignore = "enable after merge"]
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
