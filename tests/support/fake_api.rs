//! Test support: a tiny fake Anthropic API (HTTP/1.1 on 127.0.0.1) and a minimal stdio
//! JSON-RPC client for `agent-sidekick serve` (helpers copied from `tests/mcp_handshake.rs`,
//! test crates cannot import each other).
//!
//! The fake API answers `HEAD`/`GET` (and any other non-messages request) with `200 {}` and
//! `POST /v1/messages*` with a terminal `400` error whose message is [`FAKE_MARKER`], so the
//! real `claude` CLI exits 1 within seconds without ever reaching Anthropic.

#![allow(dead_code)]

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Message of the 400 error returned for every `POST /v1/messages*`.
pub const FAKE_MARKER: &str = "FAKE-SERVER-REACHED";

/// Characters of the `authorization` / `x-api-key` values kept in a [`Record`].
pub const SECRET_PREFIX_CHARS: usize = 20;

/// Upper bound for request heads/bodies we are willing to read.
const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;

/// One request as seen by the fake API. Credential headers are stored only as a prefix
/// plus the full length, so a real secret would never end up in test output.
#[derive(Debug, Clone)]
pub struct Record {
    pub method: String,
    pub path: String,
    /// First [`SECRET_PREFIX_CHARS`] characters and the length of `x-api-key`.
    pub x_api_key: Option<(String, usize)>,
    /// First [`SECRET_PREFIX_CHARS`] characters and the length of `authorization`.
    pub authorization: Option<(String, usize)>,
    pub user_agent: Option<String>,
    /// Canary strings (given to [`FakeApi::start_with_canaries`]) found anywhere in the raw
    /// request (request line, headers or body).
    pub canaries_seen: Vec<String>,
}

pub struct FakeApi {
    pub base_url: String,
    records: Arc<Mutex<Vec<Record>>>,
}

impl FakeApi {
    pub fn start() -> Self {
        Self::start_with_canaries(&[])
    }

    /// Like [`FakeApi::start`]; every request is additionally scanned for `canaries`.
    pub fn start_with_canaries(canaries: &[&str]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake API");
        let port = listener.local_addr().unwrap().port();
        let records = Arc::new(Mutex::new(Vec::new()));
        let canaries: Arc<Vec<String>> =
            Arc::new(canaries.iter().map(|c| (*c).to_owned()).collect());
        let recs = Arc::clone(&records);
        // The accept loop lives as long as the test process; tests are short-lived.
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let recs = Arc::clone(&recs);
                let canaries = Arc::clone(&canaries);
                std::thread::spawn(move || handle(stream, &recs, &canaries));
            }
        });
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            records,
        }
    }

    pub fn records(&self) -> Vec<Record> {
        self.records.lock().unwrap().clone()
    }

    /// All `POST /v1/messages*` requests.
    pub fn message_posts(&self) -> Vec<Record> {
        self.records()
            .into_iter()
            .filter(|r| r.method == "POST" && r.path.starts_with("/v1/messages"))
            .collect()
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn handle(mut stream: TcpStream, records: &Mutex<Vec<Record>>, canaries: &[String]) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    // Request head.
    let head_end = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_REQUEST_BYTES {
            return;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split_whitespace();
    let method = request_line.next().unwrap_or_default().to_owned();
    let path = request_line.next().unwrap_or_default().to_owned();
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_owned()));
        }
    }
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };

    // Body: Content-Length, or chunked until the terminating chunk. Drained so the client
    // does not see a reset before reading our response.
    let chunked =
        header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    let content_length: usize = header("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    loop {
        let body = &buf[head_end..];
        let done = if chunked {
            find(body, b"0\r\n\r\n").is_some()
        } else {
            body.len() >= content_length
        };
        if done || buf.len() > MAX_REQUEST_BYTES {
            break;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }

    let raw = String::from_utf8_lossy(&buf);
    let prefix = |v: String| {
        let len = v.chars().count();
        (v.chars().take(SECRET_PREFIX_CHARS).collect::<String>(), len)
    };
    records.lock().unwrap().push(Record {
        method: method.clone(),
        path: path.clone(),
        x_api_key: header("x-api-key").map(prefix),
        authorization: header("authorization").map(prefix),
        user_agent: header("user-agent"),
        canaries_seen: canaries
            .iter()
            .filter(|c| raw.contains(c.as_str()))
            .cloned()
            .collect(),
    });

    let (status, body) = if method == "POST" && path.starts_with("/v1/messages") {
        (
            "400 Bad Request",
            json!({
                "type": "error",
                "error": {"type": "invalid_request_error", "message": FAKE_MARKER}
            })
            .to_string(),
        )
    } else {
        ("200 OK", "{}".to_owned())
    };
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    if method != "HEAD" {
        response.push_str(&body);
    }
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

// ---------------------------------------------------------------------------
// Environment hygiene
// ---------------------------------------------------------------------------

/// Whether an inherited variable must not reach the server under test: anything that
/// identifies the surrounding Claude Code session, selects a credential/provider/endpoint,
/// or routes HTTP through a proxy (which could send 127.0.0.1 traffic elsewhere).
fn is_unwanted(name: &str) -> bool {
    let up = name.to_ascii_uppercase();
    const PREFIXES: &[&str] = &[
        "CLAUDE_CODE_",
        "CLAUDE_AGENT_SDK_",
        "ANTHROPIC_",
        "AGENT_SIDEKICK_",
    ];
    const EXACT: &[&str] = &[
        "CLAUDECODE",
        "CLAUDE_PID",
        "CLAUDE_EFFORT",
        "CLAUDE_CONFIG_DIR",
        "AI_AGENT",
        "MCP_CONNECTION_NONBLOCKING",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
    ];
    PREFIXES.iter().any(|p| up.starts_with(p)) || EXACT.contains(&up.as_str())
}

/// Names of inherited variables to remove before spawning the server.
pub fn unwanted_inherited_vars() -> Vec<OsString> {
    std::env::vars_os()
        .map(|(k, _)| k)
        .filter(|k| k.to_str().is_some_and(is_unwanted))
        .collect()
}

// ---------------------------------------------------------------------------
// JSON-RPC client for `agent-sidekick serve`
// ---------------------------------------------------------------------------

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    next_id: u64,
}

