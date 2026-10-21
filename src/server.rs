//! MCP server (rmcp, stdio) exposing `sidekick_run`, `sidekick_backends` and
//! `sidekick_status`.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{
        CallToolResult, ContentBlock, Implementation, ProgressNotificationParam, ProgressToken,
        ServerCapabilities, ServerConfig,
    },
    schemars,
    service::{Peer, RequestContext},
    tool, tool_handler, tool_router, ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
};
use serde::Deserialize;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::backends::reach::PROBE_TIMEOUT;
use crate::backends::{self, Backend, BackendType, Backends, Credential};
use crate::config::{Config, MAX_TIMEOUT_S, MAX_TURNS_LIMIT};
use crate::locate::{self, LocateError, Located};
use crate::redact::{redact_all, Secrets};
use crate::runner::{self, RunError, RunOutcome, RunRequest, TAIL_CHARS};
use crate::token::{self, SecretId, Token, TokenError, TokenSource};

const INSTRUCTIONS: &str = "agent-sidekick delegates work to a separate, headless agent: by \
default a second Claude Code instance with its own account, optionally other configured \
backends (a local model behind an Anthropic-compatible endpoint, or another agent CLI). Call \
`sidekick_backends` to see the configured backends with their description, cost and \
reachability, and pick one by its description and cost. Use `sidekick_run` to hand off a \
self-contained prompt (optionally with `backend` and a `fallback`); it runs in the given \
working directory and returns which backend ran, exit code, log path and the tail of the \
output. Use `sidekick_status` to see running/queued jobs per backend and diagnose setup \
problems. If a run fails because no token or key is stored, ask the user to run \
`agent-sidekick setup` (or `agent-sidekick setup --backend <name>`) in a terminal.";

/// Upper bound for all reachability checks of one `sidekick_backends` call.
const BACKENDS_CHECK_BUDGET: Duration = Duration::from_secs(PROBE_TIMEOUT.as_secs() + 1);

/// Maximum length (characters) of a failure reason in the `backend: ... fallback after` line.
const SHORT_REASON_CHARS: usize = 200;

/// Interval of `notifications/progress` while a run is queued or active. Claude Code resets its
/// MCP tool idle timeout (30 min by default) on every progress notification.
pub const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);

/// Test hook: overrides [`PROGRESS_INTERVAL`] (whole seconds, >= 1). Intentionally undocumented.
const ENV_PROGRESS_INTERVAL: &str = "AGENT_SIDEKICK_PROGRESS_INTERVAL_S";

/// How long shutdown waits for cancelled runs to kill their process trees and return.
const SHUTDOWN_RUNS_GRACE: Duration = Duration::from_secs(3);

/// First line of the result of a run the client cancelled while it was running.
pub const CANCELLED_LINE: &str = "cancelled by client (process tree killed)";

/// Result text for a run the client cancelled while it was still queued.
pub const CANCELLED_QUEUED_MSG: &str =
    "cancelled by client while queued; the agent was not started.";

/// Parameters of `sidekick_run`.
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
pub struct RunParams {
    /// The prompt for the delegated agent. It should be self-contained: the agent does not see
    /// this conversation.
    pub prompt: String,
    /// Working directory for the agent; must exist. Default: the server's current directory.
    /// A leading `~` is expanded to the home directory; relative paths are resolved against the
    /// server's current directory.
    pub cwd: Option<String>,
    /// Maximum number of agentic turns (1-200). Default: from config.toml (30 unless configured).
    pub max_turns: Option<u32>,
    /// Model for the agent, e.g. "sonnet" or "opus". Default: the backend's model from
    /// config.toml, else the account's/endpoint's default. Ignored by command backends.
    pub model: Option<String>,
    /// Tools the agent may use without asking, e.g. ["Read", "Edit", "Bash(git:*)"].
    /// Default: Claude Code's defaults for headless mode. Ignored by command backends.
    pub allowed_tools: Option<Vec<String>>,
    /// Timeout in seconds (1-7200); the whole process tree is killed when it expires.
    /// Default: from config.toml (1800 unless configured).
    pub timeout_s: Option<u64>,
    /// Backend to run on; see sidekick_backends. Default: the configured default backend.
    pub backend: Option<String>,
    /// Backend to use if the chosen one is unreachable, lacks credentials, fails to start, or
    /// exits with an error. Not used after a timeout or cancellation.
    pub fallback: Option<String>,
}

/// Validate `params` and apply defaults from `cfg`. `base_dir` resolves relative `cwd`s,
/// `home` expands a leading `~`. The error is a user-facing message. `backend` and
/// `fallback` are not looked at here (see [`select_backends`]).
pub fn resolve_request(
    params: RunParams,
    cfg: &Config,
    base_dir: &Path,
    home: Option<&Path>,
) -> Result<RunRequest, String> {
    if params.prompt.trim().is_empty() {
        return Err("Invalid parameter 'prompt': must not be empty.".into());
    }

    let max_turns = params.max_turns.unwrap_or(cfg.default_max_turns);
    if max_turns == 0 || max_turns > MAX_TURNS_LIMIT {
        return Err(format!(
            "Invalid parameter 'max_turns': {max_turns}; must be between 1 and {MAX_TURNS_LIMIT} \
             (max is {MAX_TURNS_LIMIT})."
        ));
    }

    let timeout_s = params.timeout_s.unwrap_or(cfg.default_timeout_s);
    if timeout_s == 0 || timeout_s > MAX_TIMEOUT_S {
        return Err(format!(
            "Invalid parameter 'timeout_s': {timeout_s}; must be between 1 and {MAX_TIMEOUT_S} \
             (max is {MAX_TIMEOUT_S})."
        ));
    }

    // Only the caller's model; the backend's default model (for the implicit backend:
    // `default_model` from config.toml) is applied when the launch is built.
    let model = params
        .model
        .map(|m| m.trim().to_owned())
        .filter(|m| !m.is_empty());

    let allowed_tools = params
        .allowed_tools
        .map(|tools| {
            tools
                .into_iter()
                .map(|t| t.trim().to_owned())
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
        })
        // An empty list cannot be expressed on the command line; treat it as "not given".
        .filter(|tools| !tools.is_empty());

    let cwd = resolve_cwd(params.cwd.as_deref(), base_dir, home)?;

    Ok(RunRequest {
        prompt: params.prompt,
        cwd,
        max_turns,
        model,
        allowed_tools,
        timeout: Duration::from_secs(timeout_s),
    })
}

/// Expand `~`, resolve relative paths against `base_dir` and check the directory exists.
fn resolve_cwd(cwd: Option<&str>, base_dir: &Path, home: Option<&Path>) -> Result<PathBuf, String> {
    let raw = match cwd.map(str::trim).filter(|c| !c.is_empty()) {
        None => return Ok(base_dir.to_path_buf()),
        Some(c) => c,
    };
    let expanded = match expand_tilde(raw, home) {
        Some(p) => p,
        None => {
            return Err(format!(
                "Invalid parameter 'cwd': could not expand '~' in '{raw}' because the home \
                 directory is unknown. Pass an absolute path."
            ))
        }
    };
    let path = if expanded.is_absolute() {
        expanded
    } else {
        base_dir.join(expanded)
    };
    if !path.exists() {
        return Err(format!(
            "Working directory does not exist: {}. Pass an existing directory as 'cwd' \
             (absolute paths are safest).",
            path.display()
        ));
    }
    if !path.is_dir() {
        return Err(format!(
            "Working directory is not a directory: {}. Pass an existing directory as 'cwd'.",
            path.display()
        ));
    }
    Ok(path)
}

/// `~`, `~/x` and `~\x` are expanded; `~user` is left alone. `None` if `~` needs a home dir
/// that is unknown.
fn expand_tilde(raw: &str, home: Option<&Path>) -> Option<PathBuf> {
    let Some(rest) = raw.strip_prefix('~') else {
        return Some(PathBuf::from(raw));
    };
    if rest.is_empty() {
        return home.map(Path::to_path_buf);
    }
    match rest.strip_prefix(['/', '\\']) {
        Some(sub) => home.map(|h| h.join(sub)),
        None => Some(PathBuf::from(raw)),
    }
}

