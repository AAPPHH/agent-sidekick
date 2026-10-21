//! End-to-end: the REAL `claude` CLI, launched through `agent-sidekick serve`, talks to a
//! local FAKE Anthropic API (tests/support/fake_api.rs). Zero quota: every
//! `POST /v1/messages` is answered with a 400 `FAKE-SERVER-REACHED`, so claude exits 1.
//!
//! Opt-in: runs only with `AGENT_SIDEKICK_E2E=1` and a locatable `claude` executable;
//! otherwise every test prints a skip note and passes (CI-safe).
//!
//! The server's environment is scrubbed of everything from the surrounding Claude Code
//! session, credentials, endpoints and proxies; `ANTHROPIC_BASE_URL` (Test A) and the
//! endpoint `base_url` (Tests B/C) are set by the test itself to the fake on 127.0.0.1.

#[path = "support/fake_api.rs"]
mod fake_api;

use std::path::{Path, PathBuf};
use std::time::Duration;

use fake_api::{log_path_of, text_of, FakeApi, Record, Server, FAKE_MARKER};
use serde_json::{json, Value};

const RUN_WITHIN: Duration = Duration::from_secs(180);
const QUICK: Duration = Duration::from_secs(30);

const OAUTH_TOKEN: &str = "sk-ant-oat01-E2EFAKETOKEN-0123456789abcdef_ghijkl";
/// Part of [`OAUTH_TOKEN`] that redaction of `sk-ant-` prefixes alone would not hide.
const OAUTH_TOKEN_SUFFIX: &str = "E2EFAKETOKEN-0123456789abcdef_ghijkl";
const ENDPOINT_KEY: &str = "e2e-endpoint-key-123456";
const LEAK_OAUTH: &str = "sk-ant-oat01-MUSTNOTLEAK";
const LEAK_API_KEY: &str = "sk-ant-api03-MUSTNOTLEAK";
const LEAK_CANARY: &str = "MUSTNOTLEAK";

/// Variables a Claude Code VS Code session hands to the MCP servers it starts (fake values).
const FAKE_SESSION_VARS: &[(&str, &str)] = &[
    ("CLAUDECODE", "1"),
    ("CLAUDE_CODE_SESSION_ID", "fake"),
    ("CLAUDE_CODE_ENTRYPOINT", "claude-vscode"),
    ("CLAUDE_CODE_MESSAGING_TOKEN", "fake-messaging-secret"),
];

/// Path of `claude` if the e2e tests are enabled, else `None` (after printing why).
fn e2e_claude(test: &str) -> Option<PathBuf> {
    if std::env::var("AGENT_SIDEKICK_E2E").as_deref() != Ok("1") {
        eprintln!("[skip] {test}: set AGENT_SIDEKICK_E2E=1 to run the e2e tests against the real claude CLI");
        return None;
    }
    match agent_sidekick::locate::find_claude(None) {
        Ok(located) => Some(located.path),
        Err(e) => {
            eprintln!("[skip] {test}: claude executable not found ({e})");
            None
        }
    }
}

