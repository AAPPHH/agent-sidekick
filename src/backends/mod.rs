//! Configurable backends: where a `sidekick_run` is executed.
//!
//! Each backend runs one harness (agent CLI, see [`crate::harness`]), configured with
//! `harness = "claude" | "command"` or the legacy `type`. Internally a backend keeps its
//! [`BackendType`] (shown in results and logs as before):
//!
//! - `claude-oauth`: the official `claude` CLI with a Claude account's OAuth token
//!   (the v0.1 behaviour; implicit backend `claude` when config.toml has no `[backends]`).
//! - `claude-endpoint`: the `claude` CLI against an Anthropic-compatible endpoint
//!   (`ANTHROPIC_BASE_URL`, e.g. Ollama or LM Studio) with an API key.
//! - `command`: any other agent CLI, configured as an argument array with placeholders;
//!   optionally with a stored secret handed over in the variable `secret_env`.
//! - `opencode`: the OpenCode CLI (`harness = "opencode"`, no legacy `type`) against an
//!   OpenAI-compatible endpoint, Ollama or OpenRouter, in a per-backend isolated directory.
//! - `codex`: the OpenAI Codex CLI (`harness = "codex"`, no legacy `type`) with a ChatGPT
//!   login or an API key, in a per-backend `CODEX_HOME`.
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
pub const IMPLICIT_BACKEND: &str = "claude";
/// Name of the implicit backend before v0.6. A claude-oauth backend with this name (declared
/// in an older config.toml) keeps the same token and config dir as [`IMPLICIT_BACKEND`].
pub const OLD_IMPLICIT_BACKEND: &str = "sidekick";

/// `true` for the names whose claude-oauth backend uses the v0.1 token and the default
/// config dir `~/.claude-sidekick`: [`IMPLICIT_BACKEND`] and [`OLD_IMPLICIT_BACKEND`].
pub fn is_primary_oauth_name(name: &str) -> bool {
    name == IMPLICIT_BACKEND || name == OLD_IMPLICIT_BACKEND
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendType {
    ClaudeOauth,
    ClaudeEndpoint,
    Command,
    /// Only reachable through `harness = "opencode"`; `type = "opencode"` is rejected by
    /// [`resolve`] (the legacy `type` stays limited to the v0.3 values).
    #[serde(rename = "opencode")]
    OpenCode,
    /// Only reachable through `harness = "codex"`; `type = "codex"` is rejected by
    /// [`resolve`] like `type = "opencode"`.
    Codex,
}

impl BackendType {
    pub fn as_str(self) -> &'static str {
        match self {
            BackendType::ClaudeOauth => "claude-oauth",
            BackendType::ClaudeEndpoint => "claude-endpoint",
            BackendType::Command => "command",
            BackendType::OpenCode => "opencode",
            BackendType::Codex => "codex",
        }
    }
}

/// How a codex backend authenticates (`auth = "chatgpt" | "api-key"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodexAuth {
    /// The user's ChatGPT login (subscription), done once with `codex login` in the
    /// backend's own `CODEX_HOME`; Codex keeps and refreshes it in `<CODEX_HOME>/auth.json`.
    Chatgpt,
    /// An OpenAI API key (or the key of a Responses-compatible gateway with `base_url`),
    /// stored with `setup --backend` or read from `api_key_env`.
    ApiKey,
}

impl CodexAuth {
    pub fn as_str(self) -> &'static str {
        match self {
            CodexAuth::Chatgpt => "chatgpt",
            CodexAuth::ApiKey => "api-key",
        }
    }
}

impl fmt::Display for CodexAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Codex's Windows sandbox implementation (`windows_sandbox = "mxc" | "elevated" |
/// "unelevated"`), passed as `-c windows.sandbox=<value>`. Ignored on other platforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WindowsSandbox {
    /// Windows 11 24H2/25H2 with the MXC capability (recommended by OpenAI).
    #[default]
    Mxc,
    /// Needs a one-time admin setup (`codex sandbox` as administrator).
    Elevated,
    /// Weaker isolation, no admin setup.
    Unelevated,
}

