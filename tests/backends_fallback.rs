//! Black-box tests of `sidekick_run` with configured backends: backend selection, fallback
//! rules, per-backend concurrency and placeholder substitution without a shell. The
//! backends are `command` backends running small generated scripts (sh on Unix, .cmd with
//! the prompt on stdin on Windows); no real agent and no network are involved.

mod support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use support::{text_of, Server};

/// Deadline for a single run (scripts finish in well under a second, or are killed after
/// their `timeout_s`).
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

/// `[backends.<name>]` for a command backend running `program`. Unix: the prompt is the
/// single argument `{prompt}`; Windows (.cmd): the prompt goes via stdin.
fn command_backend(name: &str, program: &Path, default: bool, extra: &str) -> String {
    let program = program.to_string_lossy();
    let invocation = if cfg!(windows) && program.ends_with(".cmd") {
        format!("command = ['{program}']\nprompt_via_stdin = true\n")
    } else {
        format!("command = ['{program}', '{{prompt}}']\n")
    };
    format!(
        "[backends.{name}]\ntype = \"command\"\ndescription = \"{name} test backend\"\n\
         default = {default}\ncost = \"free\"\n{invocation}{extra}\n"
    )
}

/// Script that prints `GOOD-RAN` and then the prompt it received: on Unix the argument
/// count and the first argument, on Windows stdin.
fn good_script(dir: &Path) -> PathBuf {
    script(
        dir,
        "good",
        "echo GOOD-RAN\nprintf 'ARGC:%s\\n' \"$#\"\nprintf 'ARG:[%s]\\n' \"$1\"",
        "echo GOOD-RAN\nfindstr \"^\"",
    )
}

fn bad_script(dir: &Path) -> PathBuf {
    script(
        dir,
        "bad",
        "echo BAD-RAN\nexit 3",
        "echo BAD-RAN\nexit /b 3",
    )
}

fn hang_script(dir: &Path) -> PathBuf {
    script(dir, "hang", "exec sleep 60", "ping -n 60 127.0.0.1 >nul")
}

/// A temp config dir with `config.toml` and a server started on it.
fn start(config: &str, dir: &tempfile::TempDir) -> Server {
    std::fs::write(dir.path().join("config.toml"), config).unwrap();
    let mut server = Server::start(dir.path(), &[]);
    server.initialize();
    server
}

fn run(server: &mut Server, cwd: &Path, args: Value) -> (bool, String) {
    let mut args = args;
    args["cwd"] = json!(cwd.to_string_lossy());
    let res = server.call_tool_within("sidekick_run", args, RUN_WITHIN);
    (res["isError"] == json!(true), text_of(&res))
}

struct Fixture {
    dir: tempfile::TempDir,
    work: PathBuf,
    server: Server,
}

/// Backends: `bad` (default, exits 3), `good`, `missing` (nonexistent absolute program),
/// `hang` (sleeps 60 s).
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let scripts = dir.path().join("scripts");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let missing = scripts.join(if cfg!(windows) {
        "missing-agent.exe"
    } else {
        "missing-agent"
    });
    let config = [
        command_backend("bad", &bad_script(&scripts), true, ""),
        command_backend("good", &good_script(&scripts), false, ""),
        command_backend("missing", &missing, false, ""),
        command_backend("hang", &hang_script(&scripts), false, ""),
    ]
    .concat();
    let server = start(&config, &dir);
    Fixture { dir, work, server }
}

#[test]
fn fallback_after_nonzero_exit_reports_primary_run() {
    let mut f = fixture();
    let (is_error, text) = run(
        &mut f.server,
        &f.work,
        json!({"prompt": "hello", "fallback": "good"}),
    );
    assert!(!is_error, "{text}");
    let mut lines = text.lines();
    assert_eq!(
        lines.next(),
        Some("backend: good (command) — fallback after 'bad' failed: exit_code 3"),
        "{text}"
    );
    let primary = lines.next().unwrap_or_default();
    assert!(primary.starts_with("primary: exit_code 3, log "), "{text}");
    assert!(
        primary.ends_with("it ran and may have left changes in cwd"),
        "{text}"
    );
    assert!(text.contains("exit_code: 0"), "{text}");
    assert!(text.contains("GOOD-RAN"), "{text}");
    // The primary's log exists.
    let log = primary
        .strip_prefix("primary: exit_code 3, log ")
        .and_then(|s| s.split(" — ").next())
        .unwrap();
    let log_text = std::fs::read_to_string(log).unwrap();
    assert!(log_text.contains("BAD-RAN"), "{log_text}");
    drop(f.dir);
}

