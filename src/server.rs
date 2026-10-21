//! MCP server (rmcp, stdio) exposing `sidekick_run` and `sidekick_status`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt,
};
use serde::Deserialize;
use tokio::sync::Semaphore;

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
    if outcome.timed_out {
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

/// A run counts as failed (tool error) on timeout or any non-zero / missing exit code.
fn outcome_is_error(outcome: &RunOutcome) -> bool {
    outcome.timed_out || outcome.exit_code != Some(0)
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

#[derive(Debug)]
struct State {
    config: Config,
    /// `Config::load` error, reported by `sidekick_status`.
    config_error: Option<String>,
    semaphore: Arc<Semaphore>,
    running: AtomicUsize,
    queued: AtomicUsize,
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
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;

        // Pure validation first: cheap, and independent of the claude install and the token.
        let base_dir =
            match std::env::current_dir() {
                Ok(d) => d,
                Err(e) => return tool_error(format!(
                    "Cannot determine the server's current directory: {e}. Pass an absolute 'cwd'."
                )),
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
        let ctx = RunContext {
            claude: located.path,
            token,
            agent_config_dir,
            logs_dir,
        };
        let timeout = req.timeout;

        let permit = {
            let _queued = CountGuard::new(&state.queued);
            match state.semaphore.clone().acquire_owned().await {
                Ok(p) => p,
                Err(_) => return fail("Server is shutting down.".into()),
            }
        };
        let _running = CountGuard::new(&state.running);
        tracing::info!(
            cwd = %req.cwd.display(),
            max_turns = req.max_turns,
            model = ?req.model,
            timeout_s = timeout.as_secs(),
            prompt_chars = req.prompt.chars().count(),
            "sidekick_run started"
        );
        let result = runner::run(&ctx, req).await;
        drop(permit);

        match result {
            Ok(outcome) => {
                tracing::info!(
                    exit_code = ?outcome.exit_code,
                    timed_out = outcome.timed_out,
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
/// client disconnects.
pub async fn serve() -> anyhow::Result<()> {
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

    let service = SidekickServer::new(config, config_error)
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| anyhow::anyhow!("MCP initialization failed: {e}"))?;
    let reason = service.waiting().await?;
    tracing::info!("MCP client disconnected ({reason:?}); shutting down");
    Ok(())
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
