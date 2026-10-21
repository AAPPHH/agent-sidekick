//! The `claude` harness: the official Claude Code CLI, run as `claude -p`.
//!
//! Two modes, chosen by the backend config (see `crate::backends::resolve`):
//! - OAuth (`harness = "claude"` without `base_url`, legacy `type = "claude-oauth"`): a Claude
//!   account's token in `CLAUDE_CODE_OAUTH_TOKEN`. This is the only place an OAuth token is
//!   ever put into a child environment.
//! - Endpoint (`harness = "claude"` with `base_url`, legacy `type = "claude-endpoint"`):
//!   `ANTHROPIC_BASE_URL` plus an API key in `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN`;
//!   an inherited `CLAUDE_CODE_OAUTH_TOKEN` is removed (defence in depth).
//!
//! Both modes get their own `CLAUDE_CONFIG_DIR` and the common session isolation of
//! [`crate::backends::launch::child_env`]. argv comes from [`crate::runner::claude_launch`].

use std::path::{Path, PathBuf};

use super::{wrong_harness, CheckDeps, Harness, HarnessKind, LaunchInput, SecretLoader};
use crate::backends::launch::{child_env, eq_name, BASE_URL_ENV, BASE_URL_WARNING};
use crate::backends::{Backend, BackendKind, Credential, Reachability, IMPLICIT_BACKEND};
use crate::redact::Secrets;
use crate::runner::{Launch, RunRequest};
use crate::token::{SecretId, Token, TokenError};