#[test]
fn fallback_when_primary_program_is_missing() {
    let mut f = fixture();
    let (is_error, text) = run(
        &mut f.server,
        &f.work,
        json!({"prompt": "hello", "backend": "missing", "fallback": "good"}),
    );
    assert!(!is_error, "{text}");
    assert!(
        text.starts_with("backend: good (command) — fallback after 'missing' failed: "),
        "{text}"
    );
    assert!(!text.contains("primary: exit_code"), "{text}");
    assert!(text.contains("GOOD-RAN"), "{text}");

    // Without a fallback the failure is a tool error naming the backend.
    let (is_error, text) = run(
        &mut f.server,
        &f.work,
        json!({"prompt": "hello", "backend": "missing"}),
    );
    assert!(is_error, "{text}");
    assert!(
        text.contains("backend 'missing' (command) failed"),
        "{text}"
    );
}

#[test]
fn no_fallback_after_timeout() {
    let mut f = fixture();
    let started = Instant::now();
    let (is_error, text) = run(
        &mut f.server,
        &f.work,
        json!({"prompt": "hello", "backend": "hang", "fallback": "good", "timeout_s": 2}),
    );
    assert!(is_error, "{text}");
    assert!(text.starts_with("backend: hang (command)\n"), "{text}");
    assert!(text.contains("timed out after 2s"), "{text}");
    assert!(!text.contains("fallback"), "{text}");
    assert!(!text.contains("GOOD-RAN"), "{text}");
    assert!(started.elapsed() < Duration::from_secs(45));
}

#[test]
fn failing_primary_without_fallback_is_an_error() {
    let mut f = fixture();
    let (is_error, text) = run(&mut f.server, &f.work, json!({"prompt": "hello"}));
    assert!(is_error, "{text}");
    assert!(text.starts_with("backend: bad (command)\n"), "{text}");
    assert!(text.contains("exit_code: 3"), "{text}");
    assert!(text.contains("BAD-RAN"), "{text}");
}

#[test]
fn unknown_backend_lists_names() {
    let mut f = fixture();
    let (is_error, text) = run(
        &mut f.server,
        &f.work,
        json!({"prompt": "hello", "backend": "nope"}),
    );
    assert!(is_error, "{text}");
    assert!(text.contains("unknown backend 'nope'"), "{text}");
    assert!(text.contains("bad, good, hang, missing"), "{text}");

    let (is_error, text) = run(
        &mut f.server,
        &f.work,
        json!({"prompt": "hello", "fallback": "nope"}),
    );
    assert!(is_error, "{text}");
    assert!(text.contains("'fallback'"), "{text}");

    let (is_error, text) = run(
        &mut f.server,
        &f.work,
        json!({"prompt": "hello", "backend": "good", "fallback": "good"}),
    );
    assert!(is_error, "{text}");
    assert!(text.contains("same backend"), "{text}");

    // sidekick_backends lists them all, without secrets.
    let res = f.server.call_tool("sidekick_backends", json!({}));
    let report: Value = serde_json::from_str(&text_of(&res)).unwrap();
    let names: Vec<&str> = report["backends"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["bad", "good", "hang", "missing"], "{report}");
    let missing = &report["backends"][3];
    assert_eq!(missing["reachable"], false, "{report}");
    assert_eq!(report["backends"][1]["reachable"], true, "{report}");
}

#[test]
fn oauth_to_oauth_fallback_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let config = "[backends.main]\ntype = \"claude-oauth\"\ndescription = \"main account\"\n\
                  default = true\n\n[backends.second]\ntype = \"claude-oauth\"\n\
                  description = \"second account\"\n";
    let mut server = start(config, &dir);
    let (is_error, text) = run(
        &mut server,
        dir.path(),
        json!({"prompt": "hello", "fallback": "second"}),
    );
    assert!(is_error, "{text}");
    assert!(text.contains("both claude-oauth"), "{text}");
    assert!(text.contains("not supported"), "{text}");
}

