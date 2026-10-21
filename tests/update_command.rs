//! `agent-sidekick update` and the update hint of `doctor`, run as a binary against a
//! local fake release API (`AGENT_SIDEKICK_UPDATE_URL`).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Fake GitHub API: answers every request with `status` and `body`, records request lines.
struct FakeReleases {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl FakeReleases {
    fn start(status: &'static str, body: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recs = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf).into_owned();
                recs.lock()
                    .unwrap()
                    .push(head.lines().next().unwrap_or_default().to_owned());
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        Self { base_url, requests }
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

/// Runs the binary with an isolated config dir, no keyring, no claude and no proxy.
fn run(args: &[&str], config_dir: &Path, update_url: &str) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent-sidekick"));
    cmd.args(args);
    for (k, _) in std::env::vars_os() {
        let up = k.to_string_lossy().to_ascii_uppercase();
        if up.starts_with("AGENT_SIDEKICK_") || up.ends_with("_PROXY") {
            cmd.env_remove(&k);
        }
    }
    cmd.env("AGENT_SIDEKICK_CONFIG_DIR", config_dir)
        .env(
            "AGENT_SIDEKICK_CLAUDE_CONFIG_DIR",
            config_dir.join("claude"),
        )
        .env("AGENT_SIDEKICK_DISABLE_KEYRING", "1")
        .env("AGENT_SIDEKICK_CLAUDE", config_dir.join("no-such-claude"))
        .env("AGENT_SIDEKICK_UPDATE_URL", update_url);
    cmd.output().expect("run agent-sidekick")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn update_prints_install_command_for_a_newer_release() {
    let api = FakeReleases::start("200 OK", r#"{"tag_name":"v999.0.0"}"#);
    let dir = tempfile::tempdir().unwrap();
    let out = run(&["update"], dir.path(), &api.base_url);
    let text = stdout(&out);
    assert!(out.status.success(), "{text}{}", stderr(&out));
    assert!(text.contains("999.0.0 is available"), "{text}");
    let expected = if cfg!(windows) {
        "/releases/latest/download/install.ps1 | iex"
    } else {
        "/releases/latest/download/install.sh | sh"
    };
    assert!(text.contains(expected), "{text}");
    assert!(text.contains("reload VS Code"), "{text}");
    assert_eq!(api.request_count(), 1);
    // `update` itself never writes into the config dir.
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn update_reports_up_to_date() {
    let api = FakeReleases::start(
        "200 OK",
        concat!(r#"{"tag_name":"v"#, env!("CARGO_PKG_VERSION"), r#""}"#),
    );
    let dir = tempfile::tempdir().unwrap();
    let out = run(&["update"], dir.path(), &api.base_url);
    assert!(out.status.success());
    assert!(stdout(&out).contains("is up to date"), "{}", stdout(&out));
}

#[test]
fn update_without_any_release_is_not_an_error() {
    let api = FakeReleases::start("404 Not Found", r#"{"message":"Not Found"}"#);
    let dir = tempfile::tempdir().unwrap();
    let out = run(&["update"], dir.path(), &api.base_url);
    assert!(out.status.success());
    assert!(stdout(&out).contains("No release"), "{}", stdout(&out));
}

#[test]
fn update_rate_limit_fails_with_next_step() {
    let api = FakeReleases::start("403 Forbidden", r#"{"message":"API rate limit exceeded"}"#);
    let dir = tempfile::tempdir().unwrap();
    let out = run(&["update"], dir.path(), &api.base_url);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("rate limit"), "{err}");
    assert!(err.contains("next step:"), "{err}");
}

#[test]
fn update_garbage_and_offline_fail() {
    let api = FakeReleases::start("200 OK", "<html>not json</html>");
    let dir = tempfile::tempdir().unwrap();
    let out = run(&["update"], dir.path(), &api.base_url);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("unexpected answer"),
        "{}",
        stderr(&out)
    );

    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let out = run(&["update"], dir.path(), &format!("http://127.0.0.1:{port}"));
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("could not reach"), "{err}");
    assert!(err.contains("next step:"), "{err}");
}

#[test]
fn update_with_an_invalid_update_url_names_the_variable() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(&["update"], dir.path(), "not a url user:secret");
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("AGENT_SIDEKICK_UPDATE_URL"), "{err}");
    assert!(!err.contains("could not reach"), "{err}");
    assert!(!err.contains("secret"), "{err}");
    assert!(err.contains("next step:"), "{err}");
}

#[test]
fn doctor_offline_makes_no_request() {
    let api = FakeReleases::start("200 OK", r#"{"tag_name":"v999.0.0"}"#);
    let dir = tempfile::tempdir().unwrap();
    let out = run(&["doctor", "--offline"], dir.path(), &api.base_url);
    let text = stdout(&out);
    assert!(text.contains("update check: skipped (--offline)"), "{text}");
    assert_eq!(api.request_count(), 0);
    assert!(!dir.path().join("update-check.json").exists());
}

#[test]
fn doctor_checks_once_and_remembers_the_result() {
    let api = FakeReleases::start("200 OK", r#"{"tag_name":"v999.0.0"}"#);
    let dir = tempfile::tempdir().unwrap();
    let first = run(&["doctor"], dir.path(), &api.base_url);
    let text = stdout(&first);
    assert!(
        text.contains("[INFO] update: agent-sidekick 999.0.0 is available"),
        "{text}"
    );
    assert_eq!(api.request_count(), 1);
    assert!(dir.path().join("update-check.json").is_file());

    // Same day: no request and the hint is not repeated (shown at most once per day).
    let second = run(&["doctor"], dir.path(), &api.base_url);
    assert!(
        !stdout(&second).contains("[INFO] update"),
        "{}",
        stdout(&second)
    );
    assert_eq!(
        api.request_count(),
        1,
        "second doctor run must use the record"
    );
    // The hint never decides doctor's exit code.
    assert_eq!(first.status.code(), second.status.code());
}

#[test]
fn doctor_check_failure_is_quiet_and_creates_no_config_dir() {
    let api = FakeReleases::start("403 Forbidden", "{}");
    let dir = tempfile::tempdir().unwrap();
    let out = run(&["doctor"], dir.path(), &api.base_url);
    assert!(
        stdout(&out).contains("[INFO] update check failed"),
        "{}",
        stdout(&out)
    );
    assert!(!stdout(&out).contains("[FAIL] update"));

    // Missing config dir: no request and the directory is not created.
    let missing = dir.path().join("missing");
    let before = api.request_count();
    let out = run(&["doctor"], &missing, &api.base_url);
    assert!(
        stdout(&out).contains("update check: skipped"),
        "{}",
        stdout(&out)
    );
    assert_eq!(api.request_count(), before);
    assert!(!missing.exists());
}