impl WindowsSandbox {
    pub fn as_str(self) -> &'static str {
        match self {
            WindowsSandbox::Mxc => "mxc",
            WindowsSandbox::Elevated => "elevated",
            WindowsSandbox::Unelevated => "unelevated",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    #[default]
    Default,
    AcceptEdits,
    Auto,
    DontAsk,
    Plan,
    BypassPermissions,
}

impl PermissionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionMode::Default => "default",
            PermissionMode::AcceptEdits => "acceptEdits",
            PermissionMode::Auto => "auto",
            PermissionMode::DontAsk => "dontAsk",
            PermissionMode::Plan => "plan",
            PermissionMode::BypassPermissions => "bypassPermissions",
        }
    }
}

impl fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

pub const BYPASS_WARNING: &str = "permission_mode = \"bypassPermissions\": the agent runs every \
     tool without asking (only disallowed_tools still apply); prefer \"acceptEdits\" with \
     allowed_tools";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudePermissions {
    pub mode: PermissionMode,
    pub allowed_tools: Option<Vec<String>>,
    pub disallowed_tools: Vec<String>,
}

impl ClaudePermissions {
    pub fn effective_allowed<'a>(
        &'a self,
        requested: Option<&'a [String]>,
    ) -> Option<&'a [String]> {
        requested
            .filter(|t| !t.is_empty())
            .or(self.allowed_tools.as_deref())
            .filter(|t| !t.is_empty())
    }

    pub fn header_lines(&self, requested: Option<&[String]>) -> Vec<String> {
        let allowed = match (requested.filter(|t| !t.is_empty()), &self.allowed_tools) {
            (Some(t), _) => format!("{} (request)", t.join(" ")),
            (None, Some(t)) if !t.is_empty() => format!("{} (backend default)", t.join(" ")),
            _ => "(none: read-only tools)".to_owned(),
        };
        let disallowed = if self.disallowed_tools.is_empty() {
            "(none)".to_owned()
        } else {
            self.disallowed_tools.join(" ")
        };
        vec![
            format!("permission_mode: {}", self.mode),
            format!("allowed_tools: {allowed}"),
            format!("disallowed_tools: {disallowed}"),
        ]
    }
}

/// The `auth` value as written in config.toml: claude-endpoint uses `x-api-key`/`bearer`,
/// codex `chatgpt`/`api-key`; [`resolve`] checks which values a backend may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RawAuth {
    XApiKey,
    Bearer,
    Chatgpt,
    ApiKey,
}

impl RawAuth {
    pub fn as_str(self) -> &'static str {
        match self {
            RawAuth::XApiKey => "x-api-key",
            RawAuth::Bearer => "bearer",
            RawAuth::Chatgpt => "chatgpt",
            RawAuth::ApiKey => "api-key",
        }
    }
}

/// The model provider of an opencode backend (`provider = "..."`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OpencodeProvider {
    /// Any OpenAI-compatible chat completions endpoint (`base_url` ends before
    /// `/chat/completions`, e.g. `https://chat.kiconnect.nrw/api/v1`); needs a key.
    OpenaiCompatible,
    /// A local or remote Ollama server (`base_url` like `http://localhost:11434`, `/v1` is
    /// appended); no key.
    Ollama,
    /// OpenRouter (OpenCode's built-in provider); needs a key, no `base_url`.
    Openrouter,
}

impl OpencodeProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            OpencodeProvider::OpenaiCompatible => "openai-compatible",
            OpencodeProvider::Ollama => "ollama",
            OpencodeProvider::Openrouter => "openrouter",
        }
    }

    /// Whether the provider needs an API key (stored with `setup --backend` or from
    /// `api_key_env`).
    pub fn needs_key(self) -> bool {
        !matches!(self, OpencodeProvider::Ollama)
    }
}

