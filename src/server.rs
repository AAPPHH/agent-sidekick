//! MCP server (rmcp, stdio) exposing `sidekick_run` and `sidekick_status`.

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

use crate::config::{Config, MAX_TIMEOUT_S, MAX_TURNS_LIMIT};
use crate::locate::{self, LocateError, Located};
use crate::redact::{redact, redact_all};
use crate::runner::{self, RunContext, RunError, RunOutcome, RunRequest, TAIL_CHARS};
use crate::token::{self, Token, TokenError, TokenSource};

const INSTRUCTIONS: &str = "agent-sidekick delegates work to a second, headless Claude Code \
instance that runs with its own account and credentials. Use `sidekick_run` to hand off a \
self-contained prompt (it runs in the given working directory and returns exit code, log path \
and the tail of the output). Use `sidekick_status` to see running/queued jobs and diagnose setup \
problems. If a run fails because no token is stored, ask the user to run `agent-sidekick setup` \
in a terminal.";

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
    /// Model for the agent, e.g. "sonnet" or "opus". Default: from config.toml, else the
    /// account's default model.
    pub model: Option<String>,
    /// Tools the agent may use without asking, e.g. ["Read", "Edit", "Bash(git:*)"].
    /// Default: Claude Code's defaults for headless mode.
    pub allowed_tools: Option<Vec<String>>,
    /// Timeout in seconds (1-7200); the whole process tree is killed when it expires.
    /// Default: from config.toml (1800 unless configured).
    pub timeout_s: Option<u64>,
}

/// Validate `params` and apply defaults from `cfg`. `base_dir` resolves relative `cwd`s,
/// `home` expands a leading `~`. The error is a user-facing message.
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

    let model = params
        .model
        .map(|m| m.trim().to_owned())
        .filter(|m| !m.is_empty())
        .or_else(|| cfg.default_model.clone());

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

fn token_error_message(err: &TokenError) -> String {
    match err {
        TokenError::Missing | TokenError::Empty => {
            "No token stored for the agent account. Run 'agent-sidekick setup' in a terminal."
                .into()
        }
        TokenError::Storage(e) => format!(
            "Could not read the agent account's token ({e}). Run 'agent-sidekick doctor' in a \
             terminal, or 'agent-sidekick setup' to store it again."
        ),
    }
}

fn run_error_message(err: &RunError) -> String {
    match err {
        RunError::CwdMissing(p) => format!(
            "Working directory does not exist: {}. Pass an existing directory as 'cwd'.",
            p.display()
        ),
        RunError::Spawn { .. } => format!(
            "{err}. Check the claude installation with 'agent-sidekick doctor', or set \
             AGENT_SIDEKICK_CLAUDE to the absolute path of the claude executable."
        ),
        RunError::Io(_) => err.to_string(),
    }
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

/// Increments a counter for as long as the guard lives (also on cancellation).
struct CountGuard<'a>(&'a AtomicUsize);

impl<'a> CountGuard<'a> {
    fn new(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}

impl Drop for CountGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
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
    /// `Config::load` error, reported by `sidekick_status`.
    config_error: Option<String>,
    semaphore: Arc<Semaphore>,
    running: AtomicUsize,
    queued: AtomicUsize,
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

#[tool_router]
impl SidekickServer {
    pub fn new(config: Config, config_error: Option<String>) -> Self {
        let permits = config.max_concurrent.max(1);
        Self {
            state: Arc::new(State {
                config,
                config_error,
                semaphore: Arc::new(Semaphore::new(permits)),
                running: AtomicUsize::new(0),
                queued: AtomicUsize::new(0),
                shutdown: CancellationToken::new(),
            }),
        }
    }

