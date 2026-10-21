//! Runs the agent's `claude -p` as a child process with timeout, cancellation and kill-tree.
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

/// Prompts longer than this (in characters) are sent via stdin instead of as an argument,
/// so they stay far below the Windows command line limit (32767 UTF-16 units for the whole
/// line) and Linux' per-argument limit (128 KiB).
pub const PROMPT_STDIN_THRESHOLD: usize = 8000;

/// In-memory output kept per run (bytes); everything else lives only in the log file.
const MEM_TAIL_BYTES: usize = 64 * 1024;
/// Longest piece of a single output line buffered before it is flushed to the log (bytes).
/// Bounds memory for huge lines without a newline (minified JSON, base64, binary).
const PUMP_CHUNK_BYTES: usize = 64 * 1024;
/// How long reader tasks may keep draining after the child exited normally. Grandchildren
/// that inherited the pipes can keep them open; after this the tree is killed.
const READER_GRACE: Duration = Duration::from_secs(5);
/// Same, after a timeout / cancellation kill.
const READER_GRACE_AFTER_KILL: Duration = Duration::from_secs(2);
/// Unix: time between SIGTERM and SIGKILL on timeout/cancel, so claude can shut down its
/// own (detached) tool processes.
#[cfg(unix)]
const TERM_GRACE: Duration = Duration::from_secs(3);
/// Written to the log (and tail) when the reader tasks had to be aborted.
pub const READERS_ABORTED_MSG: &str =
    "[agent-sidekick] output readers did not finish (pipes held open by leftover processes); aborted";
/// Written to the log (and tail) when the client cancelled a running run.
pub const CANCELLED_MSG: &str = "[agent-sidekick] cancelled by client, process tree killed";
/// Written to the log (and returned as tail) when the run was cancelled before spawning.
pub const CANCELLED_BEFORE_START_MSG: &str =
    "[agent-sidekick] cancelled by client before start; claude was not started";

/// Variables removed from the inherited environment before the agent's own are set.
/// Besides our own variables these are all credentials/providers that outrank
/// `CLAUDE_CODE_OAUTH_TOKEN` in Claude Code's authentication precedence
/// (code.claude.com/docs/en/authentication), so the caller's account could otherwise be
/// used instead of the agent's.
const DROPPED_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_CODE_OAUTH_TOKEN",
    crate::token::ENV_TOKEN,
];

/// Inherited on purpose (a proxy may be needed), but worth a warning: the agent's token
/// is sent to that endpoint.
const BASE_URL_ENV: &str = "ANTHROPIC_BASE_URL";

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
/// When the prompt goes via stdin (see [`prompt_via_stdin`]), the trailing
/// `-- <prompt>` is dropped by the caller.
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

/// Why (and whether) the prompt is sent via stdin instead of as the last argument.
/// `claude -p` reads the prompt from stdin when no positional prompt is given.
/// - Batch scripts go through cmd.exe, which cannot reliably receive arbitrary text.
/// - Long prompts would hit the OS command line limits.
pub fn prompt_via_stdin(batch: bool, prompt: &str) -> Option<&'static str> {
    if batch {
        Some("stdin (batch script)")
    } else if prompt.chars().count() > PROMPT_STDIN_THRESHOLD {
        Some("stdin (prompt longer than 8000 characters)")
    } else {
        None
    }
}

