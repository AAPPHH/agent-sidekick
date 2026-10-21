//! Runs `agent_sidekick::runner::run` against a generated fake `claude` script.
//! Behaviour is baked into each generated script (no shared env mutation between tests).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agent_sidekick::runner::{run, RunContext, RunError, RunRequest, READERS_ABORTED_MSG};
use agent_sidekick::token::Token;

const TOKEN: &str = "plainsecretvalue_0123456789";

struct Fake {
    dir: tempfile::TempDir,
    ctx: RunContext,
}

/// Writes the fake claude (`body` is shell on Unix, batch on Windows) into a fresh tempdir.
fn fake(body: &str) -> Fake {
    let dir = tempfile::tempdir().unwrap();
    let claude = write_script(dir.path(), body);
    let ctx = RunContext {
        claude,
        token: Token::new(TOKEN),
        agent_config_dir: dir.path().join("agent-config"),
        logs_dir: dir.path().join("logs"),
    };
    Fake { dir, ctx }
}

#[cfg(unix)]
fn write_script(dir: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("claude");
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[cfg(windows)]
fn write_script(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("claude.cmd");
    std::fs::write(
        &path,
        format!("@echo off\r\n{}", body.replace('\n', "\r\n")),
    )
    .unwrap();
    path
}

fn request(f: &Fake, prompt: &str, timeout: Duration) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        cwd: f.dir.path().to_path_buf(),
        max_turns: 4,
        model: Some("sonnet".into()),
        allowed_tools: Some(vec!["Read".into(), "Bash(git:*)".into()]),
        timeout,
    }
}

#[cfg(unix)]
const ECHO_SCRIPT: &str = r#"for a in "$@"; do printf 'ARG:%s\n' "$a"; done
printf 'CFG:%s\n' "$CLAUDE_CONFIG_DIR"
if [ -n "$CLAUDE_CODE_OAUTH_TOKEN" ]; then echo TOKEN_SET:yes; fi
echo "TOKEN:$CLAUDE_CODE_OAUTH_TOKEN"
echo "PWD:$(pwd)"
echo "to stderr $CLAUDE_CODE_OAUTH_TOKEN" >&2
exit 3
"#;

#[cfg(windows)]
const ECHO_SCRIPT: &str = r#"echo ARGS:%*
echo CFG:%CLAUDE_CONFIG_DIR%
if defined CLAUDE_CODE_OAUTH_TOKEN echo TOKEN_SET:yes
echo TOKEN:%CLAUDE_CODE_OAUTH_TOKEN%
echo PWD:%CD%
echo to stderr %CLAUDE_CODE_OAUTH_TOKEN% 1>&2
echo STDIN-BEGIN
findstr "^"
echo STDIN-END
exit /b 3
"#;

#[tokio::test]
async fn propagates_args_env_exit_code_and_redacts() {
    let f = fake(ECHO_SCRIPT);
    let prompt = "-starts with a dash and has spaces";
    let out = run(&f.ctx, request(&f, prompt, Duration::from_secs(30)))
        .await
        .unwrap();
    let t = &out.tail;

    assert_eq!(out.exit_code, Some(3), "tail:\n{t}");
    assert!(!out.timed_out);

    #[cfg(unix)]
    {
        let args: Vec<&str> = t.lines().filter_map(|l| l.strip_prefix("ARG:")).collect();
        assert_eq!(
            args,
            [
                "-p",
                "--max-turns",
                "4",
                "--model",
                "sonnet",
                "--allowedTools",
                "Read",
                "Bash(git:*)",
                "--",
                prompt
            ]
        );
    }
    #[cfg(windows)]
    {
        let args = t.lines().find_map(|l| l.strip_prefix("ARGS:")).unwrap();
        assert!(
            args.starts_with("-p --max-turns 4 --model sonnet --allowedTools Read"),
            "{args}"
        );
        assert!(args.contains("Bash(git:*)"), "{args}");
        // The prompt is not an argument for batch scripts, it arrives on stdin.
        assert!(!args.contains("--") || !args.contains(prompt), "{args}");
        let stdin = t.split("STDIN-BEGIN\n").nth(1).unwrap();
        let stdin = stdin.split("STDIN-END").next().unwrap();
        assert_eq!(stdin.trim_end(), prompt);
    }

    let cfg = t.lines().find_map(|l| l.strip_prefix("CFG:")).unwrap();
    assert_eq!(Path::new(cfg.trim()), f.ctx.agent_config_dir);
    assert!(t.contains("TOKEN_SET:yes"));
    assert!(t.contains("TOKEN:[REDACTED]"), "tail:\n{t}");
    assert!(t.contains("to stderr [REDACTED]"), "tail:\n{t}");
    let pwd = t.lines().find_map(|l| l.strip_prefix("PWD:")).unwrap();
    assert_eq!(
        std::fs::canonicalize(pwd.trim()).unwrap(),
        std::fs::canonicalize(f.dir.path()).unwrap()
    );
    assert!(!t.contains(TOKEN));

    // Log file: in logs_dir, named <YYYYmmdd-HHMMSS>-<8 hex>.log, has header, no token.
    assert_eq!(out.log_path.parent().unwrap(), f.ctx.logs_dir);
    let name = out.log_path.file_name().unwrap().to_str().unwrap();
    assert_eq!(name.len(), "20260101-000000-abcdef01.log".len(), "{name}");
    assert!(name.ends_with(".log"));
    let log = std::fs::read_to_string(&out.log_path).unwrap();
    assert!(log.contains("max_turns: 4"));
    assert!(log.contains(prompt));
    assert!(log.contains("TOKEN:[REDACTED]"));
    assert!(!log.contains(TOKEN));
}

