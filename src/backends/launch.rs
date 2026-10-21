//! Child environment, credentials and [`Launch`] building per backend type.
//! INTERFACE STUB: implemented by the `launch` task.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::Backend;
use crate::runner::{Launch, RunRequest};
use crate::token::Token;

/// The credential a backend needs for one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// claude-oauth: becomes `CLAUDE_CODE_OAUTH_TOKEN`.
    OauthToken(Token),
    /// claude-endpoint: becomes `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN`.
    ApiKey(Token),
    /// command backends.
    None,
}

/// Load the credential for `backend`. `Ok((cred, source))` where `source` describes where
/// it came from (never the value). The error is a user-facing message with a next step.
pub fn load_credential(_backend: &Backend) -> Result<(Credential, String), String> {
    todo!("launch task")
}

/// The executable for `backend`: claude (via `locate::find_claude(claude_path)`) for
/// claude-* types, `command[0]` resolved via PATH (or as given if absolute) for `command`.
pub fn resolve_program(_backend: &Backend, _claude_path: Option<&Path>) -> Result<PathBuf, String> {
    todo!("launch task")
}

/// Child environment for `backend`: `base` minus the removed variables (session
/// isolation, competing credentials), plus `pass_env` exceptions (global + per backend),
/// the backend's `env` table, and the type-specific variables (see the design doc).
pub fn child_env(
    _backend: &Backend,
    _base: impl IntoIterator<Item = (OsString, OsString)>,
    _global_pass_env: &[String],
    _credential: &Credential,
) -> Vec<(OsString, OsString)> {
    todo!("launch task")
}

/// Replace `{prompt}`, `{cwd}`, `{max_turns}` in one template element in a single pass:
/// replacement text is never scanned again, unknown `{x}` stays literal.
pub fn substitute(_template: &str, _prompt: &str, _cwd: &Path, _max_turns: u32) -> String {
    todo!("launch task")
}

/// Build the [`Launch`] for one run of `backend` with the inherited environment `base`.
pub fn build_launch(
    _backend: &Backend,
    _req: &RunRequest,
    _credential: &Credential,
    _program: &Path,
    _base: impl IntoIterator<Item = (OsString, OsString)>,
    _global_pass_env: &[String],
) -> Result<Launch, String> {
    todo!("launch task")
}