#[test]
fn invalid_config_refuses_runs() {
    let dir = tempfile::tempdir().unwrap();
    let good = good_script(dir.path());
    let config = [
        command_backend("one", &good, true, ""),
        command_backend("two", &good, true, ""),
    ]
    .concat();
    let mut server = start(&config, &dir);
    let (is_error, text) = run(&mut server, dir.path(), json!({"prompt": "hello"}));
    assert!(is_error, "{text}");
    assert!(text.starts_with("config.toml is invalid: "), "{text}");
    assert!(text.contains("no run was started"), "{text}");
    assert!(!text.contains("GOOD-RAN"), "{text}");

    let res = server.call_tool("sidekick_backends", json!({}));
    assert_ne!(res["isError"], json!(true), "{res}");
    let report: Value = serde_json::from_str(&text_of(&res)).unwrap();
    assert!(
        report["config_error"]
            .as_str()
            .is_some_and(|e| !e.is_empty()),
        "{report}"
    );
    assert_eq!(report["backends"], json!([]), "{report}");

    let status = text_of(&server.call_tool("sidekick_status", json!({})));
    assert!(status.contains("config error: "), "{status}");
    assert!(!status.contains("config error: none"), "{status}");
}

/// The per-backend line of `sidekick_status` for `name`.
fn backend_line(server: &mut Server, name: &str) -> String {
    let text = text_of(&server.call_tool("sidekick_status", json!({})));
    text.lines()
        .find(|l| l.trim_start().starts_with(&format!("{name} (")))
        .unwrap_or_else(|| panic!("no line for backend {name}: {text}"))
        .trim()
        .to_owned()
}

#[test]
fn per_backend_max_concurrent_queues_runs() {
    let dir = tempfile::tempdir().unwrap();
    let slow = script(
        dir.path(),
        "slow",
        "sleep 6\necho SLOW-DONE",
        "ping -n 7 127.0.0.1 >nul\necho SLOW-DONE",
    );
    // Global max_concurrent stays 2: only the backend's own limit can queue the second run.
    let config = command_backend("slow", &slow, true, "max_concurrent = 1\n");
    let mut server = start(&config, &dir);
    let args = |server: &mut Server| {
        server.send_request(
            "tools/call",
            json!({
                "name": "sidekick_run",
                "arguments": {"prompt": "hello", "cwd": dir.path().to_string_lossy()}
            }),
        )
    };
    let first = args(&mut server);
    let second = args(&mut server);

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let line = backend_line(&mut server, "slow");
        if line.contains("running 1, queued 1, max_concurrent 1") {
            break;
        }
        assert!(Instant::now() < deadline, "never saw a queued run: {line}");
        std::thread::sleep(Duration::from_millis(100));
    }

    for id in [first, second] {
        let (res, _) = server.wait_response(id, RUN_WITHIN);
        let text = text_of(&res);
        assert_ne!(res["isError"], json!(true), "{text}");
        assert!(text.contains("SLOW-DONE"), "{text}");
    }
    assert_eq!(
        backend_line(&mut server, "slow"),
        "slow (command, default): running 0, queued 0, max_concurrent 1"
    );
}

#[test]
fn prompt_with_shell_metacharacters_arrives_unchanged() {
    let mut f = fixture();
    let prompt = if cfg!(windows) {
        r#"a & b | c > out.txt "q" %PATH% ^x !y! {cwd} (z)"#
    } else {
        r#"it's "q" $HOME $(echo INJECTED) `echo X` ; echo Y & b | c > out.txt {cwd} * \n"#
    };
    let (is_error, text) = run(
        &mut f.server,
        &f.work,
        json!({"prompt": prompt, "backend": "good"}),
    );
    assert!(!is_error, "{text}");
    assert!(text.starts_with("backend: good (command)\n"), "{text}");
    let lines: Vec<&str> = text.lines().collect();
    if cfg!(windows) {
        assert!(lines.contains(&prompt), "{text}");
    } else {
        assert!(lines.contains(&"ARGC:1"), "{text}");
        let expected = format!("ARG:[{prompt}]");
        assert!(lines.contains(&expected.as_str()), "{text}");
    }
    assert!(
        !f.work.join("out.txt").exists(),
        "a shell interpreted the prompt"
    );
}

/// Backends `bad` (default, exits 3), `good` and `slow` (max_concurrent 1, ~5 s), and a server
/// sending progress every second.
fn slow_fixture(dir: &tempfile::TempDir) -> Server {
    let scripts = dir.path().join("scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    let slow = script(
        &scripts,
        "slow",
        "sleep 5\necho SLOW-DONE",
        "ping -n 6 127.0.0.1 >nul\necho SLOW-DONE",
    );
    let config = [
        command_backend("bad", &bad_script(&scripts), true, ""),
        command_backend("good", &good_script(&scripts), false, ""),
        command_backend("slow", &slow, false, "max_concurrent = 1\n"),
    ]
    .concat();
    std::fs::write(dir.path().join("config.toml"), config).unwrap();
    let mut server = Server::start(dir.path(), &[("AGENT_SIDEKICK_PROGRESS_INTERVAL_S", "1")]);
    server.initialize();
    server
}

