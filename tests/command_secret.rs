//! Black-box test of `secret_env` for command backends: the secret is stored with
//! `agent-sidekick setup --backend <name> --token-stdin` (keyring disabled, so it lands in
//! the file store of a temp config dir), handed to the backend's program in that variable,
//! and never shown by `sidekick_run`, `sidekick_status`, `sidekick_backends` or any log.
//! The backend is a generated script (sh on Unix, .cmd with the prompt on stdin on
//! Windows) that echoes the variable; no real agent and no network are involved.

mod support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};
use support::{text_of, Server};

const SECRET: &str = "kic-secret-value-0123456789";
const INHERITED: &str = "inherited-openai-value-987654";
const RUN_WITHIN: Duration = Duration::from_secs(60);

/// Writes an executable script `name` with `unix` (sh) or `windows` (cmd) as its body.
fn script(dir: &Path, name: &str, unix: &str, windows: &str) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = windows;
        let path = dir.join(format!("{name}.sh"));
        std::fs::write(&path, format!("#!/bin/sh\n{unix}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }
    #[cfg(windows)]
    {
        let _ = unix;
        let path = dir.join(format!("{name}.cmd"));
        let body = windows.replace('\n', "\r\n");
        std::fs::write(&path, format!("@echo off\r\n{body}\r\n")).unwrap();
        path
    }
}

/// `[backends.<name>]` for a command backend running `program` (Windows .cmd: prompt via
/// stdin).
fn command_backend(name: &str, program: &Path, default: bool, extra: &str) -> String {
    let program = program.to_string_lossy();
    let invocation = if cfg!(windows) && program.ends_with(".cmd") {
        format!("command = ['{program}']\nprompt_via_stdin = true\n")
    } else {
        format!("command = ['{program}', '{{prompt}}']\n")
    };
    format!(
        "[backends.{name}]\ntype = \"command\"\ndescription = \"{name} test backend\"\n\
         default = {default}\n{invocation}{extra}\n"
    )
}

/// Prints `ECHO-RAN` and `KEY:[<value of OPENAI_API_KEY>]`.
fn echo_script(dir: &Path) -> PathBuf {
    script(
        dir,
        "echo-key",
        "echo ECHO-RAN\nprintf 'KEY:[%s]\\n' \"$OPENAI_API_KEY\"",
        "findstr \"^\" >nul\necho ECHO-RAN\necho KEY:[%OPENAI_API_KEY%]",
    )
}

fn run(server: &mut Server, cwd: &Path, backend: &str) -> (bool, String) {
    let res = server.call_tool_within(
        "sidekick_run",
        json!({"prompt": "hello", "backend": backend, "cwd": cwd.to_string_lossy()}),
        RUN_WITHIN,
    );
    (res["isError"] == json!(true), text_of(&res))
}

/// `agent-sidekick setup --backend <name> --token-stdin` with `value` on stdin.
fn setup_backend(config_dir: &Path, name: &str, value: &str) -> (bool, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agent-sidekick"))
        .args(["setup", "--backend", name, "--token-stdin"])
        .env("AGENT_SIDEKICK_CONFIG_DIR", config_dir)
        .env(
            "AGENT_SIDEKICK_CLAUDE_CONFIG_DIR",
            config_dir.join("claude"),
        )
        .env("AGENT_SIDEKICK_DISABLE_KEYRING", "1")
        // A claude that does not exist: setup --backend then never runs the claude CLI
        // (it would only query the MCP registration for a hint).
        .env(
            "AGENT_SIDEKICK_CLAUDE",
            config_dir.join("no-such-claude.exe"),
        )
        .env_remove("AGENT_SIDEKICK_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agent-sidekick setup");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{value}\n").as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Every file below `dir`, recursively.
fn files_below(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(files_below(&path));
        } else {
            out.push(path);
        }
    }
    out
}

fn log_path_of(text: &str) -> PathBuf {
    let line = text
        .lines()
        .find_map(|l| l.strip_prefix("log: "))
        .unwrap_or_else(|| panic!("no log line in: {text}"));
    PathBuf::from(line.trim())
}