/// User-facing text for a finished run.
pub fn format_outcome(outcome: &RunOutcome, timeout: Duration) -> String {
    let mut s = String::new();
    if outcome.cancelled {
        let _ = writeln!(s, "{CANCELLED_LINE}");
    } else if outcome.timed_out {
        let _ = writeln!(
            s,
            "timed out after {}s (process tree killed)",
            timeout.as_secs()
        );
    }
    match outcome.exit_code {
        Some(code) => {
            let _ = writeln!(s, "exit_code: {code}");
        }
        None => s.push_str("exit_code: none (process was killed)\n"),
    }
    let _ = writeln!(s, "log: {}", outcome.log_path.display());
    let _ = writeln!(s, "duration_s: {:.1}", outcome.duration.as_secs_f64());
    let _ = writeln!(s, "--- output (last {TAIL_CHARS} chars) ---");
    s.push_str(&outcome.tail);
    s
}

/// A run counts as failed (tool error) on cancellation, timeout or any non-zero / missing
/// exit code.
fn outcome_is_error(outcome: &RunOutcome) -> bool {
    outcome.cancelled || outcome.timed_out || outcome.exit_code != Some(0)
}

fn run_error_message(err: &RunError, ty: BackendType) -> String {
    match err {
        RunError::CwdMissing(p) => format!(
            "Working directory does not exist: {}. Pass an existing directory as 'cwd'.",
            p.display()
        ),
        RunError::Spawn { .. } if ty == BackendType::Command => format!(
            "{err}. Check the backend's `command` in config.toml \
             ('agent-sidekick backends test <name>')."
        ),
        RunError::Spawn { .. } => format!(
            "{err}. Check the claude installation with 'agent-sidekick doctor', or set \
             AGENT_SIDEKICK_CLAUDE to the absolute path of the claude executable."
        ),
        RunError::Io(_) => err.to_string(),
        RunError::Wait(_) => format!(
            "{err}. It may have changed files in cwd; no fallback is attempted for a run that \
             already started."
        ),
    }
}

/// Message when config.toml (or its `[backends]`) is invalid: no run is started, and in
/// particular no silent fallback to the implicit backend.
fn config_invalid_message(err: &str) -> String {
    format!(
        "config.toml is invalid: {err} — fix it (agent-sidekick doctor shows details); no run \
         was started"
    )
}

/// Trimmed, non-empty value of an optional name parameter.
fn non_blank(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// Resolve the `backend` / `fallback` parameters against `backends`. Errors are user-facing
/// and list the available names where that helps.
pub fn select_backends<'a>(
    backends: &'a Backends,
    backend: Option<&str>,
    fallback: Option<&str>,
) -> Result<(&'a Backend, Option<&'a Backend>), String> {
    let available = || backends.names().join(", ");
    let lookup = |param: &str, name: &str| {
        backends.get(name).ok_or_else(|| {
            format!(
                "Invalid parameter '{param}': unknown backend '{name}'. Available backends: {}. \
                 See sidekick_backends.",
                available()
            )
        })
    };
    let primary = match non_blank(backend) {
        Some(name) => lookup("backend", name)?,
        None => backends.default_backend(),
    };
    let fallback = match non_blank(fallback) {
        None => None,
        Some(name) => {
            let fb = lookup("fallback", name)?;
            if fb.name == primary.name {
                return Err(format!(
                    "Invalid parameter 'fallback': '{name}' is the same backend as the one the \
                     run uses; choose a different backend or omit 'fallback'."
                ));
            }
            if primary.backend_type() == BackendType::ClaudeOauth
                && fb.backend_type() == BackendType::ClaudeOauth
            {
                return Err(format!(
                    "Invalid parameter 'fallback': '{}' and '{}' are both claude-oauth backends \
                     (Claude accounts). Falling back from one Claude account to another is not \
                     supported: agent-sidekick does not switch accounts or work around rate \
                     limits. Use a claude-endpoint or command backend as fallback, or omit it.",
                    primary.name, fb.name
                ));
            }
            Some(fb)
        }
    };
    Ok((primary, fallback))
}

/// Result of one attempt to run a backend.
enum Attempt {
    /// The agent process ran (any exit code, timeout or cancellation).
    Ran(RunOutcome),
    /// The backend failed before its process ran (credentials, program, unreachable endpoint,
    /// launch building, spawn error). A fallback may be used.
    NotStarted(String),
    /// The run was stopped before it started (client cancelled while queued, server shutdown,
    /// working directory vanished). No fallback.
    Stopped(String),
}

/// Whether a fallback may be used after `attempt`: the primary failed before running, or
/// ran and exited unsuccessfully; never after a timeout or cancellation.
fn fallback_eligible(attempt: &Attempt) -> bool {
    match attempt {
        Attempt::Ran(o) => !o.timed_out && !o.cancelled && o.exit_code != Some(0),
        Attempt::NotStarted(_) => true,
        Attempt::Stopped(_) => false,
    }
}

/// First line of `reason`, cut to [`SHORT_REASON_CHARS`].
fn short_reason(reason: &str) -> String {
    let line = reason.lines().next().unwrap_or("").trim();
    let mut out: String = line.chars().take(SHORT_REASON_CHARS).collect();
    if line.chars().count() > SHORT_REASON_CHARS {
        out.push('…');
    }
    out
}

/// Why a run that exited counts as failed, e.g. `exit_code 3`.
fn exit_reason(o: &RunOutcome) -> String {
    match o.exit_code {
        Some(c) => format!("exit_code {c}"),
        None => "exit_code none (process was killed or ended by a signal)".to_owned(),
    }
}

fn backend_label(b: &Backend) -> String {
    format!("{} ({})", b.name, b.backend_type())
}

/// Text (and error flag) of a `sidekick_run` result. `ran` is the backend whose outcome is
/// shown; `fallback_from` is `(primary, primary attempt)` when the fallback ran.
fn format_run_result(
    ran: &Backend,
    outcome: &RunOutcome,
    req: &RunRequest,
    fallback_from: Option<(&Backend, &Attempt)>,
    extra_lines: &[String],
) -> String {
    let mut s = String::new();
    match fallback_from {
        None => {
            let _ = writeln!(s, "backend: {}", backend_label(ran));
        }
        Some((primary, attempt)) => {
            let reason = match attempt {
                Attempt::Ran(o) => exit_reason(o),
                Attempt::NotStarted(r) | Attempt::Stopped(r) => short_reason(r),
            };
            let _ = writeln!(
                s,
                "backend: {} — fallback after '{}' failed: {reason}",
                backend_label(ran),
                primary.name
            );
            if let Attempt::Ran(o) = attempt {
                let code = o
                    .exit_code
                    .map_or_else(|| "none".to_owned(), |c| c.to_string());
                let _ = writeln!(
                    s,
                    "primary: exit_code {code}, log {} — it ran and may have left changes in cwd",
                    o.log_path.display()
                );
            }
        }
    }
    for line in extra_lines {
        let _ = writeln!(s, "{line}");
    }
    if ran.backend_type() == BackendType::Command
        && (req.model.is_some() || req.allowed_tools.is_some())
    {
        s.push_str("note: model/allowed_tools were ignored (command backend)\n");
    }
    s.push_str(&format_outcome(outcome, req.timeout));
    s
}

/// Running/queued counters of one scope (global or one backend).
#[derive(Debug, Default)]
struct Counters {
    running: AtomicUsize,
    queued: AtomicUsize,
}

impl Counters {
    fn running(&self) -> usize {
        self.running.load(Ordering::SeqCst)
    }

    fn queued(&self) -> usize {
        self.queued.load(Ordering::SeqCst)
    }
}