impl Server {
    /// Spawns `agent-sidekick serve` with a scrubbed environment (see
    /// [`unwanted_inherited_vars`]), the keyring disabled, `config_dir` as its config dir,
    /// and `env` on top. PATH, HOME/USERPROFILE etc. are inherited unchanged.
    pub fn start(config_dir: &Path, env: &[(&str, &str)]) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent-sidekick"));
        cmd.arg("serve");
        for k in unwanted_inherited_vars() {
            cmd.env_remove(k);
        }
        cmd.env("AGENT_SIDEKICK_CONFIG_DIR", config_dir)
            .env("AGENT_SIDEKICK_DISABLE_KEYRING", "1")
            // Cut claude's side traffic to third parties; harmless if unknown.
            .env("DISABLE_AUTOUPDATER", "1")
            .env("DISABLE_TELEMETRY", "1")
            .env("DISABLE_ERROR_REPORTING", "1")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in env {
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
        let mut server = Self {
            child,
            stdin: Some(stdin),
            lines,
            next_id: 1,
        };
        server.initialize();
        server
    }

    fn send(&mut self, msg: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
    }

    /// Send a request and wait (with a deadline) for the response with the same id.
    /// Every stdout line must be JSON: anything else means logs leak into the protocol.
    pub fn request(&mut self, method: &str, params: Value, within: Duration) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self
                .lines
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no response to {method} within {within:?}"));
            let msg: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("non-JSON line on stdout ({e}): {line}"));
            if msg.get("id") == Some(&json!(id)) {
                assert!(msg.get("error").is_none(), "{method} failed: {msg}");
                return msg["result"].clone();
            }
        }
    }

    fn initialize(&mut self) {
        self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "e2e-fake-api-test", "version": "0.0.0"}
            }),
            REQUEST_TIMEOUT,
        );
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    }

    pub fn call_tool(&mut self, name: &str, args: Value, within: Duration) -> Value {
        self.request(
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

/// Concatenated text content of a tool result.
pub fn text_of(result: &Value) -> String {
    result["content"]
        .as_array()
        .expect("content array")
        .iter()
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `log: <path>` line of a `sidekick_run` result.
pub fn log_path_of(text: &str) -> Option<std::path::PathBuf> {
    text.lines()
        .find_map(|l| l.strip_prefix("log: "))
        .map(|p| std::path::PathBuf::from(p.trim()))
}
