//! MCP server (rmcp, stdio) exposing `sidekick_run`, `sidekick_backends` and
//! `sidekick_status`.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
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
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::backends::reach::PROBE_TIMEOUT;
use crate::backends::{self, Backend, BackendType, Backends, Credential};
use crate::ask::{self, AskJob, AskServer, AskWire, Answered, JobEvent, Session};
use crate::dispatch::{self, DispatchRequest, DispatchResult, Dispatcher, Identity, RunExtras};
use crate::sync::{DataStore, ExperienceInput};
use crate::trust::{Dimension, Outcome, Scope, Visibility};
use crate::config::{Config, MAX_TIMEOUT_S, MAX_TURNS_LIMIT};
use crate::locate::{self, LocateError, Located};
use crate::redact::{redact_all, Secrets};
use crate::runner::{self, RunError, RunOutcome, RunRequest, TAIL_CHARS};
use crate::token::{self, SecretId, Token, TokenError, TokenSource};

/// Server instructions. Codex uses (at least) the first 512 characters for tool selection, so
/// the tools and the long run time come first (tested in tests/mcp_handshake.rs).
const INSTRUCTIONS: &str = "agent-sidekick hands a self-contained task to a separate, \
headless agent (another Claude Code account, OpenCode, Codex, Gemini or a local model). \
`sidekick_backends`: the configured backends with description, cost, models and reachability; \
pick one by description and cost. `sidekick_run`: run a prompt in a working directory \
(optional `backend`, `model`, `fallback`); it returns when the agent is done, which can take \
many minutes. `sidekick_status`: running/queued jobs and setup problems. Without `backend`, a \
run goes to the configured default backend. The result names the backend and model that ran, \
the exit code, the log path and the tail of the output. Registered models need `dimension` \
(dispatch path, then record a verdict with `sidekick_record_experience` and `job_id`). A worker may ask a question: the call then returns status `question`; reply with `sidekick_answer`. Mixed tasks: `sidekick_workflow` (steps with one dimension each). If a run fails because no token, key \
or login is stored, ask the user to run `agent-sidekick setup` (or `agent-sidekick setup \
--backend <name>`) in a terminal.";

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
    /// Model for the agent: selects among the models the backend offers (see
    /// sidekick_backends: `models`, `default_model`). Validated against the backend's
    /// allowlist when it has one (an unlisted model is an error; nothing runs). Default: the
    /// backend's `default_model`, else the account's/endpoint's default. A backend with
    /// `models: null` accepts any model if it is a Claude or Codex backend (e.g. "sonnet",
    /// "opus") and ignores the model if it is a command backend.
    pub model: Option<String>,
    /// Tools the agent may use without asking, e.g. ["Read", "Edit", "Bash(git:*)"].
    /// Default: Claude Code's defaults for headless mode. Ignored by command, opencode and
    /// codex backends (opencode/codex: use the backend's read_only setting).
    pub allowed_tools: Option<Vec<String>>,
    /// Timeout in seconds (1-7200) for each attempt; the whole process tree is killed when it
    /// expires. With a fallback the request can take up to about twice this.
    /// Default: from config.toml (1800 unless configured).
    pub timeout_s: Option<u64>,
    /// Backend to run on; see sidekick_backends. Default: the configured default backend.
    pub backend: Option<String>,
    /// Backend to use if the chosen one is unreachable, lacks credentials, fails to start, or
    /// exits with an error. Not used after a timeout or cancellation. The fallback gets its
    /// own full `timeout_s` (and may wait in its own queue), so a request with a fallback can
    /// take up to about 2x timeout_s plus queue time.
    pub fallback: Option<String>,
    /// search | inventory | write-tests | write-code | decisions. Required when the resolved
    /// backend and model have a registry row (then the run is a dispatch: own worktree, gated
    /// tools, diff check, logged job); ignored for unregistered backends.
    pub dimension: Option<String>,
    /// global (default) | repo:<name> | module:<repo>/<path>; only used for registered models.
    pub scope: Option<String>,
}

/// Parameters of `sidekick_steckbrief`.
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
pub struct SteckbriefParams {
    /// Stable model id (from an earlier Steckbrief or `query`). Alternative: `backend` (+ `model`).
    pub model_id: Option<String>,
    /// Backend whose model is meant (default backend if neither id nor backend is given).
    pub backend: Option<String>,
    /// Model of that backend (default: the backend's default model).
    pub model: Option<String>,
    /// global (default) | repo:<name> | module:<repo>/<path>.
    pub scope: Option<String>,
    /// true: full table with counters, limits, profile and benchmarks instead of the short delegation guide.
    pub detail: Option<bool>,
}

/// Parameters of `sidekick_record_experience`.
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
pub struct RecordParams {
    pub model_id: Option<String>,
    pub backend: Option<String>,
    pub model: Option<String>,
    /// search | inventory | write-tests | write-code | decisions. Required without `job_id` (not for reset).
    pub dimension: Option<String>,
    /// With `job_id`: accepted | rejected. Otherwise accepted | rejected | aborted | note | seed.
    /// `blocked` is written by the sidekick only. reset (no job_id; dimension and scope are
    /// ignored; only for data_name prefixes whose owner is listed in peers.toml) starts the
    /// model's trust over except for earlier blocks. seed sets the trust value directly: only for
    /// search and inventory, needs `seed_value`.
    pub outcome: String,
    /// global (default) | repo:<name> | module:<repo>/<path>.
    pub scope: Option<String>,
    /// The verdict path: id of a job returned by sidekick_dispatch (or sidekick_run on a
    /// registered backend). Model, dimension, scope, repo, steps and duration are taken from
    /// that job's logged event and review_s is measured; only outcome (accepted | rejected),
    /// conclusion and visibility are yours. One verdict per job, none for blocked jobs.
    pub job_id: Option<String>,
    pub steps: Option<i64>,
    pub duration_s: Option<f64>,
    /// Seconds the control model spent reviewing the result.
    pub review_s: Option<f64>,
    /// 0..1, only for outcome seed.
    pub seed_value: Option<f64>,
    pub summary: Option<String>,
    /// Required and non-empty for outcome rejected: the formulation rule for next time.
    pub conclusion: Option<String>,
    /// private (default; summary and conclusion never leave this machine) | shared.
    pub visibility: Option<String>,
}

/// Parameters of `sidekick_query`.
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
pub struct QueryParams {
    /// Read-only SQL over the tables `models`, `protected` and `experiences`.
    pub sql: String,
}

/// Parameters of `sidekick_dispatch`.
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
pub struct DispatchParams {
    /// Self-contained task for the agent.
    pub prompt: String,
    /// Git repository the job works on (the agent gets its own worktree of it). Default: the
    /// server's current directory.
    pub cwd: Option<String>,
    /// search | inventory | write-tests | write-code | decisions.
    pub dimension: String,
    /// global (default) | repo:<name> | module:<repo>/<path>.
    pub scope: Option<String>,
    pub backend: Option<String>,
    pub model: Option<String>,
    /// Name of the job's worktree and branch (`sidekick/<job_id>`). Default: generated.
    pub job_id: Option<String>,
    pub max_turns: Option<u32>,
    pub timeout_s: Option<u64>,
    /// Identity overrides for the registry lookup (default: derived from the backend).
    pub checkpoint: Option<String>,
    pub quant: Option<String>,
}

/// Parameters of `sidekick_answer`.
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
pub struct AnswerParams {
    /// The job_id of the question (from the `question` result).
    pub job_id: String,
    /// Your answer; the blocked worker receives it as the result of ask_orchestrator.
    pub answer: String,
}

pub use crate::workflow::WorkflowParams;

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
    if let Some(f) = &outcome.failure {
        let _ = writeln!(s, "failed: {f}");
    }
    if let Some(rl) = &outcome.rate_limit {
        let line = rl.line();
        if !outcome.tail.lines().any(|l| l == line) {
            let _ = writeln!(s, "{line}");
        }
    }
    let _ = writeln!(s, "log: {}", outcome.log_path.display());
    let _ = writeln!(s, "duration_s: {:.1}", outcome.duration.as_secs_f64());
    let _ = writeln!(s, "--- output (last {TAIL_CHARS} chars) ---");
    s.push_str(&outcome.tail);
    s
}