/// Official install commands, from code.claude.com/docs/en/setup (checked 2026-10-21).
const INSTALL_HINT: &str = "\
macOS, Linux, WSL:   curl -fsSL https://claude.ai/install.sh | bash
Windows PowerShell:  irm https://claude.ai/install.ps1 | iex
Windows CMD:         curl -fsSL https://claude.ai/install.cmd -o install.cmd && install.cmd && del install.cmd
Homebrew:            brew install --cask claude-code
WinGet:              winget install Anthropic.ClaudeCode
(see https://code.claude.com/docs/en/setup)";

/// Adapter of the `claude` harness.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeHarness;

impl Harness for ClaudeHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Claude
    }

    fn locate(&self, _backend: &Backend, claude_path: Option<&Path>) -> Result<PathBuf, String> {
        find_claude(claude_path)
    }

    fn locate_installed(&self, claude_path: Option<&Path>) -> Option<Result<PathBuf, String>> {
        Some(find_claude(claude_path))
    }

    fn load_credential(
        &self,
        backend: &Backend,
        env_lookup: &dyn Fn(&str) -> Option<String>,
        secret_loader: SecretLoader<'_>,
    ) -> Result<(Credential, String), String> {
        let name = &backend.name;
        match &backend.kind {
            BackendKind::ClaudeOauth { .. } => {
                let id = SecretId::Oauth(name.clone());
                match secret_loader(&id) {
                    Ok((token, source)) => Ok((Credential::OauthToken(token), source.to_string())),
                    Err(TokenError::Missing) | Err(TokenError::Empty) => {
                        Err(missing_oauth_message(name))
                    }
                    Err(TokenError::Invalid) => Err(format!(
                        "The stored {id} is invalid (it contains line breaks or control \
                         characters). Run 'agent-sidekick setup --backend {name}' to store it again."
                    )),
                    Err(TokenError::Storage(e)) => Err(format!(
                        "Cannot read the {id}: {e}. Run 'agent-sidekick doctor' or \
                         'agent-sidekick setup --backend {name}' in a terminal."
                    )),
                }
            }
            BackendKind::ClaudeEndpoint {
                api_key_env: Some(var),
                ..
            } => {
                let value = env_lookup(var).map(Token::new);
                match value {
                    Some(t) if !t.expose().is_empty() => {
                        Ok((Credential::ApiKey(t), format!("environment variable {var}")))
                    }
                    _ => Err(format!(
                        "Environment variable {var} (api_key_env of backend '{name}') is not set in \
                         the MCP server's environment; set it in the registration \
                         (claude mcp add ... -e) or store a key with \
                         'agent-sidekick setup --backend {name}'"
                    )),
                }
            }
            BackendKind::ClaudeEndpoint {
                api_key_env: None, ..
            } => {
                let id = SecretId::Endpoint(name.clone());
                match secret_loader(&id) {
                    Ok((key, source)) => Ok((Credential::ApiKey(key), source.to_string())),
                    Err(TokenError::Missing) | Err(TokenError::Empty) => Err(format!(
                        "No API key stored for backend '{name}'. Run \
                         'agent-sidekick setup --backend {name}' in a terminal (local servers that \
                         ignore keys accept any value such as 'ollama'), or set api_key_env in \
                         config.toml."
                    )),
                    Err(TokenError::Invalid) => Err(format!(
                        "The stored {id} is invalid (it contains line breaks or control \
                         characters). Run 'agent-sidekick setup --backend {name}' to store it again."
                    )),
                    Err(TokenError::Storage(e)) => Err(format!(
                        "Cannot read the {id}: {e}. Run 'agent-sidekick doctor' or \
                         'agent-sidekick setup --backend {name}' in a terminal."
                    )),
                }
            }
            BackendKind::Command { .. } | BackendKind::OpenCode { .. } => {
                Err(wrong_harness(self, backend))
            }
        }
    }

    fn build_launch(&self, input: LaunchInput<'_>) -> Result<Launch, String> {
        let LaunchInput {
            backend,
            req,
            credential,
            program,
            base,
            global_pass_env,
            model,
            mut header,
        } = input;
        if matches!(
            backend.kind,
            BackendKind::Command { .. } | BackendKind::OpenCode { .. }
        ) {
            return Err(wrong_harness(self, backend));
        }
        let env = child_env(backend, base, global_pass_env, credential);
        match &backend.kind {
            BackendKind::ClaudeEndpoint { base_url, .. } => {
                header.push(format!("endpoint: {base_url}"));
            }
            _ => {
                if env.iter().any(|(k, _)| eq_name(k, BASE_URL_ENV)) {
                    tracing::warn!(
                        "{BASE_URL_ENV} is inherited from the server environment; the agent's requests go to that endpoint"
                    );
                    header.push(BASE_URL_WARNING.to_owned());
                }
            }
        }
        let req2 = RunRequest {
            model,
            ..req.clone()
        };
        let secrets = match credential.secret() {
            Some(t) => Secrets::new([t.expose()]),
            None => Secrets::none(),
        };
        Ok(crate::runner::claude_launch(
            program, &req2, env, secrets, header,
        ))
    }

    fn check(&self, backend: &Backend, deps: &CheckDeps<'_>) -> Reachability {
        match &backend.kind {
            BackendKind::ClaudeOauth { .. } => {
                let claude = match (deps.find_claude)() {
                    Ok(p) => p,
                    Err(e) => return Reachability::unreachable(format!("claude not found: {e}")),
                };
                let id = SecretId::Oauth(backend.name.clone());
                match (deps.secret)(&id) {
                    Ok(source) => Reachability::ok(format!(
                        "claude {}; token from {source}",
                        claude.display()
                    )),
                    Err(TokenError::Missing) => Reachability::unreachable(format!(
                        "no token stored; run `agent-sidekick setup --backend {}`",
                        backend.name
                    )),
                    Err(e) => Reachability::unreachable(format!("cannot read the {id}: {e}")),
                }
            }
            BackendKind::ClaudeEndpoint {
                base_url,
                api_key_env,
                ..
            } => {
                if let Err(e) = (deps.find_claude)() {
                    return Reachability::unreachable(format!("claude not found: {e}"));
                }
                match api_key_env {
                    Some(var) => {
                        if (deps.env_lookup)(var).is_none_or(|v| v.trim().is_empty()) {
                            return Reachability::unreachable(format!(
                                "env var {var} (api_key_env) is not set"
                            ));
                        }
                    }
                    None => {
                        let id = SecretId::Endpoint(backend.name.clone());
                        match (deps.secret)(&id) {
                            Ok(_) => {}
                            Err(TokenError::Missing) => {
                                return Reachability::unreachable(format!(
                                    "no API key stored; run `agent-sidekick setup --backend {}` \
                                     (for local servers that ignore keys, enter any value, e.g. `ollama`)",
                                    backend.name
                                ))
                            }
                            Err(e) => {
                                return Reachability::unreachable(format!(
                                    "cannot read the {id}: {e}"
                                ))
                            }
                        }
                    }
                }
                (deps.probe)(base_url)
            }
            BackendKind::Command { .. } | BackendKind::OpenCode { .. } => {
                Reachability::unreachable(wrong_harness(self, backend))
            }
        }
    }

    fn install_hint(&self) -> Option<&'static str> {
        Some(INSTALL_HINT)
    }
}

/// `claude` via [`crate::locate::find_claude`] (`AGENT_SIDEKICK_CLAUDE`, `claude_path`, PATH,
/// well-known install locations).
fn find_claude(claude_path: Option<&Path>) -> Result<PathBuf, String> {
    crate::locate::find_claude(claude_path)
        .map(|l| l.path)
        .map_err(|e| e.to_string())
}

fn missing_oauth_message(name: &str) -> String {
    if name == IMPLICIT_BACKEND {
        format!(
            "No token stored for backend '{name}'. Run 'agent-sidekick setup' (or \
             'agent-sidekick setup --backend {name}') in a terminal."
        )
    } else {
        format!(
            "No token stored for backend '{name}'. Run 'agent-sidekick setup --backend {name}' \
             in a terminal."
        )
    }
}