    /// Delegate a prompt to a separate, headless Claude Code instance that runs with its own
    /// account (not the caller's). The agent runs `claude -p` in `cwd` with the given limits and
    /// cannot see this conversation, so the prompt must be self-contained. Runs beyond the
    /// configured concurrency limit wait in a queue. Returns the exit code, the path of the full
    /// log file, the duration and the last 4000 characters of the agent's output. Timeouts and
    /// non-zero exit codes are reported as tool errors.
    #[tool]
    async fn sidekick_run(
        &self,
        Parameters(params): Parameters<RunParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;

        // Pure validation first: cheap, and independent of the claude install and the token.
        let base_dir = match std::env::current_dir() {
            Ok(d) => d,
            Err(e) => {
                return tool_error(format!(
                    "Cannot determine the server's current directory: {e}. Pass an absolute 'cwd'."
                ))
            }
        };
        let home = crate::paths::home_dir().ok();
        let req = match resolve_request(params, &state.config, &base_dir, home.as_deref()) {
            Ok(r) => r,
            Err(msg) => return tool_error(msg),
        };

        let claude_cfg = state.config.claude_path.clone();
        let located = match blocking(move || locate::find_claude(claude_cfg.as_deref())).await {
            Ok(Ok(l)) => l,
            Ok(Err(e)) => return tool_error(format!("Claude Code executable not found: {e}")),
            Err(e) => return tool_error(format!("Locating claude failed: {e}")),
        };
        let token = match blocking(token::load).await {
            Ok(Ok((t, _))) => t,
            Ok(Err(e)) => return tool_error(token_error_message(&e)),
            Err(e) => return tool_error(format!("Loading the token failed: {e}")),
        };
        let secret = Some(token.expose().to_owned());
        let fail = |msg: String| Ok(text_result(redact(&msg, secret.as_deref()), true));

        let (agent_config_dir, logs_dir) = match (
            crate::paths::agent_claude_config_dir(),
            crate::paths::logs_dir(),
        ) {
            (Ok(a), Ok(l)) => (a, l),
            (Err(e), _) | (_, Err(e)) => {
                return fail(format!("Cannot determine directories: {e:#}"))
            }
        };
        let run_ctx = RunContext {
            claude: located.path,
            token,
            agent_config_dir,
            logs_dir,
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

        let permit = {
            let _queued = CountGuard::new(&state.queued);
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    tracing::info!("sidekick_run cancelled while queued");
                    return fail(CANCELLED_QUEUED_MSG.into());
                }
                p = state.semaphore.clone().acquire_owned() => match p {
                    Ok(p) => p,
                    Err(_) => return fail("Server is shutting down.".into()),
                },
            }
        };
        // The token may have fired while the permit was being handed over: never spawn then.
        // Check `ctx.ct` directly as well: the forwarder into `cancel` may not have run yet.
        if cancel.is_cancelled() || ctx.ct.is_cancelled() {
            tracing::info!("sidekick_run cancelled before start");
            return fail(CANCELLED_QUEUED_MSG.into());
        }
        let _running = CountGuard::new(&state.running);
        let _ = run_started.set(Instant::now());
        tracing::info!(
            cwd = %req.cwd.display(),
            max_turns = req.max_turns,
            model = ?req.model,
            timeout_s = timeout.as_secs(),
            prompt_chars = req.prompt.chars().count(),
            "sidekick_run started"
        );
        let result = runner::run(&run_ctx, req, cancel.clone()).await;
        drop(ticker);
        drop(permit);

        match result {
            Ok(outcome) => {
                tracing::info!(
                    exit_code = ?outcome.exit_code,
                    timed_out = outcome.timed_out,
                    cancelled = outcome.cancelled,
                    log = %outcome.log_path.display(),
                    "sidekick_run finished"
                );
                let text = redact(&format_outcome(&outcome, timeout), secret.as_deref());
                Ok(text_result(text, outcome_is_error(&outcome)))
            }
            Err(e) => {
                let msg = run_error_message(&e);
                tracing::warn!("sidekick_run failed: {}", redact(&msg, secret.as_deref()));
                fail(msg)
            }
        }
    }