/// A run counts as failed (tool error) on cancellation, timeout, any non-zero / missing
/// exit code, or a harness-reported failure (e.g. opencode exited 0 without an answer).
fn outcome_is_error(outcome: &RunOutcome) -> bool {
    outcome.cancelled
        || outcome.timed_out
        || outcome.exit_code != Some(0)
        || outcome.failure.is_some()
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
        RunError::Spawn { .. } if ty == BackendType::OpenCode => format!(
            "{err}. Check the OpenCode installation with 'agent-sidekick harness list', or set \
             `harness_path` of the backend ('agent-sidekick backends test <name>')."
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
                     limits. Use a claude-endpoint, opencode or command backend as fallback, or omit it.",
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
        Attempt::Ran(o) => {
            !o.timed_out && !o.cancelled && (o.exit_code != Some(0) || o.failure.is_some())
        }
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
    match (o.exit_code, &o.failure) {
        (Some(0), Some(f)) => format!("{f} (exit_code 0)"),
        (_, _) => exit_code_reason(o),
    }
}

fn exit_code_reason(o: &RunOutcome) -> String {
    match o.exit_code {
        Some(c) => format!("exit_code {c}"),
        None => "exit_code none (process was killed or ended by a signal)".to_owned(),
    }
}

fn backend_label(b: &Backend) -> String {
    format!("{} ({})", b.name, b.backend_type())
}

/// [`backend_label`] plus `, model <m>` when the run on `b` uses a model (the requested one
/// or the backend's default); nothing when the backend has no default and none was requested
/// or ignores the model (command backend without `{model}`).
fn run_label(b: &Backend, req: &RunRequest) -> String {
    match b.effective_model(req.model.as_deref()) {
        Ok(Some(m)) => format!("{}, model {m}", backend_label(b)),
        _ => backend_label(b),
    }
}

/// The request for the fallback attempt on `fb`: the caller's model if `fb` offers it,
/// otherwise `fb`'s default model, with a note for the result.
fn fallback_request(fb: &Backend, req: &RunRequest) -> (RunRequest, Option<String>) {
    match (fb.effective_model(req.model.as_deref()), &req.model) {
        (Err(_), Some(m)) => {
            let fb_req = RunRequest {
                model: None,
                ..req.clone()
            };
            let default = fb.model.as_deref().unwrap_or("(none)");
            let note = format!(
                "note: model {m} not offered by {}; used its default {default}",
                fb.name
            );
            (fb_req, Some(note))
        }
        _ => (req.clone(), None),
    }
}

/// Result text of an attempt on `backend` that ended as [`Attempt::Stopped`] (cancelled while
/// queued, server shutdown, working directory gone, wait error): the backend line, the reason
/// and, if a fallback was requested, why it was not used.
fn stopped_text(
    backend: &Backend,
    req: &RunRequest,
    reason: &str,
    fallback: Option<&Backend>,
) -> String {
    let mut s = format!(
        "backend: {}\n{}",
        run_label(backend, req),
        reason.trim_end()
    );
    if let Some(fb) = fallback {
        let _ = write!(
            s,
            "\nfallback '{}' ({}) not attempted: the run had already started or was stopped",
            fb.name,
            fb.backend_type()
        );
    }
    s
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
            let _ = writeln!(s, "backend: {}", run_label(ran, req));
        }
        Some((primary, attempt)) => {
            let reason = match attempt {
                Attempt::Ran(o) => exit_reason(o),
                Attempt::NotStarted(r) | Attempt::Stopped(r) => short_reason(r),
            };
            let _ = writeln!(
                s,
                "backend: {} — fallback after '{}' failed: {reason}",
                run_label(ran, req),
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
                if o.rate_limit.is_some() {
                    let _ = writeln!(s, "fallback reason: rate limited");
                }
            }
        }
    }
    for line in extra_lines {
        let _ = writeln!(s, "{line}");
    }
    if let Some(what) = backends::launch::command_ignored(ran, req) {
        let _ = writeln!(s, "note: {what} ignored (command backend)");
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

#[derive(Debug, Clone)]
struct Limit {
    permits: usize,
    semaphore: Arc<Semaphore>,
}

impl Limit {
    fn new(permits: usize) -> Self {
        let permits = permits.max(1);
        Self {
            permits,
            semaphore: Arc::new(Semaphore::new(permits)),
        }
    }

    fn reuse_or_new(old: Option<&Limit>, permits: usize) -> Self {
        match old {
            Some(l) if l.permits == permits.max(1) => l.clone(),
            _ => Self::new(permits),
        }
    }
}

/// One entry of the `sidekick_backends` result. Never contains secrets or env values.
#[derive(Debug, Clone, serde::Serialize)]
struct BackendInfo {
    name: String,
    #[serde(rename = "type")]
    kind: &'static str,
    description: String,
    account: Option<String>,
    cost: &'static str,
    default: bool,
    /// Allowlist of models (`null`: claude-* accepts any model, a command backend without
    /// `{model}` takes none).
    models: Option<Vec<String>>,
    /// Model used when the request names none (`null`: the account's/endpoint's default, or
    /// none for a command backend without `{model}`).
    default_model: Option<String>,
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

/// Phase of a `sidekick_run` request, shared between the attempts and the progress ticker.
/// Reset when the fallback attempt begins, so the fallback's queue wait is reported as queued
/// and its own timeout is added to `total`.
#[derive(Debug, Clone)]
pub(crate) struct RunPhase {
    /// Name of the fallback backend once the fallback attempt has begun; `None` for the
    /// primary attempt.
    fallback: Option<String>,
    /// When the current attempt began (preparation and queue).
    attempt_began: Instant,
    /// When the current attempt's agent started (left the queue).
    started: Option<Instant>,
}

impl RunPhase {
    pub(crate) fn primary() -> Self {
        Self {
            fallback: None,
            attempt_began: Instant::now(),
            started: None,
        }
    }

    fn fallback(name: &str) -> Self {
        Self {
            fallback: Some(name.to_owned()),
            attempt_began: Instant::now(),
            started: None,
        }
    }
}

pub(crate) type SharedPhase = Arc<std::sync::Mutex<RunPhase>>;

/// Run `f` on the phase; a poisoned lock is still usable (the data is plain timestamps).
fn with_phase<T>(phase: &std::sync::Mutex<RunPhase>, f: impl FnOnce(&mut RunPhase) -> T) -> T {
    let mut guard = phase
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&mut guard)
}

/// What the progress ticker knows about the current attempt, as offsets from the arrival of
/// the request.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PhaseView<'a> {
    /// Fallback backend name if the current attempt is the fallback.
    fallback: Option<&'a str>,
    /// When the current attempt began.
    attempt_began: Duration,
    /// When the current attempt's agent started, if it has.
    started: Option<Duration>,
}

/// One progress notification. `progress` is the number of whole seconds since the request
/// arrived (it only grows); `total` is known while an agent runs: the time up to its start
/// plus its timeout (never less than `progress`, e.g. while a timed-out process tree is being
/// killed). While an attempt is queued there is no `total`. The message never contains agent
/// output.
fn progress_param(
    token: ProgressToken,
    since_request: Duration,
    phase: &PhaseView<'_>,
    timeout: Duration,
) -> ProgressNotificationParam {
    let elapsed = since_request.as_secs();
    let param = ProgressNotificationParam::new(token, elapsed as f64);
    let who = match phase.fallback {
        None => String::new(),
        Some(name) => format!("fallback '{name}' "),
    };
    match phase.started {
        None => {
            let queued = since_request.saturating_sub(phase.attempt_began).as_secs();
            param.with_message(format!(
                "{who}queued for {queued}s, waiting for a free slot"
            ))
        }
        Some(q) => {
            let total = (q.as_secs() + timeout.as_secs()).max(elapsed);
            let agent = if who.is_empty() { "agent " } else { "" };
            param.with_total(total as f64).with_message(format!(
                "{who}{agent}running for {}s",
                since_request.saturating_sub(q).as_secs()
            ))
        }
    }
}

/// Sends `notifications/progress` every `interval` until dropped. `phase` is updated by the
/// attempts (start of the fallback, agent started).
fn spawn_progress_ticker(
    peer: Peer<RoleServer>,
    token: ProgressToken,
    phase: SharedPhase,
    timeout: Duration,
    interval: Duration,
) -> AbortOnDrop {
    let entered = Instant::now();
    AbortOnDrop(tokio::spawn(async move {
        let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticks.tick().await;
            let snapshot = with_phase(&phase, |p| p.clone());
            let view = PhaseView {
                fallback: snapshot.fallback.as_deref(),
                attempt_began: snapshot.attempt_began.saturating_duration_since(entered),
                started: snapshot
                    .started
                    .map(|s| s.saturating_duration_since(entered)),
            };
            let param = progress_param(token.clone(), entered.elapsed(), &view, timeout);
            if let Err(e) = peer.notify_progress(param).await {
                tracing::debug!("progress notification failed, stopping ticker: {e}");
                break;
            }
        }
    }))
}

const COOLDOWN_DEFAULT_S: u64 = 60;
const COOLDOWN_MAX_S: u64 = 3600;

type CooldownMap = HashMap<String, Instant>;

fn cooldown_until(retry_after_s: Option<u64>, now: Instant) -> Instant {
    let secs = retry_after_s
        .unwrap_or(COOLDOWN_DEFAULT_S)
        .min(COOLDOWN_MAX_S);
    now + Duration::from_secs(secs)
}

fn remaining_s(until: Instant, now: Instant) -> Option<u64> {
    let left = until.checked_duration_since(now)?;
    let secs = left.as_secs() + u64::from(left.subsec_nanos() > 0);
    (secs > 0).then_some(secs)
}

fn fallback_skipped_line(name: &str, secs: u64) -> String {
    format!("fallback '{name}' skipped: rate limited, retry in {secs}s")
}

fn set_cooldown(map: &mut CooldownMap, name: &str, retry_after_s: Option<u64>, now: Instant) {
    let until = cooldown_until(retry_after_s, now);
    let entry = map.entry(name.to_owned()).or_insert(until);
    *entry = (*entry).max(until);
}

fn cooldown_remaining(map: &mut CooldownMap, name: &str, now: Instant) -> Option<u64> {
    let left = map.get(name).and_then(|&until| remaining_s(until, now));
    if left.is_none() {
        map.remove(name);
    }
    left
}

fn active_cooldowns(map: &mut CooldownMap, now: Instant) -> Vec<(String, u64)> {
    map.retain(|_, until| remaining_s(*until, now).is_some());
    let mut list: Vec<_> = map
        .iter()
        .filter_map(|(name, &until)| Some((name.clone(), remaining_s(until, now)?)))
        .collect();
    list.sort();
    list
}

#[derive(Debug)]
pub(crate) struct Snapshot {
    config: Config,
    /// `Config::load` error, reported by `sidekick_status` and `sidekick_backends`.
    config_error: Option<String>,
    /// The validated backends; `Err` if config.toml or its `[backends]` is invalid (then
    /// `sidekick_run` refuses to run instead of falling back to the implicit backend).
    backends: Result<Backends, String>,
    /// Global concurrency limit (`max_concurrent` in config.toml).
    global: Limit,
    /// Per-backend semaphores (`max_concurrent` of the backend).
    per_backend: HashMap<String, Limit>,
}

impl Snapshot {
    fn build(
        config: Config,
        config_error: Option<String>,
        backends: Result<Backends, String>,
        previous: Option<&Snapshot>,
    ) -> Self {
        let global = Limit::reuse_or_new(previous.map(|p| &p.global), config.max_concurrent);
        let per_backend = backends
            .iter()
            .flat_map(Backends::iter)
            .filter_map(|b| {
                let n = b.max_concurrent?;
                let old = previous.and_then(|p| p.per_backend.get(&b.name));
                Some((b.name.clone(), Limit::reuse_or_new(old, n)))
            })
            .collect();
        Self {
            config,
            config_error,
            backends,
            global,
            per_backend,
        }
    }

    fn with_error(&self, err: String) -> Self {
        Self {
            config: self.config.clone(),
            config_error: Some(err),
            backends: self.backends.clone(),
            global: self.global.clone(),
            per_backend: self.per_backend.clone(),
        }
    }