/// Per-backend concurrency state.
#[derive(Debug, Default)]
struct BackendSlot {
    /// `max_concurrent` of the backend, if configured.
    semaphore: Option<Arc<Semaphore>>,
    counters: Counters,
}

/// One entry of the `sidekick_backends` result. Never contains secrets or env values.
#[derive(Debug, Clone, serde::Serialize)]
struct BackendInfo {
    name: String,
    #[serde(rename = "type")]
    kind: &'static str,
    description: String,
    cost: &'static str,
    default: bool,
    reachable: bool,
    detail: String,
    max_concurrent: Option<usize>,
    running: usize,
    queued: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
struct BackendsReport {
    config_error: Option<String>,
    backends: Vec<BackendInfo>,
}

/// Run blocking work (keyring access, filesystem probing) off the async runtime; a panic in
/// it becomes an error message instead of tearing down the request.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(f).await.map_err(|e| {
        format!(
            "internal error ({})",
            if e.is_panic() { "panic" } else { "cancelled" }
        )
    })
}

/// Increments counters for as long as the guard lives (also on cancellation).
struct CountGuard<'a>(Vec<&'a AtomicUsize>);

impl<'a> CountGuard<'a> {
    fn all(counters: impl IntoIterator<Item = &'a AtomicUsize>) -> Self {
        let counters: Vec<_> = counters.into_iter().collect();
        for c in &counters {
            c.fetch_add(1, Ordering::SeqCst);
        }
        Self(counters)
    }
}

impl Drop for CountGuard<'_> {
    fn drop(&mut self) {
        for c in &self.0 {
            c.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// Aborts a helper task (progress ticker, token forwarder) when the request ends in any way.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A token that fires when the client cancels the request (`notifications/cancelled`) or when
/// the server shuts down, whichever comes first.
fn run_cancel_token(
    request: &CancellationToken,
    shutdown: &CancellationToken,
) -> (CancellationToken, AbortOnDrop) {
    let token = shutdown.child_token();
    let (request, linked) = (request.clone(), token.clone());
    let forward = tokio::spawn(async move {
        tokio::select! {
            _ = request.cancelled() => linked.cancel(),
            _ = linked.cancelled() => {}
        }
    });
    (token, AbortOnDrop(forward))
}

/// Progress interval, overridable through [`ENV_PROGRESS_INTERVAL`].
fn progress_interval() -> Duration {
    std::env::var(ENV_PROGRESS_INTERVAL)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .map_or(PROGRESS_INTERVAL, Duration::from_secs)
}

/// One progress notification. `progress` is the number of whole seconds since the request
/// arrived (it only grows); `total` is known once the run has started (queue time + timeout).
/// The message never contains agent output.
fn progress_param(
    token: ProgressToken,
    since_request: Duration,
    queued_for: Option<Duration>,
    timeout: Duration,
) -> ProgressNotificationParam {
    let elapsed = since_request.as_secs();
    let param = ProgressNotificationParam::new(token, elapsed as f64);
    match queued_for {
        None => param.with_message(format!("queued for {elapsed}s, waiting for a free slot")),
        Some(q) => param
            .with_total((q.as_secs() + timeout.as_secs()) as f64)
            .with_message(format!(
                "agent running for {}s",
                since_request.saturating_sub(q).as_secs()
            )),
    }
}

/// Sends `notifications/progress` every `interval` until dropped. `run_started` is set once the
/// run leaves the queue.
fn spawn_progress_ticker(
    peer: Peer<RoleServer>,
    token: ProgressToken,
    run_started: Arc<OnceLock<Instant>>,
    timeout: Duration,
    interval: Duration,
) -> AbortOnDrop {
    let entered = Instant::now();
    AbortOnDrop(tokio::spawn(async move {
        let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticks.tick().await;
            let queued_for = run_started
                .get()
                .map(|s| s.saturating_duration_since(entered));
            let param = progress_param(token.clone(), entered.elapsed(), queued_for, timeout);
            if let Err(e) = peer.notify_progress(param).await {
                tracing::debug!("progress notification failed, stopping ticker: {e}");
                break;
            }
        }
    }))
}

#[derive(Debug)]
struct State {
    config: Config,
    /// `Config::load` error, reported by `sidekick_status` and `sidekick_backends`.
    config_error: Option<String>,
    /// The validated backends; `Err` if config.toml or its `[backends]` is invalid (then
    /// `sidekick_run` refuses to run instead of falling back to the implicit backend).
    backends: Result<Backends, String>,
    /// Global concurrency limit (`max_concurrent` in config.toml).
    semaphore: Arc<Semaphore>,
    /// Global running/queued counters.
    counters: Counters,
    /// Per-backend semaphores (`max_concurrent` of the backend) and counters.
    per_backend: HashMap<String, BackendSlot>,
    /// Cancelled when the server shuts down (client disconnect or signal); every run's
    /// cancellation token is a child of it, so in-flight process trees get killed.
    shutdown: CancellationToken,
}

#[derive(Debug, Clone)]
pub struct SidekickServer {
    state: Arc<State>,
}

fn text_result(text: String, is_error: bool) -> CallToolResult {
    let content = vec![ContentBlock::text(text)];
    if is_error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    }
}

fn tool_error(msg: impl AsRef<str>) -> Result<CallToolResult, McpError> {
    Ok(text_result(redact_all(msg.as_ref()), true))
}

/// Redact every secret of the run's attempts (and every `sk-ant-…` token).
fn redact_with(secrets: &[Secrets], text: &str) -> String {
    let mut out = redact_all(text);
    for s in secrets {
        out = s.redact(&out);
    }
    out
}

#[tool_router]
impl SidekickServer {
    /// Server state for `config`; `config_error` is the error of `Config::load` (then
    /// `config` holds the defaults and no backend is usable).
    pub fn new(config: Config, config_error: Option<String>) -> Self {
        let backends = match &config_error {
            Some(e) => Err(e.clone()),
            None => backends::resolve(&config),
        };
        Self::with_backends(config, config_error, backends)
    }

    fn with_backends(
        config: Config,
        config_error: Option<String>,
        backends: Result<Backends, String>,
    ) -> Self {
        let permits = config.max_concurrent.max(1);
        let per_backend = backends
            .iter()
            .flat_map(Backends::iter)
            .map(|b| {
                let slot = BackendSlot {
                    semaphore: b.max_concurrent.map(|n| Arc::new(Semaphore::new(n.max(1)))),
                    counters: Counters::default(),
                };
                (b.name.clone(), slot)
            })
            .collect();
        Self {
            state: Arc::new(State {
                config,
                config_error,
                backends,
                semaphore: Arc::new(Semaphore::new(permits)),
                counters: Counters::default(),
                per_backend,
                shutdown: CancellationToken::new(),
            }),
        }
    }

