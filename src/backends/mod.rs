//! Configurable backends: where a `sidekick_run` is executed.
//!
//! - `claude-oauth`: the official `claude` CLI with a Claude account's OAuth token
//!   (the v0.1 behaviour; implicit backend `sidekick` when config.toml has no `[backends]`).
//! - `claude-endpoint`: the `claude` CLI against an Anthropic-compatible endpoint
//!   (`ANTHROPIC_BASE_URL`, e.g. Ollama or LM Studio) with an API key.
//! - `command`: any other agent CLI, configured as an argument array with placeholders;
//!   optionally with a stored secret handed over in the variable `secret_env`.
//!
//! Submodules: [`resolve`] (config validation), [`launch`] (environment, credentials,
//! [`crate::runner::Launch`] building), [`reach`] (reachability checks).

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub mod launch;
pub mod reach;
pub mod resolve;

pub use launch::{build_launch, load_credential, resolve_program, Credential};
pub use reach::{check_backend, probe_http, Reachability};
pub use resolve::{resolve, resolve_with, ResolveDefaults};

/// Name of the implicit backend (and of the legacy secret locations).
pub const IMPLICIT_BACKEND: &str = "sidekick";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendType {
    ClaudeOauth,
    ClaudeEndpoint,
    Command,
}

impl BackendType {
    pub fn as_str(self) -> &'static str {
        match self {
            BackendType::ClaudeOauth => "claude-oauth",
            BackendType::ClaudeEndpoint => "claude-endpoint",
            BackendType::Command => "command",
        }
    }
}

impl fmt::Display for BackendType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cost {
    Free,
    Quota,
    Paid,
}

impl Cost {
    pub fn as_str(self) -> &'static str {
        match self {
            Cost::Free => "free",
            Cost::Quota => "quota",
            Cost::Paid => "paid",
        }
    }
}

impl fmt::Display for Cost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a claude-endpoint key is sent: `x-api-key` header (`ANTHROPIC_API_KEY`, default)
/// or `Authorization: Bearer` (`ANTHROPIC_AUTH_TOKEN`, needed e.g. by Ollama's cloud).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EndpointAuth {
    #[default]
    XApiKey,
    Bearer,
}

impl EndpointAuth {
    /// The environment variable claude reads the key from.
    pub fn env_var(self) -> &'static str {
        match self {
            EndpointAuth::XApiKey => "ANTHROPIC_API_KEY",
            EndpointAuth::Bearer => "ANTHROPIC_AUTH_TOKEN",
        }
    }
}

/// One `[backends.<name>]` table exactly as written in config.toml. Every field is optional
/// here; [`resolve`] checks which ones are required/allowed for the `type`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawBackend {
    #[serde(rename = "type")]
    pub kind: Option<BackendType>,
    pub description: Option<String>,
    pub default: Option<bool>,
    pub cost: Option<Cost>,
    pub max_concurrent: Option<usize>,
    /// Default model: claude-* `--model`; command: the value of `{model}` when the request
    /// names none. Must be one of `models` when both are set.
    pub model: Option<String>,
    /// Models the backend offers (an allowlist for the request's `model`; the first one is
    /// the default when `model` is not set). Required for a command backend whose command
    /// uses `{model}`.
    pub models: Option<Vec<String>>,
    /// Extra, non-secret environment variables for the child (all types).
    pub env: Option<BTreeMap<String, String>>,
    /// Inherited variables to keep for this backend although they would be removed.
    pub pass_env: Option<Vec<String>>,
    /// claude-*: `CLAUDE_CONFIG_DIR` for the agent (`~` is expanded).
    pub config_dir: Option<PathBuf>,
    /// claude-endpoint: e.g. `http://localhost:11434`.
    pub base_url: Option<String>,
    /// claude-endpoint: name of an environment variable holding the key.
    pub api_key_env: Option<String>,
    /// claude-endpoint: how the key is sent.
    pub auth: Option<EndpointAuth>,
    /// command: program and arguments; placeholders `{prompt}`, `{cwd}`, `{max_turns}`,
    /// `{model}`.
    pub command: Option<Vec<String>>,
    /// command: send the prompt on stdin instead of via `{prompt}`.
    pub prompt_via_stdin: Option<bool>,
    /// command: name of the environment variable that receives the backend's stored secret
    /// (keyring user `cmd-<name>`, stored with `setup --backend <name>`).
    pub secret_env: Option<String>,
}

