//! Child environment, credentials and [`Launch`] building per backend type.
//!
//! Session isolation: the MCP server is usually started by a Claude Code session and
//! inherits its variables (`CLAUDECODE`, `CLAUDE_CODE_SESSION_ID`, the messaging socket and
//! token, ...). A child `claude` that sees them identifies as part of that session and, in
//! API-key mode, refuses to start ("Not logged in"). They are therefore removed for every
//! backend type, together with competing credentials. `pass_env` (global and per backend)
//! re-allows specific names, except the agent's own secrets.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use super::{Backend, BackendKind, BackendType};
use crate::redact::Secrets;
use crate::runner::{Launch, RunRequest};
use crate::token::{SecretId, Token, TokenError, TokenSource};

/// The credential a backend needs for one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// claude-oauth: becomes `CLAUDE_CODE_OAUTH_TOKEN`.
    OauthToken(Token),
    /// claude-endpoint: becomes `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN`.
    ApiKey(Token),
    /// command backend with `secret_env`: becomes that variable.
    CommandSecret(Token),
    /// command backends without `secret_env`.
    None,
}

impl Credential {
    /// The secret value, if any (for redaction; never print it).
    pub fn secret(&self) -> Option<&Token> {
        match self {
            Credential::OauthToken(t) | Credential::ApiKey(t) | Credential::CommandSecret(t) => {
                Some(t)
            }
            Credential::None => None,
        }
    }
}

/// Prefixes of inherited variables that are always removed (session isolation).
const REMOVED_PREFIXES: &[&str] = &["CLAUDE_CODE_", "CLAUDE_AGENT_SDK_"];

/// Exact names of inherited variables that are always removed.
const REMOVED_NAMES: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
    CONFIG_DIR_ENV,
    "AI_AGENT",
    "MCP_CONNECTION_NONBLOCKING",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    crate::token::ENV_TOKEN,
];

/// Exceptions to [`REMOVED_PREFIXES`]: host configuration, not session identity or a
/// credential. `CLAUDE_CODE_GIT_BASH_PATH` tells claude on Windows where bash.exe is; without
/// it a child claude may refuse to start ("requires git-bash").
const KEPT_PREFIXED_NAMES: &[&str] = &["CLAUDE_CODE_GIT_BASH_PATH"];

/// Removed for claude-endpoint and command backends: claude sends these headers with every
/// API request, so a caller's gateway credential would reach the backend's endpoint.
/// `pass_env` may re-allow it for command backends only.
const CUSTOM_HEADERS_ENV: &str = "ANTHROPIC_CUSTOM_HEADERS";

/// Removed for claude-endpoint backends (not re-allowable via `pass_env`): model overrides
/// of the caller's session that may not exist on the backend's endpoint. The backend's own
/// `env` table may still set them explicitly.
const ENDPOINT_MODEL_VARS: &[&str] = &[
    "ANTHROPIC_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
];

/// Never kept from the inherited environment, whatever `pass_env` says.
const NEVER_PASSED: &[&str] = &[OAUTH_TOKEN_ENV, crate::token::ENV_TOKEN];

/// For claude-* backends these decide which account/endpoint claude uses; an inherited
/// value is never kept via `pass_env` (it would outrank or redirect the backend's own
/// credential, i.e. the caller's account could be used instead).
const CLAUDE_CREDENTIAL_VARS: &[&str] =
    &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", CONFIG_DIR_ENV];

const OAUTH_TOKEN_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";
const CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";
const BASE_URL_ENV: &str = "ANTHROPIC_BASE_URL";

/// Same text as the v0.1 runner warning.
const BASE_URL_WARNING: &str = "warning: ANTHROPIC_BASE_URL is inherited from the server environment; claude sends its requests (with the agent token) to that endpoint";

/// Which request parameters a command backend ignores: `allowed_tools` always, `model` when
/// its command has no `{model}`. `None` for claude-* backends or when none of them was set.
/// Returns `"model/allowed_tools"`, `"model"` or `"allowed_tools"`.
pub fn command_ignored(backend: &Backend, req: &RunRequest) -> Option<&'static str> {
    if !matches!(backend.kind, BackendKind::Command { .. }) {
        return None;
    }
    let model = !backend.takes_model() && non_blank(req.model.as_deref()).is_some();
    let tools = req
        .allowed_tools
        .iter()
        .flatten()
        .any(|t| !t.trim().is_empty());
    match (model, tools) {
        (true, true) => Some("model/allowed_tools"),
        (true, false) => Some("model"),
        (false, true) => Some("allowed_tools"),
        (false, false) => None,
    }
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

/// Load the credential for `backend`. `Ok((cred, source))` where `source` describes where
/// it came from (never the value). The error is a user-facing message with a next step.
pub fn load_credential(backend: &Backend) -> Result<(Credential, String), String> {
    load_credential_with(
        backend,
        &|name| std::env::var(name).ok(),
        &crate::token::load_secret,
    )
}

type SecretLoader<'a> = &'a dyn Fn(&SecretId) -> Result<(Token, TokenSource), TokenError>;

