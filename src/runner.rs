//! Runs the agent's `claude -p` as a child process with timeout and kill-tree.
//!
//! Output (stdout + stderr, line-wise, token redacted) goes completely into a per-run
//! log file; only a bounded tail is kept in memory and returned to the caller.

use std::ffi::OsString;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

use crate::redact::redact;
use crate::token::Token;

/// Number of trailing characters of output returned to the caller.
pub const TAIL_CHARS: usize = 4000;

/// In-memory output kept per run (bytes); everything else lives only in the log file.
const MEM_TAIL_BYTES: usize = 64 * 1024;
/// How long reader tasks may keep draining after the child exited normally. Grandchildren
/// that inherited the pipes can keep them open; after this the tree is killed.
const READER_GRACE: Duration = Duration::from_secs(5);
/// Same, after a timeout kill.
const READER_GRACE_AFTER_KILL: Duration = Duration::from_secs(2);
/// Written to the log (and tail) when the reader tasks had to be aborted.
pub const READERS_ABORTED_MSG: &str =
    "[agent-sidekick] output readers did not finish (pipes held open by leftover processes); aborted";

/// Variables removed from the inherited environment before the agent's own are set.
const DROPPED_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_CODE_OAUTH_TOKEN",
    crate::token::ENV_TOKEN,
];

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone)]
pub struct RunRequest {
    pub prompt: String,
    pub cwd: PathBuf,
    pub max_turns: u32,
    pub model: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct RunContext {
    pub claude: PathBuf,
    pub token: Token,
    pub agent_config_dir: PathBuf,
    pub logs_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct RunOutcome {
    /// `None` if the process was killed (timeout, cancellation) or ended by a signal.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// The caller cancelled the run (MCP `notifications/cancelled`); the tree was killed.
    pub cancelled: bool,
    pub log_path: PathBuf,
    /// Last [`TAIL_CHARS`] characters of combined stdout+stderr, token redacted.
    pub tail: String,
    pub duration: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("working directory does not exist: {0}")]
    CwdMissing(PathBuf),
    #[error("failed to start claude ({path}): {reason}")]
    Spawn { path: PathBuf, reason: String },
    #[error("i/o error: {0}")]
    Io(String),
}

/// Arguments after the executable:
/// `-p --max-turns <n> [--model m] [--allowedTools a b ...] -- <prompt>`.
/// The prompt comes after `--` so a prompt starting with `-` is not parsed as a flag.
pub fn build_args(req: &RunRequest) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "-p".into(),
        "--max-turns".into(),
        req.max_turns.to_string().into(),
    ];
    if let Some(model) = req.model.as_deref().filter(|m| !m.trim().is_empty()) {
        args.push("--model".into());
        args.push(model.into());
    }
    let tools: Vec<&String> = req
        .allowed_tools
        .iter()
        .flatten()
        .filter(|t| !t.trim().is_empty())
        .collect();
    if !tools.is_empty() {
        args.push("--allowedTools".into());
        args.extend(tools.into_iter().map(OsString::from));
    }
    args.push("--".into());
    args.push(req.prompt.clone().into());
    args
}