fn dump(records: &[Record]) -> String {
    records
        .iter()
        .map(|r| format!("{r:?}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_fake_reached(fake: &FakeApi, result: &Value, text: &str) {
    assert_eq!(result["isError"], json!(true), "{result}");
    assert!(text.contains("exit_code: 1"), "{text}");
    assert!(
        text.contains(FAKE_MARKER),
        "{text}\n--- fake API saw:\n{}",
        dump(&fake.records())
    );
    assert!(
        !fake.message_posts().is_empty(),
        "no POST /v1/messages:\n{}",
        dump(&fake.records())
    );
}

fn read_log(text: &str) -> String {
    let log = log_path_of(text).unwrap_or_else(|| panic!("no log path in: {text}"));
    std::fs::read_to_string(&log).unwrap_or_else(|e| panic!("read {}: {e}", log.display()))
}

/// Result of a `sidekick_run` through the implicit claude-oauth backend.
struct OauthRun {
    fake: FakeApi,
    result: Value,
    text: String,
    _dir: tempfile::TempDir,
}

/// Test A setup: no config.toml (implicit backend `sidekick`), token via
/// `AGENT_SIDEKICK_TOKEN`, `ANTHROPIC_BASE_URL` pointing at the fake.
fn oauth_run(claude: &Path) -> OauthRun {
    let fake = FakeApi::start();
    assert!(fake.base_url.starts_with("http://127.0.0.1:"));
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path().join("config");
    let claude_config = dir.path().join("claude-config");
    let work = dir.path().join("work");
    for d in [&config_dir, &claude_config, &work] {
        std::fs::create_dir_all(d).unwrap();
    }
    let claude = claude.to_string_lossy().into_owned();
    let claude_config = claude_config.to_string_lossy().into_owned();
    let mut env: Vec<(&str, &str)> = vec![
        ("AGENT_SIDEKICK_CLAUDE", &claude),
        ("AGENT_SIDEKICK_TOKEN", OAUTH_TOKEN),
        ("AGENT_SIDEKICK_CLAUDE_CONFIG_DIR", &claude_config),
        ("ANTHROPIC_BASE_URL", &fake.base_url),
    ];
    env.extend_from_slice(FAKE_SESSION_VARS);
    let mut server = Server::start(&config_dir, &env);
    let result = server.call_tool(
        "sidekick_run",
        json!({
            "prompt": "say hi",
            "cwd": work.to_string_lossy(),
            "max_turns": 1,
            "timeout_s": 120
        }),
        RUN_WITHIN,
    );
    drop(server);
    let text = text_of(&result);
    eprintln!(
        "--- tool result:\n{text}\n--- fake API saw:\n{}",
        dump(&fake.records())
    );
    OauthRun {
        fake,
        result,
        text,
        _dir: dir,
    }
}

/// Test A: works with the v0.1 server code (implicit claude-oauth backend).
#[test]
fn oauth_token_reaches_fake_api_and_is_not_leaked() {
    let Some(claude) = e2e_claude("oauth_token_reaches_fake_api_and_is_not_leaked") else {
        return;
    };
    let run = oauth_run(&claude);
    assert_fake_reached(&run.fake, &run.result, &run.text);

    for post in run.fake.message_posts() {
        let (prefix, len) = post
            .authorization
            .clone()
            .unwrap_or_else(|| panic!("POST without authorization: {post:?}"));
        assert!(prefix.starts_with("Bearer sk-ant-oat01-"), "{post:?}");
        assert_eq!(len, "Bearer ".len() + OAUTH_TOKEN.len(), "{post:?}");
        assert!(
            post.x_api_key.is_none(),
            "x-api-key sent in oauth mode: {post:?}"
        );
    }
    // The startup probe carries no credentials.
    for r in run.fake.records().iter().filter(|r| r.path == "/api/hello") {
        assert!(r.authorization.is_none() && r.x_api_key.is_none(), "{r:?}");
    }

    assert!(!run.text.contains(OAUTH_TOKEN_SUFFIX), "{}", run.text);
    let log = read_log(&run.text);
    assert!(!log.contains(OAUTH_TOKEN_SUFFIX), "token in run log");
    assert!(log.contains(FAKE_MARKER), "{log}");
}

/// The inherited VS Code session variables must not reach the child (session isolation in
/// the backends launch code).
#[test]
#[ignore = "enable after wave-2 merge"]
fn oauth_child_does_not_inherit_session_identity() {
    let Some(claude) = e2e_claude("oauth_child_does_not_inherit_session_identity") else {
        return;
    };
    let run = oauth_run(&claude);
    assert_fake_reached(&run.fake, &run.result, &run.text);
    for post in run.fake.message_posts() {
        let ua = post.user_agent.clone().unwrap_or_default();
        assert!(!ua.contains("claude-vscode"), "{post:?}");
    }
}

/// Server with a single `[backends.local]` claude-endpoint backend pointing at `fake`.
/// Inherits fake session vars plus an OAuth token and API key that must never leak.
fn endpoint_server(fake: &FakeApi, dir: &Path, claude: &Path, auth: Option<&str>) -> Server {
    let config_dir = dir.join("config");
    let endpoint_config = dir.join("claude-local");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&endpoint_config).unwrap();
    let auth_line = auth
        .map(|a| format!("auth = \"{a}\"\n"))
        .unwrap_or_default();
    let config = format!(
        "[backends.local]\n\
         type = \"claude-endpoint\"\n\
         description = \"Fake local Anthropic-compatible endpoint (e2e test)\"\n\
         default = true\n\
         cost = \"free\"\n\
         base_url = \"{}\"\n\
         api_key_env = \"E2E_LOCAL_KEY\"\n\
         config_dir = '{}'\n\
         {auth_line}",
        fake.base_url,
        endpoint_config.display()
    );
    std::fs::write(config_dir.join("config.toml"), config).unwrap();
    let claude = claude.to_string_lossy().into_owned();
    let mut env: Vec<(&str, &str)> = vec![
        ("AGENT_SIDEKICK_CLAUDE", &claude),
        ("E2E_LOCAL_KEY", ENDPOINT_KEY),
        ("CLAUDE_CODE_OAUTH_TOKEN", LEAK_OAUTH),
        ("ANTHROPIC_API_KEY", LEAK_API_KEY),
    ];
    env.extend_from_slice(FAKE_SESSION_VARS);
    Server::start(&config_dir, &env)
}

fn endpoint_case(test: &str, auth: Option<&str>) -> Option<(FakeApi, Vec<Record>)> {
    let claude = e2e_claude(test)?;
    let fake = FakeApi::start_with_canaries(&[LEAK_CANARY]);
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let mut server = endpoint_server(&fake, dir.path(), &claude, auth);

    let backends = server.call_tool("sidekick_backends", json!({}), QUICK);
    let btext = text_of(&backends);
    assert_ne!(backends["isError"], json!(true), "{btext}");
    assert!(
        !btext.contains(LEAK_CANARY) && !btext.contains(ENDPOINT_KEY),
        "{btext}"
    );
    let json_part = btext
        .find('[')
        .zip(btext.rfind(']'))
        .map(|(a, b)| &btext[a..=b])
        .unwrap_or_else(|| panic!("no JSON array in: {btext}"));
    let list: Value = serde_json::from_str(json_part).unwrap_or_else(|e| panic!("{e}: {btext}"));
    let local = list
        .as_array()
        .and_then(|a| a.iter().find(|b| b["name"] == "local"))
        .unwrap_or_else(|| panic!("no backend 'local': {btext}"));
    assert_eq!(local["reachable"], json!(true), "{btext}");
    assert_eq!(local["type"], "claude-endpoint", "{btext}");

    let result = server.call_tool(
        "sidekick_run",
        json!({
            "prompt": "say hi",
            "cwd": work.to_string_lossy(),
            "max_turns": 1,
            "timeout_s": 120,
            "backend": "local"
        }),
        RUN_WITHIN,
    );
    drop(server);
    let text = text_of(&result);
    let records = fake.records();
    eprintln!(
        "--- tool result:\n{text}\n--- fake API saw:\n{}",
        dump(&records)
    );

    assert_fake_reached(&fake, &result, &text);
    assert_eq!(
        text.lines().next(),
        Some("backend: local (claude-endpoint)"),
        "{text}"
    );
    assert!(!text.contains(LEAK_CANARY), "{text}");
    assert!(
        !text.contains(ENDPOINT_KEY),
        "endpoint key not redacted: {text}"
    );
    let log = read_log(&text);
    assert!(!log.contains(LEAK_CANARY), "leak in run log");
    assert!(!log.contains(ENDPOINT_KEY), "endpoint key in run log");
    for r in &records {
        assert!(r.canaries_seen.is_empty(), "inherited secret sent: {r:?}");
        let ua = r.user_agent.clone().unwrap_or_default();
        assert!(!ua.contains("claude-vscode"), "{r:?}");
    }
    Some((fake, records))
}

/// Test B: claude-endpoint with the default `x-api-key` auth.
#[test]
#[ignore = "enable after wave-2 merge"]
fn endpoint_backend_sends_api_key_and_no_oauth_token() {
    let Some((fake, _)) = endpoint_case("endpoint_backend_sends_api_key_and_no_oauth_token", None)
    else {
        return;
    };
    for post in fake.message_posts() {
        let (key, len) = post
            .x_api_key
            .clone()
            .unwrap_or_else(|| panic!("POST without x-api-key: {post:?}"));
        assert!(key.starts_with("e2e-endpoint"), "{post:?}");
        assert_eq!(len, ENDPOINT_KEY.len(), "{post:?}");
        assert!(
            post.authorization.is_none(),
            "authorization header sent: {post:?}"
        );
    }
}

/// Test C: claude-endpoint with `auth = "bearer"` (ANTHROPIC_AUTH_TOKEN).
#[test]
#[ignore = "enable after wave-2 merge"]
fn endpoint_backend_bearer_auth() {
    let Some((fake, _)) = endpoint_case("endpoint_backend_bearer_auth", Some("bearer")) else {
        return;
    };
    for post in fake.message_posts() {
        let (auth, len) = post
            .authorization
            .clone()
            .unwrap_or_else(|| panic!("POST without authorization: {post:?}"));
        assert!(auth.starts_with("Bearer e2e-endpoint"), "{post:?}");
        assert_eq!(len, "Bearer ".len() + ENDPOINT_KEY.len(), "{post:?}");
        assert!(post.x_api_key.is_none(), "x-api-key sent: {post:?}");
    }
}

/// The fake API itself (always runs; no claude involved).
#[test]
fn fake_api_answers_probe_and_messages() {
    use std::io::{Read, Write};
    let fake = FakeApi::start_with_canaries(&[LEAK_CANARY]);
    let addr = fake.base_url.trim_start_matches("http://").to_owned();
    let exchange = |req: &str| {
        let mut s = std::net::TcpStream::connect(&addr).unwrap();
        s.write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    };
    let head = exchange("HEAD /api/hello HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head.ends_with("\r\n\r\n"), "HEAD must have no body: {head}");

    let body = r#"{"model":"m","note":"MUSTNOTLEAK"}"#;
    let post = exchange(&format!(
        "POST /v1/messages?beta=true HTTP/1.1\r\nHost: x\r\nx-api-key: abcdefghijklmnopqrstuvwxyz\r\n\
         User-Agent: test-agent\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    ));
    assert!(post.starts_with("HTTP/1.1 400"), "{post}");
    assert!(post.contains(FAKE_MARKER), "{post}");

    let records = fake.records();
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[0].method, "HEAD");
    assert!(records[0].canaries_seen.is_empty());
    let r = &fake.message_posts()[0];
    assert_eq!(r.x_api_key, Some(("abcdefghijklmnopqrst".into(), 26)));
    assert_eq!(r.user_agent.as_deref(), Some("test-agent"));
    assert_eq!(r.canaries_seen, vec![LEAK_CANARY.to_owned()]);
}