    /// Delegate a prompt to a separate, headless agent. By default this is a
    /// headless Claude Code instance that runs with its own account (not the caller's);
    /// `backend` selects another configured backend (see sidekick_backends: choose by
    /// description and cost), `fallback` names a backend to use if the chosen one is unreachable, lacks credentials,
    /// fails to start or exits with an error (not after a timeout). The agent runs in `cwd`
    /// with the given limits and cannot see this conversation, so the prompt must be
    /// self-contained. Runs beyond the concurrency limits wait in a queue. Returns which
    /// backend ran, the exit code, the path of the full log file, the duration and the last
    /// 4000 characters of the agent's output. Timeouts and non-zero exit codes are reported
    /// as tool errors.
    #[tool]
    async fn sidekick_run(
        &self,
        Parameters(params): Parameters<RunParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;

        // No silent fallback to the implicit backend when config.toml is broken.
        let backends = match &state.backends {
            Ok(b) => b,
            Err(e) => return tool_error(config_invalid_message(e)),
        };

        // Pure validation first: cheap, and independent of installs and credentials.
        let base_dir = match std::env::current_dir() {
            Ok(d) => d,
            Err(e) => {
                return tool_error(format!(
                    "Cannot determine the server's current directory: {e}. Pass an absolute 'cwd'."
                ))
            }
        };
        let home = crate::paths::home_dir().ok();
        let (backend_param, fallback_param) = (params.backend.clone(), params.fallback.clone());
        let req = match resolve_request(params, &state.config, &base_dir, home.as_deref()) {
            Ok(r) => r,
            Err(msg) => return tool_error(msg),
        };
        let (primary, fallback) = match select_backends(
            backends,
            backend_param.as_deref(),
            fallback_param.as_deref(),
        ) {
            Ok(sel) => sel,
            Err(msg) => return tool_error(msg),
        };
        let timeout = req.timeout;

        let (cancel, _forward) = run_cancel_token(&ctx.ct, &state.shutdown);
        let run_started = Arc::new(OnceLock::new());
        // Keeps Claude Code's MCP idle timeout from aborting long runs (and queue waits).
        let ticker = ctx.meta.get_progress_token().map(|token| {
            spawn_progress_ticker(
                ctx.peer.clone(),
                token,
                run_started.clone(),
                timeout,
                progress_interval(),
            )
        });

        let mut secrets = Vec::new();
        let first = self
            .attempt(primary, &req, &cancel, &ctx.ct, &run_started, &mut secrets)
            .await;
        let (text, is_error) = match (first, fallback) {
            (first, Some(fb)) if fallback_eligible(&first) => {
                let why = match &first {
                    Attempt::Ran(o) => exit_reason(o),
                    Attempt::NotStarted(r) | Attempt::Stopped(r) => short_reason(r),
                };
                tracing::warn!(
                    primary = %primary.name,
                    fallback = %fb.name,
                    "primary backend failed ({}); trying the fallback",
                    redact_with(&secrets, &why)
                );
                let second = self
                    .attempt(fb, &req, &cancel, &ctx.ct, &run_started, &mut secrets)
                    .await;
                match (second, &first) {
                    (Attempt::Ran(o), _) => (
                        format_run_result(fb, &o, &req, Some((primary, &first)), &[]),
                        outcome_is_error(&o),
                    ),
                    (Attempt::NotStarted(r) | Attempt::Stopped(r), Attempt::Ran(o)) => {
                        let line = format!(
                            "fallback '{}' ({}) failed too: {}",
                            fb.name,
                            fb.backend_type(),
                            short_reason(&r)
                        );
                        (format_run_result(primary, o, &req, None, &[line]), true)
                    }
                    (
                        Attempt::NotStarted(r2) | Attempt::Stopped(r2),
                        Attempt::NotStarted(r1) | Attempt::Stopped(r1),
                    ) => (
                        format!(
                            "backend '{}' ({}) failed: {r1}\nfallback '{}' ({}) failed: {r2}",
                            primary.name,
                            primary.backend_type(),
                            fb.name,
                            fb.backend_type()
                        ),
                        true,
                    ),
                }
            }
            (Attempt::Ran(o), _) => (
                format_run_result(primary, &o, &req, None, &[]),
                outcome_is_error(&o),
            ),
            (Attempt::NotStarted(r), _) => (
                format!(
                    "backend '{}' ({}) failed: {r}",
                    primary.name,
                    primary.backend_type()
                ),
                true,
            ),
            (Attempt::Stopped(r), _) => (r, true),
        };
        drop(ticker);
        Ok(text_result(redact_with(&secrets, &text), is_error))
    }

    /// List the configured backends the agent can run on, as JSON: name, type, description,
    /// cost (free | quota | paid), default, reachable (with a detail), max_concurrent and the
    /// current running/queued runs. Choose a backend for `sidekick_run` by its description and
    /// cost; without `backend`, sidekick_run uses the default one. Never shows secrets.
    #[tool]
    async fn sidekick_backends(&self) -> Result<CallToolResult, McpError> {
        let report = self.backends_report().await;
        let text = serde_json::to_string_pretty(&report)
            .unwrap_or_else(|e| format!("{{\"error\": \"serialization failed: {e}\"}}"));
        Ok(text_result(redact_all(&text), false))
    }

    /// Show the sidekick's state and setup: running and queued runs (global and per backend),
    /// concurrency limits, default limits, which claude executable is used, directories,
    /// config file and errors, and where each Claude account backend's token and each command
    /// backend's secret comes from (never the value itself).
    #[tool]
    async fn sidekick_status(&self) -> Result<CallToolResult, McpError> {
        let claude_cfg = self.state.config.claude_path.clone();
        let located = blocking(move || locate::find_claude(claude_cfg.as_deref())).await;
        let ids: Vec<SecretId> = match &self.state.backends {
            Ok(b) => b
                .iter()
                .filter(|b| {
                    matches!(
                        b.kind,
                        backends::BackendKind::ClaudeOauth { .. }
                            | backends::BackendKind::Command {
                                secret_env: Some(_),
                                ..
                            }
                    )
                })
                .filter_map(Backend::secret_id)
                .collect(),
            Err(_) => Vec::new(),
        };
        let tokens = blocking(move || {
            ids.into_iter()
                .map(|id| {
                    let r = token::load_secret(&id);
                    (id, r)
                })
                .collect::<Vec<_>>()
        })
        .await;
        let text = self.status_text(located, tokens);
        Ok(text_result(redact_all(&text), false))
    }
}

type LocateResult = Result<Result<Located, LocateError>, String>;
type TokenResults = Result<Vec<(SecretId, Result<(Token, TokenSource), TokenError>)>, String>;