    fn backend_names(&self) -> String {
        match &self.backends {
            Ok(b) => b
                .iter()
                .map(|b| b.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            Err(_) => String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FileState {
    Missing,
    Present {
        modified: Option<std::time::SystemTime>,
        len: u64,
        hash: u64,
    },
    Unreadable(String),
}

fn read_config_file(path: &Path) -> (FileState, Result<Config, String>) {
    use std::hash::{Hash, Hasher};
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (FileState::Missing, Ok(Config::default()))
        }
        Err(e) => {
            let msg = redact_all(&format!("reading {}: {e}", path.display()));
            return (FileState::Unreadable(msg.clone()), Err(msg));
        }
    };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    let state = FileState::Present {
        modified: std::fs::metadata(path).and_then(|m| m.modified()).ok(),
        len: bytes.len() as u64,
        hash: hasher.finish(),
    };
    let config = match std::str::from_utf8(&bytes) {
        Ok(text) => Config::parse(text, path).map_err(|e| redact_all(&format!("{e:#}"))),
        Err(e) => Err(format!("parsing {}: not UTF-8: {e}", path.display())),
    };
    (state, config)
}

#[derive(Debug)]
struct Reload {
    path: PathBuf,
    file: std::sync::Mutex<FileState>,
}

#[derive(Debug)]
struct State {
    snapshot: std::sync::RwLock<Arc<Snapshot>>,
    reload: Option<Reload>,
    /// Global running/queued counters.
    counters: Counters,
    per_backend: std::sync::Mutex<HashMap<String, Arc<Counters>>>,
    /// Cancelled when the server shuts down (client disconnect or signal); every run's
    /// cancellation token is a child of it, so in-flight process trees get killed.
    shutdown: CancellationToken,
    /// Backend name -> end of its rate-limit cooldown.
    cooldowns: std::sync::Mutex<CooldownMap>,
    data: tokio::sync::OnceCell<Arc<DataStore>>,
    broker: Arc<ask::Broker>,
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

fn stale_config_note(snap: &Snapshot) -> Option<String> {
    match (&snap.config_error, &snap.backends) {
        (Some(e), Ok(_)) => Some(format!(
            "note: config.toml is invalid since its last change ({e}); the last valid config \
             is still in use"
        )),
        _ => None,
    }
}

fn tool_error_with(snap: &Snapshot, msg: impl AsRef<str>) -> Result<CallToolResult, McpError> {
    match stale_config_note(snap) {
        Some(note) => tool_error(format!("{}\n{note}", msg.as_ref())),
        None => tool_error(msg),
    }
}

/// Redact every secret of the run's attempts (and every `sk-ant-…` token).
fn redact_with(secrets: &[Secrets], text: &str) -> String {
    let mut out = redact_all(text);
    for s in secrets {
        out = s.redact(&out);
    }
    out
}

fn push_note(id: &str, pushed: bool, visibility: Visibility) -> String {
    let sent = match visibility {
        Visibility::Private => "pushed (Zahlen; summary/conclusion lokal)",
        Visibility::Shared => "pushed (Zahlen und Texte)",
    };
    format!("recorded event {id}; {}", if pushed { sent } else { "not pushed (offline, rejected or data_name not set); sidekick_sync retries" })
}

fn format_dispatch(r: &DispatchResult) -> String {
    let mut s = format!(
        "status: {:?}\nmodel_id: {}\nwrite_right: {}\nwrite_tools_withheld: {}\nsperre: {}\nduration_s: {:.1}\nexit_code: {:?}\nlog: {}\n",
        r.status, r.model_id, r.write_right, r.write_tools_withheld, r.sperre.as_str(), r.duration_s, r.exit_code, r.log_path.display()
    );
    let opt = |s: &mut String, k: &str, v: Option<String>| {
        if let Some(v) = v {
            let _ = writeln!(s, "{k}: {v}");
        }
    };
    opt(&mut s, "sperre_note", r.sperre_note.clone());
    opt(&mut s, "branch", r.branch.clone());
    opt(&mut s, "patch", r.patch_path.as_ref().map(|p| p.display().to_string()));
    opt(&mut s, "block_reason", r.block_reason.clone());
    opt(&mut s, "reason", r.reason.clone());
    if r.proposal_mode {
        s.push_str("mode: Vorschlagsmodus (no write right; the proposed diff was applied only in the job worktree)\n");
    }
    opt(&mut s, "evidence_hash", r.evidence_hash.clone());
    opt(&mut s, "event_id", r.event_id.clone());
    let _ = writeln!(s, "applied: {}\ndiff_discarded: {}", r.applied, r.diff_discarded);
    if !r.unsupported_limits.is_empty() {
        let _ = writeln!(s, "limits not enforced by this harness: {}", r.unsupported_limits.join(", "));
    }
    for w in &r.warnings {
        let _ = writeln!(s, "warning: {w}");
    }
    let _ = write!(s, "--- output tail ---\n{}", r.tail);
    s
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
        Self::with_snapshot(Snapshot::build(config, config_error, backends, None), None)
    }

    fn with_snapshot(snapshot: Snapshot, reload: Option<Reload>) -> Self {
        Self {
            state: Arc::new(State {
                snapshot: std::sync::RwLock::new(Arc::new(snapshot)),
                reload,
                counters: Counters::default(),
                per_backend: std::sync::Mutex::default(),
                shutdown: CancellationToken::new(),
                cooldowns: std::sync::Mutex::default(),
                data: tokio::sync::OnceCell::new(),
                broker: Arc::default(),
            }),
        }
    }

    fn reloading(path: PathBuf) -> Self {
        let (file, loaded) = read_config_file(&path);
        if let Err(msg) = &loaded {
            tracing::warn!("config error, sidekick_run is disabled: {msg}");
        }
        let snapshot = match loaded {
            Ok(config) => {
                let backends = backends::resolve(&config);
                Snapshot::build(config, None, backends, None)
            }
            Err(msg) => Snapshot::build(Config::default(), Some(msg.clone()), Err(msg), None),
        };
        let reload = Reload {
            path,
            file: std::sync::Mutex::new(file),
        };
        Self::with_snapshot(snapshot, Some(reload))
    }

    /// Delegate a prompt to a separate, headless agent that runs with its own credentials
    /// (not the caller's). Without `backend` it runs on the configured default backend;
    /// `backend` selects another configured backend (see sidekick_backends: choose by
    /// description and cost) and `model` one of that backend's models (validated against its
    /// allowlist), `fallback` names a backend to use if the chosen one is unreachable, lacks credentials,
    /// fails to start or exits with an error (not after a timeout; the fallback gets its own
    /// timeout_s, so the request can take up to about 2x timeout_s). If the resolved backend and
    /// model are registered (have a Steckbrief), `dimension` is required and the run is a
    /// dispatch (see sidekick_dispatch); a registered `fallback` runs as a dispatch with the same
    /// `dimension` too, and the request is rejected without `dimension`; a model that is not
    /// registered on a backend that has registered models is rejected; other backends run as before without
    /// any trust effect. The agent runs in `cwd`
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
        let snap = state.refresh();

        // No silent fallback to the implicit backend when config.toml is broken.
        let backends = match &snap.backends {
            Ok(b) => b,
            Err(e) => return tool_error(config_invalid_message(e)),
        };

        // Pure validation first: cheap, and independent of installs and credentials.
        let base_dir = match std::env::current_dir() {
            Ok(d) => d,
            Err(e) => {
                return tool_error_with(
                    &snap,
                    format!(
                    "Cannot determine the server's current directory: {e}. Pass an absolute 'cwd'."
                ),
                )
            }
        };
        let home = crate::paths::home_dir().ok();
        let (backend_param, fallback_param) = (params.backend.clone(), params.fallback.clone());
        let req = match resolve_request(params.clone(), &snap.config, &base_dir, home.as_deref()) {
            Ok(r) => r,
            Err(msg) => return tool_error_with(&snap, msg),
        };
        let (primary, fallback) = match select_backends(
            backends,
            backend_param.as_deref(),
            fallback_param.as_deref(),
        ) {
            Ok(sel) => sel,
            Err(msg) => return tool_error_with(&snap, msg),
        };
        // A model the primary does not offer is an invalid parameter: no run, no fallback.
        if let Err(msg) = primary.effective_model(req.model.as_deref()) {
            return tool_error_with(&snap, msg);
        }
        let caller = backends::caller_account();
        if let Err(msg) = primary.account_guard(caller.as_deref()) {
            return tool_error_with(&snap, msg);
        }
        if let Some(Err(msg)) = fallback.map(|fb| fb.account_guard(caller.as_deref())) {
            return tool_error_with(&snap, format!("fallback: {msg}"));
        }
        let dimension = params.dimension.clone().filter(|d| !d.trim().is_empty());
        let dispatch_params = |backend: &Backend, model: Option<String>, dimension: String| DispatchParams {
            prompt: params.prompt.clone(),
            cwd: params.cwd.clone(),
            dimension,
            scope: params.scope.clone(),
            backend: Some(backend.name.clone()),
            model,
            max_turns: params.max_turns,
            timeout_s: params.timeout_s,
            ..Default::default()
        };
        let needs_dimension = |backend: &Backend| {
            format!(
                "backend {} with this model is registered (has a Steckbrief), so sidekick_run needs `dimension` \
                 (search | inventory | write-tests | write-code | decisions) and optionally `scope`; the run goes through \
                 the dispatch path (worktree, gated tools, diff check, logged job)",
                backend.name
            )
        };
        match self.registry_gate(&snap, primary, req.model.as_deref()).await {
            Err(msg) => return tool_error_with(&snap, msg),
            Ok(true) => {
                let Some(dimension) = dimension else { return tool_error_with(&snap, needs_dimension(primary)) };
                return self.sidekick_dispatch(Parameters(dispatch_params(primary, params.model.clone(), dimension)), ctx).await;
            }
            Ok(false) => {}
        }
        let fallback_dispatch = match fallback {
            None => None,
            Some(fb) => {
                let fb_model = fallback_request(fb, &req).0.model;
                match self.registry_gate(&snap, fb, fb_model.as_deref()).await {
                    Err(msg) => return tool_error_with(&snap, format!("fallback: {msg}")),
                    Ok(false) => None,
                    Ok(true) => match dimension {
                        Some(dimension) => Some(dispatch_params(fb, fb_model, dimension)),
                        None => return tool_error_with(&snap, format!("fallback: {}", needs_dimension(fb))),
                    },
                }
            }
        };
        if let (Some(secs), Some(dp)) = (state.cooldown(&primary.name), &fallback_dispatch) {
            tracing::info!(primary = %primary.name, secs, "primary rate limited; the registered fallback runs as dispatch");
            return self.sidekick_dispatch(Parameters(dp.clone()), ctx).await;
        }
        let direct = match state.cooldown(&primary.name) {
            None => None,
            Some(secs) => match fallback.filter(|fb| state.cooldown(&fb.name).is_none()) {
                Some(fb) => Some((secs, fb)),
                None => {
                    return tool_error_with(
                        &snap,
                        format!(
                            "backend {} is rate limited, retry in {secs}s; choose another backend \
                         (see sidekick_backends) or set fallback",
                            primary.name
                        ),
                    )
                }
            },
        };
        let timeout = req.timeout;

        let (cancel, _forward) = run_cancel_token(&ctx.ct, &state.shutdown);
        let phase: SharedPhase = Arc::new(std::sync::Mutex::new(RunPhase::primary()));
        // Keeps Claude Code's MCP idle timeout from aborting long runs (and queue waits).
        let ticker = ctx.meta.get_progress_token().map(|token| {
            spawn_progress_ticker(
                ctx.peer.clone(),
                token,
                phase.clone(),
                timeout,
                progress_interval(),
            )
        });

        let mut secrets = Vec::new();
        let (text, is_error) = if let Some((secs, fb)) = direct {
            with_phase(&phase, |p| *p = RunPhase::fallback(&fb.name));
            let (fb_req, model_note) = fallback_request(fb, &req);
            let reason = format!(
                "fallback reason: primary {} rate limited, retry in {secs}s",
                primary.name
            );
            let mut lines = vec![reason.clone()];
            lines.extend(model_note);
            match self
                .attempt(&snap, fb, &fb_req, &cancel, &ctx.ct, &phase, &mut secrets)
                .await
            {
                Attempt::Ran(o) => (
                    format_run_result(fb, &o, &fb_req, None, &lines),
                    outcome_is_error(&o),
                ),
                Attempt::NotStarted(r) => (
                    format!(
                        "backend '{}' ({}) failed: {r}\n{reason}",
                        fb.name,
                        fb.backend_type()
                    ),
                    true,
                ),
                Attempt::Stopped(r) => (
                    format!("{}\n{reason}", stopped_text(fb, &fb_req, &r, None)),
                    true,
                ),
            }
        } else {
            let first = self
                .attempt(&snap, primary, &req, &cancel, &ctx.ct, &phase, &mut secrets)
                .await;
            match (first, fallback) {
                (first, Some(fb))
                    if fallback_eligible(&first) && state.cooldown(&fb.name).is_some() =>
                {
                    let secs = state.cooldown(&fb.name).unwrap_or(1);
                    let line = fallback_skipped_line(&fb.name, secs);
                    match &first {
                        Attempt::Ran(o) => {
                            (format_run_result(primary, o, &req, None, &[line]), true)
                        }
                        Attempt::NotStarted(r) | Attempt::Stopped(r) => (
                            format!(
                                "backend '{}' ({}) failed: {r}\n{line}",
                                primary.name,
                                primary.backend_type()
                            ),
                            true,
                        ),
                    }
                }
                (first, Some(fb)) if fallback_eligible(&first) && fallback_dispatch.is_some() => {
                    tracing::warn!(primary = %primary.name, fallback = %fb.name, "primary backend failed; the registered fallback runs as dispatch");
                    drop(ticker);
                    return self.sidekick_dispatch(Parameters(fallback_dispatch.clone().expect("checked above")), ctx).await;
                }
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
                    // The fallback gets its own queue wait and its own full timeout.
                    with_phase(&phase, |p| *p = RunPhase::fallback(&fb.name));
                    let (fb_req, model_note) = fallback_request(fb, &req);
                    let second = self
                        .attempt(&snap, fb, &fb_req, &cancel, &ctx.ct, &phase, &mut secrets)
                        .await;
                    match (second, &first) {
                        (Attempt::Ran(o), _) => (
                            format_run_result(
                                fb,
                                &o,
                                &fb_req,
                                Some((primary, &first)),
                                model_note.as_slice(),
                            ),
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
                (Attempt::Stopped(r), fb) => (stopped_text(primary, &req, &r, fb), true),
            }
        };
        drop(ticker);
        let text = match stale_config_note(&snap) {
            Some(note) if is_error => format!(
                "{text}
{note}"
            ),
            _ => text,
        };
        Ok(text_result(redact_with(&secrets, &text), is_error))
    }

    /// List the configured backends the agent can run on, as JSON: name, type, description,
    /// cost (free | quota | paid), default, models (the models the backend offers, or null),
    /// default_model, reachable (with a detail), max_concurrent and the current running/queued
    /// runs. Pick the backend for `sidekick_run` by its description and cost, and the model
    /// from that backend's `models` (or omit `model` to use its `default_model`); without
    /// `backend`, sidekick_run uses the default one. Never shows secrets.
    #[tool]
    async fn sidekick_backends(&self) -> Result<CallToolResult, McpError> {
        let snap = self.state.refresh();
        let report = self.backends_report(&snap).await;
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
        let snap = self.state.refresh();
        let claude_cfg = snap.config.claude_path.clone();
        let located = blocking(move || locate::find_claude(claude_cfg.as_deref())).await;
        let ids: Vec<SecretId> = match &snap.backends {
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
                            | backends::BackendKind::OpenCode { .. }
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
        let text = self.status_text(&snap, located, tokens);
        Ok(text_result(redact_all(&text), false))
    }

    /// Short Steckbrief (about 6 lines, German) of a model as delegation guide: role, what it is
    /// good for, how to word the task, what not to give it, lessons from earlier rejections and
    /// blocks. Read this before choosing what to delegate and how to phrase the task. With
    /// detail=true the full table: registry data, harness limits not enforced, trust per
    /// dimension and scope with counters, last block, review share, write and decisions right,
    /// cost balance, newest conclusions, profile and benchmarks.
    #[tool]
    async fn sidekick_steckbrief(
        &self,
        Parameters(p): Parameters<SteckbriefParams>,
    ) -> Result<CallToolResult, McpError> {
        let snap = self.state.refresh();
        let store = match self.store(&snap).await {
            Ok(s) => s,
            Err(e) => return tool_error(e),
        };
        let scope = match p.scope.as_deref().map(Scope::parse).transpose() {
            Ok(s) => s.unwrap_or(Scope::Global),
            Err(e) => return tool_error(format!("{e:#}")),
        };
        let (id, backend) = match Self::model_id_for(&snap, p.model_id, p.backend.as_deref(), p.model.as_deref()) {
            Ok(v) => v,
            Err(e) => return tool_error_with(&snap, e),
        };
        let now = chrono::Utc::now().timestamp_millis();
        let detail = p.detail.unwrap_or(false);
        let label = backend.as_ref().map(|b| match &b.account {
            Some(a) => format!("{}, {a}", b.name),
            None => b.name.clone(),
        });
        let built = blocking(move || {
            store
                .data
                .steckbrief(&id, &scope, Some(&store.local_root), now)
                .map(|s| if detail { s.render() } else { s.render_short(label.as_deref()) })
                .map_err(|e| format!("{e:#}"))
        })
        .await;
        match built {
            Ok(Ok(mut text)) => {
                if let Some(b) = backend.filter(|_| detail) {
                    let unsupported = dispatch::unsupported_limits(&b);
                    if !unsupported.is_empty() {
                        let _ = write!(text, "\nharness limits not enforced: {}", unsupported.join(", "));
                    }
                }
                Ok(text_result(redact_all(&text), false))
            }
            Ok(Err(e)) | Err(e) => tool_error(e),
        }
    }

    /// Write one experience event about a delegated job: outcome accepted | rejected | aborted |
    /// note | seed, with review_s (your review time), conclusion and visibility (private by
    /// default). Outcome rejected REQUIRES a non-empty conclusion phrased as a formulation rule
    /// for next time (what the task must say); it shows up in the Steckbrief under "Aus Fehlern". Trust is computed from these events; seed (search/inventory only) sets a value
    /// directly. Pushes the event to the shared data repository when data_name is configured.
    #[tool]
    async fn sidekick_record_experience(
        &self,
        Parameters(p): Parameters<RecordParams>,
    ) -> Result<CallToolResult, McpError> {
        let snap = self.state.refresh();
        let store = match self.store(&snap).await {
            Ok(s) => s,
            Err(e) => return tool_error(e),
        };
        if let Some(job_id) = p.job_id.clone().filter(|j| !j.trim().is_empty()) {
            let verdict = (|| -> anyhow::Result<(Outcome, Visibility)> {
                Ok((Outcome::parse(&p.outcome)?, p.visibility.as_deref().map(Visibility::parse).transpose()?.unwrap_or(Visibility::Private)))
            })();
            let (outcome, visibility) = match verdict {
                Ok(v) => v,
                Err(e) => return tool_error(format!("{e:#}")),
            };
            let conclusion = p.conclusion;
            return match blocking(move || store.record_verdict(job_id.trim(), outcome, conclusion, visibility).map_err(|e| format!("{e:#}"))).await {
                Ok(Ok((id, pushed))) => Ok(text_result(push_note(&id, pushed, visibility), false)),
                Ok(Err(e)) | Err(e) => tool_error(e),
            };
        }
        let (model_id, _) = match Self::model_id_for(&snap, p.model_id, p.backend.as_deref(), p.model.as_deref()) {
            Ok(v) => v,
            Err(e) => return tool_error_with(&snap, e),
        };
        let visibility = match p.visibility.as_deref().map(Visibility::parse).transpose() {
            Ok(v) => v.unwrap_or(Visibility::Private),
            Err(e) => return tool_error(format!("{e:#}")),
        };
        let input = (|| -> anyhow::Result<ExperienceInput> {
            Ok(ExperienceInput {
                model_id,
                dimension: match (p.dimension.as_deref(), p.outcome.as_str()) {
                    (Some(d), _) => Dimension::parse(d)?,
                    (None, "reset") => Dimension::Search,
                    (None, _) => anyhow::bail!("dimension is required without job_id"),
                },
                scope: p.scope.as_deref().map(Scope::parse).transpose()?.unwrap_or(Scope::Global),
                outcome: Outcome::parse(&p.outcome)?,
                job_id: p.job_id,
                steps: p.steps,
                duration_s: p.duration_s,
                review_s: p.review_s,
                seed_value: p.seed_value,
                summary: p.summary,
                conclusion: p.conclusion,
                visibility,
            })
        })();
        let input = match input {
            Ok(i) => i,
            Err(e) => return tool_error(format!("{e:#}")),
        };
        match blocking(move || store.record(input).map_err(|e| format!("{e:#}"))).await {
            Ok(Ok((id, pushed))) => Ok(text_result(push_note(&id, pushed, visibility), false)),
            Ok(Err(e)) | Err(e) => tool_error(e),
        }
    }

    /// Read-only SQL (DataFusion) over the tables models, protected and experiences (all
    /// events: aggregate, shared experiences and the local log). Returns a text table.
    #[tool]
    async fn sidekick_query(&self, Parameters(p): Parameters<QueryParams>) -> Result<CallToolResult, McpError> {
        let snap = self.state.refresh();
        let store = match self.store(&snap).await {
            Ok(s) => s,
            Err(e) => return tool_error(e),
        };
        match store.data.query(Some(&store.local_root), &p.sql).await {
            Ok(t) => Ok(text_result(redact_all(&t), false)),
            Err(e) => tool_error(format!("{e:#}")),
        }
    }

    /// Pull the shared data (registry, aggregate, other experiences) and push pending local
    /// events. Offline is not an error; pending events are pushed on the next sync.
    #[tool]
    async fn sidekick_sync(&self) -> Result<CallToolResult, McpError> {
        let snap = self.state.refresh();
        let store = match self.store(&snap).await {
            Ok(s) => s,
            Err(e) => return tool_error(e),
        };
        match blocking(move || store.sync().map_err(|e| format!("{e:#}"))).await {
            Ok(Ok(t)) => Ok(text_result(redact_all(&t), false)),
            Ok(Err(e)) | Err(e) => tool_error(e),
        }
    }

    /// Run a job in its own git worktree under the trust rules of the chosen backend's model:
    /// without write right the agent gets no write tools; the diff is checked against protected
    /// paths and terms and the scope limits. The result is a branch and patch (never applied,
    /// except for the Gold model); violations discard the diff and record a block. Backends
    /// without a registry row are rejected. Waits in the same queues as sidekick_run. The worker
    /// may ask a question: the call then returns status `question` with the job_id; answer with
    /// sidekick_answer.
    #[tool]
    async fn sidekick_dispatch(
        &self,
        Parameters(p): Parameters<DispatchParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let snap = self.state.refresh();
        let prep = match self.prepare_dispatch(&snap, &p).await {
            Ok(v) => v,
            Err(msg) => return tool_error_with(&snap, msg),
        };
        let phase: SharedPhase = Arc::new(std::sync::Mutex::new(RunPhase::primary()));
        let session = self.state.broker.open(&prep.dreq.job_id, &self.state.shutdown, phase, prep.dreq.timeout);
        let (server, job_snap, job_session) = (self.clone(), snap.clone(), session.clone());
        tokio::spawn(async move {
            let report = server.execute_job(job_snap, prep, job_session.clone(), JobExtras::default()).await;
            job_session.send(JobEvent::Done { text: report.text, is_error: report.is_error });
        });
        self.wait_event(&session, &ctx).await
    }

    /// Answer a question a delegated worker asked (status `question` of sidekick_dispatch,
    /// sidekick_run, sidekick_workflow or an earlier sidekick_answer). The answer is delivered to
    /// the blocked worker; the call then waits for the job's next event (another question or the
    /// final result) and returns it in the same format as sidekick_dispatch.
    #[tool]
    async fn sidekick_answer(
        &self,
        Parameters(p): Parameters<AnswerParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        match self.state.broker.answer(p.job_id.trim(), p.answer) {
            Answered::Unknown => tool_error(format!("no running job `{}` with a question (finished, cancelled or unknown)", p.job_id.trim())),
            Answered::Delivered(session) => self.wait_event(&session, &ctx).await,
            Answered::NoOpenQuestion(session) => {
                let r = self.wait_event(&session, &ctx).await?;
                let note = "note: the worker has no open question (already answered, or it got no answer in time); your answer was not delivered\n";
                Ok(text_result(format!("{note}{}", text_of(&r)), r.is_error.unwrap_or(false)))
            }
        }
    }

    /// Submit a workflow: steps [{id, prompt, dimension, scope?, backend?, model?, depends_on?}]
    /// plus cwd. Every step must pass the dispatch gating (registered model, account guard);
    /// otherwise the whole workflow is rejected with the reasons. Independent steps run in
    /// parallel; a dependent step's prompt gets the results of its dependencies. Writing steps
    /// share one workflow branch (each diff is checked with its own trust rules and committed
    /// only when clean); a blocked or failed step skips its dependents. Nothing reaches the
    /// working tree until the end: when every writing step ran on the Gold model the combined
    /// patch is applied, otherwise the workflow branch, the combined patch and the per-step
    /// results (each with its own job_id for sidekick_record_experience) are returned. Questions
    /// of any step surface as in sidekick_dispatch (answer with sidekick_answer).
    #[tool]
    async fn sidekick_workflow(
        &self,
        Parameters(p): Parameters<WorkflowParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let snap = self.state.refresh();
        let plan = match crate::workflow::plan(self, &snap, p).await {
            Ok(v) => v,
            Err(msg) => return tool_error_with(&snap, msg),
        };
        let phase: SharedPhase = Arc::new(std::sync::Mutex::new(RunPhase::primary()));
        let session = self.state.broker.open(&plan.id, &self.state.shutdown, phase, plan.total_timeout());
        with_phase(&session.phase, |p| p.started = Some(Instant::now()));
        let (server, job_snap, job_session) = (self.clone(), snap.clone(), session.clone());
        tokio::spawn(async move {
            let (text, is_error) = crate::workflow::run(server, job_snap, plan, job_session.clone()).await;
            job_session.send(JobEvent::Done { text, is_error });
        });
        self.wait_event(&session, &ctx).await
    }
}

#[derive(Clone)]
pub(crate) struct Prepared {
    pub backend: Backend,
    pub dreq: DispatchRequest,
    pub store: Arc<DataStore>,
}

#[derive(Default)]
pub(crate) struct JobExtras {
    pub base: Option<String>,
    pub hold_gold: bool,
    pub step: Option<String>,
}

pub(crate) struct JobReport {
    pub text: String,
    pub is_error: bool,
    pub result: Option<DispatchResult>,
}

fn format_question(job_id: &str, step: Option<&str>, workflow: &str, text: &str) -> String {
    let mut s = format!("status: question\njob_id: {job_id}\n");
    if let Some(step) = step {
        let _ = writeln!(s, "workflow: {workflow}\nstep: {step}");
    }
    let _ = write!(s, "question: {text}\nanswer with sidekick_answer(job_id, answer); the call returns the job's next event");
    s
}

fn text_of(r: &CallToolResult) -> String {
    r.content.iter().filter_map(|c| c.as_text().map(|t| t.text.clone())).collect::<Vec<_>>().join("\n")
}

type LocateResult = Result<Result<Located, LocateError>, String>;
type TokenResults = Result<Vec<(SecretId, Result<(Token, TokenSource), TokenError>)>, String>;

impl SidekickServer {
    pub(crate) async fn prepare_dispatch(&self, snap: &Snapshot, p: &DispatchParams) -> Result<Prepared, String> {
        let backends = snap.backends.as_ref().map_err(|e| config_invalid_message(e))?;
        let store = self.store(snap).await?;
        let base_dir = std::env::current_dir().map_err(|e| format!("Cannot determine the server's current directory: {e}. Pass an absolute 'cwd'."))?;
        let home = crate::paths::home_dir().ok();
        let parsed = (|| -> anyhow::Result<(Dimension, Scope)> {
            Ok((Dimension::parse(&p.dimension)?, p.scope.as_deref().map(Scope::parse).transpose()?.unwrap_or(Scope::Global)))
        })();
        let (dimension, scope) = parsed.map_err(|e| format!("{e:#}"))?;
        let run_params = RunParams {
            prompt: p.prompt.clone(),
            cwd: p.cwd.clone(),
            max_turns: p.max_turns,
            model: p.model.clone(),
            timeout_s: p.timeout_s,
            ..Default::default()
        };
        let req = resolve_request(run_params, &snap.config, &base_dir, home.as_deref())?;
        let (backend, _) = select_backends(backends, p.backend.as_deref(), None)?;
        backend.effective_model(req.model.as_deref())?;
        backend.account_guard(backends::caller_account().as_deref())?;
        if let Some(secs) = self.state.cooldown(&backend.name) {
            return Err(format!("backend {} is rate limited, retry in {secs}s; choose another backend", backend.name));
        }
        let dreq = DispatchRequest {
            prompt: req.prompt.clone(),
            repo: req.cwd.clone(),
            dimension,
            scope,
            job_id: p.job_id.clone().unwrap_or_else(|| format!("j-{}", &uuid::Uuid::new_v4().simple().to_string()[..8])),
            model: req.model.clone(),
            max_turns: req.max_turns,
            timeout: req.timeout,
            identity: Identity { checkpoint: p.checkpoint.clone(), quant: p.quant.clone() },
        };
        Ok(Prepared { backend: backend.clone(), dreq, store })
    }

    pub(crate) async fn execute_job(&self, snap: Arc<Snapshot>, prep: Prepared, session: Arc<Session>, extras: JobExtras) -> JobReport {
        let fail = |msg: String| JobReport { text: redact_all(&msg), is_error: true, result: None };
        let st = &self.state;
        let Prepared { backend, dreq, store } = prep;
        let cancel = session.cancel.clone();
        let (backend_permit, global_permit) = match self.acquire_permits(&snap, &backend, &cancel).await {
            Ok(p) => p,
            Err(msg) => return fail(msg),
        };
        if cancel.is_cancelled() {
            return fail(CANCELLED_QUEUED_MSG.into());
        }
        let counters = st.backend_counters(&backend.name);
        let _running = CountGuard::all([&st.counters.running, &counters.running]);
        let logs_dir = match crate::paths::logs_dir() {
            Ok(d) => d,
            Err(e) => return fail(format!("Cannot determine the logs directory: {e:#}")),
        };
        let dispatcher = Dispatcher {
            data: store.data.clone(),
            local_root: store.local_root.clone(),
            work_dir: store.base.join("work"),
            logs_dir,
            machine_id: store.machine_id.clone(),
            user_id: store.user_id.clone(),
            launcher: dispatch::real_launcher(snap.config.claude_path.clone(), snap.config.pass_env.clone()),
        };
        let pause = Arc::new(runner::Pause::default());
        st.broker.attach(&dreq.job_id, &session.id);
        let ask_job = AskJob::new(&dreq.job_id, extras.step.clone(), session.clone(), st.broker.clone(), pause.clone());
        let ask_server = AskServer::start(&dispatcher.work_dir.join("ask"), ask_job.clone())
            .map_err(|e| tracing::warn!(job = %dreq.job_id, "ask_orchestrator unavailable: {e:#}"))
            .ok();
        let wire = ask_server.as_ref().and_then(|s| {
            let exe = std::env::current_exe().map_err(|e| tracing::warn!("ask_orchestrator unavailable: {e}")).ok()?;
            Some(AskWire { exe, job_id: dreq.job_id.clone(), endpoint: s.endpoint.clone() })
        });
        tracing::info!(backend = %backend.name, job = %dreq.job_id, "sidekick_dispatch started");
        let run_extras = RunExtras { ask: wire, base: extras.base, hold_gold: extras.hold_gold, pause: Some(pause) };
        let outcome = dispatcher.run_with(&backend, &dreq, cancel.clone(), run_extras).await;
        drop(ask_server);
        drop(global_permit);
        drop(backend_permit);
        let (result, event) = match outcome {
            Ok(v) => v,
            Err(e) => return fail(format!("{e:#}")),
        };
        let mut text = format_dispatch(&result);
        let asked = ask_job.asked();
        if !asked.is_empty() {
            let _ = write!(text, "\nquestions asked: {}", asked.len());
        }
        if let Some(event) = event {
            let extra = ask::question_events(&event, &asked);
            let mut all = vec![event];
            all.extend(extra);
            for e in all {
                let store = store.clone();
                match blocking(move || store.append(e).map_err(|e| format!("{e:#}"))).await {
                    Ok(Ok((_, pushed))) => {
                        if !pushed && !text.contains("not pushed yet") {
                            text.push_str("\nnote: event stored locally, not pushed yet (sidekick_sync retries)");
                        }
                    }
                    Ok(Err(e)) | Err(e) => {
                        let _ = write!(text, "\nwarning: event could not be recorded: {e}");
                    }
                }
            }
        }
        let is_error = matches!(result.status, dispatch::Status::Blocked | dispatch::Status::Failed);
        JobReport { text: redact_all(&text), is_error, result: Some(result) }
    }

    async fn wait_event(&self, session: &Arc<Session>, ctx: &RequestContext<RoleServer>) -> Result<CallToolResult, McpError> {
        let ticker = ctx.meta.get_progress_token().map(|token| {
            spawn_progress_ticker(ctx.peer.clone(), token, session.phase.clone(), session.timeout, progress_interval())
        });
        let event = tokio::select! {
            e = session.next() => e,
            _ = ctx.ct.cancelled() => {
                session.cancel.cancel();
                self.state.broker.close(&session.id);
                drop(ticker);
                return tool_error(CANCELLED_LINE);
            }
        };
        drop(ticker);
        match event {
            Some(JobEvent::Question { job_id, step, text }) => Ok(text_result(redact_all(&format_question(&job_id, step.as_deref(), &session.id, &text)), false)),
            Some(JobEvent::Done { text, is_error }) => {
                self.state.broker.close(&session.id);
                Ok(text_result(text, is_error))
            }
            None => tool_error("the job ended without a result"),
        }
    }

    async fn acquire_permits(
        &self,
        snap: &Snapshot,
        backend: &Backend,
        cancel: &CancellationToken,
    ) -> Result<(Option<OwnedSemaphorePermit>, OwnedSemaphorePermit), String> {
        let st = &self.state;
        let counters = st.backend_counters(&backend.name);
        let _queued = CountGuard::all([&st.counters.queued, &counters.queued]);
        let backend_permit = match snap.per_backend.get(&backend.name) {
            None => None,
            Some(sem) => tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    tracing::info!("sidekick_run cancelled while queued");
                    return Err(CANCELLED_QUEUED_MSG.into());
                }
                p = sem.semaphore.clone().acquire_owned() => match p {
                    Ok(p) => Some(p),
                    Err(_) => return Err("Server is shutting down.".into()),
                },
            },
        };
        let global_permit = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                tracing::info!("sidekick_run cancelled while queued");
                return Err(CANCELLED_QUEUED_MSG.into());
            }
            p = snap.global.semaphore.clone().acquire_owned() => match p {
                Ok(p) => p,
                Err(_) => return Err("Server is shutting down.".into()),
            },
        };
        Ok((backend_permit, global_permit))
    }