    /// Show the sidekick's state and setup: running and queued runs, concurrency limit, default
    /// limits, which claude executable is used, directories, config file and where the agent's
    /// token comes from (never the token itself).
    #[tool]
    async fn sidekick_status(&self) -> Result<CallToolResult, McpError> {
        let claude_cfg = self.state.config.claude_path.clone();
        let located = blocking(move || locate::find_claude(claude_cfg.as_deref())).await;
        let token = blocking(token::load).await;
        let text = self.status_text(located, token);
        Ok(text_result(redact_all(&text), false))
    }
}

type LocateResult = Result<Result<Located, LocateError>, String>;
type TokenResult = Result<Result<(Token, TokenSource), TokenError>, String>;

impl SidekickServer {
    fn status_text(&self, located: LocateResult, token: TokenResult) -> String {
        let st = &self.state;
        let cfg = &st.config;
        let path_or_err = |r: anyhow::Result<PathBuf>| match r {
            Ok(p) => p.display().to_string(),
            Err(e) => format!("unavailable ({e:#})"),
        };

        let mut s = String::new();
        let _ = writeln!(s, "agent-sidekick {}", env!("CARGO_PKG_VERSION"));
        let _ = writeln!(s, "running: {}", st.running.load(Ordering::SeqCst));
        let _ = writeln!(s, "queued: {}", st.queued.load(Ordering::SeqCst));
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
        match &st.config_error {
            Some(e) => {
                let _ = writeln!(s, "config error: {e} (using defaults)");
            }
            None => s.push_str("config error: none\n"),
        }
        // Only the source is shown; the token value never leaves this match.
        match token {
            Ok(Ok((_, source))) => {
                let _ = writeln!(s, "token: present ({source})");
            }
            Ok(Err(TokenError::Missing)) => {
                s.push_str("token: missing - run 'agent-sidekick setup' in a terminal\n");
            }
            Ok(Err(e)) => {
                let _ = writeln!(s, "token: error - {e}");
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

/// Load the config (falling back to defaults on error) and serve MCP over stdio until the
/// client disconnects or a termination signal arrives. In both cases in-flight runs are
/// cancelled (their process trees killed) before this returns.
pub async fn serve() -> anyhow::Result<()> {
    // Linux: become a child subreaper so the runner can find setsid'ed descendants.
    runner::init_process_supervision();
    let (config, config_error) = match Config::load() {
        Ok(c) => (c, None),
        Err(e) => {
            let msg = redact_all(&format!("{e:#}"));
            tracing::warn!("config error, using defaults: {msg}");
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
        while self.running.load(Ordering::SeqCst) + self.queued.load(Ordering::SeqCst) > 0 {
            if Instant::now() >= deadline {
                tracing::warn!(
                    running = self.running.load(Ordering::SeqCst),
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
        assert_eq!(req.model.as_deref(), Some("sonnet"));
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
            let _running = CountGuard::new(&st.running);
            token.cancelled().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        while state.running.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        state.shutdown_runs().await;
        assert_eq!(state.running.load(Ordering::SeqCst), 0);
        run.await.unwrap();
    }

    #[test]
    fn status_never_shows_token() {
        let server = SidekickServer::new(Config::default(), Some("bad config".into()));
        let secret = "sk-ant-oat01-SECRETSECRET";
        let text = server.status_text(
            Err("stub".into()),
            Ok(Ok((Token::new(secret), TokenSource::Env))),
        );
        assert!(!text.contains(secret));
        assert!(text.contains("token: present (environment"), "{text}");
        assert!(text.contains("config error: bad config"), "{text}");
        assert!(text.contains("max_concurrent: 2"), "{text}");

        let text = server.status_text(Err("stub".into()), Ok(Err(TokenError::Missing)));
        assert!(text.contains("token: missing"), "{text}");
    }
}