#[tokio::test]
async fn redacts_oauth_shaped_tokens_too() {
    #[cfg(unix)]
    let body = "echo leaked sk-ant-oat01-SomeOther_Token-1\nexit 0\n";
    #[cfg(windows)]
    let body = "echo leaked sk-ant-oat01-SomeOther_Token-1\nexit /b 0\n";
    let f = fake(body);
    let out = run(&f.ctx, request(&f, "hi", Duration::from_secs(30)))
        .await
        .unwrap();
    assert_eq!(out.exit_code, Some(0));
    assert!(out.tail.contains("leaked [REDACTED]"), "{}", out.tail);
    assert!(!out.tail.contains("SomeOther_Token"));
}

#[tokio::test]
async fn long_output_is_truncated_but_logged_completely() {
    #[cfg(unix)]
    let body = "i=1\nwhile [ $i -le 300 ]; do echo \"line $i xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"; i=$((i+1)); done\n";
    #[cfg(windows)]
    let body =
        "for /L %%i in (1,1,300) do @echo line %%i xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n";
    let f = fake(body);
    let out = run(&f.ctx, request(&f, "hi", Duration::from_secs(30)))
        .await
        .unwrap();
    assert_eq!(out.exit_code, Some(0));
    assert!(out.tail.starts_with("[... "), "{}", &out.tail[..80]);
    assert!(out
        .tail
        .contains("earlier characters omitted, see log file"));
    assert!(out
        .tail
        .trim_end()
        .ends_with("line 300 xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"));
    assert!(!out.tail.contains("line 1 x"));
    let marker_len = out.tail.find('\n').unwrap() + 1;
    assert_eq!(
        out.tail[marker_len..].chars().count(),
        agent_sidekick::runner::TAIL_CHARS
    );

    let log = std::fs::read_to_string(&out.log_path).unwrap();
    assert!(log.contains("line 1 x"));
    assert!(log.contains("line 300 x"));
}

#[tokio::test]
async fn missing_cwd_is_an_error() {
    let f = fake("exit 0\n");
    let mut req = request(&f, "hi", Duration::from_secs(5));
    req.cwd = f.dir.path().join("does-not-exist");
    match run(&f.ctx, req).await {
        Err(RunError::CwdMissing(p)) => assert!(p.ends_with("does-not-exist")),
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn missing_executable_is_a_spawn_error() {
    let mut f = fake("exit 0\n");
    f.ctx.claude = f.dir.path().join("no-such-claude.exe");
    let err = run(&f.ctx, request(&f, "hi", Duration::from_secs(5)))
        .await
        .unwrap_err();
    assert!(matches!(err, RunError::Spawn { .. }), "{err:?}");
    assert!(!err.to_string().contains(TOKEN));
}

#[tokio::test]
async fn timeout_kills_process_tree() {
    let timeout = Duration::from_secs(2);
    #[cfg(unix)]
    let (f, pid_file) = {
        // Relative to the run's cwd, which is the fake's tempdir.
        let f = fake("echo started\nsleep 300 &\necho $! > grandchild.pid\nwait\n");
        let pid_file = f.dir.path().join("grandchild.pid");
        (f, pid_file)
    };
    // `ping` inherits the stdout pipe; if it survived, the readers could not finish and
    // the runner would log READERS_ABORTED_MSG.
    #[cfg(windows)]
    let f = fake("echo started\nping -n 300 127.0.0.1\n");

    let begin = Instant::now();
    let out = run(&f.ctx, request(&f, "hang", timeout)).await.unwrap();
    let elapsed = begin.elapsed();

    assert!(out.timed_out);
    assert_eq!(out.exit_code, None);
    assert!(elapsed >= timeout);
    assert!(elapsed < timeout + Duration::from_secs(10), "{elapsed:?}");
    assert!(out.tail.contains("started"));
    assert!(out
        .tail
        .contains("[agent-sidekick] timed out after 2s, process tree killed"));
    let log = std::fs::read_to_string(&out.log_path).unwrap();
    assert!(log.contains("timed out after 2s"));
    assert!(!log.contains(READERS_ABORTED_MSG), "{log}");

    #[cfg(unix)]
    {
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let pid = pid.trim();
        // The orphaned sleep is reaped by init asynchronously; allow a moment.
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let alive = std::process::Command::new("kill")
                .args(["-0", pid])
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success();
            if !alive {
                break;
            }
            assert!(Instant::now() < deadline, "grandchild {pid} still alive");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}