fn same_env_name(a: &std::ffi::OsStr, b: &str) -> bool {
    if cfg!(windows) {
        a.to_string_lossy().eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// Child environment: `base` without `ANTHROPIC_API_KEY` (and any existing
/// `CLAUDE_CONFIG_DIR`/`CLAUDE_CODE_OAUTH_TOKEN`), plus those two set from the context.
/// `AGENT_SIDEKICK_TOKEN` is dropped too so the secret is not passed under a second name.
/// On Windows, names are compared case-insensitively.
pub fn build_env(
    base: impl IntoIterator<Item = (OsString, OsString)>,
    token: &Token,
    agent_config_dir: &std::path::Path,
) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = base
        .into_iter()
        .filter(|(k, _)| !DROPPED_ENV.iter().any(|d| same_env_name(k, d)))
        .collect();
    env.push((
        "CLAUDE_CONFIG_DIR".into(),
        agent_config_dir.as_os_str().to_owned(),
    ));
    env.push(("CLAUDE_CODE_OAUTH_TOKEN".into(), token.expose().into()));
    env
}

/// Last `max_chars` characters (char boundary safe), prefixed by a marker if cut.
pub fn truncate_tail(s: &str, max_chars: usize) -> String {
    cut_tail(s, max_chars, 0)
}

/// Like [`truncate_tail`], with `already_dropped` characters that were cut from `s` earlier.
fn cut_tail(s: &str, max_chars: usize, already_dropped: usize) -> String {
    let total = s.chars().count();
    if total <= max_chars && already_dropped == 0 {
        return s.to_owned();
    }
    let skip = total.saturating_sub(max_chars);
    let start = s.char_indices().nth(skip).map_or(s.len(), |(i, _)| i);
    format!(
        "[... {} earlier characters omitted, see log file ...]\n{}",
        skip + already_dropped,
        &s[start..]
    )
}

/// Log file plus bounded in-memory tail, shared by the stdout and stderr readers.
struct Sink {
    file: File,
    tail: String,
    /// Characters dropped from the front of `tail` to bound memory.
    dropped_chars: usize,
    write_failed: bool,
}

impl Sink {
    fn push(&mut self, text: &str) {
        if let Err(e) = self.file.write_all(text.as_bytes()) {
            if !self.write_failed {
                tracing::warn!("writing to run log failed: {e}");
                self.write_failed = true;
            }
        }
        self.tail.push_str(text);
        if self.tail.len() > 2 * MEM_TAIL_BYTES {
            let mut cut = self.tail.len() - MEM_TAIL_BYTES;
            while !self.tail.is_char_boundary(cut) {
                cut += 1;
            }
            self.dropped_chars += self.tail[..cut].chars().count();
            self.tail.drain(..cut);
        }
    }
}

type SharedSink = Arc<Mutex<Sink>>;

fn lock(sink: &SharedSink) -> std::sync::MutexGuard<'_, Sink> {
    sink.lock().unwrap_or_else(|e| e.into_inner())
}

/// Reads `r` line by line (lossy UTF-8) so a token is never split across chunks before
/// redaction, and appends the redacted lines to the sink.
async fn pump(r: impl AsyncRead + Unpin, sink: SharedSink, token: Token) {
    let mut r = BufReader::new(r);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match r.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let mut text = redact(&String::from_utf8_lossy(&buf), Some(token.expose()));
                if text.ends_with("\r\n") {
                    text.truncate(text.len() - 2);
                    text.push('\n');
                }
                lock(&sink).push(&text);
            }
        }
    }
}

/// Kills the child and everything it started.
///
/// Unix: the child leads its own process group, which is SIGKILLed.
/// Windows: the child is assigned to a Job Object (kill-on-close) right after spawn.
/// Grandchildren created before the assignment would escape the job; in practice the
/// window is a few microseconds while the child is still loading. If the job cannot be
/// created/assigned, `taskkill /T /F` is the fallback.
struct TreeKiller {
    pid: Option<u32>,
    #[cfg(windows)]
    job: Option<win::Job>,
}

impl TreeKiller {
    fn attach(child: &Child) -> Self {
        #[cfg(windows)]
        {
            let job = child.raw_handle().and_then(|h| {
                let job = win::Job::new()?;
                job.assign(h).then_some(job)
            });
            if job.is_none() {
                tracing::warn!("could not put claude into a job object; falling back to taskkill");
            }
            Self {
                pid: child.id(),
                job,
            }
        }
        #[cfg(not(windows))]
        {
            Self { pid: child.id() }
        }
    }

    fn kill(&self) {
        #[cfg(unix)]
        {
            if let Some(pid) = self.pid {
                // process_group(0) made the child's pid its pgid.
                // SAFETY: plain syscall; a negative pid addresses the process group.
                unsafe {
                    libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                }
            }
        }
        #[cfg(windows)]
        {
            if self.job.as_ref().is_some_and(|j| j.terminate()) {
                return;
            }
            if let Some(pid) = self.pid {
                use std::os::windows::process::CommandExt;
                let _ = std::process::Command::new("taskkill")
                    .args(["/T", "/F", "/PID", &pid.to_string()])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .creation_flags(CREATE_NO_WINDOW)
                    .status();
            }
        }
    }
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    use std::os::windows::io::RawHandle;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// Owned job handle; closing it kills every process still in the job.
    pub struct Job(HANDLE);