/// Starts a run on `slow` that holds its only slot; returns its request id.
fn hold_slow_slot(server: &mut Server, cwd: &Path) -> u64 {
    let id = server.send_request(
        "tools/call",
        json!({
            "name": "sidekick_run",
            "arguments": {"prompt": "hold", "backend": "slow", "cwd": cwd.to_string_lossy()}
        }),
    );
    wait_backend_line(server, "slow", "running 1, queued 0");
    id
}

fn wait_backend_line(server: &mut Server, name: &str, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let line = backend_line(server, name);
        if line.contains(needle) {
            return;
        }
        assert!(Instant::now() < deadline, "never saw '{needle}': {line}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn progress_while_fallback_is_queued_says_queued_and_stays_within_total() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = slow_fixture(&dir);
    let holder = hold_slow_slot(&mut server, dir.path());

    let id = server.send_request(
        "tools/call",
        json!({
            "name": "sidekick_run",
            "arguments": {
                "prompt": "hello",
                "fallback": "slow",
                "timeout_s": 30,
                "cwd": dir.path().to_string_lossy()
            },
            "_meta": {"progressToken": "fb-1"}
        }),
    );
    let (result, notifications) = server.wait_response(id, RUN_WITHIN);
    let text = text_of(&result);
    assert_ne!(result["isError"], json!(true), "{text}");
    assert!(
        text.starts_with("backend: slow (command) — fallback after 'bad' failed: exit_code 3"),
        "{text}"
    );

    let progress: Vec<&Value> = notifications
        .iter()
        .filter(|n| n["method"] == "notifications/progress")
        .map(|n| &n["params"])
        .collect();
    let messages: Vec<&str> = progress
        .iter()
        .map(|p| p["message"].as_str().unwrap_or_default())
        .collect();
    // The fallback's queue wait is reported as queued (not "agent running"), then it runs.
    let fb_queued = messages
        .iter()
        .position(|m| m.starts_with("fallback 'slow' queued for "))
        .unwrap_or_else(|| panic!("no queued fallback progress: {messages:?}"));
    let fb_running = messages
        .iter()
        .position(|m| m.starts_with("fallback 'slow' running for "))
        .unwrap_or_else(|| panic!("no running fallback progress: {messages:?}"));
    assert!(fb_queued < fb_running, "{messages:?}");
    assert!(
        !messages[fb_queued..fb_running]
            .iter()
            .any(|m| m.contains("running")),
        "{messages:?}"
    );
    for p in &progress {
        let value = p["progress"].as_f64().unwrap();
        if let Some(total) = p["total"].as_f64() {
            assert!(value <= total, "progress beyond total: {p}");
        }
    }
    let values: Vec<f64> = progress
        .iter()
        .map(|p| p["progress"].as_f64().unwrap())
        .collect();
    assert!(values.windows(2).all(|w| w[1] > w[0]), "{values:?}");

    let (res, _) = server.wait_response(holder, RUN_WITHIN);
    assert!(text_of(&res).contains("SLOW-DONE"), "{res}");
}

#[test]
fn stopped_run_still_names_backend() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = slow_fixture(&dir);
    let holder = hold_slow_slot(&mut server, dir.path());

    // The cwd exists when the run is validated and vanishes while it waits for a slot.
    let gone = dir.path().join("gone");
    std::fs::create_dir_all(&gone).unwrap();
    let id = server.send_request(
        "tools/call",
        json!({
            "name": "sidekick_run",
            "arguments": {
                "prompt": "hello",
                "backend": "slow",
                "fallback": "good",
                "cwd": gone.to_string_lossy()
            }
        }),
    );
    wait_backend_line(&mut server, "slow", "running 1, queued 1");
    std::fs::remove_dir_all(&gone).unwrap();

    let (result, _) = server.wait_response(id, RUN_WITHIN);
    let text = text_of(&result);
    assert_eq!(result["isError"], json!(true), "{text}");
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("backend: slow (command)"), "{text}");
    assert!(
        lines
            .next()
            .is_some_and(|l| l.starts_with("Working directory does not exist")),
        "{text}"
    );
    assert!(
        text.contains("fallback 'good' (command) not attempted"),
        "{text}"
    );
    assert!(!text.contains("GOOD-RAN"), "{text}");

    let (res, _) = server.wait_response(holder, RUN_WITHIN);
    assert!(text_of(&res).contains("SLOW-DONE"), "{res}");
}