    async fn store(&self, snap: &Snapshot) -> Result<Arc<DataStore>, String> {
        self.state
            .data
            .get_or_try_init(|| async {
                let base = crate::paths::data_dir().map_err(|e| format!("{e:#}"))?;
                let (repo, name) = (snap.config.data_repo.clone(), snap.config.data_name.clone());
                blocking(move || {
                    DataStore::open(&base, &repo, name.as_deref()).map(Arc::new).map_err(|e| format!("data store: {e:#}"))
                })
                .await?
            })
            .await
            .cloned()
    }

    pub(crate) async fn registry_gate(&self, snap: &Snapshot, backend: &Backend, model: Option<&str>) -> Result<bool, String> {
        let model = backend.effective_model(model)?;
        let store = self.store(snap).await?;
        let rows = store.data.models().map_err(|e| format!("registry cannot be read: {e:#}"))?;
        let id = Identity::default().key(backend, model.as_deref()).model_id();
        if rows.iter().any(|r| r.model_id() == id) {
            return Ok(true);
        }
        if rows.iter().any(|r| dispatch::same_registry_scope(backend, &r.key)) {
            return Err(format!(
                "backend {} has registered models, but `{}` is not one of them (model_id {id}); it would run without gating. \
                 Register it or choose a registered model (see sidekick_steckbrief)",
                backend.name,
                model.as_deref().unwrap_or("default")
            ));
        }
        Ok(false)
    }