/// Testable core of [`load_credential`]: environment and secret store are injected.
fn load_credential_with(
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
        BackendKind::Command {
            secret_env: Some(var),
            ..
        } => {
            let id = SecretId::Command(name.clone());
            match secret_loader(&id) {
                Ok((secret, source)) => Ok((Credential::CommandSecret(secret), source.to_string())),
                Err(TokenError::Missing) | Err(TokenError::Empty) => Err(format!(
                    "No secret stored for backend '{name}' (secret_env {var}). Run \
                     'agent-sidekick setup --backend {name}' in a terminal."
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
        BackendKind::Command {
            secret_env: None, ..
        } => Ok((Credential::None, "none needed".to_owned())),
    }
}

fn missing_oauth_message(name: &str) -> String {
    if name == super::IMPLICIT_BACKEND {
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

// ---------------------------------------------------------------------------
// Program
// ---------------------------------------------------------------------------

/// The executable for `backend`: claude (via `locate::find_claude(claude_path)`) for
/// claude-* types, `command[0]` resolved via PATH (or as given if absolute) for `command`.
pub fn resolve_program(backend: &Backend, claude_path: Option<&Path>) -> Result<PathBuf, String> {
    match &backend.kind {
        BackendKind::ClaudeOauth { .. } | BackendKind::ClaudeEndpoint { .. } => {
            crate::locate::find_claude(claude_path)
                .map(|l| l.path)
                .map_err(|e| e.to_string())
        }
        BackendKind::Command { command, .. } => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            resolve_command_program(
                &backend.name,
                command.first().map(String::as_str).unwrap_or(""),
                std::env::var_os("PATH"),
                &cwd,
            )
        }
    }
}

/// `program` absolute -> must be an existing file; relative with a separator -> error
/// (it would depend on the server's working directory); bare name -> PATH search
/// (`which`; on Windows PATHEXT applies, so `aider` finds `aider.exe`/`aider.cmd`).
/// `command[0]` of a command backend. Absolute: must be an existing file. Bare name: looked
/// up in the absolute entries of `path_var` only — empty or relative PATH entries are
/// ignored so a program is never picked up from the server's working directory (on Unix,
/// `which` would otherwise resolve an empty entry against `cwd`). Relative paths with a
/// separator are rejected for the same reason.
pub(crate) fn resolve_command_program(
    backend: &str,
    program: &str,
    path_var: Option<OsString>,
    cwd: &Path,
) -> Result<PathBuf, String> {
    if program.trim().is_empty() {
        return Err(format!("backend {backend}: command[0] is empty"));
    }
    let p = Path::new(program);
    if p.is_absolute() {
        if !p.is_file() {
            return Err(format!(
                "backend {backend}: program '{program}' does not exist or is not a file"
            ));
        }
        if !is_executable(p) {
            return Err(format!(
                "backend {backend}: program '{program}' is not executable (chmod +x)"
            ));
        }
        return Ok(p.to_path_buf());
    }
    if p.components().count() != 1
        || program.contains('/')
        || program.contains('\\') && cfg!(windows)
    {
        return Err(format!(
            "backend {backend}: program '{program}' is a relative path; use an absolute path or \
             a bare program name that is found in PATH"
        ));
    }
    let not_found = || format!("backend {backend}: program '{program}' not found in PATH");
    let dirs: Vec<PathBuf> = path_var
        .as_deref()
        .map(|v| {
            std::env::split_paths(v)
                .filter(|d| !d.as_os_str().is_empty() && d.is_absolute())
                .collect()
        })
        .unwrap_or_default();
    if dirs.is_empty() {
        return Err(not_found());
    }
    let joined = std::env::join_paths(dirs).map_err(|_| not_found())?;
    which::which_in(program, Some(joined), cwd).map_err(|_| not_found())
}

/// Unix: any exec bit set (metadata follows symlinks). Elsewhere: always true.
#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_p: &Path) -> bool {
    true
}

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

/// Environment variable names are case-insensitive on Windows.
fn eq_name(a: &OsStr, b: &str) -> bool {
    if cfg!(windows) {
        a.to_string_lossy().eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

fn has_prefix(name: &OsStr, prefix: &str) -> bool {
    let name = name.to_string_lossy();
    if cfg!(windows) {
        name.to_ascii_uppercase().starts_with(prefix)
    } else {
        name.starts_with(prefix)
    }
}

fn in_list(name: &OsStr, list: &[&str]) -> bool {
    list.iter().any(|n| eq_name(name, n))
}

/// Is `name` one of the `api_key_env` variables of the configured backends?
fn is_key_var(name: &OsStr, backend: &Backend) -> bool {
    backend.foreign_secret_env.iter().any(|v| eq_name(name, v))
}

/// Removed from the inherited environment for this backend (before `pass_env`).
fn is_removed(name: &OsStr, backend: &Backend) -> bool {
    let ty = backend.backend_type();
    (REMOVED_PREFIXES.iter().any(|p| has_prefix(name, p)) && !in_list(name, KEPT_PREFIXED_NAMES))
        || in_list(name, REMOVED_NAMES)
        || is_key_var(name, backend)
        || (ty != BackendType::ClaudeOauth
            && (eq_name(name, BASE_URL_ENV) || eq_name(name, CUSTOM_HEADERS_ENV)))
        || (ty == BackendType::ClaudeEndpoint && in_list(name, ENDPOINT_MODEL_VARS))
}

/// May an inherited variable that matches the removal rules be kept via `pass_env`?
fn may_pass(name: &OsStr, backend: &Backend) -> bool {
    if in_list(name, NEVER_PASSED) || is_key_var(name, backend) {
        return false;
    }
    match backend.backend_type() {
        BackendType::Command => true,
        BackendType::ClaudeOauth => !in_list(name, CLAUDE_CREDENTIAL_VARS),
        BackendType::ClaudeEndpoint => {
            !in_list(name, CLAUDE_CREDENTIAL_VARS)
                && !eq_name(name, BASE_URL_ENV)
                && !eq_name(name, CUSTOM_HEADERS_ENV)
                && !in_list(name, ENDPOINT_MODEL_VARS)
        }
    }
}

/// Set `name=value`, replacing every existing entry with the same name.
fn set_var(env: &mut Vec<(OsString, OsString)>, name: &str, value: impl Into<OsString>) {
    env.retain(|(k, _)| !eq_name(k, name));
    env.push((name.into(), value.into()));
}

/// Child environment for `backend`: `base` minus the removed variables (session
/// isolation, competing credentials), plus `pass_env` exceptions (global + per backend),
/// the backend's `env` table, and the type-specific variables (see the design doc). For a
/// command backend with `secret_env`, that variable is set to the stored secret last, so it
/// replaces any inherited value of the same name.
pub fn child_env(
    backend: &Backend,
    base: impl IntoIterator<Item = (OsString, OsString)>,
    global_pass_env: &[String],
    credential: &Credential,
) -> Vec<(OsString, OsString)> {
    let (env, mismatch) = child_env_inner(backend, base, global_pass_env, credential);
    debug_assert!(
        !mismatch,
        "credential type does not match backend {} ({})",
        backend.name,
        backend.backend_type()
    );
    env
}

/// [`child_env`] plus whether `credential` did not match the backend type (in which case
/// no secret was put into the environment).
fn child_env_inner(
    backend: &Backend,
    base: impl IntoIterator<Item = (OsString, OsString)>,
    global_pass_env: &[String],
    credential: &Credential,
) -> (Vec<(OsString, OsString)>, bool) {
    let passed = |k: &OsStr| {
        global_pass_env
            .iter()
            .chain(backend.pass_env.iter())
            .any(|p| eq_name(k, p))
    };
    let mut env: Vec<(OsString, OsString)> = base
        .into_iter()
        .filter(|(k, _)| !is_removed(k, backend) || (passed(k) && may_pass(k, backend)))
        .collect();

    for (k, v) in &backend.env {
        set_var(&mut env, k, v);
    }

    let mismatch = match (&backend.kind, credential) {
        (BackendKind::ClaudeOauth { config_dir }, cred) => {
            set_var(&mut env, CONFIG_DIR_ENV, config_dir.as_os_str());
            match cred {
                Credential::OauthToken(t) => {
                    set_var(&mut env, OAUTH_TOKEN_ENV, t.expose());
                    false
                }
                _ => true,
            }
        }
        (
            BackendKind::ClaudeEndpoint {
                base_url,
                auth,
                config_dir,
                ..
            },
            cred,
        ) => {
            set_var(&mut env, CONFIG_DIR_ENV, config_dir.as_os_str());
            set_var(&mut env, BASE_URL_ENV, base_url.as_str());
            let mismatch = match cred {
                Credential::ApiKey(k) => {
                    set_var(&mut env, auth.env_var(), k.expose());
                    false
                }
                _ => true,
            };
            // Defence in depth: never an OAuth token for an endpoint.
            env.retain(|(k, _)| !eq_name(k, OAUTH_TOKEN_ENV));
            mismatch
        }
        (BackendKind::Command { secret_env, .. }, cred) => match (secret_env, cred) {
            (Some(var), Credential::CommandSecret(t)) => {
                set_var(&mut env, var, t.expose());
                false
            }
            (None, Credential::None) => false,
            _ => true,
        },
    };
    (env, mismatch)
}

// ---------------------------------------------------------------------------
// Placeholders
// ---------------------------------------------------------------------------

/// Replace `{prompt}`, `{cwd}`, `{max_turns}` and `{model}` in one template element in a
/// single pass: replacement text is never scanned again, unknown `{x}` stays literal (so
/// does `{model}` when `model` is `None`).
pub fn substitute(
    template: &str,
    prompt: &str,
    cwd: &Path,
    max_turns: u32,
    model: Option<&str>,
) -> String {
    let mut out = String::with_capacity(template.len() + prompt.len());
    let mut rest = template;
    while let Some(idx) = rest.find('{') {
        out.push_str(&rest[..idx]);
        let tail = &rest[idx..];
        if let Some(after) = tail.strip_prefix("{prompt}") {
            out.push_str(prompt);
            rest = after;
        } else if let Some(after) = tail.strip_prefix("{cwd}") {
            out.push_str(&cwd.to_string_lossy());
            rest = after;
        } else if let Some(after) = tail.strip_prefix("{max_turns}") {
            out.push_str(&max_turns.to_string());
            rest = after;
        } else if let (Some(m), Some(after)) = (model, tail.strip_prefix("{model}")) {
            out.push_str(m);
            rest = after;
        } else {
            out.push('{');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------
// Launch
// ---------------------------------------------------------------------------

/// Build the [`Launch`] for one run of `backend` with the inherited environment `base`.
pub fn build_launch(
    backend: &Backend,
    req: &RunRequest,
    credential: &Credential,
    program: &Path,
    base: impl IntoIterator<Item = (OsString, OsString)>,
    global_pass_env: &[String],
) -> Result<Launch, String> {
    let name = &backend.name;
    let ty = backend.backend_type();
    let command_secret = match &backend.kind {
        BackendKind::Command { secret_env, .. } => secret_env.as_deref(),
        _ => None,
    };
    let credential_ok = matches!(
        (ty, command_secret, credential),
        (BackendType::ClaudeOauth, _, Credential::OauthToken(_))
            | (BackendType::ClaudeEndpoint, _, Credential::ApiKey(_))
            | (BackendType::Command, None, Credential::None)
            | (BackendType::Command, Some(_), Credential::CommandSecret(_))
    );
    if !credential_ok {
        return Err(format!(
            "backend {name}: internal error: credential type does not match backend type {ty}"
        ));
    }
    // The caller validates the model first; checked again here because it may land in argv.
    let model = backend
        .effective_model(req.model.as_deref())
        .map_err(|e| format!("backend {name}: {e}"))?;
    // Same form as the first line of the sidekick_run result.
    let mut header = vec![match &model {
        Some(m) => format!("backend: {name} ({ty}), model {m}"),
        None => format!("backend: {name} ({ty})"),
    }];

    match &backend.kind {
        BackendKind::ClaudeOauth { .. } | BackendKind::ClaudeEndpoint { .. } => {
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
                model: model.clone(),
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
        BackendKind::Command {
            command,
            prompt_via_stdin,
            secret_env,
        } => {
            if let Some(e) = batch_argument_error(name, program, *prompt_via_stdin) {
                return Err(e);
            }
            let env = child_env(backend, base, global_pass_env, credential);
            let args: Vec<OsString> = command
                .iter()
                .skip(1)
                .map(|a| {
                    substitute(a, &req.prompt, &req.cwd, req.max_turns, model.as_deref()).into()
                })
                .collect();
            let stdin = prompt_via_stdin.then(|| req.prompt.clone());
            header.push(format!("program: {}", program.display()));
            header.push(format!(
                "prompt delivery: {}",
                if *prompt_via_stdin {
                    "stdin"
                } else {
                    "argument"
                }
            ));
            if let Some(what) = command_ignored(backend, req) {
                header.push(format!("note: {what} ignored by this command backend"));
            }
            if let Some(var) = secret_env {
                // The name only; the value never goes into the header.
                header.push(format!(
                    "secret_env: {var} (stored secret, value not shown)"
                ));
            }
            // Secrets shorter than MIN_SECRET_LEN are dropped here (not redacted).
            let secrets = match credential.secret() {
                Some(t) => Secrets::new([t.expose()]),
                None => Secrets::none(),
            };
            Ok(Launch {
                program: program.to_path_buf(),
                args,
                env,
                stdin,
                secrets,
                header,
            })
        }
    }
}

/// A command backend whose program is a `.cmd`/`.bat` script cannot take the prompt as an
/// argument (cmd.exe re-parses it). Shared by [`build_launch`] and the reachability check.
pub(crate) fn batch_argument_error(
    backend: &str,
    program: &Path,
    prompt_via_stdin: bool,
) -> Option<String> {
    (crate::locate::is_batch_script(program) && !prompt_via_stdin).then(|| {
        format!(
            "backend {backend}: command[0] is a .cmd/.bat script; cmd.exe cannot pass \
             arbitrary prompt text safely. Use prompt_via_stdin = true or point \
             command[0] at an .exe"
        )
    })
}

fn non_blank(s: Option<&str>) -> Option<&str> {
    s.filter(|m| !m.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::{Cost, EndpointAuth};
    use std::collections::BTreeMap;
    use std::time::Duration;

    const OAUTH_TOKEN: &str = "sk-ant-oat01-agent-token-value";
    const KEY_A: &str = "endpoint-key-A-1234567890";

    fn backend(name: &str, kind: BackendKind) -> Backend {
        Backend {
            name: name.to_owned(),
            description: "test".to_owned(),
            cost: Cost::Free,
            is_default: false,
            max_concurrent: None,
            model: None,
            models: None,
            env: BTreeMap::new(),
            pass_env: Vec::new(),
            implicit: false,
            kind,
            foreign_secret_env: Vec::new(),
        }
    }

    fn oauth(name: &str) -> Backend {
        backend(
            name,
            BackendKind::ClaudeOauth {
                config_dir: PathBuf::from("/cfg/oauth"),
            },
        )
    }

    fn endpoint(name: &str, auth: EndpointAuth) -> Backend {
        backend(
            name,
            BackendKind::ClaudeEndpoint {
                base_url: "http://localhost:11434".to_owned(),
                api_key_env: None,
                auth,
                config_dir: PathBuf::from("/cfg/endpoint"),
            },
        )
    }

    fn command(name: &str, cmd: &[&str], stdin: bool) -> Backend {
        backend(
            name,
            BackendKind::Command {
                command: cmd.iter().map(|s| s.to_string()).collect(),
                prompt_via_stdin: stdin,
                secret_env: None,
            },
        )
    }

    /// A command backend that gets its stored secret in `var`.
    fn command_with_secret(name: &str, var: &str) -> Backend {
        backend(
            name,
            BackendKind::Command {
                command: vec!["aider".into(), "{prompt}".into()],
                prompt_via_stdin: false,
                secret_env: Some(var.to_owned()),
            },
        )
    }

    const CMD_SECRET_A: &str = "cmd-secret-A-1234567890";
    const CMD_SECRET_B: &str = "cmd-secret-B-0987654321";

    fn cmd_cred(v: &str) -> Credential {
        Credential::CommandSecret(Token::new(v))
    }

    fn oauth_cred() -> Credential {
        Credential::OauthToken(Token::new(OAUTH_TOKEN))
    }

    fn key_cred(k: &str) -> Credential {
        Credential::ApiKey(Token::new(k))
    }

    fn base(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    /// The inherited environment of an MCP server started by a Claude Code VS Code session.
    fn session_base() -> Vec<(OsString, OsString)> {
        base(&[
            ("PATH", "/usr/bin"),
            ("HOME", "/home/u"),
            ("CLAUDECODE", "1"),
            ("CLAUDE_CODE_ENTRYPOINT", "claude-vscode"),
            ("CLAUDE_CODE_SESSION_ID", "sess"),
            ("CLAUDE_CODE_CHILD_SESSION", "1"),
            ("CLAUDE_CODE_SESSION_ATTENDED", "1"),
            ("CLAUDE_CODE_MESSAGING_SOCKET", "/tmp/sock"),
            ("CLAUDE_CODE_MESSAGING_TOKEN", "messaging-secret"),
            ("CLAUDE_CODE_EXECPATH", "/x"),
            ("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "64000"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "sk-ant-oat01-caller-token"),
            ("CLAUDE_PID", "42"),
            ("CLAUDE_AGENT_SDK_VERSION", "1.0"),
            ("CLAUDE_EFFORT", "high"),
            ("CLAUDE_CONFIG_DIR", "/home/u/.claude"),
            ("AI_AGENT", "claude"),
            ("MCP_CONNECTION_NONBLOCKING", "1"),
            ("ANTHROPIC_API_KEY", "caller-api-key-123456"),
            ("ANTHROPIC_AUTH_TOKEN", "caller-auth-token-123456"),
            ("ANTHROPIC_BASE_URL", "https://proxy.example"),
            ("AGENT_SIDEKICK_TOKEN", "sk-ant-oat01-env-token"),
            ("OPENAI_API_BASE", "http://localhost:1234"),
        ])
    }

    fn get<'a>(env: &'a [(OsString, OsString)], name: &str) -> Option<&'a OsStr> {
        env.iter()
            .find(|(k, _)| k.as_os_str() == OsStr::new(name))
            .map(|(_, v)| v.as_os_str())
    }

    fn has(env: &[(OsString, OsString)], name: &str) -> bool {
        env.iter().any(|(k, _)| eq_name(k, name))
    }

    fn count(env: &[(OsString, OsString)], name: &str) -> usize {
        env.iter().filter(|(k, _)| eq_name(k, name)).count()
    }

    fn contains_value(env: &[(OsString, OsString)], value: &str) -> bool {
        env.iter().any(|(_, v)| v.to_string_lossy().contains(value))
    }

    const SESSION_VARS: &[&str] = &[
        "CLAUDECODE",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_SESSION_ATTENDED",
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "CLAUDE_CODE_MESSAGING_TOKEN",
        "CLAUDE_CODE_EXECPATH",
        "CLAUDE_PID",
        "CLAUDE_AGENT_SDK_VERSION",
        "CLAUDE_EFFORT",
        "AI_AGENT",
        "MCP_CONNECTION_NONBLOCKING",
        "AGENT_SIDEKICK_TOKEN",
    ];

    fn req(prompt: &str) -> RunRequest {
        RunRequest {
            prompt: prompt.to_owned(),
            cwd: PathBuf::from("/work/dir"),
            max_turns: 7,
            model: None,
            allowed_tools: None,
            timeout: Duration::from_secs(10),
        }
    }

    // ---- child_env ---------------------------------------------------------

    #[test]
    fn oauth_env() {
        let env = child_env(&oauth("sidekick"), session_base(), &[], &oauth_cred());
        assert_eq!(
            get(&env, "CLAUDE_CODE_OAUTH_TOKEN"),
            Some(OsStr::new(OAUTH_TOKEN))
        );
        assert_eq!(
            get(&env, "CLAUDE_CONFIG_DIR"),
            Some(OsStr::new("/cfg/oauth"))
        );
        assert_eq!(count(&env, "CLAUDE_CODE_OAUTH_TOKEN"), 1);
        assert_eq!(count(&env, "CLAUDE_CONFIG_DIR"), 1);
        assert!(!has(&env, "ANTHROPIC_API_KEY"));
        assert!(!has(&env, "ANTHROPIC_AUTH_TOKEN"));
        // v0.1 behaviour: an inherited base URL is kept for claude-oauth (with a warning).
        assert_eq!(
            get(&env, "ANTHROPIC_BASE_URL"),
            Some(OsStr::new("https://proxy.example"))
        );
        assert!(!contains_value(&env, "caller"));
        assert_eq!(get(&env, "PATH"), Some(OsStr::new("/usr/bin")));
        assert_eq!(get(&env, "HOME"), Some(OsStr::new("/home/u")));
    }

    #[test]
    fn endpoint_env_x_api_key() {
        let env = child_env(
            &endpoint("local", EndpointAuth::XApiKey),
            session_base(),
            &[],
            &key_cred(KEY_A),
        );
        assert_eq!(
            get(&env, "ANTHROPIC_BASE_URL"),
            Some(OsStr::new("http://localhost:11434"))
        );
        assert_eq!(get(&env, "ANTHROPIC_API_KEY"), Some(OsStr::new(KEY_A)));
        assert_eq!(
            get(&env, "CLAUDE_CONFIG_DIR"),
            Some(OsStr::new("/cfg/endpoint"))
        );
        assert_eq!(count(&env, "ANTHROPIC_BASE_URL"), 1);
        assert!(!has(&env, "ANTHROPIC_AUTH_TOKEN"));
        assert!(!has(&env, "CLAUDE_CODE_OAUTH_TOKEN"));
        assert!(!contains_value(&env, "caller"));
        assert!(!contains_value(&env, "sk-ant-"));
    }

    #[test]
    fn endpoint_env_bearer() {
        let env = child_env(
            &endpoint("cloud", EndpointAuth::Bearer),
            session_base(),
            &[],
            &key_cred(KEY_A),
        );
        assert_eq!(get(&env, "ANTHROPIC_AUTH_TOKEN"), Some(OsStr::new(KEY_A)));
        assert!(!has(&env, "ANTHROPIC_API_KEY"));
        assert!(!has(&env, "CLAUDE_CODE_OAUTH_TOKEN"));
    }

    #[test]
    fn endpoint_never_gets_oauth_token_even_via_pass_env() {
        let mut b = endpoint("local", EndpointAuth::XApiKey);
        b.pass_env = vec![
            "CLAUDE_CODE_OAUTH_TOKEN".into(),
            "AGENT_SIDEKICK_TOKEN".into(),
        ];
        let global = vec!["CLAUDE_CODE_OAUTH_TOKEN".to_owned()];
        let env = child_env(&b, session_base(), &global, &key_cred(KEY_A));
        assert!(!has(&env, "CLAUDE_CODE_OAUTH_TOKEN"));
        assert!(!has(&env, "AGENT_SIDEKICK_TOKEN"));
        assert!(!contains_value(&env, "sk-ant-"));
    }

    #[test]
    fn mismatched_credential_puts_no_secret_anywhere() {
        // OAuth token handed to an endpoint backend by mistake.
        let (env, mismatch) = child_env_inner(
            &endpoint("local", EndpointAuth::XApiKey),
            session_base(),
            &[],
            &oauth_cred(),
        );
        assert!(mismatch);
        assert!(!has(&env, "CLAUDE_CODE_OAUTH_TOKEN"));
        assert!(!has(&env, "ANTHROPIC_API_KEY"));
        assert!(!contains_value(&env, OAUTH_TOKEN));

        // API key handed to an oauth backend.
        let (env, mismatch) =
            child_env_inner(&oauth("sidekick"), session_base(), &[], &key_cred(KEY_A));
        assert!(mismatch);
        assert!(!has(&env, "CLAUDE_CODE_OAUTH_TOKEN"));
        assert!(!has(&env, "ANTHROPIC_API_KEY"));
        assert!(!contains_value(&env, KEY_A));

        // Any secret handed to a command backend.
        for cred in [oauth_cred(), key_cred(KEY_A)] {
            let (env, mismatch) =
                child_env_inner(&command("c", &["x"], true), session_base(), &[], &cred);
            assert!(mismatch);
            assert!(!contains_value(&env, OAUTH_TOKEN));
            assert!(!contains_value(&env, KEY_A));
        }

        // Matching credentials are not flagged.
        let (_, m) = child_env_inner(&oauth("s"), session_base(), &[], &oauth_cred());
        assert!(!m);
        let (_, m) = child_env_inner(
            &command("c", &["x"], true),
            session_base(),
            &[],
            &Credential::None,
        );
        assert!(!m);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "credential type does not match")]
    fn mismatched_credential_debug_asserts() {
        let _ = child_env(
            &endpoint("local", EndpointAuth::XApiKey),
            session_base(),
            &[],
            &oauth_cred(),
        );
    }

    #[test]
    fn no_key_leak_between_backends() {
        let shared = session_base();
        let a = child_env(
            &endpoint("a", EndpointAuth::XApiKey),
            shared.clone(),
            &[],
            &key_cred(KEY_A),
        );
        assert!(contains_value(&a, KEY_A));
        let o = child_env(&oauth("sidekick"), shared.clone(), &[], &oauth_cred());
        let c = child_env(
            &command("aider", &["aider", "{prompt}"], false),
            shared.clone(),
            &[],
            &Credential::None,
        );
        assert!(!contains_value(&o, KEY_A));
        assert!(!contains_value(&c, KEY_A));
        assert!(!has(&o, "ANTHROPIC_API_KEY"));
        assert!(!has(&c, "ANTHROPIC_API_KEY"));
        // The oauth token of the agent does not reach the command backend either.
        assert!(!contains_value(&c, OAUTH_TOKEN));
        // Building another endpoint with key B from the same base does not contain key A.
        let b = child_env(
            &endpoint("b", EndpointAuth::Bearer),
            shared,
            &[],
            &key_cred("endpoint-key-B-0987654321"),
        );
        assert!(!contains_value(&b, KEY_A));
    }

    #[test]
    fn session_vars_removed_for_all_types() {
        let envs = [
            child_env(&oauth("s"), session_base(), &[], &oauth_cred()),
            child_env(
                &endpoint("e", EndpointAuth::XApiKey),
                session_base(),
                &[],
                &key_cred(KEY_A),
            ),
            child_env(
                &command("c", &["x", "{prompt}"], false),
                session_base(),
                &[],
                &Credential::None,
            ),
        ];
        for env in &envs {
            for v in SESSION_VARS {
                assert!(!has(env, v), "{v} not removed");
            }
            assert!(!has(env, "CLAUDE_CODE_MAX_OUTPUT_TOKENS"));
            assert!(!contains_value(env, "messaging-secret"));
            assert!(!contains_value(env, "/home/u/.claude"));
        }
        // command: no claude variables at all, no base URL, no inherited keys.
        let cmd = &envs[2];
        for v in [
            "CLAUDE_CONFIG_DIR",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
        ] {
            assert!(!has(cmd, v), "{v} present for command");
        }
        assert_eq!(
            get(cmd, "OPENAI_API_BASE"),
            Some(OsStr::new("http://localhost:1234"))
        );
    }

    #[test]
    fn pass_env_keeps_listed_names() {
        let global = vec!["CLAUDE_CODE_MAX_OUTPUT_TOKENS".to_owned()];
        let env = child_env(&oauth("s"), session_base(), &global, &oauth_cred());
        assert_eq!(
            get(&env, "CLAUDE_CODE_MAX_OUTPUT_TOKENS"),
            Some(OsStr::new("64000"))
        );
        assert!(!has(&env, "CLAUDE_CODE_SESSION_ID"));

        let mut b = command("c", &["x", "{prompt}"], false);
        b.pass_env = vec!["AI_AGENT".into(), "ANTHROPIC_API_KEY".into()];
        let env = child_env(&b, session_base(), &[], &Credential::None);
        assert_eq!(get(&env, "AI_AGENT"), Some(OsStr::new("claude")));
        // command backends may opt in to an inherited key (e.g. aider with Anthropic).
        assert_eq!(
            get(&env, "ANTHROPIC_API_KEY"),
            Some(OsStr::new("caller-api-key-123456"))
        );
        assert!(!has(&env, "CLAUDE_CODE_MAX_OUTPUT_TOKENS"));
    }

    #[test]
    fn pass_env_cannot_reintroduce_competing_claude_credentials() {
        let names: Vec<String> = [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CONFIG_DIR",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "AGENT_SIDEKICK_TOKEN",
            "ANTHROPIC_BASE_URL",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let o = child_env(&oauth("s"), session_base(), &names, &oauth_cred());
        assert!(!has(&o, "ANTHROPIC_API_KEY"));
        assert!(!has(&o, "ANTHROPIC_AUTH_TOKEN"));
        assert_eq!(get(&o, "CLAUDE_CONFIG_DIR"), Some(OsStr::new("/cfg/oauth")));
        assert_eq!(
            get(&o, "CLAUDE_CODE_OAUTH_TOKEN"),
            Some(OsStr::new(OAUTH_TOKEN))
        );
        assert!(!contains_value(&o, "caller"));

        let e = child_env(
            &endpoint("e", EndpointAuth::XApiKey),
            session_base(),
            &names,
            &key_cred(KEY_A),
        );
        assert_eq!(count(&e, "ANTHROPIC_API_KEY"), 1);
        assert_eq!(get(&e, "ANTHROPIC_API_KEY"), Some(OsStr::new(KEY_A)));
        assert_eq!(count(&e, "ANTHROPIC_BASE_URL"), 1);
        assert!(!has(&e, "ANTHROPIC_AUTH_TOKEN"));
        assert!(!contains_value(&e, "caller"));
        assert!(!contains_value(&e, "proxy.example"));

        let mut c = command("c", &["x"], true);
        c.pass_env = names;
        let c = child_env(&c, session_base(), &[], &Credential::None);
        assert!(!has(&c, "CLAUDE_CODE_OAUTH_TOKEN"));
        assert!(!has(&c, "AGENT_SIDEKICK_TOKEN"));
        assert!(has(&c, "ANTHROPIC_BASE_URL"));
    }

    #[test]
    fn backend_env_applied_and_overrides() {
        let mut b = endpoint("e", EndpointAuth::XApiKey);
        b.env
            .insert("CLAUDE_CODE_ATTRIBUTION_HEADER".into(), "0".into());
        b.env.insert("HOME".into(), "/other".into());
        let env = child_env(&b, session_base(), &[], &key_cred(KEY_A));
        assert_eq!(
            get(&env, "CLAUDE_CODE_ATTRIBUTION_HEADER"),
            Some(OsStr::new("0"))
        );
        assert_eq!(get(&env, "HOME"), Some(OsStr::new("/other")));
        assert_eq!(count(&env, "HOME"), 1);

        // Type-specific variables win over a (normally rejected) reserved env entry.
        let mut b = oauth("s");
        b.env.insert("CLAUDE_CONFIG_DIR".into(), "/evil".into());
        b.env
            .insert("CLAUDE_CODE_OAUTH_TOKEN".into(), "bogus-token-value".into());
        let env = child_env(&b, session_base(), &[], &oauth_cred());
        assert_eq!(
            get(&env, "CLAUDE_CONFIG_DIR"),
            Some(OsStr::new("/cfg/oauth"))
        );
        assert_eq!(
            get(&env, "CLAUDE_CODE_OAUTH_TOKEN"),
            Some(OsStr::new(OAUTH_TOKEN))
        );
        assert_eq!(count(&env, "CLAUDE_CODE_OAUTH_TOKEN"), 1);

        let mut c = command("c", &["x"], true);
        c.env.insert("AIDER_MODEL".into(), "ollama/qwen".into());
        let env = child_env(&c, session_base(), &[], &Credential::None);
        assert_eq!(get(&env, "AIDER_MODEL"), Some(OsStr::new("ollama/qwen")));
    }

    #[test]
    fn endpoint_drops_oauth_token_from_backend_env() {
        let mut b = endpoint("e", EndpointAuth::XApiKey);
        b.env
            .insert("CLAUDE_CODE_OAUTH_TOKEN".into(), "bogus-token-value".into());
        let env = child_env(&b, session_base(), &[], &key_cred(KEY_A));
        assert!(!has(&env, "CLAUDE_CODE_OAUTH_TOKEN"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_names_case_insensitive() {
        let b = base(&[
            ("Path", "C:\\bin"),
            ("claudecode", "1"),
            ("Claude_Code_Session_Id", "x"),
            ("claude_agent_sdk_version", "1"),
            ("anthropic_api_key", "caller-api-key-123456"),
            ("Anthropic_Base_Url", "https://proxy.example"),
            ("claude_code_max_output_tokens", "64000"),
            ("claude_code_oauth_token", "sk-ant-oat01-caller"),
            ("Agent_Sidekick_Token", "sk-ant-oat01-env"),
        ]);
        let global = vec![
            "CLAUDE_CODE_MAX_OUTPUT_TOKENS".to_owned(),
            "Claude_Code_OAuth_Token".to_owned(),
            "agent_sidekick_token".to_owned(),
        ];
        let env = child_env(
            &endpoint("e", EndpointAuth::XApiKey),
            b.clone(),
            &global,
            &key_cred(KEY_A),
        );
        assert_eq!(count(&env, "CLAUDECODE"), 0);
        assert_eq!(count(&env, "CLAUDE_CODE_SESSION_ID"), 0);
        assert_eq!(count(&env, "CLAUDE_AGENT_SDK_VERSION"), 0);
        assert_eq!(count(&env, "CLAUDE_CODE_OAUTH_TOKEN"), 0);
        assert_eq!(count(&env, "AGENT_SIDEKICK_TOKEN"), 0);
        assert_eq!(count(&env, "ANTHROPIC_API_KEY"), 1);
        assert_eq!(count(&env, "ANTHROPIC_BASE_URL"), 1);
        assert_eq!(get(&env, "ANTHROPIC_API_KEY"), Some(OsStr::new(KEY_A)));
        assert_eq!(count(&env, "CLAUDE_CODE_MAX_OUTPUT_TOKENS"), 1);
        assert_eq!(get(&env, "Path"), Some(OsStr::new("C:\\bin")));

        // backend.env with different case replaces the inherited value.
        let mut c = command("c", &["x"], true);
        c.env.insert("PATH".into(), "D:\\bin".into());
        let env = child_env(&c, b, &[], &Credential::None);
        assert_eq!(count(&env, "PATH"), 1);
        assert_eq!(get(&env, "PATH"), Some(OsStr::new("D:\\bin")));
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_names_case_sensitive() {
        let b = base(&[
            ("claudecode", "1"),
            ("claude_code_session_id", "x"),
            ("CLAUDECODE", "1"),
        ]);
        let env = child_env(&command("c", &["x"], true), b, &[], &Credential::None);
        assert!(get(&env, "claudecode").is_some());
        assert!(get(&env, "claude_code_session_id").is_some());
        assert!(get(&env, "CLAUDECODE").is_none());
        // pass_env must match exactly.
        let b = base(&[("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "1")]);
        let env = child_env(
            &command("c", &["x"], true),
            b,
            &["claude_code_max_output_tokens".to_owned()],
            &Credential::None,
        );
        assert!(env.is_empty());
    }

    #[test]
    fn command_secret_env_is_set() {
        let mut base = session_base();
        base.push(("OPENAI_API_KEY".into(), "caller-openai-key-123456".into()));
        let b = command_with_secret("kic", "OPENAI_API_KEY");
        let env = child_env(&b, base, &[], &cmd_cred(CMD_SECRET_A));
        assert_eq!(get(&env, "OPENAI_API_KEY"), Some(OsStr::new(CMD_SECRET_A)));
        assert_eq!(count(&env, "OPENAI_API_KEY"), 1);
        assert!(!contains_value(&env, "caller-openai-key"));
        // The usual isolation still applies.
        for v in SESSION_VARS {
            assert!(!has(&env, v), "{v}");
        }
        assert!(!has(&env, "ANTHROPIC_API_KEY"));
        assert!(!has(&env, "CLAUDE_CODE_OAUTH_TOKEN"));
    }

    #[test]
    fn command_secret_overrides_pass_env_and_inherited_anthropic_key() {
        // secret_env = ANTHROPIC_API_KEY (e.g. Aider with Claude models): the caller's
        // inherited key is removed, and even with pass_env the stored secret wins.
        let mut b = command_with_secret("aider", "ANTHROPIC_API_KEY");
        let env = child_env(&b, session_base(), &[], &cmd_cred(CMD_SECRET_A));
        assert_eq!(
            get(&env, "ANTHROPIC_API_KEY"),
            Some(OsStr::new(CMD_SECRET_A))
        );
        assert!(!contains_value(&env, "caller-api-key"));
        b.pass_env = vec!["ANTHROPIC_API_KEY".into()];
        let env = child_env(&b, session_base(), &[], &cmd_cred(CMD_SECRET_A));
        assert_eq!(count(&env, "ANTHROPIC_API_KEY"), 1);
        assert_eq!(
            get(&env, "ANTHROPIC_API_KEY"),
            Some(OsStr::new(CMD_SECRET_A))
        );
        assert!(!contains_value(&env, "caller-api-key"));
    }

    #[cfg(windows)]
    #[test]
    fn command_secret_replaces_inherited_value_case_insensitively() {
        let b = command_with_secret("kic", "OPENAI_API_KEY");
        let base = base(&[("openai_api_key", "caller-openai-key-123456")]);
        let env = child_env(&b, base, &[], &cmd_cred(CMD_SECRET_A));
        assert_eq!(count(&env, "OPENAI_API_KEY"), 1);
        assert_eq!(get(&env, "OPENAI_API_KEY"), Some(OsStr::new(CMD_SECRET_A)));
        assert!(!contains_value(&env, "caller-openai-key"));
    }

    #[test]
    fn command_secret_only_for_its_backend() {
        let shared = session_base();
        let a = child_env(
            &command_with_secret("a", "OPENAI_API_KEY"),
            shared.clone(),
            &[],
            &cmd_cred(CMD_SECRET_A),
        );
        assert!(contains_value(&a, CMD_SECRET_A));
        // Another command backend with its own secret in the same variable.
        let b = child_env(
            &command_with_secret("b", "OPENAI_API_KEY"),
            shared.clone(),
            &[],
            &cmd_cred(CMD_SECRET_B),
        );
        assert!(!contains_value(&b, CMD_SECRET_A));
        assert_eq!(get(&b, "OPENAI_API_KEY"), Some(OsStr::new(CMD_SECRET_B)));
        // A command backend without secret_env and the claude-* types never see it.
        let plain = child_env(
            &command("c", &["x", "{prompt}"], false),
            shared.clone(),
            &[],
            &Credential::None,
        );
        let o = child_env(&oauth("s"), shared.clone(), &[], &oauth_cred());
        let e = child_env(
            &endpoint("e", EndpointAuth::XApiKey),
            shared,
            &[],
            &key_cred(KEY_A),
        );
        for env in [&plain, &o, &e] {
            assert!(!contains_value(env, CMD_SECRET_A));
            assert!(!has(env, "OPENAI_API_KEY"));
        }
    }

    #[test]
    fn mismatched_command_credentials_put_no_secret_anywhere() {
        let with = command_with_secret("kic", "OPENAI_API_KEY");
        let without = command("c", &["x", "{prompt}"], false);
        let cases = [
            // secret_env set, but no / the wrong kind of credential.
            (&with, Credential::None),
            (&with, oauth_cred()),
            (&with, key_cred(KEY_A)),
            // A command secret handed to a backend without secret_env.
            (&without, cmd_cred(CMD_SECRET_A)),
        ];
        for (b, cred) in cases {
            let (env, mismatch) = child_env_inner(b, session_base(), &[], &cred);
            assert!(mismatch, "{} {cred:?}", b.name);
            assert!(!has(&env, "OPENAI_API_KEY"));
            assert!(!contains_value(&env, CMD_SECRET_A));
            assert!(!contains_value(&env, OAUTH_TOKEN));
            assert!(!contains_value(&env, KEY_A));
        }
        // A command secret handed to claude-* backends.
        for b in [oauth("s"), endpoint("e", EndpointAuth::XApiKey)] {
            let (env, mismatch) = child_env_inner(&b, session_base(), &[], &cmd_cred(CMD_SECRET_A));
            assert!(mismatch);
            assert!(!contains_value(&env, CMD_SECRET_A));
        }
        let (_, m) = child_env_inner(&with, session_base(), &[], &cmd_cred(CMD_SECRET_A));
        assert!(!m);
    }

    // ---- custom headers, model overrides, api_key_env, git bash ------------

    const CANARY: &str = "X-Caller-Gateway-Auth: Bearer gw-MUSTNOTLEAK-0123456789";

    fn caller_overrides() -> Vec<(OsString, OsString)> {
        let mut b = session_base();
        b.extend(base(&[
            ("ANTHROPIC_CUSTOM_HEADERS", CANARY),
            ("ANTHROPIC_MODEL", "caller-model"),
            ("ANTHROPIC_SMALL_FAST_MODEL", "caller-small"),
            ("ANTHROPIC_DEFAULT_FABLE_MODEL", "caller-fable"),
            ("ANTHROPIC_DEFAULT_OPUS_MODEL", "caller-opus"),
            ("ANTHROPIC_DEFAULT_SONNET_MODEL", "caller-sonnet"),
            ("ANTHROPIC_DEFAULT_HAIKU_MODEL", "caller-haiku"),
        ]));
        b
    }

    const MODEL_VARS: [&str; 6] = [
        "ANTHROPIC_MODEL",
        "ANTHROPIC_SMALL_FAST_MODEL",
        "ANTHROPIC_DEFAULT_FABLE_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    ];

    #[test]
    fn endpoint_drops_custom_headers_and_model_overrides() {
        let all: Vec<String> = std::iter::once("ANTHROPIC_CUSTOM_HEADERS".to_owned())
            .chain(MODEL_VARS.iter().map(|s| s.to_string()))
            .collect();
        let mut b = endpoint("e", EndpointAuth::XApiKey);
        // pass_env (per backend and global) cannot re-allow them for claude-endpoint.
        b.pass_env = all.clone();
        let env = child_env(&b, caller_overrides(), &all, &key_cred(KEY_A));
        assert!(!contains_value(&env, "MUSTNOTLEAK"));
        for v in all.iter() {
            assert!(!has(&env, v), "{v} must be removed");
        }
        // The backend's own env table may set a model explicitly.
        b.env
            .insert("ANTHROPIC_SMALL_FAST_MODEL".into(), "qwen3-coder".into());
        let env = child_env(&b, caller_overrides(), &[], &key_cred(KEY_A));
        assert_eq!(
            get(&env, "ANTHROPIC_SMALL_FAST_MODEL"),
            Some(OsStr::new("qwen3-coder"))
        );
        assert!(!has(&env, "ANTHROPIC_MODEL"));
    }

    #[test]
    fn command_drops_custom_headers_unless_passed_keeps_models() {
        let c = command("c", &["x"], true);
        let env = child_env(&c, caller_overrides(), &[], &Credential::None);
        assert!(!has(&env, "ANTHROPIC_CUSTOM_HEADERS"));
        assert!(!contains_value(&env, "MUSTNOTLEAK"));
        for v in MODEL_VARS {
            assert!(has(&env, v), "{v} stays for command backends");
        }
        let mut c = command("c", &["x"], true);
        c.pass_env = vec!["ANTHROPIC_CUSTOM_HEADERS".into()];
        let env = child_env(&c, caller_overrides(), &[], &Credential::None);
        assert_eq!(
            get(&env, "ANTHROPIC_CUSTOM_HEADERS"),
            Some(OsStr::new(CANARY))
        );
    }

    #[test]
    fn oauth_keeps_custom_headers_and_models() {
        let env = child_env(&oauth("s"), caller_overrides(), &[], &oauth_cred());
        assert_eq!(
            get(&env, "ANTHROPIC_CUSTOM_HEADERS"),
            Some(OsStr::new(CANARY))
        );
        for v in MODEL_VARS {
            assert!(has(&env, v), "{v}");
        }
    }

    #[test]
    fn git_bash_path_survives_session_isolation() {
        let mut b = session_base();
        b.push((
            "CLAUDE_CODE_GIT_BASH_PATH".into(),
            r"D:\Git\bin\bash.exe".into(),
        ));
        let envs = [
            child_env(&oauth("s"), b.clone(), &[], &oauth_cred()),
            child_env(
                &endpoint("e", EndpointAuth::Bearer),
                b.clone(),
                &[],
                &key_cred(KEY_A),
            ),
            child_env(&command("c", &["x"], true), b, &[], &Credential::None),
        ];
        for env in &envs {
            assert_eq!(
                get(env, "CLAUDE_CODE_GIT_BASH_PATH"),
                Some(OsStr::new(r"D:\Git\bin\bash.exe"))
            );
            // The rest of the prefix rule still applies.
            assert!(!has(env, "CLAUDE_CODE_SESSION_ID"));
            assert!(!has(env, "CLAUDE_CODE_ENTRYPOINT"));
        }
    }

    const PROXY_KEY: &str = "proxy-key-value-1234567890";

    /// `foreign_secret_env` as `Backends::new` fills it for a config with one endpoint
    /// using `api_key_env = "PROXY_KEY"`.
    fn with_key_vars(mut b: Backend) -> Backend {
        b.foreign_secret_env = vec!["PROXY_KEY".to_owned()];
        b
    }

    #[test]
    fn api_key_env_never_reaches_other_backends() {
        let mut shared = session_base();
        shared.push(("PROXY_KEY".into(), PROXY_KEY.into()));
        shared.push(("OTHER_VAR".into(), "kept".into()));
        let global = vec!["PROXY_KEY".to_owned()];

        let mut c = with_key_vars(command("c", &["x"], true));
        c.pass_env = vec!["PROXY_KEY".into()];
        let mut o = with_key_vars(oauth("s"));
        o.pass_env = vec!["PROXY_KEY".into()];
        let mut other_ep = with_key_vars(endpoint("e2", EndpointAuth::XApiKey));
        other_ep.pass_env = vec!["PROXY_KEY".into()];
        let mut own = with_key_vars(endpoint("e", EndpointAuth::Bearer));
        if let BackendKind::ClaudeEndpoint { api_key_env, .. } = &mut own.kind {
            *api_key_env = Some("PROXY_KEY".into());
        }

        let envs = [
            child_env(&c, shared.clone(), &global, &Credential::None),
            child_env(&o, shared.clone(), &global, &oauth_cred()),
            child_env(&other_ep, shared.clone(), &global, &key_cred(KEY_A)),
        ];
        for env in &envs {
            assert!(!has(env, "PROXY_KEY"));
            assert!(!contains_value(env, PROXY_KEY));
            assert_eq!(get(env, "OTHER_VAR"), Some(OsStr::new("kept")));
        }
        // The owning endpoint gets the value only as its auth variable.
        let env = child_env(&own, shared.clone(), &global, &key_cred(PROXY_KEY));
        assert!(!has(&env, "PROXY_KEY"));
        assert_eq!(
            get(&env, "ANTHROPIC_AUTH_TOKEN"),
            Some(OsStr::new(PROXY_KEY))
        );
        assert_eq!(count(&env, "ANTHROPIC_AUTH_TOKEN"), 1);

        // A command backend whose own secret_env is that name gets its stored secret.
        let s = with_key_vars(command_with_secret("k", "PROXY_KEY"));
        let env = child_env(&s, shared.clone(), &[], &cmd_cred(CMD_SECRET_A));
        assert_eq!(get(&env, "PROXY_KEY"), Some(OsStr::new(CMD_SECRET_A)));
        assert_eq!(count(&env, "PROXY_KEY"), 1);
        assert!(!contains_value(&env, PROXY_KEY));
    }

    #[test]
    fn api_key_env_removal_is_case_insensitive_on_windows() {
        let shared = base(&[("Proxy_Key", PROXY_KEY), ("PATH", "/usr/bin")]);
        let env = child_env(
            &with_key_vars(oauth("s")),
            shared,
            &["proxy_key".to_owned()],
            &oauth_cred(),
        );
        // Windows: the same variable (removed, pass_env cannot keep it). Unix: a different one.
        assert_eq!(has(&env, "Proxy_Key"), !cfg!(windows));
    }

    // ---- substitute --------------------------------------------------------

    #[test]
    fn substitute_basic() {
        let cwd = Path::new("/work/dir");
        assert_eq!(substitute("{prompt}", "hi", cwd, 3, None), "hi");
        assert_eq!(
            substitute("--dir={cwd} -n {max_turns}", "p", cwd, 12, None),
            format!("--dir={} -n 12", cwd.to_string_lossy())
        );
        assert_eq!(
            substitute("no placeholders", "p", cwd, 1, None),
            "no placeholders"
        );
        assert_eq!(substitute("", "p", cwd, 1, None), "");
        assert_eq!(substitute("{prompt}{prompt}", "ab", cwd, 1, None), "abab");
    }

    #[test]
    fn substitute_is_single_pass() {
        let cwd = Path::new("/work/dir");
        assert_eq!(
            substitute("{prompt}", "use {cwd} and {max_turns}", cwd, 5, None),
            "use {cwd} and {max_turns}"
        );
        assert_eq!(
            substitute("x{prompt}y", "{prompt}", cwd, 5, None),
            "x{prompt}y"
        );
        assert_eq!(substitute("{{prompt}}", "P", cwd, 5, None), "{P}");
        assert_eq!(
            substitute("{{cwd}", "P", cwd, 5, None),
            format!("{{{}", cwd.to_string_lossy())
        );
    }

    #[test]
    fn substitute_unknown_placeholders_literal() {
        let cwd = Path::new("/w");
        assert_eq!(
            substitute("{model} {x} {", "P", cwd, 1, None),
            "{model} {x} {"
        );
        assert_eq!(substitute("{prompt", "P", cwd, 1, None), "{prompt");
        assert_eq!(substitute("}{prompt}{", "P", cwd, 1, None), "}P{");
        assert_eq!(substitute("{PROMPT}", "P", cwd, 1, None), "{PROMPT}");
        // Multi-byte characters around braces.
        assert_eq!(substitute("ä{prompt}ö{", "ü", cwd, 1, None), "äüö{");
    }

    #[test]
    fn substitute_model_single_pass() {
        let cwd = Path::new("/w");
        let m = Some("qwen-qwen3-8-27b");
        assert_eq!(
            substitute("openai/{model}", "P", cwd, 1, m),
            "openai/qwen-qwen3-8-27b"
        );
        assert_eq!(substitute("{model}", "P", cwd, 1, Some("A B")), "A B");
        // A prompt containing `{model}` stays literal ...
        assert_eq!(
            substitute("{prompt}|{model}", "use {model}", cwd, 1, m),
            "use {model}|qwen-qwen3-8-27b"
        );
        // ... and so does a model value containing other placeholders.
        assert_eq!(
            substitute("{model}:{prompt}", "P", cwd, 1, Some("{prompt}{cwd}")),
            "{prompt}{cwd}:P"
        );
        assert_eq!(substitute("{{model}}", "P", cwd, 1, Some("m")), "{m}");
        assert_eq!(substitute("{MODEL}", "P", cwd, 1, Some("m")), "{MODEL}");
    }

    // ---- build_launch ------------------------------------------------------

    const NASTY: &str = "a; rm -rf / && echo $(x) `y` | z > f \"q\" 'r' %PATH% ^& {cwd}";

    #[test]
    fn command_argument_delivery() {
        let b = command(
            "aider",
            &[
                "aider",
                "--yes-always",
                "--message",
                "{prompt}",
                "--cwd={cwd}",
            ],
            false,
        );
        let l = build_launch(
            &b,
            &req(NASTY),
            &Credential::None,
            Path::new("/usr/bin/aider"),
            session_base(),
            &[],
        )
        .unwrap();
        assert_eq!(l.program, PathBuf::from("/usr/bin/aider"));
        let expected: Vec<OsString> = vec![
            "--yes-always".into(),
            "--message".into(),
            NASTY.into(),
            format!("--cwd={}", Path::new("/work/dir").to_string_lossy()).into(),
        ];
        assert_eq!(l.args, expected);
        assert_eq!(l.stdin, None);
        assert!(l.secrets.is_empty());
        assert_eq!(
            l.header,
            vec![
                "backend: aider (command)".to_owned(),
                "program: /usr/bin/aider".to_owned(),
                "prompt delivery: argument".to_owned(),
            ]
        );
        for v in SESSION_VARS {
            assert!(!has(&l.env, v));
        }
    }

    #[test]
    fn command_stdin_delivery() {
        let b = command("agent", &["agent", "--turns", "{max_turns}"], true);
        let mut r = req(NASTY);
        r.model = Some("sonnet".into());
        let l = build_launch(
            &b,
            &r,
            &Credential::None,
            Path::new("/opt/agent"),
            Vec::new(),
            &[],
        )
        .unwrap();
        assert_eq!(l.args, vec![OsString::from("--turns"), OsString::from("7")]);
        assert_eq!(l.stdin.as_deref(), Some(NASTY));
        assert!(l.header.contains(&"prompt delivery: stdin".to_owned()));
        assert!(l
            .header
            .contains(&"note: model ignored by this command backend".to_owned()));
        assert_eq!(l.header[0], "backend: agent (command)");
        assert!(!l
            .args
            .iter()
            .any(|a| a.to_string_lossy().contains("sonnet")));
    }

    #[test]
    fn command_note_only_when_model_or_tools_set() {
        let b = command("agent", &["agent"], true);
        let p = Path::new("/opt/agent");
        let has_note = |l: &Launch| l.header.iter().any(|h| h.starts_with("note: "));
        let l = build_launch(&b, &req("x"), &Credential::None, p, Vec::new(), &[]).unwrap();
        assert!(!has_note(&l));
        let mut r = req("x");
        r.allowed_tools = Some(vec!["Read".into()]);
        let l = build_launch(&b, &r, &Credential::None, p, Vec::new(), &[]).unwrap();
        assert!(l
            .header
            .contains(&"note: allowed_tools ignored by this command backend".to_owned()));
        r.model = Some("m".into());
        let l = build_launch(&b, &r, &Credential::None, p, Vec::new(), &[]).unwrap();
        assert!(l
            .header
            .contains(&"note: model/allowed_tools ignored by this command backend".to_owned()));
        let mut r = req("x");
        r.allowed_tools = Some(vec![]);
        r.model = Some("  ".into());
        let l = build_launch(&b, &r, &Credential::None, p, Vec::new(), &[]).unwrap();
        assert!(!has_note(&l));
    }

    /// A command backend with `{model}` and an allowlist.
    fn command_with_models(models: &[&str]) -> Backend {
        let mut b = command(
            "kic",
            &[
                "aider",
                "--model",
                "openai/{model}",
                "--message",
                "{prompt}",
            ],
            false,
        );
        b.models = Some(models.iter().map(|m| (*m).to_owned()).collect());
        b.model = Some(models[0].to_owned());
        b
    }

    #[test]
    fn command_model_placeholder_is_substituted() {
        let b = command_with_models(&["openai-gpt-oss-120b", "qwen-qwen3-8-27b"]);
        let p = Path::new("/usr/bin/aider");
        let args = |l: &Launch| -> Vec<String> {
            l.args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        // Default model.
        let l = build_launch(&b, &req("x"), &Credential::None, p, Vec::new(), &[]).unwrap();
        assert_eq!(
            args(&l),
            ["--model", "openai/openai-gpt-oss-120b", "--message", "x"]
        );
        assert_eq!(
            l.header[0],
            "backend: kic (command), model openai-gpt-oss-120b"
        );
        // Requested model; a prompt mentioning {model} stays literal.
        let mut r = req("say {model}");
        r.model = Some(" qwen-qwen3-8-27b ".into());
        let l = build_launch(&b, &r, &Credential::None, p, Vec::new(), &[]).unwrap();
        assert_eq!(
            args(&l),
            [
                "--model",
                "openai/qwen-qwen3-8-27b",
                "--message",
                "say {model}"
            ]
        );
        // The model is used, so only allowed_tools would be noted.
        assert!(!l.header.iter().any(|h| h.starts_with("note: ")));
        r.allowed_tools = Some(vec!["Read".into()]);
        let l = build_launch(&b, &r, &Credential::None, p, Vec::new(), &[]).unwrap();
        assert!(l
            .header
            .contains(&"note: allowed_tools ignored by this command backend".to_owned()));
    }

    #[test]
    fn model_outside_allowlist_is_rejected_by_build_launch() {
        let b = command_with_models(&["a", "b"]);
        let p = Path::new("/usr/bin/aider");
        let mut r = req("x");
        r.model = Some("--evil".into());
        let e = build_launch(&b, &r, &Credential::None, p, Vec::new(), &[]).unwrap_err();
        assert!(e.contains("not offered by backend 'kic'"), "{e}");
        assert!(e.contains("Allowed models: a, b"), "{e}");
        // The same applies to a claude backend with an allowlist.
        let mut c = endpoint("local", EndpointAuth::XApiKey);
        c.models = Some(vec!["qwen3-coder".into()]);
        c.model = Some("qwen3-coder".into());
        r.model = Some("opus".into());
        let e = build_launch(
            &c,
            &r,
            &key_cred(KEY_A),
            Path::new("/bin/claude"),
            Vec::new(),
            &[],
        )
        .unwrap_err();
        assert!(e.contains("not offered by backend 'local'"), "{e}");
    }

    #[test]
    fn command_batch_script_rejected_without_stdin() {
        let b = command("aider", &["aider", "{prompt}"], false);
        for prog in [r"C:\tools\aider.cmd", "/x/aider.BAT"] {
            let err = build_launch(
                &b,
                &req("x"),
                &Credential::None,
                Path::new(prog),
                Vec::new(),
                &[],
            )
            .unwrap_err();
            assert!(
                err.contains("backend aider: command[0] is a .cmd/.bat script"),
                "{err}"
            );
            assert!(err.contains("prompt_via_stdin = true"), "{err}");
        }
        // With stdin delivery a batch script is fine.
        let b = command("aider", &["aider"], true);
        let l = build_launch(
            &b,
            &req("x"),
            &Credential::None,
            Path::new(r"C:\tools\aider.cmd"),
            Vec::new(),
            &[],
        )
        .unwrap();
        assert_eq!(l.stdin.as_deref(), Some("x"));
    }

    #[test]
    fn command_secret_launch_header_and_secrets() {
        let b = command_with_secret("kic", "OPENAI_API_KEY");
        let l = build_launch(
            &b,
            &req("x"),
            &cmd_cred(CMD_SECRET_A),
            Path::new("/usr/bin/aider"),
            session_base(),
            &[],
        )
        .unwrap();
        assert_eq!(
            get(&l.env, "OPENAI_API_KEY"),
            Some(OsStr::new(CMD_SECRET_A))
        );
        assert_eq!(l.secrets.values(), [CMD_SECRET_A.to_owned()]);
        assert!(
            l.header.contains(
                &"secret_env: OPENAI_API_KEY (stored secret, value not shown)".to_owned()
            ),
            "{:?}",
            l.header
        );
        assert!(!l.header.iter().any(|h| h.contains(CMD_SECRET_A)));
        assert!(!l
            .args
            .iter()
            .any(|a| a.to_string_lossy().contains(CMD_SECRET_A)));

        // Short secrets are passed but not redacted.
        let l = build_launch(
            &b,
            &req("x"),
            &cmd_cred("short"),
            Path::new("/usr/bin/aider"),
            Vec::new(),
            &[],
        )
        .unwrap();
        assert!(l.secrets.is_empty());
        assert_eq!(get(&l.env, "OPENAI_API_KEY"), Some(OsStr::new("short")));

        // Wrong credential kinds are rejected before anything is built.
        let p = Path::new("/usr/bin/aider");
        for cred in [Credential::None, oauth_cred(), key_cred(KEY_A)] {
            let err = build_launch(&b, &req("x"), &cred, p, Vec::new(), &[]).unwrap_err();
            assert!(err.contains("credential type does not match"), "{err}");
        }
        let plain = command("c", &["c"], true);
        let err = build_launch(
            &plain,
            &req("x"),
            &cmd_cred(CMD_SECRET_A),
            p,
            Vec::new(),
            &[],
        )
        .unwrap_err();
        assert!(err.contains("credential type does not match"), "{err}");
    }

    #[test]
    fn claude_oauth_launch() {
        let mut b = oauth("sidekick");
        b.model = Some("sonnet".into());
        let l = build_launch(
            &b,
            &req("do it"),
            &oauth_cred(),
            Path::new("/bin/claude"),
            session_base(),
            &[],
        )
        .unwrap();
        assert_eq!(
            l.header[0],
            "backend: sidekick (claude-oauth), model sonnet"
        );
        assert_eq!(l.header[1], BASE_URL_WARNING);
        assert!(l.header.contains(&"program: /bin/claude".to_owned()));
        assert!(l.header.contains(&"prompt delivery: argument".to_owned()));
        let args: Vec<String> = l
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            ["-p", "--max-turns", "7", "--model", "sonnet", "--", "do it"]
        );
        assert_eq!(
            get(&l.env, "CLAUDE_CODE_OAUTH_TOKEN"),
            Some(OsStr::new(OAUTH_TOKEN))
        );
        assert_eq!(l.secrets.values(), [OAUTH_TOKEN.to_owned()]);
        assert_eq!(l.stdin, None);

        // No inherited base URL -> no warning.
        let l = build_launch(
            &b,
            &req("do it"),
            &oauth_cred(),
            Path::new("/bin/claude"),
            base(&[("PATH", "/usr/bin")]),
            &[],
        )
        .unwrap();
        assert!(!l.header.iter().any(|h| h.starts_with("warning:")));
    }

    #[test]
    fn model_precedence() {
        let mut b = endpoint("local", EndpointAuth::XApiKey);
        b.model = Some("qwen3-coder".into());
        let p = Path::new("/bin/claude");
        let model_of = |l: &Launch| {
            let i = l.args.iter().position(|a| a == "--model")?;
            Some(l.args[i + 1].to_string_lossy().into_owned())
        };
        let l = build_launch(&b, &req("x"), &key_cred(KEY_A), p, Vec::new(), &[]).unwrap();
        assert_eq!(model_of(&l).as_deref(), Some("qwen3-coder"));
        let mut r = req("x");
        r.model = Some("other".into());
        let l = build_launch(&b, &r, &key_cred(KEY_A), p, Vec::new(), &[]).unwrap();
        assert_eq!(model_of(&l).as_deref(), Some("other"));
        r.model = Some(" ".into());
        let l = build_launch(&b, &r, &key_cred(KEY_A), p, Vec::new(), &[]).unwrap();
        assert_eq!(model_of(&l).as_deref(), Some("qwen3-coder"));
        b.model = None;
        let l = build_launch(&b, &req("x"), &key_cred(KEY_A), p, Vec::new(), &[]).unwrap();
        assert_eq!(model_of(&l), None);
    }

    #[test]
    fn claude_endpoint_launch_header_and_secrets() {
        let b = endpoint("local", EndpointAuth::XApiKey);
        let l = build_launch(
            &b,
            &req("x"),
            &key_cred(KEY_A),
            Path::new("/bin/claude"),
            session_base(),
            &[],
        )
        .unwrap();
        assert_eq!(l.header[0], "backend: local (claude-endpoint)");
        assert_eq!(l.header[1], "endpoint: http://localhost:11434");
        assert!(!l.header.iter().any(|h| h.starts_with("warning:")));
        assert_eq!(l.secrets.values(), [KEY_A.to_owned()]);
        assert!(!has(&l.env, "CLAUDE_CODE_OAUTH_TOKEN"));

        // Short placeholder keys are not redacted (would mangle output).
        let l = build_launch(
            &b,
            &req("x"),
            &key_cred("ollama"),
            Path::new("/bin/claude"),
            Vec::new(),
            &[],
        )
        .unwrap();
        assert!(l.secrets.is_empty());
        assert_eq!(get(&l.env, "ANTHROPIC_API_KEY"), Some(OsStr::new("ollama")));
    }

    #[test]
    fn claude_batch_script_uses_stdin() {
        let l = build_launch(
            &oauth("s"),
            &req("prompt text"),
            &oauth_cred(),
            Path::new(r"C:\npm\claude.cmd"),
            Vec::new(),
            &[],
        )
        .unwrap();
        assert_eq!(l.stdin.as_deref(), Some("prompt text"));
        assert!(!l.args.iter().any(|a| a == "prompt text"));
    }

    #[test]
    fn build_launch_rejects_mismatched_credential() {
        let p = Path::new("/bin/claude");
        let cases = [
            (oauth("s"), key_cred(KEY_A)),
            (oauth("s"), Credential::None),
            (endpoint("e", EndpointAuth::XApiKey), oauth_cred()),
            (endpoint("e", EndpointAuth::XApiKey), Credential::None),
            (command("c", &["c"], true), oauth_cred()),
        ];
        for (b, cred) in cases {
            let err = build_launch(&b, &req("x"), &cred, p, Vec::new(), &[]).unwrap_err();
            assert!(err.contains("credential type does not match"), "{err}");
        }
    }

    // ---- load_credential ---------------------------------------------------

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn missing(_: &SecretId) -> Result<(Token, TokenSource), TokenError> {
        Err(TokenError::Missing)
    }

    fn unreachable_store(_: &SecretId) -> Result<(Token, TokenSource), TokenError> {
        panic!("secret store must not be used")
    }

    #[test]
    fn load_oauth_token() {
        let b = oauth("work");
        let loader = |id: &SecretId| {
            assert_eq!(id, &SecretId::Oauth("work".into()));
            Ok((Token::new(OAUTH_TOKEN), TokenSource::Keyring))
        };
        let (cred, source) = load_credential_with(&b, &no_env, &loader).unwrap();
        assert_eq!(cred, oauth_cred());
        assert_eq!(source, TokenSource::Keyring.to_string());
        assert!(!source.contains(OAUTH_TOKEN));
    }

    #[test]
    fn load_oauth_missing_messages() {
        let err = load_credential_with(&oauth("work"), &no_env, &missing).unwrap_err();
        assert!(err.contains("No token stored for backend 'work'"), "{err}");
        assert!(err.contains("agent-sidekick setup --backend work"), "{err}");

        let err = load_credential_with(&oauth("sidekick"), &no_env, &missing).unwrap_err();
        assert!(err.contains("'agent-sidekick setup'"), "{err}");
        assert!(
            err.contains("agent-sidekick setup --backend sidekick"),
            "{err}"
        );

        let storage = |_: &SecretId| Err(TokenError::Storage("locked".into()));
        let err = load_credential_with(&oauth("work"), &no_env, &storage).unwrap_err();
        assert!(
            err.contains("locked") && err.contains("setup --backend work"),
            "{err}"
        );
    }

    #[test]
    fn load_endpoint_from_env_var() {
        let mut b = endpoint("local", EndpointAuth::XApiKey);
        if let BackendKind::ClaudeEndpoint { api_key_env, .. } = &mut b.kind {
            *api_key_env = Some("OLLAMA_API_KEY".into());
        }
        let env = |k: &str| (k == "OLLAMA_API_KEY").then(|| format!("  {KEY_A}\n"));
        let (cred, source) = load_credential_with(&b, &env, &unreachable_store).unwrap();
        assert_eq!(cred, key_cred(KEY_A));
        assert_eq!(source, "environment variable OLLAMA_API_KEY");

        for env in [&no_env as &dyn Fn(&str) -> Option<String>, &|_: &str| {
            Some("  ".to_owned())
        }] {
            let err = load_credential_with(&b, env, &unreachable_store).unwrap_err();
            assert!(
                err.contains("Environment variable OLLAMA_API_KEY (api_key_env of backend 'local') is not set"),
                "{err}"
            );
            assert!(err.contains("setup --backend local"), "{err}");
        }
    }

    #[test]
    fn load_endpoint_from_store() {
        let b = endpoint("local", EndpointAuth::Bearer);
        let loader = |id: &SecretId| {
            assert_eq!(id, &SecretId::Endpoint("local".into()));
            Ok((
                Token::new("ollama"),
                TokenSource::File(PathBuf::from("/c/endpoint-local.key")),
            ))
        };
        let (cred, source) = load_credential_with(&b, &no_env, &loader).unwrap();
        assert_eq!(cred, key_cred("ollama"));
        assert!(source.contains("endpoint-local.key"), "{source}");

        let err = load_credential_with(&b, &no_env, &missing).unwrap_err();
        assert!(
            err.contains("No API key stored for backend 'local'"),
            "{err}"
        );
        assert!(
            err.contains("agent-sidekick setup --backend local"),
            "{err}"
        );
        assert!(err.contains("'ollama'"), "{err}");
    }

    #[test]
    fn load_command_secret() {
        let b = command_with_secret("kic", "OPENAI_API_KEY");
        let loader = |id: &SecretId| {
            assert_eq!(id, &SecretId::Command("kic".into()));
            Ok((
                Token::new(CMD_SECRET_A),
                TokenSource::File(PathBuf::from("/c/cmd-kic.key")),
            ))
        };
        let (cred, source) = load_credential_with(&b, &no_env, &loader).unwrap();
        assert_eq!(cred, cmd_cred(CMD_SECRET_A));
        assert!(source.contains("cmd-kic.key"), "{source}");
        assert!(!source.contains(CMD_SECRET_A));
        // The environment is never consulted, even if the variable is set there.
        let env = |_: &str| -> Option<String> { panic!("environment must not be read") };
        assert!(load_credential_with(&b, &env, &loader).is_ok());

        let err = load_credential_with(&b, &no_env, &missing).unwrap_err();
        assert_eq!(
            err,
            "No secret stored for backend 'kic' (secret_env OPENAI_API_KEY). Run \
             'agent-sidekick setup --backend kic' in a terminal."
        );
        let storage = |_: &SecretId| Err(TokenError::Storage("locked".into()));
        let err = load_credential_with(&b, &no_env, &storage).unwrap_err();
        assert!(
            err.contains("Cannot read the secret of backend `kic`: locked"),
            "{err}"
        );
        assert!(err.contains("setup --backend kic"), "{err}");
    }

    #[test]
    fn credential_secret_accessor() {
        assert_eq!(oauth_cred().secret().unwrap().expose(), OAUTH_TOKEN);
        assert_eq!(key_cred(KEY_A).secret().unwrap().expose(), KEY_A);
        assert_eq!(
            cmd_cred(CMD_SECRET_A).secret().unwrap().expose(),
            CMD_SECRET_A
        );
        assert!(Credential::None.secret().is_none());
        assert!(!format!("{:?}", cmd_cred(CMD_SECRET_A)).contains(CMD_SECRET_A));
    }

    #[test]
    fn load_command_needs_nothing() {
        let b = command("c", &["c"], true);
        let (cred, source) = load_credential_with(&b, &no_env, &unreachable_store).unwrap();
        assert_eq!(cred, Credential::None);
        assert_eq!(source, "none needed");
        // The public function does not touch the store for command backends either.
        assert_eq!(load_credential(&b).unwrap().0, Credential::None);
    }

    // ---- resolve_program ---------------------------------------------------

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, b"fake").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    #[test]
    fn empty_or_relative_path_entries_never_resolve_from_cwd() {
        let dir = tempfile::TempDir::new().unwrap();
        let file = if cfg!(windows) {
            "myagent.exe"
        } else {
            "myagent"
        };
        touch(dir.path(), file);
        // The program sits in `cwd`, but PATH only has empty / relative entries.
        for path in ["", ".", "bin"] {
            let err =
                resolve_command_program("a", "myagent", Some(path.into()), dir.path()).unwrap_err();
            assert!(err.contains("not found in PATH"), "{path:?}: {err}");
        }
        let sep = if cfg!(windows) { ";" } else { ":" };
        let other = tempfile::TempDir::new().unwrap();
        let mixed = format!("{sep}{}{sep}", other.path().display());
        let err =
            resolve_command_program("a", "myagent", Some(mixed.into()), dir.path()).unwrap_err();
        assert!(err.contains("not found in PATH"), "{err}");
    }

    #[test]
    fn resolve_bare_name_via_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let file = if cfg!(windows) {
            "myagent.exe"
        } else {
            "myagent"
        };
        let exe = touch(dir.path(), file);
        let path = std::env::join_paths([dir.path()]).unwrap();
        let found =
            resolve_command_program("a", "myagent", Some(path.clone()), dir.path()).unwrap();
        assert_eq!(
            std::fs::canonicalize(found).unwrap(),
            std::fs::canonicalize(&exe).unwrap()
        );

        let err = resolve_command_program("a", "nothere", Some(path), dir.path()).unwrap_err();
        assert_eq!(err, "backend a: program 'nothere' not found in PATH");
        let err = resolve_command_program("a", "myagent", None, dir.path()).unwrap_err();
        assert!(err.contains("not found in PATH"), "{err}");
    }

    #[cfg(windows)]
    #[test]
    fn resolve_honours_pathext_for_cmd() {
        let dir = tempfile::TempDir::new().unwrap();
        touch(dir.path(), "tool.cmd");
        let path = std::env::join_paths([dir.path()]).unwrap();
        let found = resolve_command_program("a", "tool", Some(path), dir.path()).unwrap();
        assert!(
            crate::locate::is_batch_script(&found),
            "{}",
            found.display()
        );
    }

    #[test]
    fn resolve_absolute_and_relative() {
        let dir = tempfile::TempDir::new().unwrap();
        let exe = touch(dir.path(), "prog");
        let abs = exe.to_string_lossy().into_owned();
        assert_eq!(
            resolve_command_program("a", &abs, None, dir.path()).unwrap(),
            exe
        );

        let missing = dir.path().join("missing").to_string_lossy().into_owned();
        let err = resolve_command_program("a", &missing, None, dir.path()).unwrap_err();
        assert!(err.contains("does not exist"), "{err}");
        let d = dir.path().to_string_lossy().into_owned();
        assert!(resolve_command_program("a", &d, None, dir.path()).is_err());

        for rel in ["./prog", "bin/prog"] {
            let err = resolve_command_program("a", rel, None, dir.path()).unwrap_err();
            assert!(err.contains("relative path"), "{err}");
        }
        #[cfg(windows)]
        for rel in [r".\prog", r"bin\prog", "C:prog", r"\prog"] {
            let err = resolve_command_program("a", rel, None, dir.path()).unwrap_err();
            assert!(err.contains("relative path"), "{rel}: {err}");
        }
        assert!(resolve_command_program("a", "", None, dir.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn absolute_program_must_be_executable_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("agent.sh");
        std::fs::write(&p, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let abs = p.to_string_lossy().into_owned();
        let err = resolve_command_program("a", &abs, None, dir.path()).unwrap_err();
        assert!(err.contains("is not executable (chmod +x)"), "{err}");
        assert!(err.starts_with("backend a: program '"), "{err}");
        // Through a symlink, the target's mode counts.
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&p, &link).unwrap();
        let l = link.to_string_lossy().into_owned();
        assert!(resolve_command_program("a", &l, None, dir.path()).is_err());
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            resolve_command_program("a", &abs, None, dir.path()).unwrap(),
            p
        );
        assert!(resolve_command_program("a", &l, None, dir.path()).is_ok());
    }

    #[test]
    fn resolve_program_claude_uses_config_path() {
        if std::env::var_os(crate::locate::ENV_CLAUDE).is_some() {
            return; // the env override would win; not deterministic here
        }
        let dir = tempfile::TempDir::new().unwrap();
        let exe = touch(dir.path(), "my-claude");
        let p = resolve_program(&oauth("s"), Some(&exe)).unwrap();
        assert_eq!(p, exe);
        let missing = dir.path().join("missing");
        let err =
            resolve_program(&endpoint("e", EndpointAuth::XApiKey), Some(&missing)).unwrap_err();
        assert!(err.contains("config.toml claude_path"), "{err}");
    }
}