fn same_env_name(a: &std::ffi::OsStr, b: &str) -> bool {
    if cfg!(windows) {
        a.to_string_lossy().eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// Child environment: `base` without [`DROPPED_ENV`] (competing credentials such as
/// `ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN`, cloud provider switches, and any existing
/// `CLAUDE_CONFIG_DIR`/`CLAUDE_CODE_OAUTH_TOKEN`), plus those two set from the context.
/// `AGENT_SIDEKICK_TOKEN` is dropped too so the secret is not passed under a second name.
/// `ANTHROPIC_BASE_URL` is kept (see [`run`], which warns about it).
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

/// Same prefix as `redact::redact_all` matches (`sk-ant-` followed by token characters).
const TOKEN_PATTERN_PREFIX: &[u8] = b"sk-ant-";

/// Characters `redact::redact_all` treats as part of an `sk-ant-` token.
fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

/// Start of a possible occurrence of `pat` that begins before `cut` and does not end
/// before it, judged by the bytes known in `buf` (an occurrence may continue past the end
/// of `buf`). Returns the smallest such start.
fn straddling(buf: &[u8], cut: usize, pat: &[u8]) -> Option<usize> {
    if pat.len() < 2 {
        return None;
    }
    (cut.saturating_sub(pat.len() - 1)..cut).find(|&o| {
        let known = &buf[o..buf.len().min(o + pat.len())];
        known == &pat[..known.len()]
    })
}

/// Where a line that is too long may be cut without splitting a UTF-8 character, a
/// `\r\n` pair, the literal `secret`, or an `sk-ant-` prefix. Everything before the cut
/// can then be redacted on its own: no secret crosses the cut. (An `sk-ant-` token whose
/// body continues past the cut is handled by [`ChunkRedactor`].) Returns `buf.len()` if no
/// such point exists (pathological input); correctness of the split is then best effort.
fn safe_cut(buf: &[u8], secret: &[u8]) -> usize {
    let mut cut = buf.len();
    loop {
        let before = cut;
        if let Some(o) = straddling(buf, cut, secret) {
            cut = o;
        }
        if let Some(o) = straddling(buf, cut, TOKEN_PATTERN_PREFIX) {
            cut = o;
        }
        // UTF-8: if the last character starting before `cut` reaches past it, cut before it.
        let lead = (cut.saturating_sub(4)..cut)
            .rev()
            .find(|&i| buf[i] & 0xC0 != 0x80);
        if let Some(i) = lead {
            let width = match buf[i] {
                b if b & 0x80 == 0 => 1,
                b if b & 0xE0 == 0xC0 => 2,
                b if b & 0xF0 == 0xE0 => 3,
                b if b & 0xF8 == 0xF0 => 4,
                _ => 1,
            };
            if i + width > cut {
                cut = i;
            }
        }
        // The next byte (not read yet) may be the `\n` of a `\r\n`.
        if cut == buf.len() && cut > 0 && buf[cut - 1] == b'\r' {
            cut -= 1;
        }
        if cut == before || cut == 0 {
            break;
        }
    }
    if cut == 0 {
        buf.len()
    } else {
        cut
    }
}

/// Turns raw output pieces into redacted text. Pieces are whole lines, or cuts of an
/// over-long line made at [`safe_cut`] points.
struct ChunkRedactor {
    token: Token,
    /// The previous piece ended inside an `sk-ant-…` token: drop its continuation.
    in_pattern_token: bool,
}

impl ChunkRedactor {
    fn text(&mut self, piece: &[u8], line_end: bool) -> String {
        let mut piece = piece;
        if self.in_pattern_token {
            let n = piece.iter().take_while(|b| is_token_byte(**b)).count();
            self.in_pattern_token = n == piece.len();
            piece = &piece[n..];
        }
        if !line_end {
            // Does the piece end inside an `sk-ant-` token? Its continuation (next piece)
            // must then be dropped as well; this piece redacts the beginning.
            let p = TOKEN_PATTERN_PREFIX;
            let last = piece.windows(p.len()).rposition(|w| w == p);
            if let Some(i) = last {
                if piece[i + p.len()..].iter().all(|b| is_token_byte(*b)) {
                    self.in_pattern_token = true;
                }
            }
        }
        let mut text = redact(&String::from_utf8_lossy(piece), Some(self.token.expose()));
        if line_end && text.ends_with("\r\n") {
            text.truncate(text.len() - 2);
            text.push('\n');
        }
        text
    }
}

/// Reads `r` line by line (lossy UTF-8), redacts each line and appends it to the sink.
/// Lines longer than [`PUMP_CHUNK_BYTES`] are flushed in pieces cut at safe points, so the
/// memory used per reader stays bounded and a token is never split before redaction.
async fn pump(r: impl AsyncRead + Unpin, sink: SharedSink, token: Token) {
    pump_chunked(r, sink, token, PUMP_CHUNK_BYTES).await;
}

/// [`pump`] with a configurable piece size; returns the largest number of bytes buffered.
async fn pump_chunked(
    r: impl AsyncRead + Unpin,
    sink: SharedSink,
    token: Token,
    cap: usize,
) -> usize {
    let mut r = BufReader::new(r);
    let mut pending: Vec<u8> = Vec::new();
    let mut redactor = ChunkRedactor {
        token: token.clone(),
        in_pattern_token: false,
    };
    let mut max_pending = 0;
    loop {
        // Invariant: pending.len() < cap here.
        let available = match r.fill_buf().await {
            Ok(b) if !b.is_empty() => b,
            _ => break,
        };
        let room = cap.saturating_sub(pending.len()).max(1);
        let take = available.len().min(room);
        let (n, line_end) = match available[..take].iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (take, false),
        };
        pending.extend_from_slice(&available[..n]);
        r.consume(n);
        max_pending = max_pending.max(pending.len());
        if line_end {
            let text = redactor.text(&pending, true);
            lock(&sink).push(&text);
            pending.clear();
        } else if pending.len() >= cap {
            // `safe_cut` returns at least 1, so less than `cap` bytes stay buffered.
            let cut = safe_cut(&pending, token.expose().as_bytes());
            let text = redactor.text(&pending[..cut], false);
            lock(&sink).push(&text);
            pending.drain(..cut);
        }
    }
    if !pending.is_empty() {
        let text = redactor.text(&pending, true);
        lock(&sink).push(&text);
    }
    max_pending
}

/// Kills the child and everything it started.
///
/// Unix: the child leads its own process group. On timeout/cancel the group first gets
/// SIGTERM (so claude can stop its own detached tool processes) and after [`TERM_GRACE`]
/// SIGKILL. When the run ends any other way (normal exit, error, the `run` future being
/// dropped on client disconnect) `Drop` SIGKILLs the group, so leftovers never outlive the
/// run, matching the Windows job.
/// Linux: additionally sweeps `/proc` for descendants that left the group (Claude Code's
/// Bash tool starts commands with `setsid`), see [`linux`].
/// Windows: the child is assigned to a Job Object (kill-on-close) right after spawn.
/// Grandchildren created before the assignment would escape the job; in practice the
/// window is a few microseconds while the child is still loading. If the job cannot be
/// created/assigned, `taskkill /T /F` is the fallback. Dropping the job kills the tree.
struct TreeKiller {
    pid: Option<u32>,
    /// The tree was already killed explicitly; `Drop` has nothing left to do.
    done: bool,
    #[cfg(windows)]
    job: Option<win::Job>,
    #[cfg(target_os = "linux")]
    tracked: Option<linux::Tracked>,
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
                done: false,
                job,
            }
        }
        #[cfg(not(windows))]
        {
            Self {
                pid: child.id(),
                done: false,
                #[cfg(target_os = "linux")]
                tracked: child.id().map(linux::Tracked::new),
            }
        }
    }

    /// Timeout / cancellation: SIGTERM, short grace, then [`Self::kill_now`]; waits
    /// (bounded) for the child to be reaped.
    async fn kill_graceful(&mut self, child: &mut Child) {
        #[cfg(unix)]
        {
            #[cfg(target_os = "linux")]
            if let Some(t) = self.tracked.as_mut() {
                // Before any signal, while the ppid links are still intact.
                t.snapshot();
            }
            if let Some(pid) = self.pid {
                // SAFETY: plain syscall; a negative pid addresses the process group.
                unsafe {
                    libc::kill(-(pid as libc::pid_t), libc::SIGTERM);
                }
                let _ = tokio::time::timeout(TERM_GRACE, child.wait()).await;
            }
        }
        self.kill_now();
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    }

    /// Kills the whole tree right away.
    fn kill_now(&mut self) {
        self.done = true;
        #[cfg(unix)]
        {
            if let Some(pid) = self.pid {
                // process_group(0) made the child's pid its pgid.
                // SAFETY: plain syscall; a negative pid addresses the process group.
                unsafe {
                    libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                }
            }
            #[cfg(target_os = "linux")]
            if let Some(t) = self.tracked.as_ref() {
                t.sweep();
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

impl Drop for TreeKiller {
    /// Unix: SIGKILL whatever is left of the group (and on Linux sweep detached
    /// descendants). Covers normal exit with leftover background processes, error returns
    /// and the `run` future being dropped.
    ///
    /// Pid reuse: the pgid is the claude pid. While any member of the group is alive the
    /// kernel does not hand that number out again, so the signal reaches our leftovers. If
    /// the group is empty the call fails with ESRCH. Hitting an unrelated group would need
    /// the pid to wrap around and be taken by a new group leader between claude being
    /// reaped and this drop (milliseconds) -- accepted.
    ///
    /// Windows: nothing to do here; dropping the job handle kills the tree.
    fn drop(&mut self) {
        #[cfg(unix)]
        if !self.done {
            self.kill_now();
        }
    }
}

/// Call once at server start. Linux: makes this process a child subreaper
/// (`PR_SET_CHILD_SUBREAPER`), so processes that claude's tools detach (`setsid`) and
/// orphan are reparented to us instead of init and can still be found and killed when a
/// run ends. No-op on other platforms.
pub fn init_process_supervision() {
    #[cfg(target_os = "linux")]
    linux::set_subreaper();
}

/// Parsed `/proc/<pid>/stat` fields needed to find and identify descendants.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProcStat {
    pid: i32,
    ppid: i32,
    pgrp: i32,
    /// Start time in clock ticks since boot; with `pid` it identifies a process even if
    /// the pid number is reused later.
    start: u64,
}

#[cfg(any(target_os = "linux", test))]
impl ProcStat {
    /// Same process (pid and start time), regardless of a changed parent or group.
    fn same(&self, other: &ProcStat) -> bool {
        self.pid == other.pid && self.start == other.start
    }
}

/// Parses `/proc/<pid>/stat`: `pid (comm) state ppid pgrp session ... starttime ...`.
/// `comm` may contain spaces and parentheses, so fields are counted after the last `)`.
#[cfg(any(target_os = "linux", test))]
fn parse_stat(s: &str) -> Option<ProcStat> {
    let pid = s.split(' ').next()?.parse().ok()?;
    let rest = &s[s.rfind(')')? + 1..];
    // Field 3 (state) is index 0; ppid = 4, pgrp = 5, starttime = 22.
    let f: Vec<&str> = rest.split_whitespace().collect();
    Some(ProcStat {
        pid,
        ppid: f.get(1)?.parse().ok()?,
        pgrp: f.get(2)?.parse().ok()?,
        start: f.get(19)?.parse().ok()?,
    })
}

/// All processes in `procs` below any of `roots` (following ppid links), roots excluded.
#[cfg(any(target_os = "linux", test))]
fn descendants(procs: &[ProcStat], roots: &[i32]) -> Vec<ProcStat> {
    let mut found: Vec<ProcStat> = Vec::new();
    let mut frontier: Vec<i32> = roots.to_vec();
    while let Some(parent) = frontier.pop() {
        for p in procs.iter().filter(|p| p.ppid == parent) {
            if !roots.contains(&p.pid) && !found.iter().any(|f| f.pid == p.pid) {
                found.push(*p);
                frontier.push(p.pid);
            }
        }
    }
    found
}

/// Linux-only cleanup of descendants that left claude's process group.
///
/// Claude Code starts Bash-tool commands detached (`setsid`), i.e. in their own session
/// and process group, so killing claude's group misses them. We therefore also:
/// - snapshot claude's descendants from `/proc` before signalling (ppid links intact),
/// - kill those (verified by start time against pid reuse) plus their current
///   descendants, and their process groups if they lead one,
/// - if we are a subreaper (see [`init_process_supervision`]) and this is the only active
///   run: kill everything reparented to us that is not a tracked claude, because only
///   descendants of our runs can be reparented to us. With concurrent runs this step is
///   skipped, since an orphan could belong to another, still running, run.
///
/// Processes reparented to us are reaped with `waitpid(pid, WNOHANG)` for exactly those
/// pids -- never `waitpid(-1)`, which would steal exit statuses from tokio.
/// Assumption: in the server process only this module spawns long-lived children.
#[cfg(target_os = "linux")]
mod linux {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, MutexGuard};
    use std::time::{Duration, Instant};

    use super::{descendants, parse_stat, ProcStat};

    /// Pids of claude processes started by `run` whose `TreeKiller` is still alive.
    static ACTIVE: Mutex<Vec<u32>> = Mutex::new(Vec::new());
    /// Killed processes reparented to us that still need reaping.
    static TO_REAP: Mutex<Vec<i32>> = Mutex::new(Vec::new());
    static SUBREAPER: AtomicBool = AtomicBool::new(false);

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_subreaper() {
        // SAFETY: prctl with plain integer arguments (variadic: passed as c_ulong).
        let rc = unsafe {
            libc::prctl(
                libc::PR_SET_CHILD_SUBREAPER,
                1 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            )
        };
        if rc == 0 {
            SUBREAPER.store(true, Ordering::SeqCst);
        } else {
            tracing::warn!(
                "PR_SET_CHILD_SUBREAPER failed: {}",
                std::io::Error::last_os_error()
            );
        }
    }

    fn read(pid: i32) -> Option<ProcStat> {
        parse_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
    }

    fn scan() -> Vec<ProcStat> {
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok())
            .filter_map(read)
            .collect()
    }

    pub struct Tracked {
        pid: u32,
        claude: Option<ProcStat>,
        snapshot: Vec<ProcStat>,
    }

    impl Tracked {
        pub fn new(pid: u32) -> Self {
            lock(&ACTIVE).push(pid);
            Self {
                pid,
                claude: read(pid as i32),
                snapshot: Vec::new(),
            }
        }

        /// Records claude's current descendants (only while claude itself is still ours).
        /// Their ppid links break once claude dies, hence the snapshot.
        pub fn snapshot(&mut self) {
            let procs = scan();
            if let Some(c) = self.claude.filter(|c| procs.iter().any(|p| p.same(c))) {
                self.snapshot = descendants(&procs, &[c.pid]);
            }
        }

        /// SIGKILLs every process attributed to this run (see the module docs). Two
        /// passes: children forked in the race between scan and kill are reparented to
        /// us (subreaper) or still hang below a known pid, so the second pass finds them.
        pub fn sweep(&self) {
            let me = std::process::id() as i32;
            // SAFETY: getpgrp has no preconditions.
            let my_pgrp = unsafe { libc::getpgrp() };
            for _ in 0..2 {
                let procs = scan();
                let active: Vec<i32> = lock(&ACTIVE).iter().map(|p| *p as i32).collect();
                // Only processes that are verifiably still the ones we saw.
                let mut roots: Vec<i32> = self
                    .claude
                    .iter()
                    .chain(&self.snapshot)
                    .filter(|s| procs.iter().any(|p| p.same(s)))
                    .map(|p| p.pid)
                    .collect();
                if SUBREAPER.load(Ordering::SeqCst) && active.len() <= 1 {
                    roots.extend(
                        procs
                            .iter()
                            .filter(|p| p.ppid == me && !active.contains(&p.pid))
                            .map(|p| p.pid),
                    );
                }
                let mut targets: Vec<ProcStat> = procs
                    .iter()
                    .filter(|p| roots.contains(&p.pid))
                    .copied()
                    .collect();
                targets.extend(descendants(&procs, &roots));
                targets.retain(|p| p.pid > 1 && p.pid != me);
                if targets.is_empty() {
                    break;
                }
                let mut to_reap = lock(&TO_REAP);
                for t in &targets {
                    // SAFETY: plain syscalls on pids verified above.
                    unsafe {
                        libc::kill(t.pid, libc::SIGKILL);
                        if t.pgrp == t.pid && t.pgrp != my_pgrp {
                            libc::kill(-t.pgrp, libc::SIGKILL);
                        }
                    }
                    // Tracked claudes are reaped by tokio; everything else that is our
                    // child was reparented to us and is reaped here.
                    if t.ppid == me && !active.contains(&t.pid) && !to_reap.contains(&t.pid) {
                        to_reap.push(t.pid);
                    }
                }
            }
            reap();
            tracing::debug!(pid = self.pid, "process tree sweep done");
        }
    }

    impl Drop for Tracked {
        fn drop(&mut self) {
            let mut active = lock(&ACTIVE);
            if let Some(i) = active.iter().position(|p| *p == self.pid) {
                active.swap_remove(i);
            }
        }
    }

    /// Reaps the killed, reparented processes (bounded wait; leftovers are retried by the
    /// next sweep). A pid stays in the list until reaped, so it cannot be reused meanwhile.
    fn reap() {
        let deadline = Instant::now() + Duration::from_millis(200);
        let mut list = lock(&TO_REAP);
        loop {
            list.retain(|&pid| {
                let mut status = 0;
                // SAFETY: waitpid on one specific pid that is our (non-tokio) child.
                let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
                r == 0 // 0: not exited yet; pid: reaped; -1: not our child any more
            });
            if list.is_empty() || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
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

/// The OS refused the command line as too long (Windows `ERROR_FILENAME_EXCED_RANGE`,
/// Unix `E2BIG`).
fn command_line_too_long(e: &std::io::Error) -> bool {
    #[cfg(windows)]
    let code = 206;
    #[cfg(unix)]
    let code = libc::E2BIG;
    e.raw_os_error() == Some(code)
}

fn spawn_error(path: &Path, batch: bool, e: std::io::Error) -> RunError {
    let reason = if batch && e.kind() == std::io::ErrorKind::InvalidInput {
        format!(
            "{e}; the model or allowed_tools contain characters cmd.exe cannot pass to a \
             .cmd/.bat script. Set {} to the path of claude.exe instead",
            crate::locate::ENV_CLAUDE
        )
    } else if command_line_too_long(&e) {
        format!(
            "{e}; the command line is too long for the operating system. Prompts over \
             {PROMPT_STDIN_THRESHOLD} characters already go via stdin, so shorten the model \
             or allowed_tools"
        )
    } else {
        e.to_string()
    };
    RunError::Spawn {
        path: path.to_owned(),
        reason,
    }
}

fn header(
    ctx: &RunContext,
    req: &RunRequest,
    started: &str,
    delivery: &str,
    base_url_inherited: bool,
) -> String {
    let tools = req
        .allowed_tools
        .as_ref()
        .map_or_else(|| "(default)".to_owned(), |t| t.join(" "));
    let warning = if base_url_inherited {
        format!(
            "# warning: {BASE_URL_ENV} is inherited from the server environment; \
             claude sends its requests (with the agent token) to that endpoint\n"
        )
    } else {
        String::new()
    };
    let text = format!(
        "# agent-sidekick run\n\
         # started (UTC): {started}\n\
         # claude: {}\n\
         # cwd: {}\n\
         # max_turns: {}\n\
         # model: {}\n\
         # allowed_tools: {tools}\n\
         # timeout: {}s\n\
         {warning}\
         # prompt delivery: {delivery}\n\
         # prompt:\n{}\n\
         # ---- output (stdout+stderr) ----\n",
        ctx.claude.display(),
        req.cwd.display(),
        req.max_turns,
        req.model.as_deref().unwrap_or("(default)"),
        req.timeout.as_secs(),
        req.prompt,
    );
    redact(&text, Some(ctx.token.expose()))
}

/// How the wait for claude ended.
enum Ended {
    Exited(std::io::Result<std::process::ExitStatus>),
    TimedOut,
    Cancelled,
}

/// Spawn, collect output into `<logs_dir>/<timestamp>-<short-id>.log`, enforce the timeout.
/// The timestamp in the file name is UTC (`YYYYmmdd-HHMMSS`).
/// When `cancel` fires, the process tree is killed and the outcome has `cancelled = true`;
/// if it has already fired, claude is not started at all.
pub async fn run(
    ctx: &RunContext,
    req: RunRequest,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<RunOutcome, RunError> {
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
    let stdin_delivery = prompt_via_stdin(batch, &req.prompt);
    let env = build_env(std::env::vars_os(), &ctx.token, &ctx.agent_config_dir);
    let base_url_inherited = env.iter().any(|(k, _)| same_env_name(k, BASE_URL_ENV));
    if base_url_inherited {
        tracing::warn!(
            "{BASE_URL_ENV} is inherited from the server environment; the agent's requests go to that endpoint"
        );
    }
    file.write_all(
        header(
            ctx,
            &req,
            &now.to_rfc3339(),
            stdin_delivery.unwrap_or("argument"),
            base_url_inherited,
        )
        .as_bytes(),
    )
    .map_err(|e| io_err("writing log file", e))?;

    if cancel.is_cancelled() {
        let _ = writeln!(file, "{CANCELLED_BEFORE_START_MSG}");
        tracing::info!(log = %log_path.display(), "run cancelled before start");
        return Ok(RunOutcome {
            exit_code: None,
            timed_out: false,
            cancelled: true,
            log_path,
            tail: CANCELLED_BEFORE_START_MSG.to_owned(),
            duration: Duration::ZERO,
        });
    }

    let mut args = build_args(&req);
    // Drop the trailing `-- <prompt>` and send the prompt via stdin instead.
    let stdin_prompt = stdin_delivery.map(|_| {
        args.truncate(args.len() - 2);
        req.prompt.clone()
    });

    let mut cmd = Command::new(&ctx.claude);
    cmd.args(&args)
        .env_clear()
        .envs(env)
        .current_dir(&req.cwd)
        .stdin(if stdin_prompt.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
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
    let mut killer = TreeKiller::attach(&child);
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

    let ended = tokio::select! {
        status = child.wait() => Ended::Exited(status),
        _ = tokio::time::sleep(req.timeout) => Ended::TimedOut,
        _ = cancel.cancelled() => Ended::Cancelled,
    };
    let (status, timed_out, cancelled) = match ended {
        Ended::Exited(status) => (
            Some(status.map_err(|e| io_err("waiting for claude", e))?),
            false,
            false,
        ),
        Ended::TimedOut => {
            killer.kill_graceful(&mut child).await;
            (None, true, false)
        }
        Ended::Cancelled => {
            killer.kill_graceful(&mut child).await;
            (None, false, true)
        }
    };
    let duration = started.elapsed();

    let grace = if timed_out || cancelled {
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
        killer.kill_now();
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
    if cancelled {
        sink.push(&format!("\n{CANCELLED_MSG}\n"));
    }
    let _ = sink.file.flush();
    let tail = cut_tail(&sink.tail, TAIL_CHARS, sink.dropped_chars);
    drop(sink);
    // Dropping `killer` kills whatever is left of the tree on every platform (Windows: job
    // handle; Unix: process group SIGKILL plus the Linux sweep), unless it was already
    // killed explicitly above.
    drop(killer);

    let exit_code = status.and_then(|s| s.code());
    tracing::info!(
        ?exit_code,
        timed_out,
        cancelled,
        ?duration,
        "claude finished"
    );
    Ok(RunOutcome {
        exit_code,
        timed_out,
        cancelled,
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

    #[test]
    fn env_drops_competing_credentials_keeps_base_url() {
        let names = [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
        ];
        let mut base: Vec<(&str, &str)> = names.iter().map(|n| (*n, "1")).collect();
        base.push(("ANTHROPIC_BASE_URL", "https://proxy.example"));
        let env = build_env(env_of(&base), &Token::new("t"), Path::new("/a"));
        for n in names {
            assert!(get(&env, n).is_empty(), "{n} must be dropped");
        }
        assert_eq!(get(&env, "ANTHROPIC_BASE_URL"), ["https://proxy.example"]);

        if cfg!(windows) {
            let lower: Vec<String> = names.iter().map(|n| n.to_lowercase()).collect();
            let base: Vec<(&str, &str)> = lower.iter().map(|n| (n.as_str(), "1")).collect();
            let env = build_env(env_of(&base), &Token::new("t"), Path::new("/a"));
            assert_eq!(env.len(), 2, "only the two agent variables remain: {env:?}");
        }
    }

    #[test]
    fn header_warns_about_inherited_base_url_without_value() {
        let ctx = RunContext {
            claude: PathBuf::from("claude"),
            token: Token::new("secret-token-value"),
            agent_config_dir: PathBuf::from("/a"),
            logs_dir: PathBuf::from("/l"),
        };
        let r = req(None, None, "hi");
        let h = header(&ctx, &r, "now", "argument", true);
        assert!(
            h.contains("# warning: ANTHROPIC_BASE_URL is inherited"),
            "{h}"
        );
        assert!(h.contains("# prompt delivery: argument"));
        let h = header(&ctx, &r, "now", "stdin (batch script)", false);
        assert!(!h.contains("warning"));
        assert!(h.contains("# prompt delivery: stdin (batch script)"));
    }

    #[test]
    fn long_prompts_go_via_stdin() {
        let short = "x".repeat(PROMPT_STDIN_THRESHOLD);
        let long = "x".repeat(PROMPT_STDIN_THRESHOLD + 1);
        assert_eq!(prompt_via_stdin(false, &short), None);
        assert_eq!(
            prompt_via_stdin(false, &long),
            Some("stdin (prompt longer than 8000 characters)")
        );
        assert_eq!(prompt_via_stdin(true, "x"), Some("stdin (batch script)"));
        // Counted in characters, not bytes.
        assert_eq!(
            prompt_via_stdin(false, &"ä".repeat(PROMPT_STDIN_THRESHOLD)),
            None
        );
    }

    #[test]
    fn command_line_too_long_has_clear_message() {
        #[cfg(windows)]
        let code = 206;
        #[cfg(unix)]
        let code = libc::E2BIG;
        let err = spawn_error(
            Path::new("claude.exe"),
            false,
            std::io::Error::from_raw_os_error(code),
        );
        assert!(
            err.to_string().contains("command line is too long"),
            "{err}"
        );
        let err = spawn_error(
            Path::new("claude.exe"),
            false,
            std::io::Error::from(std::io::ErrorKind::NotFound),
        );
        assert!(!err.to_string().contains("too long"), "{err}");
    }

    #[test]
    fn safe_cut_respects_tokens_utf8_and_crlf() {
        let secret = b"SECRET";
        // Ends with a proper prefix of the secret.
        assert_eq!(safe_cut(b"abc SEC", secret), 4);
        // Ends with a proper prefix of the sk-ant- pattern.
        assert_eq!(safe_cut(b"abc sk-an", secret), 4);
        // A complete pattern prefix may stay (the body is handled by ChunkRedactor).
        assert_eq!(safe_cut(b"abc sk-ant-xy", secret), 13);
        // Incomplete UTF-8 character at the end ("ä" = C3 A4).
        assert_eq!(safe_cut(b"abc \xC3", secret), 4);
        assert_eq!(safe_cut("abc ä".as_bytes(), secret), 6);
        // `\r` possibly followed by `\n`.
        assert_eq!(safe_cut(b"abc\r", secret), 3);
        // Nothing safe: everything is flushed.
        assert_eq!(safe_cut(b"SECRE", secret), 5);
    }

    fn mem_sink(dir: &Path) -> SharedSink {
        Arc::new(Mutex::new(Sink {
            file: File::create(dir.join("log")).unwrap(),
            tail: String::new(),
            dropped_chars: 0,
            write_failed: false,
        }))
    }

    /// What line-wise redaction of the whole input produces.
    fn reference(input: &str, token: &str) -> String {
        input
            .split_inclusive('\n')
            .map(|l| {
                let mut t = redact(l, Some(token));
                if t.ends_with("\r\n") {
                    t.truncate(t.len() - 2);
                    t.push('\n');
                }
                t
            })
            .collect()
    }

    #[tokio::test]
    async fn pump_chunks_are_redacted_like_whole_lines() {
        let token = "plainsecretvalue_0123456789";
        let mut input = String::new();
        for i in 0..40 {
            input.push_str(&format!(
                "a{i} {token} ä😀 sk-ant-oat01-Abc_DEF-{i}xyz {token}, ü "
            ));
            if i % 7 == 0 {
                input.push_str("\r\n");
            }
        }
        input.push_str("end without newline sk-ant-oat01-tail");
        let expected = reference(&input, token);
        assert!(!expected.contains(token));
        for cap in 30..120 {
            let dir = tempfile::tempdir().unwrap();
            let sink = mem_sink(dir.path());
            let max = pump_chunked(input.as_bytes(), sink.clone(), Token::new(token), cap).await;
            assert!(max <= cap, "cap {cap}: buffered {max}");
            let mut s = lock(&sink);
            s.file.flush().unwrap();
            let log = std::fs::read_to_string(dir.path().join("log")).unwrap();
            assert_eq!(log, expected, "cap {cap}");
            assert_eq!(s.tail, expected, "cap {cap}");
            drop(s);
        }
    }

    #[tokio::test]
    async fn pump_bounds_memory_for_one_megabyte_line() {
        let token = "plainsecretvalue_0123456789";
        let mut data = vec![b'x'; 1024 * 1024];
        // Tokens straddling the first two chunk boundaries, plus one in the middle.
        for at in [PUMP_CHUNK_BYTES - 10, 2 * PUMP_CHUNK_BYTES - 3, 500_000] {
            data[at - 1] = b' ';
            data[at..at + token.len()].copy_from_slice(token.as_bytes());
            data[at + token.len()] = b' ';
        }
        let pat = b"sk-ant-oat01-straddling_token";
        let at = 3 * PUMP_CHUNK_BYTES - 12;
        data[at - 1] = b' ';
        data[at..at + pat.len()].copy_from_slice(pat);
        data[at + pat.len()] = b' ';

        let dir = tempfile::tempdir().unwrap();
        let sink = mem_sink(dir.path());
        let max = pump_chunked(&data[..], sink.clone(), Token::new(token), PUMP_CHUNK_BYTES).await;
        assert!(max <= PUMP_CHUNK_BYTES, "buffered {max}");
        let s = lock(&sink);
        assert!(s.tail.len() <= 2 * MEM_TAIL_BYTES + PUMP_CHUNK_BYTES);
        drop(s);
        let log = std::fs::read_to_string(dir.path().join("log")).unwrap();
        assert!(!log.contains("plainsecret"));
        assert!(!log.contains("straddling"));
        assert!(!log.contains("sk-ant"));
        assert_eq!(log.matches("[REDACTED]").count(), 4);
        assert_eq!(log, reference(std::str::from_utf8(&data).unwrap(), token));
    }

    #[test]
    fn parses_proc_stat_with_odd_comm() {
        let s = "4242 (we ird) (name)) S 17 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 10";
        assert_eq!(
            parse_stat(s),
            Some(ProcStat {
                pid: 4242,
                ppid: 17,
                pgrp: 4242,
                start: 987654
            })
        );
        assert_eq!(parse_stat("garbage"), None);
    }

    #[test]
    fn descendants_follow_ppid_links() {
        let p = |pid, ppid| ProcStat {
            pid,
            ppid,
            pgrp: pid,
            start: 1,
        };
        let procs = [
            p(10, 1),
            p(11, 10),
            p(12, 11),
            p(13, 1),
            p(14, 12),
            p(15, 13),
        ];
        let mut d: Vec<i32> = descendants(&procs, &[10]).iter().map(|p| p.pid).collect();
        d.sort();
        assert_eq!(d, [11, 12, 14]);
        assert!(descendants(&procs, &[14]).is_empty());
        // Identity survives reparenting, not pid reuse.
        assert!(p(11, 10).same(&p(11, 1)));
        assert!(!p(11, 10).same(&ProcStat {
            start: 2,
            ..p(11, 10)
        }));
    }
}