/// A validated backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backend {
    pub name: String,
    pub description: String,
    pub cost: Cost,
    pub is_default: bool,
    /// Per-backend concurrency limit (in addition to the global one).
    pub max_concurrent: Option<usize>,
    /// Default model (claude-*: `--model`; command: the value of `{model}`). With `models`
    /// it is always one of them.
    pub model: Option<String>,
    /// Allowlist of models a request may choose (`None`: claude-* accepts any model; a
    /// command backend without `{model}` takes no model).
    pub models: Option<Vec<String>>,
    pub env: BTreeMap<String, String>,
    pub pass_env: Vec<String>,
    /// True for the implicit `sidekick` backend created when `[backends]` is absent.
    pub implicit: bool,
    pub kind: BackendKind,
    /// Names of every configured `api_key_env` (of all backends, this one included).
    /// Removed from this backend's inherited child environment; `pass_env` cannot re-allow
    /// them. Filled by [`Backends::new`]; the endpoint that owns a key gets it through
    /// `ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN`, a command backend whose `secret_env` or
    /// `env` table names the same variable still gets that explicit value.
    pub foreign_secret_env: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendKind {
    ClaudeOauth {
        config_dir: PathBuf,
    },
    ClaudeEndpoint {
        base_url: String,
        api_key_env: Option<String>,
        auth: EndpointAuth,
        config_dir: PathBuf,
    },
    Command {
        command: Vec<String>,
        prompt_via_stdin: bool,
        /// Variable that receives the stored secret ([`crate::token::SecretId::Command`]).
        secret_env: Option<String>,
    },
}

impl Backend {
    pub fn backend_type(&self) -> BackendType {
        match self.kind {
            BackendKind::ClaudeOauth { .. } => BackendType::ClaudeOauth,
            BackendKind::ClaudeEndpoint { .. } => BackendType::ClaudeEndpoint,
            BackendKind::Command { .. } => BackendType::Command,
        }
    }

    /// Whether a run on this backend uses a model: claude-* always (`--model` or the
    /// account's/endpoint's default), a command backend only with `{model}` in its command.
    pub fn takes_model(&self) -> bool {
        match &self.kind {
            BackendKind::Command { command, .. } => command_uses_model(command),
            _ => true,
        }
    }

    /// The model a run uses for the caller's `requested` model (trimmed, blank = none):
    /// the requested one or the backend's default. `Ok(None)`: no model is passed (claude-*
    /// without default, or a command backend without `{model}`, which ignores a model).
    /// `Err`: the requested model is not in the backend's allowlist (user-facing message).
    pub fn effective_model(&self, requested: Option<&str>) -> Result<Option<String>, String> {
        if !self.takes_model() {
            return Ok(None);
        }
        let requested = requested.map(str::trim).filter(|m| !m.is_empty());
        let Some(m) = requested else {
            return Ok(self.model.clone());
        };
        match &self.models {
            Some(list) if !list.iter().any(|x| x == m) => Err(format!(
                "Invalid parameter 'model': '{m}' is not offered by backend '{}'. Allowed \
                 models: {} (default {}). Choose one of them or omit 'model'; see \
                 sidekick_backends.",
                self.name,
                list.join(", "),
                self.model.as_deref().unwrap_or("-")
            )),
            _ => Ok(Some(m.to_owned())),
        }
    }

    /// The secret this backend reads from the keyring/file store, if any.
    /// claude-endpoint with `api_key_env` reads the environment instead (`None`); a command
    /// backend has one only with `secret_env`.
    pub fn secret_id(&self) -> Option<crate::token::SecretId> {
        use crate::token::SecretId;
        match &self.kind {
            BackendKind::ClaudeOauth { .. } => Some(SecretId::Oauth(self.name.clone())),
            BackendKind::ClaudeEndpoint {
                api_key_env: None, ..
            } => Some(SecretId::Endpoint(self.name.clone())),
            BackendKind::Command {
                secret_env: Some(_),
                ..
            } => Some(SecretId::Command(self.name.clone())),
            _ => None,
        }
    }
}

/// Whether a command template uses `{model}` in an argument (`command[0]` is never
/// substituted).
pub fn command_uses_model(command: &[String]) -> bool {
    command.iter().skip(1).any(|a| a.contains("{model}"))
}

/// All configured backends (at least one, exactly one default), sorted by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backends {
    list: Vec<Backend>,
}

impl Backends {
    /// `list` must be non-empty and contain exactly one default (checked by [`resolve`]).
    /// Sets every backend's [`Backend::foreign_secret_env`] to all configured `api_key_env`
    /// names.
    pub fn new(mut list: Vec<Backend>) -> Self {
        list.sort_by(|a, b| a.name.cmp(&b.name));
        let mut key_vars: Vec<String> = Vec::new();
        for b in &list {
            if let BackendKind::ClaudeEndpoint {
                api_key_env: Some(var),
                ..
            } = &b.kind
            {
                if !key_vars.contains(var) {
                    key_vars.push(var.clone());
                }
            }
        }
        for b in &mut list {
            for v in &key_vars {
                if !b.foreign_secret_env.contains(v) {
                    b.foreign_secret_env.push(v.clone());
                }
            }
        }
        debug_assert_eq!(list.iter().filter(|b| b.is_default).count(), 1);
        Self { list }
    }

    pub fn get(&self, name: &str) -> Option<&Backend> {
        self.list.iter().find(|b| b.name == name)
    }

    pub fn default_backend(&self) -> &Backend {
        self.list
            .iter()
            .find(|b| b.is_default)
            .unwrap_or(&self.list[0])
    }

    pub fn iter(&self) -> impl Iterator<Item = &Backend> {
        self.list.iter()
    }

    pub fn names(&self) -> Vec<&str> {
        self.list.iter().map(|b| b.name.as_str()).collect()
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }
}