impl fmt::Display for OpencodeProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
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
/// here; [`resolve`] checks which ones are required/allowed for the harness/type.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawBackend {
    /// The agent CLI that runs the backend (`claude`, `opencode` or `command`). `claude`
    /// without `base_url` is the claude-oauth mode, with `base_url` the claude-endpoint mode.
    pub harness: Option<crate::harness::HarnessKind>,
    /// Legacy (v0.2/v0.3) form of `harness`: `claude-oauth`, `claude-endpoint` or `command`.
    /// Still accepted; together with `harness` both must agree.
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
    /// claude-endpoint: e.g. `http://localhost:11434`; opencode: the provider's base URL
    /// (openai-compatible, ollama).
    pub base_url: Option<String>,
    /// claude-endpoint, opencode: name of an environment variable holding the key (instead
    /// of the stored one).
    pub api_key_env: Option<String>,
    /// claude-endpoint: how the key is sent (`x-api-key`, `bearer`); codex: how the backend
    /// authenticates (`chatgpt`, `api-key`).
    pub auth: Option<RawAuth>,
    /// command: program and arguments; placeholders `{prompt}`, `{cwd}`, `{max_turns}`,
    /// `{model}`.
    pub command: Option<Vec<String>>,
    /// command: send the prompt on stdin instead of via `{prompt}`.
    pub prompt_via_stdin: Option<bool>,
    /// command: name of the environment variable that receives the backend's stored secret
    /// (keyring user `cmd-<name>`, stored with `setup --backend <name>`).
    pub secret_env: Option<String>,
    /// opencode: `openai-compatible`, `ollama` or `openrouter`.
    pub provider: Option<OpencodeProvider>,
    /// opencode, codex: explicit path of the harness executable (`~` is expanded).
    pub harness_path: Option<PathBuf>,
    /// opencode: deny the edit, bash and webfetch tools; codex: run in Codex's read-only
    /// sandbox (default false).
    pub read_only: Option<bool>,
    /// opencode: seconds to wait for the provider's response headers before OpenCode retries
    /// the request (OpenCode's `headerTimeout`; unset: OpenCode's default of 300 s).
    pub request_timeout_s: Option<u64>,
    pub model_options: Option<serde_json::Map<String, serde_json::Value>>,
    /// codex (api-key with `base_url`): the provider's wire API. Only `responses` exists
    /// (Codex 0.163 dropped chat completions); any other value is a config error.
    pub wire_api: Option<String>,
    /// codex: Windows sandbox implementation (`mxc`, `elevated`, `unelevated`; default mxc).
    pub windows_sandbox: Option<WindowsSandbox>,
    pub permission_mode: Option<PermissionMode>,
    pub allowed_tools: Option<Vec<String>>,
    pub disallowed_tools: Option<Vec<String>>,
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
    /// True for the implicit `claude` backend created when `[backends]` is absent.
    pub implicit: bool,
    pub kind: BackendKind,
    /// Names of every configured `api_key_env` and `secret_env` (of all backends, this one
    /// included). Removed from this backend's inherited child environment; `pass_env` cannot
    /// re-allow them. Filled by [`Backends::new`]; the endpoint that owns a key gets it through
    /// `ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN` (opencode: `AGENT_SIDEKICK_PROVIDER_KEY`), a
    /// command backend whose `secret_env` or `env` table names the same variable still gets
    /// that explicit value.
    pub foreign_secret_env: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendKind {
    ClaudeOauth {
        config_dir: PathBuf,
        permissions: ClaudePermissions,
    },
    ClaudeEndpoint {
        base_url: String,
        api_key_env: Option<String>,
        auth: EndpointAuth,
        config_dir: PathBuf,
        permissions: ClaudePermissions,
    },
    Command {
        command: Vec<String>,
        prompt_via_stdin: bool,
        /// Variable that receives the stored secret ([`crate::token::SecretId::Command`]).
        secret_env: Option<String>,
    },
    OpenCode {
        provider: OpencodeProvider,
        /// Normalized `base_url` (no trailing `/`); `None` for openrouter.
        base_url: Option<String>,
        /// Key from this environment variable instead of the stored secret.
        api_key_env: Option<String>,
        /// Configured opencode executable (`None`: looked up, see
        /// [`crate::harness::opencode`]).
        harness_path: Option<PathBuf>,
        read_only: bool,
        /// Header timeout in seconds (OpenCode provider option `headerTimeout`, retried on
        /// expiry); `None`: OpenCode's default.
        request_timeout_s: Option<u64>,
        model_options: serde_json::Map<String, serde_json::Value>,
        /// Per-backend isolation directory `<config-dir>/opencode/<name>`.
        base_dir: PathBuf,
    },
    Codex {
        auth: CodexAuth,
        /// api-key only: a Responses-compatible provider (normalized, no trailing `/`);
        /// `None`: OpenAI's own API through Codex's built-in `openai` provider.
        base_url: Option<String>,
        /// api-key only: key from this environment variable instead of the stored secret.
        api_key_env: Option<String>,
        /// Configured codex executable (`None`: looked up, see [`crate::harness::codex`]).
        harness_path: Option<PathBuf>,
        /// `--sandbox read-only` instead of `workspace-write`.
        read_only: bool,
        windows_sandbox: WindowsSandbox,
        /// The backend's own `CODEX_HOME`: `<config-dir>/codex/<name>`.
        base_dir: PathBuf,
    },
}