impl SidekickServer {
    /// One attempt to run `backend`: credentials, program, endpoint probe, launch, permits
    /// (per backend first, then global; both cancellable), run. Secrets of the attempt are
    /// added to `secrets` for redaction of the result.
    async fn attempt(
        &self,
        backend: &Backend,
        req: &RunRequest,
        cancel: &CancellationToken,
        request_ct: &CancellationToken,
        run_started: &OnceLock<Instant>,
        secrets: &mut Vec<Secrets>,
    ) -> Attempt {
        let st = &self.state;
        let ty = backend.backend_type();
        let b = backend.clone();
        let claude_path = st.config.claude_path.clone();
        let prepared = blocking(move || -> Result<(Credential, PathBuf), String> {
            let (cred, _source) = backends::load_credential(&b)?;
            let program =
                backends::resolve_program(&b, claude_path.as_deref()).map_err(|e| {
                    match b.backend_type() {
                        BackendType::Command => e,
                        _ => format!("Claude Code executable not found: {e}"),
                    }
                })?;
            if let backends::BackendKind::ClaudeEndpoint { base_url, .. } = &b.kind {
                let r = backends::probe_http(base_url, PROBE_TIMEOUT);
                if !r.reachable {
                    let detail = r.detail.strip_prefix("unreachable: ").unwrap_or(&r.detail);
                    return Err(format!("endpoint unreachable: {detail}"));
                }
            }
            Ok((cred, program))
        })
        .await;
        let (cred, program) = match prepared {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => return Attempt::NotStarted(e),
            Err(e) => return Attempt::NotStarted(format!("preparing the run failed: {e}")),
        };
        if let Some(t) = cred.secret() {
            secrets.push(Secrets::new([t.expose()]));
        }
        let launch = match backends::build_launch(
            backend,
            req,
            &cred,
            &program,
            std::env::vars_os(),
            &st.config.pass_env,
        ) {
            Ok(l) => l,
            Err(e) => return Attempt::NotStarted(e),
        };
        drop(cred);
        secrets.push(launch.secrets.clone());
        let logs_dir = match crate::paths::logs_dir() {
            Ok(d) => d,
            Err(e) => {
                return Attempt::NotStarted(format!("Cannot determine the logs directory: {e:#}"))
            }
        };

        let slot = st.per_backend.get(&backend.name);
        let (backend_permit, global_permit) = {
            let _queued = CountGuard::all(
                std::iter::once(&st.counters.queued).chain(slot.map(|s| &s.counters.queued)),
            );
            let backend_permit = match slot.and_then(|s| s.semaphore.clone()) {
                None => None,
                Some(sem) => tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        tracing::info!("sidekick_run cancelled while queued");
                        return Attempt::Stopped(CANCELLED_QUEUED_MSG.into());
                    }
                    p = sem.acquire_owned() => match p {
                        Ok(p) => Some(p),
                        Err(_) => return Attempt::Stopped("Server is shutting down.".into()),
                    },
                },
            };
            let global_permit = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    tracing::info!("sidekick_run cancelled while queued");
                    return Attempt::Stopped(CANCELLED_QUEUED_MSG.into());
                }
                p = st.semaphore.clone().acquire_owned() => match p {
                    Ok(p) => p,
                    Err(_) => return Attempt::Stopped("Server is shutting down.".into()),
                },
            };
            (backend_permit, global_permit)
        };
        // The token may have fired while the permit was being handed over: never spawn then.
        // Check the request token directly as well: the forwarder may not have run yet.
        if cancel.is_cancelled() || request_ct.is_cancelled() {
            tracing::info!("sidekick_run cancelled before start");
            return Attempt::Stopped(CANCELLED_QUEUED_MSG.into());
        }
        let _running = CountGuard::all(
            std::iter::once(&st.counters.running).chain(slot.map(|s| &s.counters.running)),
        );
        let _ = run_started.set(Instant::now());
        tracing::info!(
            backend = %backend.name,
            backend_type = %ty,
            cwd = %req.cwd.display(),
            max_turns = req.max_turns,
            model = ?req.model,
            timeout_s = req.timeout.as_secs(),
            prompt_chars = req.prompt.chars().count(),
            "sidekick_run started"
        );
        let result = runner::run_launch(launch, req.clone(), &logs_dir, cancel.clone()).await;
        // Release this backend's slots before a fallback queues for its own.
        drop(global_permit);
        drop(backend_permit);

        match result {
            Ok(outcome) => {
                tracing::info!(
                    backend = %backend.name,
                    exit_code = ?outcome.exit_code,
                    timed_out = outcome.timed_out,
                    cancelled = outcome.cancelled,
                    log = %outcome.log_path.display(),
                    "sidekick_run finished"
                );
                Attempt::Ran(outcome)
            }
            Err(e) => {
                let msg = run_error_message(&e, ty);
                tracing::warn!(
                    backend = %backend.name,
                    "sidekick_run failed: {}",
                    redact_with(secrets, &msg)
                );
                match e {
                    // Wait: the process ran, so retrying elsewhere could duplicate its work.
                    RunError::CwdMissing(_) | RunError::Wait(_) => Attempt::Stopped(msg),
                    _ => Attempt::NotStarted(msg),
                }
            }
        }
    }

    /// `sidekick_backends` content: every backend with a reachability check; the checks run
    /// concurrently and are bounded by [`BACKENDS_CHECK_BUDGET`] in total.
    async fn backends_report(&self) -> BackendsReport {
        let st = &self.state;
        let backends = match &st.backends {
            Ok(b) => b,
            Err(e) => {
                return BackendsReport {
                    config_error: Some(e.clone()),
                    backends: Vec::new(),
                }
            }
        };
        let deadline = tokio::time::Instant::now() + BACKENDS_CHECK_BUDGET;
        let checks: Vec<_> = backends
            .iter()
            .map(|b| {
                let b = b.clone();
                let claude_path = st.config.claude_path.clone();
                tokio::task::spawn_blocking(move || {
                    backends::check_backend(&b, claude_path.as_deref())
                })
            })
            .collect();
        let mut list = Vec::new();
        for (b, check) in backends.iter().zip(checks) {
            let (reachable, detail) = match tokio::time::timeout_at(deadline, check).await {
                Ok(Ok(r)) => (r.reachable, r.detail),
                Ok(Err(e)) => (false, format!("check failed: {e}")),
                Err(_) => (
                    false,
                    format!(
                        "check did not finish within {}s",
                        BACKENDS_CHECK_BUDGET.as_secs()
                    ),
                ),
            };
            let counters = st.per_backend.get(&b.name).map(|s| &s.counters);
            list.push(BackendInfo {
                name: b.name.clone(),
                kind: b.backend_type().as_str(),
                description: b.description.clone(),
                cost: b.cost.as_str(),
                default: b.is_default,
                reachable,
                detail: redact_all(&detail),
                max_concurrent: b.max_concurrent,
                running: counters.map_or(0, Counters::running),
                queued: counters.map_or(0, Counters::queued),
            });
        }
        BackendsReport {
            config_error: st.config_error.clone(),
            backends: list,
        }
    }

    fn status_text(&self, located: LocateResult, tokens: TokenResults) -> String {
        let st = &self.state;
        let cfg = &st.config;
        let path_or_err = |r: anyhow::Result<PathBuf>| match r {
            Ok(p) => p.display().to_string(),
            Err(e) => format!("unavailable ({e:#})"),
        };

        let mut s = String::new();
        let _ = writeln!(s, "agent-sidekick {}", env!("CARGO_PKG_VERSION"));
        let _ = writeln!(s, "running: {}", st.counters.running());
        let _ = writeln!(s, "queued: {}", st.counters.queued());
        let _ = writeln!(s, "max_concurrent: {}", cfg.max_concurrent);
        let _ = writeln!(s, "default max_turns: {}", cfg.default_max_turns);
        let _ = writeln!(s, "default timeout_s: {}", cfg.default_timeout_s);
        let _ = writeln!(
            s,
            "default model: {}",
            cfg.default_model.as_deref().unwrap_or("(account default)")
        );
        match located {
            Ok(Ok(l)) => {
                let _ = writeln!(s, "claude: {} (found via {})", l.path.display(), l.source);
            }
            Ok(Err(e)) => {
                let _ = writeln!(s, "claude: NOT FOUND - {e}");
            }
            Err(e) => {
                let _ = writeln!(s, "claude: unknown - {e}");
            }
        }
        let _ = writeln!(
            s,
            "agent claude config dir: {}",
            path_or_err(crate::paths::agent_claude_config_dir())
        );
        let _ = writeln!(s, "config dir: {}", path_or_err(crate::paths::config_dir()));
        let _ = writeln!(s, "logs dir: {}", path_or_err(crate::paths::logs_dir()));
        let _ = writeln!(
            s,
            "config file: {}",
            path_or_err(crate::paths::config_file())
        );
        match (&st.config_error, &st.backends) {
            (Some(e), _) | (None, Err(e)) => {
                let _ = writeln!(
                    s,
                    "config error: {e} (sidekick_run is disabled until config.toml is fixed)"
                );
            }
            (None, Ok(_)) => s.push_str("config error: none\n"),
        }
        s.push_str("backends:\n");
        match &st.backends {
            Ok(backends) => {
                for b in backends.iter() {
                    let counters = st.per_backend.get(&b.name).map(|s| &s.counters);
                    let _ = writeln!(
                        s,
                        "  {} ({}{}): running {}, queued {}, max_concurrent {}",
                        b.name,
                        b.backend_type(),
                        if b.is_default { ", default" } else { "" },
                        counters.map_or(0, Counters::running),
                        counters.map_or(0, Counters::queued),
                        b.max_concurrent
                            .map_or_else(|| "-".to_owned(), |n| n.to_string())
                    );
                }
            }
            Err(_) => s.push_str("  (none usable: config.toml is invalid)\n"),
        }
        // Only the source is shown; secret values never leave this match.
        match tokens {
            Ok(list) => {
                for (id, r) in list {
                    let (what, name) = match &id {
                        SecretId::Oauth(n) | SecretId::Endpoint(n) => ("token", n),
                        SecretId::Command(n) => ("secret", n),
                    };
                    match r {
                        Ok((_, source)) => {
                            let _ = writeln!(s, "{what} ({name}): present ({source})");
                        }
                        Err(TokenError::Missing) | Err(TokenError::Empty) => {
                            let hint = if id.is_legacy() {
                                "agent-sidekick setup".to_owned()
                            } else {
                                format!("agent-sidekick setup --backend {name}")
                            };
                            let _ = writeln!(
                                s,
                                "{what} ({name}): missing - run '{hint}' in a terminal"
                            );
                        }
                        Err(e) => {
                            let _ = writeln!(s, "{what} ({name}): error - {e}");
                        }
                    }
                }
            }
            Err(e) => {
                let _ = writeln!(s, "token: unknown - {e}");
            }
        }
        s
    }
}

