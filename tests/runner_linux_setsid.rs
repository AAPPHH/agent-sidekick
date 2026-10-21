//! Linux only: processes that leave claude's process group with `setsid` (as Claude Code's
//! Bash tool does) are still killed, both on timeout and after a normal exit.
//!
//! A separate test binary on purpose: `init_process_supervision()` makes the whole test
//! process a child subreaper, and the orphan sweep may then kill and reap any other
//! untracked child of this process (e.g. a `Command::status()` in a concurrent test).
//! The scenarios run sequentially in one test because the "reparented to us" sweep only
//! runs while a single run is active.
#![cfg(target_os = "linux")]

use std::path::Path;
use std::time::{Duration, Instant};

use agent_sidekick::runner::{init_process_supervision, run, RunContext, RunRequest};
use agent_sidekick::token::Token;
use tokio_util::sync::CancellationToken;

fn context(dir: &Path, body: &str) -> RunContext {
    use std::os::unix::fs::PermissionsExt;
    let claude = dir.join("claude");
    std::fs::write(&claude, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    RunContext {
        claude,
        token: Token::new("plainsecretvalue_0123456789"),
        agent_config_dir: dir.join("agent-config"),
        logs_dir: dir.join("logs"),
    }
}

fn request(dir: &Path, timeout: Duration) -> RunRequest {
    RunRequest {
        prompt: "hi".into(),
        cwd: dir.to_path_buf(),
        max_turns: 1,
        model: None,
        allowed_tools: None,
        timeout,
    }
}

/// Waits (bounded) until the pid file exists and returns the pid.
fn read_pid(file: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(s) = std::fs::read_to_string(file) {
            if !s.trim().is_empty() {
                return s.trim().to_owned();
            }
        }
        assert!(Instant::now() < deadline, "{} not written", file.display());
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Gone or a zombie (`/proc/<pid>/stat` state `Z`), checked without spawning a process.
fn assert_gone_soon(pid: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok();
        let dead = stat.as_deref().is_none_or(|s| {
            s.rfind(')')
                .and_then(|i| s[i + 1..].split_whitespace().next())
                .is_some_and(|state| state == "Z" || state == "X")
        });
        if dead {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "detached grandchild {pid} still alive"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The inner `sh` writes its own pid and then execs `sleep`, so the pid file names the
/// detached process regardless of whether `setsid` forks. Its output goes to /dev/null,
/// like a Bash-tool command that talks to claude over its own pipes.
const DETACHED: &str = "setsid sh -c 'echo $$ > detached.pid; exec sleep 300' \
                        </dev/null >/dev/null 2>&1 &\n";

#[tokio::test]
async fn setsid_descendants_are_killed() {
    init_process_supervision();

    // 1. Timeout while the detached process is running.
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(
        dir.path(),
        &format!(
            "{DETACHED}while [ ! -s detached.pid ]; do sleep 0.05; done\necho started\nsleep 300\n"
        ),
    );
    let out = run(
        &ctx,
        request(dir.path(), Duration::from_secs(2)),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(out.timed_out, "{}", out.tail);
    assert!(out.tail.contains("started"), "{}", out.tail);
    assert_gone_soon(&read_pid(&dir.path().join("detached.pid")));

    // 2. Normal exit leaving the detached process behind.
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(
        dir.path(),
        &format!("{DETACHED}while [ ! -s detached.pid ]; do sleep 0.05; done\necho done\n"),
    );
    let out = run(
        &ctx,
        request(dir.path(), Duration::from_secs(30)),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(out.exit_code, Some(0), "{}", out.tail);
    assert_gone_soon(&read_pid(&dir.path().join("detached.pid")));
}