    fn model_id_for(
        snap: &Snapshot,
        model_id: Option<String>,
        backend: Option<&str>,
        model: Option<&str>,
    ) -> Result<(String, Option<Backend>), String> {
        if let Some(id) = model_id.filter(|i| !i.trim().is_empty()) {
            return Ok((id.trim().to_string(), None));
        }
        let backends = snap.backends.as_ref().map_err(|e| config_invalid_message(e))?;
        let (b, _) = select_backends(backends, backend, None)?;
        let model = b.effective_model(model.map(str::trim).filter(|m| !m.is_empty()))?;
        let id = Identity::default().key(b, model.as_deref()).model_id();
        Ok((id, Some(b.clone())))
    }

    /// One attempt to run `backend`: credentials, program, endpoint probe, launch, permits
    /// (per backend first, then global; both cancellable), run. Secrets of the attempt are
    /// added to `secrets` for redaction of the result.
    #[allow(clippy::too_many_arguments)]
    async fn attempt(
        &self,
        snap: &Snapshot,
        backend: &Backend,
        req: &RunRequest,
        cancel: &CancellationToken,
        request_ct: &CancellationToken,
        phase: &std::sync::Mutex<RunPhase>,
        secrets: &mut Vec<Secrets>,
    ) -> Attempt {
        let st = &self.state;
        let ty = backend.backend_type();
        let b = backend.clone();
        let claude_path = snap.config.claude_path.clone();
        let prepared = blocking(move || -> Result<(Credential, PathBuf), String> {
            let (cred, _source) = backends::load_credential(&b)?;
            let program =
                backends::resolve_program(&b, claude_path.as_deref()).map_err(|e| {
                    match b.backend_type() {
                        BackendType::Command => e,
                        BackendType::OpenCode => format!("OpenCode executable not found: {e}"),
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
            b.harness().pre_run_check(&b, &cred)?;
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
            &snap.config.pass_env,
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

        let counters = st.backend_counters(&backend.name);
        let (backend_permit, global_permit) =
            match self.acquire_permits(snap, backend, cancel).await {
                Ok(p) => p,
                Err(msg) => return Attempt::Stopped(msg),
            };
        // The token may have fired while the permit was being handed over: never spawn then.
        // Check the request token directly as well: the forwarder may not have run yet.
        if cancel.is_cancelled() || request_ct.is_cancelled() {
            tracing::info!("sidekick_run cancelled before start");
            return Attempt::Stopped(CANCELLED_QUEUED_MSG.into());
        }
        let _running = CountGuard::all([&st.counters.running, &counters.running]);
        with_phase(phase, |p| p.started = Some(Instant::now()));
        tracing::info!(
            backend = %backend.name,
            backend_type = %ty,
            cwd = %req.cwd.display(),
            max_turns = req.max_turns,
            model = ?backend.effective_model(req.model.as_deref()).ok().flatten(),
            timeout_s = req.timeout.as_secs(),
            prompt_chars = req.prompt.chars().count(),
            "sidekick_run started"
        );
        let result = runner::run_launch(launch, req.clone(), &logs_dir, cancel.clone()).await;
        // Release this backend's slots before a fallback queues for its own.
        drop(global_permit);
        drop(backend_permit);

        match result {
            Ok(mut outcome) => {
                // Harnesses with structured output (opencode JSON lines) translate the run's
                // output into the result text; claude and command keep the tail as is. The full
                // output comes from the log (the buffer only holds the last 64-128 KiB, which
                // would undercount steps and tokens); the buffer is the fallback.
                let full = runner::read_logged_output(&outcome.log_path, outcome.output_offset);
                let raw = full.as_deref().unwrap_or(&outcome.output);
                let stopped = outcome.timed_out || outcome.cancelled;
                if let Some(n) = backend.harness().normalize_output(raw) {
                    // Redacted again: decoding JSON escapes could have revealed a secret.
                    outcome.tail =
                        redact_with(secrets, &runner::truncate_tail(&n.text, runner::TAIL_CHARS));
                    if outcome.exit_code == Some(0) && !stopped {
                        outcome.failure = n.failure;
                    }
                    if !stopped {
                        outcome.rate_limit = n.rate_limit;
                    }
                } else if !stopped && outcome.exit_code != Some(0) {
                    outcome.rate_limit = crate::harness::detect_rate_limit_strict(&outcome.tail);
                }
                if let Some(rl) = &outcome.rate_limit {
                    set_cooldown(
                        &mut st.cooldowns(),
                        &backend.name,
                        rl.retry_after_s,
                        Instant::now(),
                    );
                }
                outcome.output.clear();
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
    async fn backends_report(&self, snap: &Snapshot) -> BackendsReport {
        let st = &self.state;
        let backends = match &snap.backends {
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
                if let Some(secs) = st.cooldown(&b.name) {
                    return Err(secs);
                }
                let b = b.clone();
                let claude_path = snap.config.claude_path.clone();
                Ok(tokio::task::spawn_blocking(move || {
                    backends::check_backend(&b, claude_path.as_deref())
                }))
            })
            .collect();
        let mut list = Vec::new();
        for (b, check) in backends.iter().zip(checks) {
            let check = match check {
                Ok(check) => tokio::time::timeout_at(deadline, check).await,
                Err(secs) => Ok(Ok(backends::Reachability {
                    reachable: false,
                    detail: format!("rate limited, retry in {secs}s"),
                })),
            };
            let (reachable, detail) = match check {
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
            let (running, queued) = st.counts(&b.name);
            list.push(BackendInfo {
                name: b.name.clone(),
                kind: b.backend_type().as_str(),
                description: b.description.clone(),
                account: b.account.clone(),
                cost: b.cost.as_str(),
                default: b.is_default,
                models: b.models.clone(),
                default_model: b.effective_model(None).ok().flatten(),
                reachable,
                detail: redact_all(&detail),
                max_concurrent: b.max_concurrent,
                running,
                queued,
            });
        }
        BackendsReport {
            config_error: snap.config_error.clone(),
            backends: list,
        }
    }

    fn status_text(&self, snap: &Snapshot, located: LocateResult, tokens: TokenResults) -> String {
        let st = &self.state;
        let cfg = &snap.config;
        let path_or_err = |r: anyhow::Result<PathBuf>| match r {
            Ok(p) => p.display().to_string(),
            Err(e) => format!("unavailable ({e:#})"),
        };

        let mut s = String::new();
        let _ = writeln!(s, "agent-sidekick {}", env!("CARGO_PKG_VERSION"));
        let _ = writeln!(s, "running: {}", st.counters.running());
        let _ = writeln!(s, "queued: {}", st.counters.queued());
        for (name, secs) in active_cooldowns(&mut st.cooldowns(), Instant::now()) {
            let _ = writeln!(s, "cooldown: {name} retry in {secs}s");
        }
        let _ = writeln!(s, "max_concurrent: {}", cfg.max_concurrent);
        let _ = writeln!(s, "default max_turns: {}", cfg.default_max_turns);
        let _ = writeln!(s, "default timeout_s: {}", cfg.default_timeout_s);
        // `default_model` and the legacy agent config dir apply only to the implicit backend;
        // with `[backends]` each backend line shows its own model and config_dir instead.
        let implicit = cfg.backends.is_none();
        if implicit {
            let _ = writeln!(
                s,
                "default model: {}",
                cfg.default_model.as_deref().unwrap_or("(account default)")
            );
        }
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
        if implicit {
            let _ = writeln!(
                s,
                "agent claude config dir: {}",
                path_or_err(crate::paths::agent_claude_config_dir())
            );
        }
        let _ = writeln!(s, "config dir: {}", path_or_err(crate::paths::config_dir()));
        let _ = writeln!(s, "logs dir: {}", path_or_err(crate::paths::logs_dir()));
        let _ = writeln!(
            s,
            "config file: {}",
            path_or_err(crate::paths::config_file())
        );
        match (&snap.config_error, &snap.backends) {
            (Some(e), Ok(_)) => {
                let _ = writeln!(
                    s,
                    "config error: {e} (the last valid config.toml stays in use until it is fixed)"
                );
            }
            (Some(e), Err(_)) | (None, Err(e)) => {
                let _ = writeln!(
                    s,
                    "config error: {e} (sidekick_run is disabled until config.toml is fixed)"
                );
            }
            (None, Ok(_)) => s.push_str("config error: none\n"),
        }
        s.push_str("backends:\n");
        match &snap.backends {
            Ok(backends) => {
                for b in backends.iter() {
                    let (running, queued) = st.counts(&b.name);
                    let _ = write!(
                        s,
                        "  {} ({}{}): running {}, queued {}, max_concurrent {}",
                        b.name,
                        b.backend_type(),
                        if b.is_default { ", default" } else { "" },
                        running,
                        queued,
                        b.max_concurrent
                            .map_or_else(|| "-".to_owned(), |n| n.to_string())
                    );
                    // What a run uses when the caller passes no `model` (resolved like
                    // build_launch: the backend's model, for the implicit backend
                    // `default_model`).
                    let claude = match &b.kind {
                        backends::BackendKind::ClaudeOauth { config_dir, .. } => {
                            Some(("(account default)", config_dir))
                        }
                        backends::BackendKind::ClaudeEndpoint { config_dir, .. } => {
                            Some(("(endpoint default)", config_dir))
                        }
                        backends::BackendKind::Command { .. }
                        | backends::BackendKind::OpenCode { .. }
                        | backends::BackendKind::Codex { .. } => None,
                    };
                    let allowlist = b
                        .models
                        .as_ref()
                        .map(|l| format!(", models [{}]", l.join(", ")))
                        .unwrap_or_default();
                    if let Some((fallback_model, config_dir)) = claude {
                        let _ = write!(
                            s,
                            ", model {}{allowlist}, config_dir {}",
                            non_blank(b.model.as_deref()).unwrap_or(fallback_model),
                            config_dir.display()
                        );
                    } else if let Some(m) = &b.model {
                        // A command backend with `{model}` (it then always has a default).
                        let _ = write!(s, ", model {m}{allowlist}");
                    } else if matches!(b.kind, backends::BackendKind::Codex { .. }) {
                        let _ = write!(s, ", model (codex default){allowlist}");
                    }
                    s.push('\n');
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
                        SecretId::Backend(n) => ("provider key", n),
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
    let server = match crate::paths::config_file() {
        Ok(path) => SidekickServer::reloading(path),
        Err(e) => {
            let msg = redact_all(&format!("{e:#}"));
            tracing::warn!("config error, sidekick_run is disabled: {msg}");
            SidekickServer::new(Config::default(), Some(msg))
        }
    };
    let pull_server = server.clone();
    let snap = server.state.snapshot();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        max_concurrent = snap.config.max_concurrent,
        default_max_turns = snap.config.default_max_turns,
        default_timeout_s = snap.config.default_timeout_s,
        "agent-sidekick MCP server starting on stdio"
    );
    let pull_snap = snap.clone();
    drop(snap);
    tokio::spawn(async move {
        let wanted = pull_snap.config.data_name.is_some()
            || crate::paths::data_dir().is_ok_and(|d| DataStore::checkout_exists(&d));
        if !wanted {
            return;
        }
        match pull_server.store(&pull_snap).await {
            Ok(store) => match blocking(move || store.pull()).await {
                Ok(Err(e)) => tracing::warn!("data pull failed, using local data: {e:#}"),
                Err(e) => tracing::warn!("data pull failed: {e}"),
                Ok(Ok(())) => {}
            },
            Err(e) => tracing::warn!("data store unavailable: {e}"),
        }
    });

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
    fn cooldowns(&self) -> std::sync::MutexGuard<'_, CooldownMap> {
        self.cooldowns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn cooldown(&self, name: &str) -> Option<u64> {
        cooldown_remaining(&mut self.cooldowns(), name, Instant::now())
    }

    fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn refresh(&self) -> Arc<Snapshot> {
        let Some(reload) = &self.reload else {
            return self.snapshot();
        };
        let mut file = reload
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (now, loaded) = read_config_file(&reload.path);
        if now == *file {
            return self.snapshot();
        }
        *file = now;
        let previous = self.snapshot();
        let resolved = loaded.and_then(|config| {
            let backends = backends::resolve(&config)?;
            Ok((config, backends))
        });
        let next = match resolved {
            Ok((config, backends)) => {
                let next = Snapshot::build(config, None, Ok(backends), Some(&previous));
                tracing::info!(backends = %next.backend_names(), "config reloaded");
                next
            }
            Err(msg) if previous.backends.is_ok() => {
                tracing::warn!("config error after a change, keeping the last valid config: {msg}");
                previous.with_error(msg)
            }
            Err(msg) => {
                tracing::warn!("config error, sidekick_run is disabled: {msg}");
                Snapshot::build(
                    Config::default(),
                    Some(msg.clone()),
                    Err(msg),
                    Some(&previous),
                )
            }
        };
        let next = Arc::new(next);
        self.prune_counters(&next);
        *self
            .snapshot
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = next.clone();
        next
    }

    fn prune_counters(&self, snapshot: &Snapshot) {
        let configured: Vec<&str> = match &snapshot.backends {
            Ok(b) => b.iter().map(|b| b.name.as_str()).collect(),
            Err(_) => Vec::new(),
        };
        self.counter_map()
            .retain(|name, c| configured.contains(&name.as_str()) || c.running() + c.queued() > 0);
    }

    fn counter_map(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Counters>>> {
        self.per_backend
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn backend_counters(&self, name: &str) -> Arc<Counters> {
        self.counter_map()
            .entry(name.to_owned())
            .or_default()
            .clone()
    }

    fn counts(&self, name: &str) -> (usize, usize) {
        self.counter_map()
            .get(name)
            .map_or((0, 0), |c| (c.running(), c.queued()))
    }

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

    #[test]
    fn fallback_skipped_line_names_backend_and_wait() {
        assert_eq!(
            fallback_skipped_line("oc", 42),
            "fallback 'oc' skipped: rate limited, retry in 42s"
        );
    }

    #[test]
    fn cooldown_uses_retry_after_default_and_cap() {
        let now = Instant::now();
        assert_eq!(cooldown_until(Some(30), now), now + Duration::from_secs(30));
        assert_eq!(cooldown_until(None, now), now + Duration::from_secs(60));
        assert_eq!(
            cooldown_until(Some(99_999), now),
            now + Duration::from_secs(3600)
        );
    }

    #[test]
    fn cooldown_remaining_rounds_up_and_expires() {
        let now = Instant::now();
        let mut map = CooldownMap::new();
        set_cooldown(&mut map, "oc", Some(30), now);
        assert_eq!(cooldown_remaining(&mut map, "oc", now), Some(30));
        let later = now + Duration::from_millis(10_500);
        assert_eq!(cooldown_remaining(&mut map, "oc", later), Some(20));
        assert_eq!(cooldown_remaining(&mut map, "other", now), None);
        assert_eq!(
            active_cooldowns(&mut map, later),
            vec![("oc".to_owned(), 20)]
        );
        let expired = now + Duration::from_secs(30);
        assert_eq!(cooldown_remaining(&mut map, "oc", expired), None);
        assert!(map.is_empty());
    }

    #[test]
    fn cooldown_keeps_the_later_end_and_prunes_expired() {
        let now = Instant::now();
        let mut map = CooldownMap::new();
        set_cooldown(&mut map, "a", Some(100), now);
        set_cooldown(&mut map, "a", Some(10), now);
        set_cooldown(&mut map, "b", Some(5), now);
        let later = now + Duration::from_secs(6);
        assert_eq!(
            active_cooldowns(&mut map, later),
            vec![("a".to_owned(), 94)]
        );
        assert!(!map.contains_key("b"));
        assert_eq!(remaining_s(now, later), None);
    }

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
            output: "done".into(),
            failure: None,
            rate_limit: None,
            duration: Duration::from_millis(12_340),
            output_offset: 0,
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
        let primary_queued = PhaseView {
            fallback: None,
            attempt_began: Duration::ZERO,
            started: None,
        };
        let p = progress_param(
            token.clone(),
            Duration::from_millis(65_400),
            &primary_queued,
            Duration::from_secs(600),
        );
        assert_eq!(p.progress, 65.0);
        assert_eq!(p.total, None);
        assert_eq!(
            p.message.as_deref(),
            Some("queued for 65s, waiting for a free slot")
        );

        let primary_running = PhaseView {
            started: Some(Duration::from_secs(40)),
            ..primary_queued
        };
        let p = progress_param(
            token.clone(),
            Duration::from_secs(100),
            &primary_running,
            Duration::from_secs(600),
        );
        assert_eq!(p.progress_token, token);
        assert_eq!(p.progress, 100.0);
        assert_eq!(p.total, Some(640.0));
        assert_eq!(p.message.as_deref(), Some("agent running for 60s"));
    }

    #[test]
    fn progress_after_fallback_is_queued_then_running_and_never_past_total() {
        let token = ProgressToken(rmcp::model::NumberOrString::Number(8));
        let timeout = Duration::from_secs(4);
        // The primary ran from 0s to 3.5s; the fallback began at 3.6s and waits for a slot.
        let fb_queued = PhaseView {
            fallback: Some("slow"),
            attempt_began: Duration::from_millis(3_600),
            started: None,
        };
        let p = progress_param(token.clone(), Duration::from_secs(5), &fb_queued, timeout);
        assert_eq!(p.progress, 5.0);
        assert_eq!(p.total, None, "no total while queued");
        assert_eq!(
            p.message.as_deref(),
            Some("fallback 'slow' queued for 1s, waiting for a free slot")
        );

        // The fallback started at 6s: total is 6s + its own timeout.
        let fb_running = PhaseView {
            started: Some(Duration::from_secs(6)),
            ..fb_queued
        };
        for secs in 6..=10 {
            let p = progress_param(
                token.clone(),
                Duration::from_secs(secs),
                &fb_running,
                timeout,
            );
            assert_eq!(p.total, Some(10.0), "at {secs}s");
            assert!(p.progress <= p.total.unwrap(), "at {secs}s");
        }
        let p = progress_param(token.clone(), Duration::from_secs(9), &fb_running, timeout);
        assert_eq!(p.message.as_deref(), Some("fallback 'slow' running for 3s"));
        // While a timed-out tree is being killed, total grows with progress.
        let p = progress_param(token, Duration::from_secs(12), &fb_running, timeout);
        assert_eq!(p.progress, 12.0);
        assert_eq!(p.total, Some(12.0));
    }

    #[test]
    fn run_phase_resets_for_fallback() {
        let phase = std::sync::Mutex::new(RunPhase::primary());
        with_phase(&phase, |p| p.started = Some(Instant::now()));
        with_phase(&phase, |p| *p = RunPhase::fallback("good"));
        let p = with_phase(&phase, |p| p.clone());
        assert_eq!(p.fallback.as_deref(), Some("good"));
        assert!(p.started.is_none());
    }

    #[test]
    fn stopped_result_names_backend() {
        let primary = command("aider");
        let text = stopped_text(
            &primary,
            &request(None),
            "Working directory does not exist: /x.\n",
            None,
        );
        assert_eq!(
            text,
            "backend: aider (command)\nWorking directory does not exist: /x."
        );
        let fb = endpoint("local");
        let text = stopped_text(&primary, &request(None), CANCELLED_QUEUED_MSG, Some(&fb));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "backend: aider (command)");
        assert_eq!(lines[1], CANCELLED_QUEUED_MSG);
        assert!(
            lines[2].starts_with("fallback 'local' (claude-endpoint) not attempted"),
            "{text}"
        );
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
            account: None,
            cost: backends::Cost::Free,
            is_default,
            max_concurrent: None,
            model: None,
            models: None,
            env: Default::default(),
            pass_env: Vec::new(),
            implicit: false,
            kind,
            foreign_secret_env: Vec::new(),
        }
    }

    fn oauth(name: &str, is_default: bool) -> Backend {
        backend(
            name,
            backends::BackendKind::ClaudeOauth {
                config_dir: PathBuf::from("/cfg"),
                permissions: Default::default(),
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
                permissions: Default::default(),
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
            text.contains("note: model ignored (command backend)"),
            "{text}"
        );
        // No note for claude backends; the first line names the model.
        let text = format_run_result(&primary, &ok, &request(Some("opus")), None, &[]);
        assert!(!text.contains("note:"), "{text}");
        assert!(
            text.starts_with("backend: sidekick (claude-oauth), model opus\n"),
            "{text}"
        );
    }

    /// A command backend with `{model}` offering `models` (default: the first).
    fn command_with_models(name: &str, models: &[&str]) -> Backend {
        let mut b = command(name);
        b.kind = backends::BackendKind::Command {
            command: vec!["agent".into(), "--model={model}".into(), "{prompt}".into()],
            prompt_via_stdin: false,
            secret_env: None,
        };
        b.models = Some(models.iter().map(|m| (*m).to_owned()).collect());
        b.model = Some(models[0].to_owned());
        b
    }

    #[test]
    fn first_line_includes_effective_model() {
        let ok = outcome(Some(0), false);
        let kic = command_with_models("kic", &["gpt-oss", "qwen"]);
        let text = format_run_result(&kic, &ok, &request(None), None, &[]);
        assert!(
            text.starts_with("backend: kic (command), model gpt-oss\nexit_code: 0\n"),
            "{text}"
        );
        let text = format_run_result(&kic, &ok, &request(Some("qwen")), None, &[]);
        assert!(
            text.starts_with("backend: kic (command), model qwen\n"),
            "{text}"
        );
        // Claude backend: its default, or nothing when it has none.
        let mut local = endpoint("local");
        local.model = Some("qwen3-coder".into());
        let text = format_run_result(&local, &ok, &request(None), None, &[]);
        assert!(
            text.starts_with("backend: local (claude-endpoint), model qwen3-coder\n"),
            "{text}"
        );
        let text = format_run_result(&oauth("s", true), &ok, &request(None), None, &[]);
        assert!(text.starts_with("backend: s (claude-oauth)\n"), "{text}");
        // A command backend without {model} ignores the model: not on the first line.
        let text = format_run_result(&command("aider"), &ok, &request(Some("x")), None, &[]);
        assert!(text.starts_with("backend: aider (command)\n"), "{text}");
        let text = stopped_text(&kic, &request(None), "stopped", None);
        assert!(
            text.starts_with("backend: kic (command), model gpt-oss\n"),
            "{text}"
        );
    }

    #[test]
    fn allowlist_and_fallback_model() {
        let kic = command_with_models("kic", &["gpt-oss", "qwen"]);
        // Primary check (done in sidekick_run before anything starts).
        let e = kic.effective_model(Some("opus")).unwrap_err();
        assert!(e.contains("Invalid parameter 'model'"), "{e}");
        assert!(e.contains("'opus' is not offered by backend 'kic'"), "{e}");
        assert!(
            e.contains("Allowed models: gpt-oss, qwen (default gpt-oss)"),
            "{e}"
        );
        assert_eq!(
            kic.effective_model(Some(" qwen ")).unwrap().as_deref(),
            Some("qwen")
        );
        assert_eq!(
            kic.effective_model(None).unwrap().as_deref(),
            Some("gpt-oss")
        );
        // Fallback that does not offer the requested model: its default, with a note.
        let (fb_req, note) = fallback_request(&kic, &request(Some("opus")));
        assert_eq!(fb_req.model, None);
        assert_eq!(
            note.as_deref(),
            Some("note: model opus not offered by kic; used its default gpt-oss")
        );
        let ok = outcome(Some(0), false);
        let first = Attempt::NotStarted("down".into());
        let primary = oauth("sidekick", true);
        let text = format_run_result(
            &kic,
            &ok,
            &fb_req,
            Some((&primary, &first)),
            note.as_slice(),
        );
        let mut lines = text.lines();
        assert_eq!(
            lines.next(),
            Some("backend: kic (command), model gpt-oss — fallback after 'sidekick' failed: down")
        );
        assert_eq!(
            lines.next(),
            Some("note: model opus not offered by kic; used its default gpt-oss")
        );
        // Offered model, or no model requested: unchanged, no note.
        let (fb_req, note) = fallback_request(&kic, &request(Some("qwen")));
        assert_eq!((fb_req.model.as_deref(), note), (Some("qwen"), None));
        let (fb_req, note) = fallback_request(&kic, &request(None));
        assert_eq!((fb_req.model, note), (None, None));
        // Claude backend without allowlist accepts any model; a command backend without
        // {model} ignores it (and keeps it for the "ignored" note).
        let (fb_req, note) = fallback_request(&endpoint("local"), &request(Some("opus")));
        assert_eq!((fb_req.model.as_deref(), note), (Some("opus"), None));
        let (fb_req, note) = fallback_request(&command("aider"), &request(Some("opus")));
        assert_eq!((fb_req.model.as_deref(), note), (Some("opus"), None));
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
        let snap = st.snapshot();
        assert_eq!(snap.per_backend.len(), 1);
        assert_eq!(snap.per_backend["aider"].semaphore.available_permits(), 1);
        assert!(!snap.per_backend.contains_key("sidekick"));

        let aider = st.backend_counters("aider");
        let _q = CountGuard::all([&st.counters.queued, &aider.queued]);
        let text = server.status_text(&server.state.snapshot(), Err("stub".into()), Ok(Vec::new()));
        assert!(text.contains("queued: 1\n"), "{text}");
        assert!(
            text.contains("  aider (command): running 0, queued 1, max_concurrent 1\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "  sidekick (claude-oauth, default): running 0, queued 0, max_concurrent -, \
                 model (account default), config_dir /cfg\n"
            ),
            "{text}"
        );
        assert!(text.contains("config error: none"), "{text}");
    }

    #[test]
    fn status_with_explicit_backends_shows_per_backend_model_and_config_dir() {
        let cfg: Config = toml::from_str(
            "default_model = \"opus\"\n[backends.sidekick]\ntype = \"claude-oauth\"\n\
             description = \"main\"\ndefault = true\n",
        )
        .unwrap();
        assert!(cfg.backends.is_some());
        let mut list = sample_backends();
        let mut local = endpoint("local");
        local.model = Some("qwen3-coder".into());
        list = Backends::new(
            list.iter()
                .filter(|b| b.name != "local")
                .cloned()
                .chain([local])
                .collect(),
        );
        let server = SidekickServer::with_backends(cfg, None, Ok(list));
        let text = server.status_text(&server.state.snapshot(), Err("stub".into()), Ok(Vec::new()));
        // The top-level default_model is not in effect with [backends]: not shown.
        assert!(!text.contains("default model:"), "{text}");
        assert!(!text.contains("opus"), "{text}");
        assert!(!text.contains("agent claude config dir:"), "{text}");
        assert!(
            text.contains(
                "  local (claude-endpoint): running 0, queued 0, max_concurrent -, \
                 model qwen3-coder, config_dir /cfg-local\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(", model (account default), config_dir /cfg\n"),
            "{text}"
        );
        // Command backends have neither.
        assert!(
            text.contains("  aider (command): running 0, queued 0, max_concurrent 1\n"),
            "{text}"
        );
    }

    #[test]
    fn status_with_implicit_backend_shows_default_model() {
        let cfg = Config {
            default_model: Some("sonnet".into()),
            ..Config::default()
        };
        let mut b = oauth("sidekick", true);
        b.implicit = true;
        b.model = cfg.default_model.clone();
        let server = SidekickServer::with_backends(cfg, None, Ok(Backends::new(vec![b])));
        let text = server.status_text(&server.state.snapshot(), Err("stub".into()), Ok(Vec::new()));
        assert!(text.contains("default model: sonnet\n"), "{text}");
        assert!(text.contains("agent claude config dir: "), "{text}");
        assert!(
            text.contains(
                "  sidekick (claude-oauth, default): running 0, queued 0, max_concurrent -, \
                 model sonnet, config_dir /cfg\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn status_never_shows_token() {
        let server = SidekickServer::with_backends(Config::default(), None, Ok(sample_backends()));
        let secret = "sk-ant-oat01-SECRETSECRET";
        let text = server.status_text(
            &server.state.snapshot(),
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
        assert!(text.contains("max_concurrent: 16"), "{text}");
    }

    #[test]
    fn invalid_config_disables_backends() {
        let server = SidekickServer::new(Config::default(), Some("bad config".into()));
        assert_eq!(
            server.state.snapshot().backends,
            Err("bad config".to_owned())
        );
        let text = server.status_text(&server.state.snapshot(), Err("stub".into()), Ok(Vec::new()));
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
        let report = server.backends_report(&server.state.snapshot()).await;
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
        let report = server.backends_report(&server.state.snapshot()).await;
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

    fn limited(name: &str, n: Option<usize>, is_default: bool) -> Backend {
        let mut b = command(name);
        b.is_default = is_default;
        b.max_concurrent = n;
        b
    }

    #[test]
    fn limits_are_kept_or_replaced_by_name_on_reload() {
        let cfg = |n| Config {
            max_concurrent: n,
            ..Config::default()
        };
        let list = |n| {
            Ok(Backends::new(vec![
                limited("a", Some(n), true),
                limited("b", Some(2), false),
            ]))
        };
        let first = Snapshot::build(cfg(16), None, list(1), None);
        let held = first.per_backend["a"]
            .semaphore
            .clone()
            .try_acquire_owned()
            .unwrap();

        let same = Snapshot::build(cfg(16), None, list(1), Some(&first));
        assert!(Arc::ptr_eq(&same.global.semaphore, &first.global.semaphore));
        assert!(Arc::ptr_eq(
            &same.per_backend["a"].semaphore,
            &first.per_backend["a"].semaphore
        ));

        let changed = Snapshot::build(cfg(4), None, list(3), Some(&same));
        assert!(!Arc::ptr_eq(
            &changed.global.semaphore,
            &first.global.semaphore
        ));
        assert_eq!(changed.global.semaphore.available_permits(), 4);
        assert_eq!(changed.per_backend["a"].semaphore.available_permits(), 3);
        assert_eq!(first.per_backend["a"].semaphore.available_permits(), 0);
        assert!(Arc::ptr_eq(
            &changed.per_backend["b"].semaphore,
            &first.per_backend["b"].semaphore
        ));
        drop(held);
        assert_eq!(first.per_backend["a"].semaphore.available_permits(), 1);
        assert_eq!(changed.per_backend["a"].semaphore.available_permits(), 3);

        let removed = Snapshot::build(
            cfg(4),
            None,
            Ok(Backends::new(vec![limited("a", None, true)])),
            Some(&changed),
        );
        assert!(removed.per_backend.is_empty());
    }

    fn command_config(names: &[&str], extra: &str) -> String {
        let mut s = extra.to_owned();
        for (i, name) in names.iter().enumerate() {
            s.push_str(&format!(
                "[backends.{name}]\ntype = \"command\"\ndescription = \"{name}\"\n\
                 default = {}\ncommand = [\"agent\", \"{{prompt}}\"]\nmax_concurrent = 2\n",
                i == 0
            ));
        }
        s
    }

    #[test]
    fn refresh_reloads_changes_and_keeps_last_valid_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, command_config(&["a"], "max_concurrent = 3\n")).unwrap();
        let server = SidekickServer::reloading(path.clone());
        let st = &server.state;
        let first = st.refresh();
        assert!(Arc::ptr_eq(&first, &st.refresh()));
        assert_eq!(first.backend_names(), "a");
        assert_eq!(first.global.permits, 3);
        let counters = st.backend_counters("a");
        let _running = CountGuard::all([&counters.running]);
        set_cooldown(&mut st.cooldowns(), "a", Some(300), Instant::now());

        std::fs::write(&path, command_config(&["a", "b"], "max_concurrent = 5\n")).unwrap();
        let second = st.refresh();
        assert_eq!(second.backend_names(), "a, b");
        assert_eq!(second.global.permits, 5);
        assert_eq!(first.global.permits, 3);
        assert!(Arc::ptr_eq(
            &second.per_backend["a"].semaphore,
            &first.per_backend["a"].semaphore
        ));
        assert_eq!(st.counts("a"), (1, 0));
        assert!(st.cooldown("a").is_some());

        std::fs::write(&path, "max_concurrent = \"many\"\n").unwrap();
        let broken = st.refresh();
        assert_eq!(broken.backend_names(), "a, b");
        assert_eq!(broken.config.max_concurrent, 5);
        let err = broken.config_error.clone().unwrap();
        assert!(err.contains("parsing"), "{err}");
        assert!(stale_config_note(&broken).unwrap().contains(&err));
        let text = server.status_text(&broken, Err("stub".into()), Ok(Vec::new()));
        assert!(
            text.contains("the last valid config.toml stays in use"),
            "{text}"
        );

        std::fs::write(&path, command_config(&["b"], "")).unwrap();
        let fixed = st.refresh();
        assert_eq!(fixed.config_error, None);
        assert_eq!(fixed.backend_names(), "b");
        assert_eq!(fixed.global.permits, 16);
        assert_eq!(st.counts("a"), (1, 0), "running counters survive removal");
        drop(_running);

        std::fs::remove_file(&path).unwrap();
        let missing = st.refresh();
        assert_eq!(missing.config, Config::default());
        assert!(st.counts("a") == (0, 0));
    }
}