#[test]
fn command_secret_is_passed_and_never_shown() {
    let dir = tempfile::tempdir().unwrap();
    let scripts = dir.path().join("scripts");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let echo = echo_script(&scripts);
    let config = [
        command_backend(
            "kic",
            &echo,
            true,
            "secret_env = \"OPENAI_API_KEY\"\nenv = { OPENAI_API_BASE = \"http://127.0.0.1:9/v1\" }\n",
        ),
        command_backend("plain", &echo, false, ""),
    ]
    .concat();
    std::fs::write(dir.path().join("config.toml"), config).unwrap();
    // The server inherits an OPENAI_API_KEY of its own: the stored secret must replace it
    // for `kic`, and only `kic` may see the stored secret.
    let mut server = Server::start(dir.path(), &[("OPENAI_API_KEY", INHERITED)]);
    server.initialize();

    // Before setup: no secret, a clear error, and no run.
    let (is_error, text) = run(&mut server, &work, "kic");
    assert!(is_error, "{text}");
    assert!(
        text.contains("No secret stored for backend 'kic' (secret_env OPENAI_API_KEY)"),
        "{text}"
    );
    assert!(text.contains("setup --backend kic"), "{text}");
    assert!(!text.contains("ECHO-RAN"), "{text}");
    let status = text_of(&server.call_tool("sidekick_status", json!({})));
    assert!(
        status.contains("secret (kic): missing - run 'agent-sidekick setup --backend kic'"),
        "{status}"
    );
    let report: Value =
        serde_json::from_str(&text_of(&server.call_tool("sidekick_backends", json!({})))).unwrap();
    let kic = &report["backends"][0];
    assert_eq!(kic["name"], "kic", "{report}");
    assert_eq!(kic["reachable"], false, "{report}");
    assert!(
        kic["detail"]
            .as_str()
            .unwrap()
            .contains("no secret stored for secret_env OPENAI_API_KEY"),
        "{report}"
    );

    // Store the secret (file store: the keyring is disabled).
    let (ok, stdout, stderr) = setup_backend(dir.path(), "kic", SECRET);
    assert!(ok, "setup failed:\n{stdout}\n{stderr}");
    assert!(
        stdout.contains("Value for OPENAI_API_KEY of backend `kic` stored in file"),
        "{stdout}"
    );
    assert!(!stdout.contains(SECRET) && !stderr.contains(SECRET));
    let key_file = dir.path().join("cmd-kic.key");
    assert_eq!(
        std::fs::read_to_string(&key_file).unwrap().trim(),
        SECRET,
        "secret stored in {}",
        key_file.display()
    );

    // The run gets the secret in OPENAI_API_KEY; its output shows it redacted.
    let (is_error, text) = run(&mut server, &work, "kic");
    assert!(!is_error, "{text}");
    assert!(text.contains("ECHO-RAN"), "{text}");
    assert!(text.contains("KEY:[[REDACTED]]"), "{text}");
    assert!(!text.contains(SECRET), "{text}");
    assert!(!text.contains(INHERITED), "{text}");
    let log = std::fs::read_to_string(log_path_of(&text)).unwrap();
    assert!(log.contains("KEY:[[REDACTED]]"), "{log}");
    assert!(
        log.contains("secret_env: OPENAI_API_KEY (stored secret, value not shown)"),
        "{log}"
    );
    assert!(!log.contains(SECRET), "{log}");

    // Another command backend never sees it (it keeps the inherited value).
    let (is_error, text) = run(&mut server, &work, "plain");
    assert!(!is_error, "{text}");
    assert!(text.contains(&format!("KEY:[{INHERITED}]")), "{text}");
    assert!(!text.contains(SECRET), "{text}");
    assert!(!text.contains("[REDACTED]"), "{text}");

    // Status and backends show only where the secret comes from.
    let status = text_of(&server.call_tool("sidekick_status", json!({})));
    assert!(!status.contains(SECRET), "{status}");
    assert!(status.contains("secret (kic): present (file "), "{status}");
    let report_text = text_of(&server.call_tool("sidekick_backends", json!({})));
    assert!(!report_text.contains(SECRET), "{report_text}");
    let report: Value = serde_json::from_str(&report_text).unwrap();
    let kic = &report["backends"][0];
    assert_eq!(kic["reachable"], true, "{report}");
    assert!(
        kic["detail"]
            .as_str()
            .unwrap()
            .contains("secret_env OPENAI_API_KEY from file "),
        "{report}"
    );

    // No file in the config dir (run logs, server log, config) except the key file itself
    // contains the secret.
    drop(server);
    for f in files_below(dir.path()) {
        if f == key_file {
            continue;
        }
        let bytes = std::fs::read(&f).unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains(SECRET),
            "secret found in {}",
            f.display()
        );
    }
}

#[test]
fn setup_rejects_token_stdin_for_command_backend_without_secret_env() {
    let dir = tempfile::tempdir().unwrap();
    let echo = echo_script(dir.path());
    std::fs::write(
        dir.path().join("config.toml"),
        command_backend("plain", &echo, true, ""),
    )
    .unwrap();
    let (ok, stdout, stderr) = setup_backend(dir.path(), "plain", SECRET);
    assert!(!ok, "{stdout}\n{stderr}");
    assert!(stderr.contains("no secret to store"), "{stderr}");
    assert!(!dir.path().join("cmd-plain.key").exists());
}