#[tool_handler]
impl ServerHandler for SidekickServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                crate::APP_NAME,
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }
}

/// Load the config (on error: defaults, with every run refused until it is fixed) and serve
/// MCP over stdio until the
/// client disconnects or a termination signal arrives. In both cases in-flight runs are
/// cancelled (their process trees killed) before this returns.
pub async fn serve() -> anyhow::Result<()> {
    // Linux: become a child subreaper so the runner can find setsid'ed descendants.
    runner::init_process_supervision();
    let (config, config_error) = match Config::load() {
        Ok(c) => (c, None),
        Err(e) => {
            let msg = redact_all(&format!("{e:#}"));
            tracing::warn!("config error, sidekick_run is disabled: {msg}");
            (Config::default(), Some(msg))
        }
    };
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        max_concurrent = config.max_concurrent,
        default_max_turns = config.default_max_turns,
        default_timeout_s = config.default_timeout_s,
        "agent-sidekick MCP server starting on stdio"
    );

    let server = SidekickServer::new(config, config_error);
    let state = server.state.clone();
    let service = server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| anyhow::anyhow!("MCP initialization failed: {e}"))?;
    let service_ct = service.cancellation_token();
    let waiting = service.waiting();
    tokio::pin!(waiting);
    tokio::select! {
        reason = &mut waiting => {
            let reason = reason?;
            tracing::info!("MCP client disconnected ({reason:?}); shutting down");
            state.shutdown_runs().await;
        }
        signal = termination_signal() => {
            tracing::info!("received {signal}; cancelling runs and shutting down");
            // Kill the runs first: the client may follow up with SIGKILL soon.
            state.shutdown_runs().await;
            service_ct.cancel();
            if tokio::time::timeout(Duration::from_secs(2), &mut waiting).await.is_err() {
                tracing::warn!("MCP service did not stop within 2s");
            }
        }
    }
    Ok(())
}

