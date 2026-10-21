//! Runs the agent's `claude -p` as a child process with timeout and kill-tree.
//! INTERFACE STUB: implemented in step 4.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use crate::token::Token;

/// Number of trailing characters of output returned to the caller.
pub const TAIL_CHARS: usize = 4000;

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
    /// `None` if the process was killed (timeout) or ended by a signal.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
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

/// Arguments after the executable: `-p <prompt> --max-turns <n> [--model m] [--allowedTools a b]`.
pub fn build_args(_req: &RunRequest) -> Vec<OsString> {
    todo!("step 4")
}

/// Child environment: `base` without `ANTHROPIC_API_KEY` (and any existing
/// `CLAUDE_CONFIG_DIR`/`CLAUDE_CODE_OAUTH_TOKEN`), plus those two set from the context.
pub fn build_env(
    _base: impl IntoIterator<Item = (OsString, OsString)>,
    _token: &Token,
    _agent_config_dir: &std::path::Path,
) -> Vec<(OsString, OsString)> {
    todo!("step 4")
}

/// Last `max_chars` characters (char boundary safe), prefixed by a marker if cut.
pub fn truncate_tail(_s: &str, _max_chars: usize) -> String {
    todo!("step 4")
}

/// Spawn, collect output into `<logs_dir>/<timestamp>-<short-id>.log`, enforce the timeout.
pub async fn run(_ctx: &RunContext, _req: RunRequest) -> Result<RunOutcome, RunError> {
    todo!("step 4")
}