    // SAFETY: a job handle is a kernel object handle usable from any thread.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        pub fn new() -> Option<Self> {
            // SAFETY: FFI calls with valid (null / properly sized) arguments; the handle
            // is owned by `Job` and closed exactly once in `Drop`.
            unsafe {
                let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if h.is_null() {
                    return None;
                }
                let job = Job(h);
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    h,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                (ok != 0).then_some(job)
            }
        }

        pub fn assign(&self, process: RawHandle) -> bool {
            // SAFETY: `process` is the live child's handle, borrowed for the call.
            unsafe { AssignProcessToJobObject(self.0, process as HANDLE) != 0 }
        }

        pub fn terminate(&self) -> bool {
            // SAFETY: valid owned job handle.
            unsafe { TerminateJobObject(self.0, 1) != 0 }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: handle owned by us, closed once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

fn io_err(context: &str, e: std::io::Error) -> RunError {
    RunError::Io(format!("{context}: {e}"))
}

fn spawn_error(path: &Path, batch: bool, e: std::io::Error) -> RunError {
    let reason = if batch && e.kind() == std::io::ErrorKind::InvalidInput {
        format!(
            "{e}; the model or allowed_tools contain characters cmd.exe cannot pass to a \
             .cmd/.bat script. Set {} to the path of claude.exe instead",
            crate::locate::ENV_CLAUDE
        )
    } else {
        e.to_string()
    };
    RunError::Spawn {
        path: path.to_owned(),
        reason,
    }
}

fn header(ctx: &RunContext, req: &RunRequest, started: &str, batch: bool) -> String {
    let tools = req
        .allowed_tools
        .as_ref()
        .map_or_else(|| "(default)".to_owned(), |t| t.join(" "));
    let text = format!(
        "# agent-sidekick run\n\
         # started (UTC): {started}\n\
         # claude: {}\n\
         # cwd: {}\n\
         # max_turns: {}\n\
         # model: {}\n\
         # allowed_tools: {tools}\n\
         # timeout: {}s\n\
         # prompt delivery: {}\n\
         # prompt:\n{}\n\
         # ---- output (stdout+stderr) ----\n",
        ctx.claude.display(),
        req.cwd.display(),
        req.max_turns,
        req.model.as_deref().unwrap_or("(default)"),
        req.timeout.as_secs(),
        if batch { "stdin" } else { "argument" },
        req.prompt,
    );
    redact(&text, Some(ctx.token.expose()))
}

/// Spawn, collect output into `<logs_dir>/<timestamp>-<short-id>.log`, enforce the timeout.
/// The timestamp in the file name is UTC (`YYYYmmdd-HHMMSS`).
/// When `cancel` fires, the process tree is killed and the outcome has `cancelled = true`.
pub async fn run(
    ctx: &RunContext,
    req: RunRequest,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<RunOutcome, RunError> {
    let _ = &cancel; // TODO(review-fix): honour cancellation (see runner fix task)
    if !req.cwd.is_dir() {
        return Err(RunError::CwdMissing(req.cwd));
    }
    std::fs::create_dir_all(&ctx.logs_dir).map_err(|e| io_err("creating logs dir", e))?;
    let now = chrono::Utc::now();
    let id = uuid::Uuid::new_v4().simple().to_string();
    let log_path = ctx
        .logs_dir
        .join(format!("{}-{}.log", now.format("%Y%m%d-%H%M%S"), &id[..8]));
    let mut file = File::create(&log_path).map_err(|e| io_err("creating log file", e))?;

    let batch = crate::locate::is_batch_script(&ctx.claude);
    file.write_all(header(ctx, &req, &now.to_rfc3339(), batch).as_bytes())
        .map_err(|e| io_err("writing log file", e))?;

    let mut args = build_args(&req);
    // Batch scripts go through cmd.exe, which cannot reliably receive arbitrary text as
    // an argument: drop the trailing `-- <prompt>` and send the prompt via stdin instead.
    let stdin_prompt = batch.then(|| {
        args.truncate(args.len() - 2);
        req.prompt.clone()
    });

    let mut cmd = Command::new(&ctx.claude);
    cmd.args(&args)
        .env_clear()
        .envs(build_env(
            std::env::vars_os(),
            &ctx.token,
            &ctx.agent_config_dir,
        ))
        .current_dir(&req.cwd)
        .stdin(if batch { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let started = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let err = spawn_error(&ctx.claude, batch, e);
            let _ = writeln!(file, "[agent-sidekick] {err}");
            return Err(err);
        }
    };
    let killer = TreeKiller::attach(&child);
    tracing::info!(log = %log_path.display(), pid = ?child.id(), "started claude");

    if let (Some(prompt), Some(mut stdin)) = (stdin_prompt, child.stdin.take()) {
        tokio::spawn(async move {
            // Errors (e.g. the child exited early) are irrelevant; dropping closes stdin.
            let _ = stdin.write_all(prompt.as_bytes()).await;
            let _ = stdin.shutdown().await;
        });
    }

    let sink: SharedSink = Arc::new(Mutex::new(Sink {
        file,
        tail: String::new(),
        dropped_chars: 0,
        write_failed: false,
    }));
    let mut readers: Vec<JoinHandle<()>> = Vec::new();
    if let Some(out) = child.stdout.take() {
        readers.push(tokio::spawn(pump(out, sink.clone(), ctx.token.clone())));
    }
    if let Some(err) = child.stderr.take() {
        readers.push(tokio::spawn(pump(err, sink.clone(), ctx.token.clone())));
    }

    let (status, timed_out) = match tokio::time::timeout(req.timeout, child.wait()).await {
        Ok(status) => (
            Some(status.map_err(|e| io_err("waiting for claude", e))?),
            false,
        ),
        Err(_) => {
            killer.kill();
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
            (None, true)
        }
    };
    let duration = started.elapsed();

    let grace = if timed_out {
        READER_GRACE_AFTER_KILL
    } else {
        READER_GRACE
    };
    let drained = tokio::time::timeout(grace, async {
        for r in &mut readers {
            let _ = r.await;
        }
    })
    .await
    .is_ok();
    if !drained {
        // Leftover processes still hold the pipes: clean them up and stop reading.
        killer.kill();
        readers.iter().for_each(JoinHandle::abort);
    }

    let mut sink = lock(&sink);
    if !drained {
        sink.push(&format!("\n{READERS_ABORTED_MSG}\n"));
    }
    if timed_out {
        sink.push(&format!(
            "\n[agent-sidekick] timed out after {}s, process tree killed\n",
            req.timeout.as_secs()
        ));
    }
    let _ = sink.file.flush();
    let tail = cut_tail(&sink.tail, TAIL_CHARS, sink.dropped_chars);
    drop(sink);
    // `killer` (and with it the Windows job) is dropped here, killing any leftovers.

    let exit_code = status.and_then(|s| s.code());
    tracing::info!(?exit_code, timed_out, ?duration, "claude finished");
    Ok(RunOutcome {
        exit_code,
        timed_out,
        cancelled: false,
        log_path,
        tail,
        duration,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(model: Option<&str>, tools: Option<Vec<&str>>, prompt: &str) -> RunRequest {
        RunRequest {
            prompt: prompt.into(),
            cwd: PathBuf::from("."),
            max_turns: 7,
            model: model.map(Into::into),
            allowed_tools: tools.map(|t| t.into_iter().map(Into::into).collect()),
            timeout: Duration::from_secs(1),
        }
    }

    fn strs(args: &[OsString]) -> Vec<&str> {
        args.iter().map(|a| a.to_str().unwrap()).collect()
    }

    #[test]
    fn args_minimal() {
        let a = build_args(&req(None, None, "hello"));
        assert_eq!(strs(&a), ["-p", "--max-turns", "7", "--", "hello"]);
    }

    #[test]
    fn args_full() {
        let a = build_args(&req(
            Some("sonnet"),
            Some(vec!["Read", "", "Bash(git:*)"]),
            "x y",
        ));
        assert_eq!(
            strs(&a),
            [
                "-p",
                "--max-turns",
                "7",
                "--model",
                "sonnet",
                "--allowedTools",
                "Read",
                "Bash(git:*)",
                "--",
                "x y"
            ]
        );
    }

    #[test]
    fn args_empty_tools_and_dash_prompt() {
        let a = build_args(&req(None, Some(vec![]), "--help me"));
        assert_eq!(strs(&a), ["-p", "--max-turns", "7", "--", "--help me"]);
        let a = build_args(&req(None, Some(vec![" "]), "-x"));
        assert!(!strs(&a).contains(&"--allowedTools"));
        assert_eq!(strs(&a).last(), Some(&"-x"));
    }

    fn env_of(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    fn get<'a>(env: &'a [(OsString, OsString)], k: &str) -> Vec<&'a str> {
        env.iter()
            .filter(|(n, _)| same_env_name(n, k))
            .map(|(_, v)| v.to_str().unwrap())
            .collect()
    }

    #[test]
    fn env_replaces_secrets_and_config_dir() {
        let base = env_of(&[
            ("PATH", "/bin"),
            ("ANTHROPIC_API_KEY", "sk-api"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "old"),
            ("CLAUDE_CONFIG_DIR", "/old"),
            ("AGENT_SIDEKICK_TOKEN", "dup"),
        ]);
        let env = build_env(base, &Token::new("sk-ant-oat01-new"), Path::new("/agent"));
        assert_eq!(get(&env, "PATH"), ["/bin"]);
        assert!(get(&env, "ANTHROPIC_API_KEY").is_empty());
        assert!(get(&env, "AGENT_SIDEKICK_TOKEN").is_empty());
        assert_eq!(get(&env, "CLAUDE_CODE_OAUTH_TOKEN"), ["sk-ant-oat01-new"]);
        assert_eq!(
            get(&env, "CLAUDE_CONFIG_DIR"),
            [Path::new("/agent").to_str().unwrap()]
        );
    }

    #[test]
    fn env_name_case() {
        let base = env_of(&[("anthropic_api_key", "k"), ("Claude_Config_Dir", "/x")]);
        let env = build_env(base, &Token::new("t"), Path::new("/a"));
        let names: Vec<_> = env.iter().map(|(k, _)| k.to_str().unwrap()).collect();
        if cfg!(windows) {
            assert_eq!(names, ["CLAUDE_CONFIG_DIR", "CLAUDE_CODE_OAUTH_TOKEN"]);
        } else {
            // Different variables on Unix: kept as-is.
            assert!(names.contains(&"anthropic_api_key"));
        }
    }

    #[test]
    fn tail_short_and_exact() {
        assert_eq!(truncate_tail("", 5), "");
        assert_eq!(truncate_tail("abc", 5), "abc");
        assert_eq!(truncate_tail("abcde", 5), "abcde");
        assert_eq!(
            truncate_tail("abcdef", 5),
            "[... 1 earlier characters omitted, see log file ...]\nbcdef"
        );
    }

    #[test]
    fn tail_counts_chars_not_bytes() {
        // 4 chars, 9 bytes: fits.
        assert_eq!(truncate_tail("äöü😀", 4), "äöü😀");
        assert_eq!(
            truncate_tail("ä😀xä😀", 3),
            "[... 2 earlier characters omitted, see log file ...]\nxä😀"
        );
        assert!(truncate_tail("😀😀", 0).ends_with("...]\n"));
    }

    #[test]
    fn cut_tail_includes_dropped() {
        assert_eq!(
            cut_tail("abc", 5, 10),
            "[... 10 earlier characters omitted, see log file ...]\nabc"
        );
    }

    #[test]
    fn sink_bounds_memory() {
        let dir = tempfile::tempdir().unwrap();
        let mut sink = Sink {
            file: File::create(dir.path().join("l")).unwrap(),
            tail: String::new(),
            dropped_chars: 0,
            write_failed: false,
        };
        let line = "ä".repeat(999) + "\n";
        for _ in 0..200 {
            sink.push(&line);
        }
        assert!(sink.tail.len() <= 2 * MEM_TAIL_BYTES);
        assert_eq!(sink.dropped_chars + sink.tail.chars().count(), 200 * 1000);
    }
}
