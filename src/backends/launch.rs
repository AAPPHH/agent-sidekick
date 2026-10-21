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
    /// command backends.
    None,
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

/// Header note for command backends when model/allowed_tools were requested.
pub const COMMAND_IGNORED_NOTE: &str = "note: model/allowed_tools are ignored for command backends";

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
                Err(TokenError::Storage(e)) => Err(format!(
                    "Cannot read the {id}: {e}. Run 'agent-sidekick doctor' or \
                     'agent-sidekick setup --backend {name}' in a terminal."
                )),
            }
        }
        BackendKind::Command { .. } => Ok((Credential::None, "none needed".to_owned())),
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
        return if p.is_file() {
            Ok(p.to_path_buf())
        } else {
            Err(format!(
                "backend {backend}: program '{program}' does not exist or is not a file"
            ))
        };
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

/// Removed from the inherited environment for this backend type (before `pass_env`).
fn is_removed(name: &OsStr, ty: BackendType) -> bool {
    REMOVED_PREFIXES.iter().any(|p| has_prefix(name, p))
        || in_list(name, REMOVED_NAMES)
        || (ty != BackendType::ClaudeOauth && eq_name(name, BASE_URL_ENV))
}

/// May an inherited variable that matches the removal rules be kept via `pass_env`?
fn may_pass(name: &OsStr, ty: BackendType) -> bool {
    if in_list(name, NEVER_PASSED) {
        return false;
    }
    match ty {
        BackendType::Command => true,
        BackendType::ClaudeOauth => !in_list(name, CLAUDE_CREDENTIAL_VARS),
        BackendType::ClaudeEndpoint => {
            !in_list(name, CLAUDE_CREDENTIAL_VARS) && !eq_name(name, BASE_URL_ENV)
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
/// the backend's `env` table, and the type-specific variables (see the design doc).
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
    let ty = backend.backend_type();
    let passed = |k: &OsStr| {
        global_pass_env
            .iter()
            .chain(backend.pass_env.iter())
            .any(|p| eq_name(k, p))
    };
    let mut env: Vec<(OsString, OsString)> = base
        .into_iter()
        .filter(|(k, _)| !is_removed(k, ty) || (passed(k) && may_pass(k, ty)))
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
        (BackendKind::Command { .. }, cred) => !matches!(cred, Credential::None),
    };
    (env, mismatch)
}

// ---------------------------------------------------------------------------
// Placeholders
// ---------------------------------------------------------------------------

/// Replace `{prompt}`, `{cwd}`, `{max_turns}` in one template element in a single pass:
/// replacement text is never scanned again, unknown `{x}` stays literal.
pub fn substitute(template: &str, prompt: &str, cwd: &Path, max_turns: u32) -> String {
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
    let credential_ok = matches!(
        (ty, credential),
        (BackendType::ClaudeOauth, Credential::OauthToken(_))
            | (BackendType::ClaudeEndpoint, Credential::ApiKey(_))
            | (BackendType::Command, Credential::None)
    );
    if !credential_ok {
        return Err(format!(
            "backend {name}: internal error: credential type does not match backend type {ty}"
        ));
    }
    let mut header = vec![format!("backend: {name} ({ty})")];

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
            let model = non_blank(req.model.as_deref())
                .or_else(|| non_blank(backend.model.as_deref()))
                .map(str::to_owned);
            let req2 = RunRequest {
                model,
                ..req.clone()
            };
            let secrets = match credential {
                Credential::OauthToken(t) | Credential::ApiKey(t) => Secrets::new([t.expose()]),
                Credential::None => Secrets::none(),
            };
            Ok(crate::runner::claude_launch(
                program, &req2, env, secrets, header,
            ))
        }
        BackendKind::Command {
            command,
            prompt_via_stdin,
        } => {
            if crate::locate::is_batch_script(program) && !prompt_via_stdin {
                return Err(format!(
                    "backend {name}: command[0] is a .cmd/.bat script; cmd.exe cannot pass \
                     arbitrary prompt text safely. Use prompt_via_stdin = true or point \
                     command[0] at an .exe"
                ));
            }
            let env = child_env(backend, base, global_pass_env, credential);
            let args: Vec<OsString> = command
                .iter()
                .skip(1)
                .map(|a| substitute(a, &req.prompt, &req.cwd, req.max_turns).into())
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
            let tools_set = req
                .allowed_tools
                .iter()
                .flatten()
                .any(|t| !t.trim().is_empty());
            if non_blank(req.model.as_deref()).is_some() || tools_set {
                header.push(COMMAND_IGNORED_NOTE.to_owned());
            }
            Ok(Launch {
                program: program.to_path_buf(),
                args,
                env,
                stdin,
                secrets: Secrets::none(),
                header,
            })
        }
    }
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
            env: BTreeMap::new(),
            pass_env: Vec::new(),
            implicit: false,
            kind,
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
            },
        )
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

    // ---- substitute --------------------------------------------------------

    #[test]
    fn substitute_basic() {
        let cwd = Path::new("/work/dir");
        assert_eq!(substitute("{prompt}", "hi", cwd, 3), "hi");
        assert_eq!(
            substitute("--dir={cwd} -n {max_turns}", "p", cwd, 12),
            format!("--dir={} -n 12", cwd.to_string_lossy())
        );
        assert_eq!(
            substitute("no placeholders", "p", cwd, 1),
            "no placeholders"
        );
        assert_eq!(substitute("", "p", cwd, 1), "");
        assert_eq!(substitute("{prompt}{prompt}", "ab", cwd, 1), "abab");
    }

    #[test]
    fn substitute_is_single_pass() {
        let cwd = Path::new("/work/dir");
        assert_eq!(
            substitute("{prompt}", "use {cwd} and {max_turns}", cwd, 5),
            "use {cwd} and {max_turns}"
        );
        assert_eq!(substitute("x{prompt}y", "{prompt}", cwd, 5), "x{prompt}y");
        assert_eq!(substitute("{{prompt}}", "P", cwd, 5), "{P}");
        assert_eq!(
            substitute("{{cwd}", "P", cwd, 5),
            format!("{{{}", cwd.to_string_lossy())
        );
    }

    #[test]
    fn substitute_unknown_placeholders_literal() {
        let cwd = Path::new("/w");
        assert_eq!(substitute("{model} {x} {", "P", cwd, 1), "{model} {x} {");
        assert_eq!(substitute("{prompt", "P", cwd, 1), "{prompt");
        assert_eq!(substitute("}{prompt}{", "P", cwd, 1), "}P{");
        assert_eq!(substitute("{PROMPT}", "P", cwd, 1), "{PROMPT}");
        // Multi-byte characters around braces.
        assert_eq!(substitute("ä{prompt}ö{", "ü", cwd, 1), "äüö{");
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
        assert!(l.header.contains(&COMMAND_IGNORED_NOTE.to_owned()));
        assert!(!l
            .args
            .iter()
            .any(|a| a.to_string_lossy().contains("sonnet")));
    }

    #[test]
    fn command_note_only_when_model_or_tools_set() {
        let b = command("agent", &["agent"], true);
        let p = Path::new("/opt/agent");
        let l = build_launch(&b, &req("x"), &Credential::None, p, Vec::new(), &[]).unwrap();
        assert!(!l.header.contains(&COMMAND_IGNORED_NOTE.to_owned()));
        let mut r = req("x");
        r.allowed_tools = Some(vec!["Read".into()]);
        let l = build_launch(&b, &r, &Credential::None, p, Vec::new(), &[]).unwrap();
        assert!(l.header.contains(&COMMAND_IGNORED_NOTE.to_owned()));
        let mut r = req("x");
        r.allowed_tools = Some(vec![]);
        r.model = Some("  ".into());
        let l = build_launch(&b, &r, &Credential::None, p, Vec::new(), &[]).unwrap();
        assert!(!l.header.contains(&COMMAND_IGNORED_NOTE.to_owned()));
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
        assert_eq!(l.header[0], "backend: sidekick (claude-oauth)");
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
