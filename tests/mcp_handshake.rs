//! Speaks raw newline-delimited JSON-RPC to `agent-sidekick serve` over stdio.

mod support;

use std::path::Path;
#[cfg(unix)]
use std::process::Command;
use std::time::Duration;

use serde_json::{json, Value};
use support::{text_of, Server};

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
    let backends_tool = find("sidekick_backends");
    let desc = backends_tool["description"].as_str().unwrap();
    assert!(
        desc.contains("description") && desc.contains("cost"),
        "{desc}"
    );
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
        "backend",
        "fallback",
    ] {
        assert!(props.contains_key(key), "missing property {key}: {schema}");
    }
    assert_eq!(props.len(), 8, "unexpected properties: {schema}");
    assert_eq!(schema["required"], json!(["prompt"]), "{schema}");
    for key in ["backend", "fallback"] {
        let p = &props[key];
        let ty = &p["type"];
        assert!(
            ty == "string" || ty.as_array().is_some_and(|a| a.contains(&json!("string"))),
            "{p}"
        );
        assert!(
            p["description"].as_str().is_some_and(|d| !d.is_empty()),
            "{p}"
        );
    }
    assert!(props["fallback"]["description"]
        .as_str()
        .unwrap()
        .contains("timeout"));

    // Without config.toml there is exactly the implicit backend.
    let res = server.call_tool("sidekick_backends", json!({}));
    assert_ne!(res["isError"], json!(true), "{res}");
    let text = text_of(&res);
    assert!(!text.contains("sk-ant-"), "{text}");
    let report: Value = serde_json::from_str(&text).expect("sidekick_backends returns JSON");
    assert_eq!(report["config_error"], Value::Null, "{report}");
    let list = report["backends"].as_array().expect("backends array");
    assert_eq!(list.len(), 1, "{report}");
    let b = &list[0];
    assert_eq!(b["name"], "sidekick", "{report}");
    assert_eq!(b["type"], "claude-oauth", "{report}");
    assert_eq!(b["cost"], "quota", "{report}");
    assert_eq!(b["default"], true, "{report}");
    assert!(b["description"].as_str().is_some_and(|d| !d.is_empty()));
    assert!(b["reachable"].is_boolean(), "{report}");
    assert!(b["detail"].is_string(), "{report}");
    assert_eq!(b["running"], 0, "{report}");
    assert_eq!(b["queued"], 0, "{report}");
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

    // Unknown backend names are rejected before anything runs.
    let res = server.call_tool(
        "sidekick_run",
        json!({"prompt": "hello", "backend": "nope"}),
    );
    assert_eq!(res["isError"], json!(true), "{res}");
    let text = text_of(&res);
    assert!(text.contains("unknown backend 'nope'"), "{text}");
    assert!(text.contains("sidekick"), "{text}");
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
    assert!(text.contains("token (sidekick): missing"), "{text}");
    assert!(text.contains("claude: NOT FOUND"), "{text}");
    assert!(
        text.contains("sidekick (claude-oauth, default): running 0, queued 0"),
        "{text}"
    );
    drop(server);

    let secret = "sk-ant-oat01-TESTTOKENabc123_-xyz";
    let mut server = Server::start(dir.path(), &[("AGENT_SIDEKICK_TOKEN", secret)]);
    server.initialize();
    let status = server.call_tool("sidekick_status", json!({}));
    let text = text_of(&status);
    assert!(
        text.contains("token (sidekick): present (environment"),
        "{text}"
    );
    assert!(!text.contains("sk-ant-"), "{text}");
    assert!(!text.contains("TESTTOKEN"), "{text}");
}

/// With `[backends]`, the top-level `default_model` and the legacy agent config dir are not
/// in effect: status shows each claude backend's own model and config_dir instead.
#[test]
fn status_with_explicit_backends_shows_effective_model_and_config_dir() {
    let dir = tempfile::tempdir().unwrap();
    let main_dir = dir.path().join("main-claude");
    let local_dir = dir.path().join("local-claude");
    let config = format!(
        "default_model = \"opus\"\n\n\
         [backends.main]\ntype = \"claude-oauth\"\ndescription = \"main\"\ndefault = true\n\
         config_dir = '{}'\n\n\
         [backends.local]\ntype = \"claude-endpoint\"\ndescription = \"local\"\n\
         base_url = \"http://127.0.0.1:9\"\nmodel = \"qwen3-coder\"\nconfig_dir = '{}'\n",
        main_dir.display(),
        local_dir.display()
    );
    std::fs::write(dir.path().join("config.toml"), config).unwrap();
    let mut server = Server::start(dir.path(), &[]);
    server.initialize();
    let text = text_of(&server.call_tool("sidekick_status", json!({})));
    assert!(text.contains("config error: none"), "{text}");
    assert!(!text.contains("default model:"), "{text}");
    assert!(!text.contains("opus"), "{text}");
    assert!(!text.contains("agent claude config dir:"), "{text}");
    assert!(
        text.contains(&format!(
            "main (claude-oauth, default): running 0, queued 0, max_concurrent -, \
             model (account default), config_dir {}\n",
            main_dir.display()
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "local (claude-endpoint): running 0, queued 0, max_concurrent -, \
             model qwen3-coder, config_dir {}\n",
            local_dir.display()
        )),
        "{text}"
    );

    // Without [backends] the global lines describe the implicit backend.
    std::fs::write(
        dir.path().join("config.toml"),
        "default_model = \"sonnet\"\n",
    )
    .unwrap();
    let mut server = Server::start(dir.path(), &[]);
    server.initialize();
    let text = text_of(&server.call_tool("sidekick_status", json!({})));
    assert!(text.contains("default model: sonnet\n"), "{text}");
    assert!(text.contains("agent claude config dir: "), "{text}");
    assert!(text.contains(", model sonnet, config_dir "), "{text}");
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

    // Long enough that the first run is still active through all status polls on slow CI.
    let first = server.send_request("tools/call", setup.run_args(20));
    server.wait_counts((1, 0), Duration::from_secs(15));
    let second = server.send_request("tools/call", setup.run_args(20));
    server.wait_counts((1, 1), Duration::from_secs(15));

    server.cancel(second);
    server.wait_counts((1, 0), Duration::from_secs(10));

    // The first run ends by timeout; the cancelled one must not take over the slot.
    let (result, _) = server.wait_response(first, Duration::from_secs(40));
    assert!(text_of(&result).contains("timed out"), "{result}");
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(server.counts(), (0, 0));
    assert_eq!(setup.starts(), 1, "cancelled queued run was started");
}

#[test]
fn cancelled_running_run_is_killed() {
    let setup = HangingSetup::new(None);
    let mut server = setup.start(&[]);

    let id = server.send_request("tools/call", setup.run_args(120));
    server.wait_counts((1, 0), Duration::from_secs(15));
    // `running` is counted before the spawn: wait until the fake claude has really started,
    // otherwise the cancel can win the race and the process never runs.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while setup.starts() == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "fake claude never started"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
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
    // Signal path: runs cancelled (<= 3 s), service stopped (<= 2 s), runtime stopped (<= 2 s);
    // generous margin for slow CI runners.
    server.wait_exit(Duration::from_secs(20));
}