impl State {
    /// Cancel every queued and running run and wait (bounded) until they have returned, so
    /// their process trees are killed and their logs closed.
    async fn shutdown_runs(&self) {
        self.shutdown.cancel();
        let deadline = Instant::now() + SHUTDOWN_RUNS_GRACE;
        while self.counters.running() + self.counters.queued() > 0 {
            if Instant::now() >= deadline {
                tracing::warn!(
                    running = self.counters.running(),
                    "runs still active after shutdown grace; they are dropped with the runtime"
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// Resolves when a termination signal arrives: SIGTERM, SIGINT or SIGHUP on Unix, Ctrl+C /
/// Ctrl+Break on Windows. A signal that cannot be registered (e.g. no console) never fires.
#[cfg(unix)]
async fn termination_signal() -> &'static str {
    use tokio::signal::unix::{signal, SignalKind};
    async fn wait(kind: SignalKind) {
        match signal(kind) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::warn!("cannot listen for signal {kind:?}: {e}");
                std::future::pending::<()>().await;
            }
        }
    }
    tokio::select! {
        _ = wait(SignalKind::terminate()) => "SIGTERM",
        _ = wait(SignalKind::interrupt()) => "SIGINT",
        _ = wait(SignalKind::hangup()) => "SIGHUP",
    }
}

#[cfg(windows)]
async fn termination_signal() -> &'static str {
    use tokio::signal::windows::{ctrl_break, ctrl_close};
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::warn!("cannot listen for Ctrl+C: {e}");
            std::future::pending::<()>().await;
        }
    };
    let ctrl_break = async {
        match ctrl_break() {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::warn!("cannot listen for Ctrl+Break: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    let ctrl_close = async {
        match ctrl_close() {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::warn!("cannot listen for console close: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        _ = ctrl_c => "Ctrl+C",
        _ = ctrl_break => "Ctrl+Break",
        _ = ctrl_close => "console close",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(prompt: &str) -> RunParams {
        RunParams {
            prompt: prompt.into(),
            ..Default::default()
        }
    }

    #[test]
    fn defaults_applied() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            default_model: Some("sonnet".into()),
            ..Config::default()
        };
        let req = resolve_request(params("hi"), &cfg, dir.path(), None).unwrap();
        assert_eq!(req.prompt, "hi");
        assert_eq!(req.cwd, dir.path());
        assert_eq!(req.max_turns, cfg.default_max_turns);
        assert_eq!(req.timeout, Duration::from_secs(cfg.default_timeout_s));
        // default_model belongs to the implicit backend (applied by build_launch).
        assert_eq!(req.model, None);
        assert!(req.allowed_tools.is_none());
    }

    #[test]
    fn explicit_values_override_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = RunParams {
            max_turns: Some(200),
            timeout_s: Some(7200),
            model: Some("opus".into()),
            allowed_tools: Some(vec!["Read".into(), " ".into(), "Bash(git:*)".into()]),
            ..params("x")
        };
        let req = resolve_request(p, &Config::default(), dir.path(), None).unwrap();
        assert_eq!(req.max_turns, 200);
        assert_eq!(req.timeout, Duration::from_secs(7200));
        assert_eq!(req.model.as_deref(), Some("opus"));
        assert_eq!(
            req.allowed_tools,
            Some(vec!["Read".to_owned(), "Bash(git:*)".to_owned()])
        );
    }

    #[test]
    fn out_of_range_values_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::default();
        let check = |p: RunParams, needle: &str| {
            let err = resolve_request(p, &cfg, dir.path(), None).unwrap_err();
            assert!(err.contains(needle), "{err}");
        };
        check(
            RunParams {
                max_turns: Some(201),
                ..params("x")
            },
            "max is 200",
        );
        check(
            RunParams {
                max_turns: Some(0),
                ..params("x")
            },
            "max_turns",
        );
        check(
            RunParams {
                timeout_s: Some(7201),
                ..params("x")
            },
            "max is 7200",
        );
        check(
            RunParams {
                timeout_s: Some(0),
                ..params("x")
            },
            "timeout_s",
        );
        check(params("  "), "prompt");
    }

    #[test]
    fn tilde_and_relative_cwd() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join("proj")).unwrap();
        let cfg = Config::default();
        for cwd in ["~/proj", "~\\proj"] {
            let p = RunParams {
                cwd: Some(cwd.into()),
                ..params("x")
            };
            let req = resolve_request(p, &cfg, Path::new("/nowhere"), Some(home.path())).unwrap();
            assert_eq!(req.cwd, home.path().join("proj"));
        }
        let p = RunParams {
            cwd: Some("~".into()),
            ..params("x")
        };
        let req = resolve_request(p, &cfg, Path::new("/nowhere"), Some(home.path())).unwrap();
        assert_eq!(req.cwd, home.path());

        let p = RunParams {
            cwd: Some("proj".into()),
            ..params("x")
        };
        let req = resolve_request(p, &cfg, home.path(), None).unwrap();
        assert_eq!(req.cwd, home.path().join("proj"));

        let p = RunParams {
            cwd: Some("~/proj".into()),
            ..params("x")
        };
        assert!(resolve_request(p, &cfg, home.path(), None).is_err());
    }

    #[test]
    fn missing_or_file_cwd_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::default();
        let p = RunParams {
            cwd: Some("does-not-exist".into()),
            ..params("x")
        };
        let err = resolve_request(p, &cfg, dir.path(), None).unwrap_err();
        assert!(err.contains("does not exist"), "{err}");

        std::fs::write(dir.path().join("f.txt"), "x").unwrap();
        let p = RunParams {
            cwd: Some("f.txt".into()),
            ..params("x")
        };
        let err = resolve_request(p, &cfg, dir.path(), None).unwrap_err();
        assert!(err.contains("not a directory"), "{err}");
    }

    fn outcome(exit_code: Option<i32>, timed_out: bool) -> RunOutcome {
        RunOutcome {
            exit_code,
            timed_out,
            cancelled: false,
            log_path: PathBuf::from("run.log"),
            tail: "done".into(),
            duration: Duration::from_millis(12_340),
        }
    }

    #[test]
    fn outcome_formatting() {
        let ok = outcome(Some(0), false);
        let text = format_outcome(&ok, Duration::from_secs(60));
        assert_eq!(
            text,
            "exit_code: 0\nlog: run.log\nduration_s: 12.3\n--- output (last 4000 chars) ---\ndone"
        );
        assert!(!outcome_is_error(&ok));

        let to = outcome(None, true);
        let text = format_outcome(&to, Duration::from_secs(60));
        assert!(
            text.starts_with("timed out after 60s (process tree killed)\n"),
            "{text}"
        );
        assert!(outcome_is_error(&to));
        assert!(outcome_is_error(&outcome(Some(1), false)));
    }

    #[test]
    fn cancelled_outcome_formatting() {
        let mut c = outcome(None, false);
        c.cancelled = true;
        let text = format_outcome(&c, Duration::from_secs(60));
        assert_eq!(text.lines().next(), Some(CANCELLED_LINE), "{text}");
        assert_eq!(CANCELLED_LINE, "cancelled by client (process tree killed)");
        assert!(
            text.contains("exit_code: none (process was killed)"),
            "{text}"
        );
        assert!(text.contains("log: run.log"), "{text}");
        assert!(outcome_is_error(&c));

        // Cancellation wins over a timeout that raced with it; only one headline.
        c.timed_out = true;
        let text = format_outcome(&c, Duration::from_secs(60));
        assert!(text.starts_with(CANCELLED_LINE), "{text}");
        assert!(!text.contains("timed out"), "{text}");

        // Even a clean exit code is an error once the client cancelled.
        let mut c = outcome(Some(0), false);
        c.cancelled = true;
        assert!(outcome_is_error(&c));
    }

    #[test]
    fn progress_while_queued_and_running() {
        let token = ProgressToken(rmcp::model::NumberOrString::Number(7));
        let p = progress_param(
            token.clone(),
            Duration::from_millis(65_400),
            None,
            Duration::from_secs(600),
        );
        assert_eq!(p.progress, 65.0);
        assert_eq!(p.total, None);
        assert_eq!(
            p.message.as_deref(),
            Some("queued for 65s, waiting for a free slot")
        );

        let p = progress_param(
            token.clone(),
            Duration::from_secs(100),
            Some(Duration::from_secs(40)),
            Duration::from_secs(600),
        );
        assert_eq!(p.progress_token, token);
        assert_eq!(p.progress, 100.0);
        assert_eq!(p.total, Some(640.0));
        assert_eq!(p.message.as_deref(), Some("agent running for 60s"));
    }

    #[test]
    fn progress_interval_default() {
        assert_eq!(PROGRESS_INTERVAL, Duration::from_secs(30));
        if std::env::var_os(ENV_PROGRESS_INTERVAL).is_none() {
            assert_eq!(progress_interval(), PROGRESS_INTERVAL);
        }
    }

    #[tokio::test]
    async fn run_token_follows_request_and_shutdown() {
        let shutdown = CancellationToken::new();
        let request = CancellationToken::new();
        let (token, _fwd) = run_cancel_token(&request, &shutdown);
        assert!(!token.is_cancelled());
        request.cancel();
        tokio::time::timeout(Duration::from_secs(5), token.cancelled())
            .await
            .expect("request cancellation reaches the run token");
        assert!(!shutdown.is_cancelled(), "must not cancel other runs");

        let (token, _fwd) = run_cancel_token(&CancellationToken::new(), &shutdown);
        shutdown.cancel();
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn shutdown_runs_cancels_and_waits_for_guards() {
        let server = SidekickServer::new(Config::default(), None);
        let state = server.state.clone();
        let (token, _fwd) = run_cancel_token(&CancellationToken::new(), &state.shutdown);
        let st = state.clone();
        let run = tokio::spawn(async move {
            let _running = CountGuard::all([&st.counters.running]);
            token.cancelled().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        while state.counters.running() == 0 {
            tokio::task::yield_now().await;
        }
        state.shutdown_runs().await;
        assert_eq!(state.counters.running(), 0);
        run.await.unwrap();
    }

    fn backend(name: &str, kind: backends::BackendKind, is_default: bool) -> Backend {
        Backend {
            name: name.to_owned(),
            description: format!("{name} backend"),
            cost: backends::Cost::Free,
            is_default,
            max_concurrent: None,
            model: None,
            env: Default::default(),
            pass_env: Vec::new(),
            implicit: false,
            kind,
        }
    }

    fn oauth(name: &str, is_default: bool) -> Backend {
        backend(
            name,
            backends::BackendKind::ClaudeOauth {
                config_dir: PathBuf::from("/cfg"),
            },
            is_default,
        )
    }

    fn command(name: &str) -> Backend {
        backend(
            name,
            backends::BackendKind::Command {
                command: vec!["/bin/agent".into(), "{prompt}".into()],
                prompt_via_stdin: false,
                secret_env: None,
            },
            false,
        )
    }

    fn endpoint(name: &str) -> Backend {
        backend(
            name,
            backends::BackendKind::ClaudeEndpoint {
                base_url: "http://localhost:11434".into(),
                api_key_env: None,
                auth: Default::default(),
                config_dir: PathBuf::from("/cfg-local"),
            },
            false,
        )
    }

    fn sample_backends() -> Backends {
        let mut limited = command("aider");
        limited.max_concurrent = Some(1);
        Backends::new(vec![
            oauth("sidekick", true),
            oauth("other", false),
            endpoint("local"),
            limited,
        ])
    }

    #[test]
    fn select_default_and_named_backends() {
        let b = sample_backends();
        let (p, f) = select_backends(&b, None, None).unwrap();
        assert_eq!(p.name, "sidekick");
        assert!(f.is_none());
        let (p, f) = select_backends(&b, Some(" local "), Some("")).unwrap();
        assert_eq!(p.name, "local");
        assert!(f.is_none());
        let (p, f) = select_backends(&b, None, Some("aider")).unwrap();
        assert_eq!(
            (p.name.as_str(), f.unwrap().name.as_str()),
            ("sidekick", "aider")
        );
        // endpoint -> oauth is allowed (not an account switch).
        let (_, f) = select_backends(&b, Some("local"), Some("sidekick")).unwrap();
        assert_eq!(f.unwrap().name, "sidekick");
    }

    #[test]
    fn select_rejects_unknown_same_and_oauth_to_oauth() {
        let b = sample_backends();
        let err = select_backends(&b, Some("nope"), None).unwrap_err();
        assert!(err.contains("unknown backend 'nope'"), "{err}");
        assert!(err.contains("aider, local, other, sidekick"), "{err}");
        let err = select_backends(&b, None, Some("nope")).unwrap_err();
        assert!(err.contains("'fallback'"), "{err}");
        let err = select_backends(&b, None, Some("sidekick")).unwrap_err();
        assert!(err.contains("same backend"), "{err}");
        let err = select_backends(&b, Some("sidekick"), Some("other")).unwrap_err();
        assert!(err.contains("not supported"), "{err}");
        assert!(err.contains("rate"), "{err}");
    }

    #[test]
    fn fallback_only_after_failure_before_or_nonzero_exit() {
        assert!(fallback_eligible(&Attempt::NotStarted("x".into())));
        assert!(!fallback_eligible(&Attempt::Stopped("x".into())));
        assert!(fallback_eligible(&Attempt::Ran(outcome(Some(3), false))));
        assert!(fallback_eligible(&Attempt::Ran(outcome(None, false))));
        assert!(!fallback_eligible(&Attempt::Ran(outcome(Some(0), false))));
        assert!(!fallback_eligible(&Attempt::Ran(outcome(None, true))));
        let mut c = outcome(Some(1), false);
        c.cancelled = true;
        assert!(!fallback_eligible(&Attempt::Ran(c)));
    }

    fn request(model: Option<&str>) -> RunRequest {
        RunRequest {
            prompt: "p".into(),
            cwd: PathBuf::from("."),
            max_turns: 3,
            model: model.map(str::to_owned),
            allowed_tools: None,
            timeout: Duration::from_secs(60),
        }
    }

    #[test]
    fn run_result_names_backend_and_fallback() {
        let primary = oauth("sidekick", true);
        let fb = command("aider");
        let req = request(None);
        let ok = outcome(Some(0), false);

        let text = format_run_result(&primary, &ok, &req, None, &[]);
        assert!(
            text.starts_with("backend: sidekick (claude-oauth)\nexit_code: 0\n"),
            "{text}"
        );

        let mut ran = outcome(Some(3), false);
        ran.log_path = PathBuf::from("primary.log");
        let first = Attempt::Ran(ran);
        let text = format_run_result(&fb, &ok, &req, Some((&primary, &first)), &[]);
        let mut lines = text.lines();
        assert_eq!(
            lines.next(),
            Some("backend: aider (command) — fallback after 'sidekick' failed: exit_code 3")
        );
        assert_eq!(
            lines.next(),
            Some("primary: exit_code 3, log primary.log — it ran and may have left changes in cwd")
        );
        assert_eq!(lines.next(), Some("exit_code: 0"));

        let first = Attempt::NotStarted("No token stored for backend 'sidekick'.\nmore".into());
        let text = format_run_result(
            &fb,
            &ok,
            &request(Some("opus")),
            Some((&primary, &first)),
            &[],
        );
        assert!(
            text.starts_with(
                "backend: aider (command) — fallback after 'sidekick' failed: No token stored \
                 for backend 'sidekick'.\n"
            ),
            "{text}"
        );
        assert!(!text.contains("primary:"), "{text}");
        assert!(
            text.contains("note: model/allowed_tools were ignored (command backend)"),
            "{text}"
        );
        // No note for claude backends.
        let text = format_run_result(&primary, &ok, &request(Some("opus")), None, &[]);
        assert!(!text.contains("note:"), "{text}");
    }

    #[test]
    fn short_reason_is_one_bounded_line() {
        assert_eq!(short_reason("a\nb"), "a");
        let long = "x".repeat(500);
        assert_eq!(short_reason(&long).chars().count(), SHORT_REASON_CHARS + 1);
    }

    #[test]
    fn redaction_covers_every_attempt() {
        let secrets = vec![
            Secrets::new(["first-secret-value"]),
            Secrets::new(["second-secret-value"]),
            Secrets::new(["ollama"]),
        ];
        let text = redact_with(
            &secrets,
            "a first-secret-value b second-secret-value c sk-ant-oat01-xyz ollama",
        );
        assert_eq!(
            text, "a [REDACTED] b [REDACTED] c [REDACTED] ollama",
            "short placeholder keys are not redacted"
        );
    }

    #[test]
    fn per_backend_semaphores_and_counters() {
        let server = SidekickServer::with_backends(Config::default(), None, Ok(sample_backends()));
        let st = &server.state;
        assert_eq!(st.per_backend.len(), 4);
        let aider = &st.per_backend["aider"];
        assert_eq!(
            aider.semaphore.as_ref().map(|s| s.available_permits()),
            Some(1)
        );
        assert!(st.per_backend["sidekick"].semaphore.is_none());

        let _q = CountGuard::all([&st.counters.queued, &aider.counters.queued]);
        let text = server.status_text(Err("stub".into()), Ok(Vec::new()));
        assert!(text.contains("queued: 1\n"), "{text}");
        assert!(
            text.contains("  aider (command): running 0, queued 1, max_concurrent 1\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "  sidekick (claude-oauth, default): running 0, queued 0, max_concurrent -\n"
            ),
            "{text}"
        );
        assert!(text.contains("config error: none"), "{text}");
    }

    #[test]
    fn status_never_shows_token() {
        let server = SidekickServer::with_backends(Config::default(), None, Ok(sample_backends()));
        let secret = "sk-ant-oat01-SECRETSECRET";
        let text = server.status_text(
            Err("stub".into()),
            Ok(vec![
                (
                    SecretId::Oauth("sidekick".into()),
                    Ok((Token::new(secret), TokenSource::Env)),
                ),
                (SecretId::Oauth("other".into()), Err(TokenError::Missing)),
                (
                    SecretId::Command("kic".into()),
                    Ok((Token::new("cmd-secret-value-123456"), TokenSource::Keyring)),
                ),
                (SecretId::Command("aider".into()), Err(TokenError::Missing)),
            ]),
        );
        assert!(!text.contains(secret));
        assert!(!text.contains("cmd-secret-value"), "{text}");
        assert!(
            text.contains("secret (kic): present (OS keyring (service agent-sidekick))"),
            "{text}"
        );
        assert!(
            text.contains("secret (aider): missing - run 'agent-sidekick setup --backend aider'"),
            "{text}"
        );
        assert!(
            text.contains("token (sidekick): present (environment"),
            "{text}"
        );
        assert!(
            text.contains("token (other): missing - run 'agent-sidekick setup --backend other'"),
            "{text}"
        );
        assert!(text.contains("max_concurrent: 2"), "{text}");
    }

    #[test]
    fn invalid_config_disables_backends() {
        let server = SidekickServer::new(Config::default(), Some("bad config".into()));
        assert_eq!(server.state.backends, Err("bad config".to_owned()));
        let text = server.status_text(Err("stub".into()), Ok(Vec::new()));
        assert!(
            text.contains("config error: bad config (sidekick_run is disabled"),
            "{text}"
        );
        assert!(text.contains("none usable"), "{text}");
        let msg = config_invalid_message("two defaults");
        assert!(
            msg.starts_with("config.toml is invalid: two defaults"),
            "{msg}"
        );
        assert!(msg.contains("no run was started"), "{msg}");
    }

    #[tokio::test]
    async fn backends_report_with_invalid_config() {
        let server = SidekickServer::with_backends(
            Config::default(),
            None,
            Err("backends: two defaults".into()),
        );
        let report = server.backends_report().await;
        assert_eq!(
            report.config_error.as_deref(),
            Some("backends: two defaults")
        );
        assert!(report.backends.is_empty());
    }

    #[tokio::test]
    async fn backends_report_lists_command_backend() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-agent");
        let mut b = command("aider");
        b.is_default = true;
        b.kind = backends::BackendKind::Command {
            command: vec![missing.to_string_lossy().into_owned(), "{prompt}".into()],
            prompt_via_stdin: false,
            secret_env: None,
        };
        let server =
            SidekickServer::with_backends(Config::default(), None, Ok(Backends::new(vec![b])));
        let report = server.backends_report().await;
        assert_eq!(report.config_error, None);
        let json = serde_json::to_value(&report).unwrap();
        let entry = &json["backends"][0];
        assert_eq!(entry["name"], "aider");
        assert_eq!(entry["type"], "command");
        assert_eq!(entry["cost"], "free");
        assert_eq!(entry["default"], true);
        assert_eq!(entry["reachable"], false);
        assert_eq!(entry["max_concurrent"], serde_json::Value::Null);
        assert_eq!(entry["running"], 0);
    }
}