impl Backend {
    pub fn backend_type(&self) -> BackendType {
        match self.kind {
            BackendKind::ClaudeOauth { .. } => BackendType::ClaudeOauth,
            BackendKind::ClaudeEndpoint { .. } => BackendType::ClaudeEndpoint,
            BackendKind::Command { .. } => BackendType::Command,
            BackendKind::OpenCode { .. } => BackendType::OpenCode,
            BackendKind::Codex { .. } => BackendType::Codex,
        }
    }

    pub fn claude_permissions(&self) -> Option<&ClaudePermissions> {
        match &self.kind {
            BackendKind::ClaudeOauth { permissions, .. }
            | BackendKind::ClaudeEndpoint { permissions, .. } => Some(permissions),
            _ => None,
        }
    }

    /// Whether a run on this backend uses a model: claude-* always (`--model` or the
    /// account's/endpoint's default), opencode always (it always has a default model), a
    /// command backend only with `{model}` in its command.
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
    /// claude-endpoint/opencode with `api_key_env` read the environment instead (`None`); a
    /// command backend has one only with `secret_env`; an opencode backend with provider
    /// `ollama` needs none.
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
            BackendKind::OpenCode {
                provider,
                api_key_env: None,
                ..
            } if provider.needs_key() => Some(SecretId::Backend(self.name.clone())),
            BackendKind::Codex {
                auth: CodexAuth::ApiKey,
                api_key_env: None,
                ..
            } => Some(SecretId::Backend(self.name.clone())),
            _ => None,
        }
    }

    /// Whether a run needs a provider key ([`launch::Credential::ProviderKey`]): opencode
    /// with a key-based provider, codex with `auth = "api-key"`.
    pub fn needs_provider_key(&self) -> bool {
        match &self.kind {
            BackendKind::OpenCode { provider, .. } => provider.needs_key(),
            BackendKind::Codex { auth, .. } => *auth == CodexAuth::ApiKey,
            _ => false,
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
    /// and `secret_env` names.
    pub fn new(mut list: Vec<Backend>) -> Self {
        list.sort_by(|a, b| a.name.cmp(&b.name));
        let mut key_vars: Vec<String> = Vec::new();
        for b in &list {
            let var = match &b.kind {
                BackendKind::ClaudeEndpoint {
                    api_key_env: Some(var),
                    ..
                }
                | BackendKind::OpenCode {
                    api_key_env: Some(var),
                    ..
                }
                | BackendKind::Codex {
                    api_key_env: Some(var),
                    ..
                }
                | BackendKind::Command {
                    secret_env: Some(var),
                    ..
                } => var,
                _ => continue,
            };
            if !key_vars.contains(var) {
                key_vars.push(var.clone());
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
