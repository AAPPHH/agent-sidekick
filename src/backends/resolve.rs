//! Validation of `[backends]` into [`Backends`].
//!
//! Rules (see also README / NOTES):
//! - no `[backends]` section: the implicit claude-oauth backend `sidekick` (v0.1 behaviour);
//!   the top-level `default_model` applies only to this implicit backend.
//! - `[backends]` present: at least one backend, exactly one with `default = true`.
//! - `cost` is optional and defaults by type: claude-oauth `quota`, claude-endpoint `paid`,
//!   command `free`.
//! - All problems are collected and reported together, one per line, each prefixed with
//!   `backends.<name>: ` (or `backends: ` / `pass_env: ` for cross-backend/global problems).

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use super::{Backend, BackendKind, BackendType, Backends, Cost, RawBackend, IMPLICIT_BACKEND};
use crate::config::Config;

/// Inputs that come from the environment, injected for testability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveDefaults {
    /// Home directory for `~` expansion and `~/.claude-sidekick-<name>` defaults.
    pub home: Option<PathBuf>,
    /// Config dir of the implicit/`sidekick` claude-oauth backend when none is configured
    /// (`AGENT_SIDEKICK_CLAUDE_CONFIG_DIR` or `~/.claude-sidekick`).
    pub legacy_agent_config_dir: Option<PathBuf>,
}

impl ResolveDefaults {
    /// From the real environment (`paths::home_dir`, `paths::agent_claude_config_dir`).
    pub fn from_env() -> Self {
        Self {
            home: crate::paths::home_dir().ok(),
            legacy_agent_config_dir: crate::paths::agent_claude_config_dir().ok(),
        }
    }
}

/// Description of the implicit backend.
pub const IMPLICIT_DESCRIPTION: &str = "Second Claude Code account (implicit default backend; configure [backends] in config.toml to add more)";

/// Variables a backend's `env` table may never set (compared case-insensitively);
/// additionally every name starting with `AGENT_SIDEKICK_`.
pub const RESERVED_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CONFIG_DIR",
];
const RESERVED_ENV_PREFIX: &str = "AGENT_SIDEKICK_";

/// Credentials that must never be passed through to a claude-* backend (or globally).
const SECRET_PASS_ENV_CLAUDE: &[&str] = &[
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "AGENT_SIDEKICK_TOKEN",
];
/// Credentials that must never be passed through to a command backend.
const SECRET_PASS_ENV_COMMAND: &[&str] = &["CLAUDE_CODE_OAUTH_TOKEN", "AGENT_SIDEKICK_TOKEN"];
/// Variables `api_key_env` may not name: the OAuth token must never go to an endpoint.
const FORBIDDEN_API_KEY_ENV: &[&str] = &["CLAUDE_CODE_OAUTH_TOKEN", "AGENT_SIDEKICK_TOKEN"];

/// Validate `cfg.backends` (or create the implicit backend) using the real environment.
/// The error is a user-facing message naming the backend and the problem.
pub fn resolve(cfg: &Config) -> Result<Backends, String> {
    resolve_with(cfg, &ResolveDefaults::from_env())
}

/// Testable core of [`resolve`].
pub fn resolve_with(cfg: &Config, defaults: &ResolveDefaults) -> Result<Backends, String> {
    let mut errors = Vec::new();
    for e in check_pass_env(&cfg.pass_env, SECRET_PASS_ENV_CLAUDE, "all backends") {
        errors.push(format!("pass_env: {e}"));
    }

    let list = match &cfg.backends {
        None => match implicit_backend(cfg, defaults) {
            Ok(b) => vec![b],
            Err(e) => {
                errors.push(format!("backends.{IMPLICIT_BACKEND}: {e}"));
                Vec::new()
            }
        },
        Some(map) => resolve_explicit(map, defaults, &mut errors),
    };

    if errors.is_empty() {
        Ok(Backends::new(list))
    } else {
        Err(errors.join("\n"))
    }
}

/// Expand a leading `~` (`~`, `~/x`, `~\x`) with `home`.
pub fn expand_tilde(p: &Path, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return p.to_path_buf();
    };
    let Some(s) = p.to_str() else {
        return p.to_path_buf();
    };
    if s == "~" {
        return home.to_path_buf();
    }
    match s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        Some("") => home.to_path_buf(),
        Some(rest) => home.join(rest),
        None => p.to_path_buf(),
    }
}

fn implicit_backend(cfg: &Config, defaults: &ResolveDefaults) -> Result<Backend, String> {
    let config_dir = defaults.legacy_agent_config_dir.clone().ok_or_else(|| {
        "cannot determine the agent's Claude config dir (home directory unknown; set AGENT_SIDEKICK_CLAUDE_CONFIG_DIR)"
            .to_owned()
    })?;
    Ok(Backend {
        name: IMPLICIT_BACKEND.to_owned(),
        description: IMPLICIT_DESCRIPTION.to_owned(),
        cost: Cost::Quota,
        is_default: true,
        max_concurrent: None,
        model: cfg
            .default_model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_owned),
        env: BTreeMap::new(),
        pass_env: Vec::new(),
        implicit: true,
        kind: BackendKind::ClaudeOauth { config_dir },
    })
}

fn resolve_explicit(
    map: &BTreeMap<String, RawBackend>,
    defaults: &ResolveDefaults,
    errors: &mut Vec<String>,
) -> Vec<Backend> {
    if map.is_empty() {
        errors.push("[backends] is present but defines no backend".to_owned());
        return Vec::new();
    }

    let mut list = Vec::new();
    for (name, raw) in map {
        match resolve_one(name, raw, defaults) {
            Ok(b) => list.push(b),
            Err(errs) => errors.extend(errs.into_iter().map(|e| format!("backends.{name}: {e}"))),
        }
    }

    // Exactly one default (counted over all entries, also invalid ones).
    let defaults_set: Vec<&str> = map
        .iter()
        .filter(|(_, r)| r.default == Some(true))
        .map(|(n, _)| n.as_str())
        .collect();
    match defaults_set.len() {
        1 => {}
        0 => errors.push(format!(
            "backends: no default backend; set `default = true` on exactly one of: {}",
            map.keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        )),
        _ => errors.push(format!(
            "backends: more than one default backend ({}); exactly one may have `default = true`",
            defaults_set.join(", ")
        )),
    }

    // A claude-endpoint must not share its CLAUDE_CONFIG_DIR with a claude-oauth backend:
    // a saved claude.ai login there would otherwise stay active.
    for ep in &list {
        let BackendKind::ClaudeEndpoint { config_dir, .. } = &ep.kind else {
            continue;
        };
        let ep_norm = normalize_dir(config_dir);
        for oa in &list {
            if let BackendKind::ClaudeOauth { config_dir: oa_dir } = &oa.kind {
                if normalize_dir(oa_dir) == ep_norm {
                    errors.push(format!(
                        "backends.{}: config_dir {} is also used by claude-oauth backend `{}`; \
                         a claude-endpoint backend needs its own config_dir (a saved claude.ai login there would stay active)",
                        ep.name,
                        config_dir.display(),
                        oa.name
                    ));
                }
            }
        }
    }
    list
}

fn resolve_one(
    name: &str,
    raw: &RawBackend,
    defaults: &ResolveDefaults,
) -> Result<Backend, Vec<String>> {
    let mut errs = Vec::new();

    if !valid_name(name) {
        errs.push(
            "invalid backend name: use 1-32 characters a-z, 0-9, `_` or `-`, starting with a letter or digit"
                .to_owned(),
        );
    }

    let description = match raw.description.as_deref().map(str::trim) {
        Some(d) if !d.is_empty() => Some(d.to_owned()),
        Some(_) => {
            errs.push("`description` must not be blank".to_owned());
            None
        }
        None => {
            errs.push("`description` is required".to_owned());
            None
        }
    };

    if raw.max_concurrent == Some(0) {
        errs.push("`max_concurrent` must be at least 1".to_owned());
    }

    let env = raw.env.clone().unwrap_or_default();
    for (k, v) in &env {
        if !valid_env_name(k) {
            errs.push(format!("env: invalid variable name {k:?}"));
        } else if is_reserved_env(k) {
            errs.push(format!(
                "env: `{k}` is reserved (set by agent-sidekick itself or secret); \
                 reserved: {}, {RESERVED_ENV_PREFIX}*",
                RESERVED_ENV.join(", ")
            ));
        }
        if v.contains('\0') {
            errs.push(format!("env: value of `{k}` contains a NUL character"));
        }
    }

    let Some(kind) = raw.kind else {
        errs.push("`type` is required (one of: claude-oauth, claude-endpoint, command)".to_owned());
        return Err(errs);
    };

    let pass_env = raw.pass_env.clone().unwrap_or_default();
    let denied = match kind {
        BackendType::Command => SECRET_PASS_ENV_COMMAND,
        _ => SECRET_PASS_ENV_CLAUDE,
    };
    for e in check_pass_env(&pass_env, denied, &format!("a {kind} backend")) {
        errs.push(format!("pass_env: {e}"));
    }

    // Fields that are not allowed for this type.
    let set_fields: [(&str, bool, &[BackendType]); 7] = [
        (
            "model",
            raw.model.is_some(),
            &[BackendType::ClaudeOauth, BackendType::ClaudeEndpoint],
        ),
        (
            "config_dir",
            raw.config_dir.is_some(),
            &[BackendType::ClaudeOauth, BackendType::ClaudeEndpoint],
        ),
        (
            "base_url",
            raw.base_url.is_some(),
            &[BackendType::ClaudeEndpoint],
        ),
        (
            "api_key_env",
            raw.api_key_env.is_some(),
            &[BackendType::ClaudeEndpoint],
        ),
        ("auth", raw.auth.is_some(), &[BackendType::ClaudeEndpoint]),
        ("command", raw.command.is_some(), &[BackendType::Command]),
        (
            "prompt_via_stdin",
            raw.prompt_via_stdin.is_some(),
            &[BackendType::Command],
        ),
    ];
    for (field, is_set, allowed) in set_fields {
        if is_set && !allowed.contains(&kind) {
            errs.push(format!("field `{field}` is not allowed for type `{kind}`"));
        }
    }

    let model = match raw.model.as_deref().map(str::trim) {
        Some("") if kind != BackendType::Command => {
            errs.push("`model` must not be blank".to_owned());
            None
        }
        Some(m) if kind != BackendType::Command => Some(m.to_owned()),
        _ => None,
    };

    let backend_kind = match kind {
        BackendType::ClaudeOauth => {
            let dir = config_dir_for(name, raw, defaults, true, &mut errs);
            dir.map(|config_dir| BackendKind::ClaudeOauth { config_dir })
        }
        BackendType::ClaudeEndpoint => {
            let base_url = match raw.base_url.as_deref() {
                None => {
                    errs.push("`base_url` is required for type `claude-endpoint`".to_owned());
                    None
                }
                Some(u) => match normalize_base_url(u) {
                    Ok(u) => Some(u),
                    Err(e) => {
                        errs.push(e);
                        None
                    }
                },
            };
            if let Some(v) = raw.api_key_env.as_deref() {
                if !valid_env_name(v) {
                    errs.push(format!("`api_key_env`: invalid variable name {v:?}"));
                } else if FORBIDDEN_API_KEY_ENV
                    .iter()
                    .any(|f| f.eq_ignore_ascii_case(v))
                {
                    errs.push(format!(
                        "`api_key_env` must not be `{v}` (an OAuth token must never be sent to an endpoint)"
                    ));
                }
            }
            let dir = config_dir_for(name, raw, defaults, false, &mut errs);
            match (base_url, dir) {
                (Some(base_url), Some(config_dir)) => Some(BackendKind::ClaudeEndpoint {
                    base_url,
                    api_key_env: raw.api_key_env.clone(),
                    auth: raw.auth.unwrap_or_default(),
                    config_dir,
                }),
                _ => None,
            }
        }
        BackendType::Command => {
            let prompt_via_stdin = raw.prompt_via_stdin.unwrap_or(false);
            match raw.command.as_deref() {
                None => {
                    errs.push("`command` is required for type `command`".to_owned());
                    None
                }
                Some([]) => {
                    errs.push("`command` must not be empty".to_owned());
                    None
                }
                Some(cmd) => {
                    let before = errs.len();
                    if cmd[0].trim().is_empty() {
                        errs.push("`command[0]` (the program) must not be blank".to_owned());
                    }
                    if cmd.iter().any(|a| a.contains('\0')) {
                        errs.push("`command` contains a NUL character".to_owned());
                    }
                    let has_prompt = cmd.iter().any(|a| a.contains("{prompt}"));
                    if prompt_via_stdin && has_prompt {
                        errs.push(
                            "`command` must not contain `{prompt}` when `prompt_via_stdin = true`"
                                .to_owned(),
                        );
                    } else if !prompt_via_stdin && !has_prompt {
                        errs.push(
                            "`command` must contain `{prompt}` in some argument (or set `prompt_via_stdin = true`)"
                                .to_owned(),
                        );
                    }
                    (errs.len() == before).then(|| BackendKind::Command {
                        command: cmd.to_vec(),
                        prompt_via_stdin,
                    })
                }
            }
        }
    };

    if !errs.is_empty() {
        return Err(errs);
    }
    let (Some(description), Some(kind_val)) = (description, backend_kind) else {
        // Unreachable in practice: every `None` above pushed an error.
        return Err(vec!["invalid backend".to_owned()]);
    };
    Ok(Backend {
        name: name.to_owned(),
        description,
        cost: raw.cost.unwrap_or(match kind {
            BackendType::ClaudeOauth => Cost::Quota,
            BackendType::ClaudeEndpoint => Cost::Paid,
            BackendType::Command => Cost::Free,
        }),
        is_default: raw.default.unwrap_or(false),
        max_concurrent: raw.max_concurrent,
        model,
        env,
        pass_env,
        implicit: false,
        kind: kind_val,
    })
}

/// `config_dir` of a claude-* backend: configured (with `~` expanded, must be absolute) or
/// the default (`sidekick` oauth: the legacy dir; else `~/.claude-sidekick-<name>`).
fn config_dir_for(
    name: &str,
    raw: &RawBackend,
    defaults: &ResolveDefaults,
    oauth: bool,
    errs: &mut Vec<String>,
) -> Option<PathBuf> {
    match &raw.config_dir {
        Some(p) => {
            if p.as_os_str().is_empty() {
                errs.push("`config_dir` must not be empty".to_owned());
                return None;
            }
            let expanded = expand_tilde(p, defaults.home.as_deref());
            if expanded.is_absolute() {
                Some(expanded)
            } else if starts_with_tilde(p) && defaults.home.is_none() {
                errs.push(format!(
                    "`config_dir` {} uses `~` but the home directory is unknown",
                    p.display()
                ));
                None
            } else {
                errs.push(format!(
                    "`config_dir` must be an absolute path (or start with `~`), got {}",
                    p.display()
                ));
                None
            }
        }
        None if oauth && name == IMPLICIT_BACKEND => {
            let dir = defaults.legacy_agent_config_dir.clone();
            if dir.is_none() {
                errs.push(
                    "cannot determine the default config_dir (home directory unknown); set `config_dir`"
                        .to_owned(),
                );
            }
            dir
        }
        None => match &defaults.home {
            Some(h) => Some(h.join(format!(".claude-sidekick-{name}"))),
            None => {
                errs.push(
                    "cannot determine the default config_dir (home directory unknown); set `config_dir`"
                        .to_owned(),
                );
                None
            }
        },
    }
}

fn starts_with_tilde(p: &Path) -> bool {
    p.to_str()
        .is_some_and(|s| s == "~" || s.starts_with("~/") || s.starts_with("~\\"))
}

/// Errors for a list of variable names to keep from the inherited environment.
fn check_pass_env(names: &[String], denied: &[&str], scope: &str) -> Vec<String> {
    let mut errs = Vec::new();
    for n in names {
        if !valid_env_name(n) {
            errs.push(format!("invalid variable name {n:?}"));
        } else if denied.iter().any(|d| d.eq_ignore_ascii_case(n)) {
            errs.push(format!(
                "`{n}` must not be passed through to {scope} (it would defeat credential isolation)"
            ));
        }
    }
    errs
}

/// `^[a-z0-9][a-z0-9_-]{0,31}$`
fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
}

/// Non-empty, no `=` and no NUL (what the OSes accept for a variable name).
fn valid_env_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('=') && !name.contains('\0')
}

fn is_reserved_env(name: &str) -> bool {
    RESERVED_ENV.iter().any(|r| r.eq_ignore_ascii_case(name))
        || name
            .get(..RESERVED_ENV_PREFIX.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(RESERVED_ENV_PREFIX))
}

/// Checks `http(s)://host...` and trims trailing `/`.
fn normalize_base_url(url: &str) -> Result<String, String> {
    let u = url.trim();
    if u.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!(
            "`base_url` must not contain whitespace, got {url:?}"
        ));
    }
    let rest = ["http://", "https://"].iter().find_map(|s| {
        u.get(..s.len())
            .filter(|p| p.eq_ignore_ascii_case(s))
            .map(|_| &u[s.len()..])
    });
    let Some(rest) = rest else {
        return Err(format!(
            "`base_url` must start with http:// or https://, got {url:?}"
        ));
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    if host.is_empty() || host.starts_with(':') {
        return Err(format!("`base_url` has no host, got {url:?}"));
    }
    Ok(u.trim_end_matches('/').to_owned())
}

/// Lexical normalisation for comparing directories: drops `.` and trailing separators,
/// resolves `..` lexically, case-insensitive on Windows and macOS.
fn normalize_dir(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if cfg!(any(windows, target_os = "macos")) {
        PathBuf::from(out.to_string_lossy().to_lowercase())
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::EndpointAuth;

    /// An absolute directory that is valid on the current platform.
    fn abs(rel: &str) -> PathBuf {
        let root = if cfg!(windows) { "C:\\" } else { "/" };
        Path::new(root).join(rel)
    }

    fn defaults() -> ResolveDefaults {
        ResolveDefaults {
            home: Some(abs("home")),
            legacy_agent_config_dir: Some(abs("home").join(".claude-sidekick")),
        }
    }

    fn parse(toml_text: &str) -> Config {
        toml::from_str::<Config>(toml_text).expect("config parses")
    }

    fn resolve_str(toml_text: &str) -> Result<Backends, String> {
        resolve_with(&parse(toml_text), &defaults())
    }

    fn err_of(toml_text: &str) -> String {
        resolve_str(toml_text).expect_err("expected a validation error")
    }

    /// Path as a TOML literal string.
    fn lit(p: &Path) -> String {
        format!("'{}'", p.display())
    }

    const FULL: &str = r#"
max_concurrent = 2
default_model = "opus"

[backends.sidekick]
type = "claude-oauth"
description = "Second Claude subscription account. Best quality; uses that account's quota."
default = true
cost = "quota"
max_concurrent = 1
config_dir = "~/.claude-sidekick"
model = "sonnet"

[backends.local]
type = "claude-endpoint"
description = "Local qwen3-coder via Ollama. Free, slower, weaker; good for boilerplate."
cost = "free"
base_url = "http://localhost:11434"
model = "qwen3-coder"
api_key_env = "OLLAMA_API_KEY"
auth = "x-api-key"
config_dir = "~/.claude-sidekick-local"
env = { CLAUDE_CODE_ATTRIBUTION_HEADER = "0" }

[backends.aider]
type = "command"
description = "Aider with a local model. Free; edits files directly, no tool permissions."
cost = "free"
command = ["aider", "--yes-always", "--message", "{prompt}"]
prompt_via_stdin = false
pass_env = ["OPENAI_API_BASE"]
"#;

    #[test]
    fn implicit_backend_without_section() {
        let cfg = parse("default_model = \"sonnet\"\n");
        let b = resolve_with(&cfg, &defaults()).unwrap();
        assert_eq!(b.len(), 1);
        let s = b.default_backend();
        assert_eq!(s.name, "sidekick");
        assert!(s.implicit && s.is_default);
        assert_eq!(s.cost, Cost::Quota);
        assert_eq!(s.description, IMPLICIT_DESCRIPTION);
        assert_eq!(s.model.as_deref(), Some("sonnet"));
        assert!(s.env.is_empty() && s.pass_env.is_empty());
        assert_eq!(s.max_concurrent, None);
        assert_eq!(
            s.kind,
            BackendKind::ClaudeOauth {
                config_dir: abs("home").join(".claude-sidekick")
            }
        );
    }

    #[test]
    fn implicit_backend_needs_legacy_dir() {
        let d = ResolveDefaults {
            home: None,
            legacy_agent_config_dir: None,
        };
        let e = resolve_with(&Config::default(), &d).unwrap_err();
        assert!(e.starts_with("backends.sidekick: "), "{e}");
    }

    #[test]
    fn empty_section_is_an_error() {
        let e = err_of("[backends]\n");
        assert!(
            e.contains("[backends] is present but defines no backend"),
            "{e}"
        );
    }

    #[test]
    fn full_example() {
        let b = resolve_str(FULL).unwrap();
        assert_eq!(b.names(), vec!["aider", "local", "sidekick"]);
        assert_eq!(b.default_backend().name, "sidekick");

        let s = b.get("sidekick").unwrap();
        assert!(!s.implicit);
        assert_eq!(s.max_concurrent, Some(1));
        // default_model does not apply to explicit backends.
        assert_eq!(s.model.as_deref(), Some("sonnet"));
        assert_eq!(
            s.kind,
            BackendKind::ClaudeOauth {
                config_dir: abs("home").join(".claude-sidekick")
            }
        );

        let l = b.get("local").unwrap();
        assert_eq!(l.cost, Cost::Free);
        assert!(!l.is_default);
        assert_eq!(l.model.as_deref(), Some("qwen3-coder"));
        assert_eq!(l.env.get("CLAUDE_CODE_ATTRIBUTION_HEADER").unwrap(), "0");
        assert_eq!(
            l.kind,
            BackendKind::ClaudeEndpoint {
                base_url: "http://localhost:11434".into(),
                api_key_env: Some("OLLAMA_API_KEY".into()),
                auth: EndpointAuth::XApiKey,
                config_dir: abs("home").join(".claude-sidekick-local"),
            }
        );

        let a = b.get("aider").unwrap();
        assert_eq!(a.backend_type(), BackendType::Command);
        assert_eq!(a.model, None);
        assert_eq!(a.pass_env, vec!["OPENAI_API_BASE".to_string()]);
        assert_eq!(
            a.kind,
            BackendKind::Command {
                command: vec![
                    "aider".into(),
                    "--yes-always".into(),
                    "--message".into(),
                    "{prompt}".into()
                ],
                prompt_via_stdin: false,
            }
        );
    }

    #[test]
    fn default_model_does_not_apply_to_explicit_backends() {
        let b = resolve_str(
            "default_model = \"opus\"\n[backends.sidekick]\ntype = \"claude-oauth\"\ndescription = \"x\"\ndefault = true\n",
        )
        .unwrap();
        assert_eq!(b.default_backend().model, None);
    }

    #[test]
    fn cost_and_other_defaults() {
        let b = resolve_str(
            r#"
[backends.sidekick]
type = "claude-oauth"
description = "o"
default = true
[backends.other]
type = "claude-oauth"
description = "o2"
[backends.ep]
type = "claude-endpoint"
description = "e"
base_url = "https://example.com/api/"
[backends.cmd]
type = "command"
description = "c"
command = ["tool"]
prompt_via_stdin = true
"#,
        )
        .unwrap();
        assert_eq!(b.get("sidekick").unwrap().cost, Cost::Quota);
        assert_eq!(b.get("other").unwrap().cost, Cost::Quota);
        assert_eq!(b.get("ep").unwrap().cost, Cost::Paid);
        assert_eq!(b.get("cmd").unwrap().cost, Cost::Free);
        assert_eq!(
            b.get("sidekick").unwrap().kind,
            BackendKind::ClaudeOauth {
                config_dir: abs("home").join(".claude-sidekick")
            }
        );
        assert_eq!(
            b.get("other").unwrap().kind,
            BackendKind::ClaudeOauth {
                config_dir: abs("home").join(".claude-sidekick-other")
            }
        );
        assert_eq!(
            b.get("ep").unwrap().kind,
            BackendKind::ClaudeEndpoint {
                base_url: "https://example.com/api".into(),
                api_key_env: None,
                auth: EndpointAuth::XApiKey,
                config_dir: abs("home").join(".claude-sidekick-ep"),
            }
        );
        assert_eq!(
            b.get("cmd").unwrap().kind,
            BackendKind::Command {
                command: vec!["tool".into()],
                prompt_via_stdin: true
            }
        );
    }

    #[test]
    fn bearer_auth_is_parsed() {
        let b = resolve_str(
            "[backends.ep]\ntype = \"claude-endpoint\"\ndescription = \"e\"\ndefault = true\nbase_url = \"http://h:1\"\nauth = \"bearer\"\n",
        )
        .unwrap();
        assert!(matches!(
            b.default_backend().kind,
            BackendKind::ClaudeEndpoint {
                auth: EndpointAuth::Bearer,
                ..
            }
        ));
    }

    #[test]
    fn two_defaults() {
        let e = err_of(
            "[backends.a]\ntype = \"command\"\ndescription = \"a\"\ndefault = true\ncommand = [\"x\", \"{prompt}\"]\n\
             [backends.b]\ntype = \"command\"\ndescription = \"b\"\ndefault = true\ncommand = [\"x\", \"{prompt}\"]\n",
        );
        assert!(e.contains("more than one default backend (a, b)"), "{e}");
    }

    #[test]
    fn zero_defaults_even_with_single_backend() {
        let e = err_of(
            "[backends.a]\ntype = \"command\"\ndescription = \"a\"\ncommand = [\"x\", \"{prompt}\"]\n",
        );
        assert!(e.contains("no default backend"), "{e}");
        assert!(
            e.contains("set `default = true` on exactly one of: a"),
            "{e}"
        );
        let e = err_of(
            "[backends.a]\ntype = \"command\"\ndescription = \"a\"\ncommand = [\"x\", \"{prompt}\"]\n\
             [backends.b]\ntype = \"command\"\ndescription = \"b\"\ndefault = false\ncommand = [\"x\", \"{prompt}\"]\n",
        );
        assert!(e.contains("one of: a, b"), "{e}");
    }

    fn one(body: &str) -> String {
        format!("[backends.x]\ndefault = true\n{body}")
    }

    #[test]
    fn missing_and_blank_description() {
        let e = err_of(&one(
            "type = \"command\"\ncommand = [\"x\", \"{prompt}\"]\n",
        ));
        assert_eq!(e, "backends.x: `description` is required");
        let e = err_of(&one(
            "type = \"command\"\ndescription = \"   \"\ncommand = [\"x\", \"{prompt}\"]\n",
        ));
        assert_eq!(e, "backends.x: `description` must not be blank");
    }

    #[test]
    fn description_is_trimmed() {
        let b = resolve_str(&one(
            "type = \"command\"\ndescription = \"  hi \"\ncommand = [\"x\", \"{prompt}\"]\n",
        ))
        .unwrap();
        assert_eq!(b.default_backend().description, "hi");
    }

    #[test]
    fn missing_type() {
        let e = err_of(&one("description = \"d\"\n"));
        assert!(e.starts_with("backends.x: `type` is required"), "{e}");
    }

    #[test]
    fn unknown_type_and_unknown_field_fail_to_parse() {
        assert!(toml::from_str::<Config>(&one("type = \"magic\"\ndescription = \"d\"\n")).is_err());
        assert!(toml::from_str::<Config>(&one(
            "type = \"claude-oauth\"\ndescription = \"d\"\nbogus = 1\n"
        ))
        .is_err());
        assert!(toml::from_str::<Config>(&one(
            "type = \"claude-oauth\"\ndescription = \"d\"\ncost = \"cheap\"\n"
        ))
        .is_err());
    }

    #[test]
    fn fields_not_allowed_for_type() {
        let e = err_of(&one(
            "type = \"command\"\ndescription = \"d\"\ncommand = [\"x\", \"{prompt}\"]\nbase_url = \"http://h\"\nmodel = \"m\"\n",
        ));
        assert!(
            e.contains("backends.x: field `base_url` is not allowed for type `command`"),
            "{e}"
        );
        assert!(
            e.contains("backends.x: field `model` is not allowed for type `command`"),
            "{e}"
        );
        let e = err_of(&one(
            "type = \"claude-oauth\"\ndescription = \"d\"\ncommand = [\"x\"]\nprompt_via_stdin = true\nauth = \"bearer\"\napi_key_env = \"K\"\nbase_url = \"http://h\"\n",
        ));
        for f in [
            "command",
            "prompt_via_stdin",
            "auth",
            "api_key_env",
            "base_url",
        ] {
            assert!(
                e.contains(&format!(
                    "field `{f}` is not allowed for type `claude-oauth`"
                )),
                "{f}: {e}"
            );
        }
        let e = err_of(&one(
            "type = \"command\"\ndescription = \"d\"\ncommand = [\"x\", \"{prompt}\"]\nconfig_dir = \"~/x\"\n",
        ));
        assert!(
            e.contains("field `config_dir` is not allowed for type `command`"),
            "{e}"
        );
    }

    #[test]
    fn bad_names() {
        for n in [
            "Upper",
            "_x",
            "-x",
            "\"a.b\"",
            "\"a b\"",
            &"a".repeat(33),
            "\"\"",
        ] {
            let e = err_of(&format!(
                "[backends.{n}]\ntype = \"command\"\ndescription = \"d\"\ndefault = true\ncommand = [\"x\", \"{{prompt}}\"]\n"
            ));
            assert!(e.contains("invalid backend name"), "{n}: {e}");
        }
        for n in ["a", "0", "a-b_c", &"a".repeat(32)] {
            assert!(valid_name(n), "{n}");
        }
    }

    #[test]
    fn base_url_checks() {
        let e = err_of(&one(
            "type = \"claude-endpoint\"\ndescription = \"d\"\nbase_url = \"localhost:11434\"\n",
        ));
        assert!(e.contains("must start with http:// or https://"), "{e}");
        let e = err_of(&one("type = \"claude-endpoint\"\ndescription = \"d\"\n"));
        assert!(e.contains("`base_url` is required"), "{e}");
        assert!(normalize_base_url("http://").is_err());
        assert!(normalize_base_url("https:///x").is_err());
        assert!(normalize_base_url("http://:80").is_err());
        assert!(normalize_base_url("ftp://h").is_err());
        assert!(normalize_base_url("http://a b").is_err());
        assert_eq!(normalize_base_url("HTTP://h:1//").unwrap(), "HTTP://h:1");
        assert_eq!(
            normalize_base_url(" https://u@h/v1/ ").unwrap(),
            "https://u@h/v1"
        );
    }

    #[test]
    fn api_key_env_checks() {
        let e = err_of(&one(
            "type = \"claude-endpoint\"\ndescription = \"d\"\nbase_url = \"http://h\"\napi_key_env = \"A=B\"\n",
        ));
        assert!(e.contains("`api_key_env`: invalid variable name"), "{e}");
        let e = err_of(&one(
            "type = \"claude-endpoint\"\ndescription = \"d\"\nbase_url = \"http://h\"\napi_key_env = \"claude_code_oauth_token\"\n",
        ));
        assert!(e.contains("must not be"), "{e}");
    }

    #[test]
    fn endpoint_config_dir_must_differ_from_oauth() {
        let dir = abs("shared").join("dir");
        let text = format!(
            "[backends.sidekick]\ntype = \"claude-oauth\"\ndescription = \"o\"\ndefault = true\nconfig_dir = {}\n\
             [backends.ep]\ntype = \"claude-endpoint\"\ndescription = \"e\"\nbase_url = \"http://h\"\nconfig_dir = {}\n",
            lit(&dir),
            // Trailing separator and `.` are normalised away.
            lit(&dir.join(".").join("")),
        );
        let e = err_of(&text);
        assert!(e.starts_with("backends.ep: config_dir"), "{e}");
        assert!(e.contains("claude-oauth backend `sidekick`"), "{e}");

        // Default endpoint dir vs. an oauth backend explicitly using it.
        let text =
            "[backends.a]\ntype = \"claude-oauth\"\ndescription = \"o\"\ndefault = true\nconfig_dir = \"~/.claude-sidekick-ep\"\n\
             [backends.ep]\ntype = \"claude-endpoint\"\ndescription = \"e\"\nbase_url = \"http://h\"\n"
        ;
        assert!(err_of(text).contains("claude-oauth backend `a`"));
    }

    #[cfg(windows)]
    #[test]
    fn config_dir_comparison_is_case_insensitive_on_windows() {
        let text = "[backends.sidekick]\ntype = \"claude-oauth\"\ndescription = \"o\"\ndefault = true\nconfig_dir = 'C:\\Dir\\X'\n\
             [backends.ep]\ntype = \"claude-endpoint\"\ndescription = \"e\"\nbase_url = \"http://h\"\nconfig_dir = 'c:\\dir\\x\\'\n";
        assert!(err_of(text).contains("backends.ep: config_dir"));
    }

    #[test]
    fn reserved_env_keys() {
        for k in [
            "ANTHROPIC_API_KEY",
            "anthropic_base_url",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CONFIG_DIR",
            "ANTHROPIC_AUTH_TOKEN",
            "AGENT_SIDEKICK_ANYTHING",
            "agent_sidekick_x",
        ] {
            let e = err_of(&one(&format!(
                "type = \"command\"\ndescription = \"d\"\ncommand = [\"x\", \"{{prompt}}\"]\nenv = {{ {k} = \"v\" }}\n"
            )));
            assert!(e.contains(&format!("env: `{k}` is reserved")), "{k}: {e}");
        }
        let e = err_of(&one(
            "type = \"command\"\ndescription = \"d\"\ncommand = [\"x\", \"{prompt}\"]\nenv = { \"\" = \"v\", \"A=B\" = \"v\" }\n",
        ));
        assert_eq!(e.matches("invalid variable name").count(), 2, "{e}");
        assert!(!is_reserved_env("AGENT_SIDEKICK"));
        assert!(!is_reserved_env("MY_ANTHROPIC_API_KEY"));
    }

    #[test]
    fn pass_env_restrictions() {
        let e = err_of(&one(
            "type = \"claude-oauth\"\ndescription = \"d\"\npass_env = [\"CLAUDE_CODE_OAUTH_TOKEN\", \"anthropic_api_key\", \"OK_VAR\"]\n",
        ));
        assert!(
            e.contains("pass_env: `CLAUDE_CODE_OAUTH_TOKEN` must not"),
            "{e}"
        );
        assert!(e.contains("pass_env: `anthropic_api_key` must not"), "{e}");
        assert!(!e.contains("OK_VAR"), "{e}");

        let e = err_of(&one(
            "type = \"claude-endpoint\"\ndescription = \"d\"\nbase_url = \"http://h\"\npass_env = [\"ANTHROPIC_AUTH_TOKEN\", \"AGENT_SIDEKICK_TOKEN\"]\n",
        ));
        assert_eq!(e.lines().count(), 2, "{e}");

        // command: Anthropic keys allowed, OAuth token not.
        let b = resolve_str(&one(
            "type = \"command\"\ndescription = \"d\"\ncommand = [\"aider\", \"{prompt}\"]\npass_env = [\"ANTHROPIC_API_KEY\", \"ANTHROPIC_AUTH_TOKEN\"]\n",
        ))
        .unwrap();
        assert_eq!(b.default_backend().pass_env.len(), 2);
        let e = err_of(&one(
            "type = \"command\"\ndescription = \"d\"\ncommand = [\"aider\", \"{prompt}\"]\npass_env = [\"CLAUDE_CODE_OAUTH_TOKEN\", \"AGENT_SIDEKICK_TOKEN\", \"\"]\n",
        ));
        assert_eq!(e.lines().count(), 3, "{e}");
    }

    #[test]
    fn global_pass_env_restrictions() {
        let cfg = parse(
            "pass_env = [\"ANTHROPIC_API_KEY\", \"X=Y\", \"CLAUDE_CODE_MAX_OUTPUT_TOKENS\"]\n",
        );
        let e = resolve_with(&cfg, &defaults()).unwrap_err();
        let lines: Vec<&str> = e.lines().collect();
        assert_eq!(lines.len(), 2, "{e}");
        assert!(lines.iter().all(|l| l.starts_with("pass_env: ")), "{e}");
        for k in [
            "CLAUDE_CODE_OAUTH_TOKEN",
            "AGENT_SIDEKICK_TOKEN",
            "ANTHROPIC_AUTH_TOKEN",
        ] {
            let cfg = parse(&format!("pass_env = [\"{k}\"]\n"));
            assert!(resolve_with(&cfg, &defaults()).is_err(), "{k}");
        }
        let cfg = parse("pass_env = [\"CLAUDE_CODE_MAX_OUTPUT_TOKENS\"]\n");
        assert!(resolve_with(&cfg, &defaults()).is_ok());
    }

    #[test]
    fn command_prompt_placeholder_rules() {
        let e = err_of(&one(
            "type = \"command\"\ndescription = \"d\"\ncommand = [\"aider\", \"--message\"]\n",
        ));
        assert!(e.contains("must contain `{prompt}`"), "{e}");
        let e = err_of(&one(
            "type = \"command\"\ndescription = \"d\"\ncommand = [\"aider\", \"{prompt}\"]\nprompt_via_stdin = true\n",
        ));
        assert!(e.contains("must not contain `{prompt}`"), "{e}");
        let e = err_of(&one(
            "type = \"command\"\ndescription = \"d\"\ncommand = []\n",
        ));
        assert!(e.contains("`command` must not be empty"), "{e}");
        let e = err_of(&one(
            "type = \"command\"\ndescription = \"d\"\ncommand = [\" \", \"{prompt}\"]\n",
        ));
        assert!(e.contains("`command[0]`"), "{e}");
        let e = err_of(&one("type = \"command\"\ndescription = \"d\"\n"));
        assert!(e.contains("`command` is required"), "{e}");
        // Placeholder embedded in a longer argument counts.
        assert!(resolve_str(&one(
            "type = \"command\"\ndescription = \"d\"\ncommand = [\"t\", \"--msg={prompt}\"]\n"
        ))
        .is_ok());
    }

    #[test]
    fn max_concurrent_zero() {
        let e = err_of(&one(
            "type = \"command\"\ndescription = \"d\"\ncommand = [\"x\", \"{prompt}\"]\nmax_concurrent = 0\n",
        ));
        assert_eq!(e, "backends.x: `max_concurrent` must be at least 1");
    }

    #[test]
    fn blank_model() {
        let e = err_of(&one(
            "type = \"claude-oauth\"\ndescription = \"d\"\nmodel = \" \"\n",
        ));
        assert_eq!(e, "backends.x: `model` must not be blank");
    }

    #[test]
    fn tilde_expansion_in_config_dir() {
        let b = resolve_str(&one(
            "type = \"claude-oauth\"\ndescription = \"d\"\nconfig_dir = \"~/sub/dir\"\n",
        ))
        .unwrap();
        assert_eq!(
            b.default_backend().kind,
            BackendKind::ClaudeOauth {
                config_dir: abs("home").join("sub/dir")
            }
        );
        let b = resolve_str(&one(
            "type = \"claude-oauth\"\ndescription = \"d\"\nconfig_dir = \"~\"\n",
        ))
        .unwrap();
        assert_eq!(
            b.default_backend().kind,
            BackendKind::ClaudeOauth {
                config_dir: abs("home")
            }
        );
        let text = one(&format!(
            "type = \"claude-oauth\"\ndescription = \"d\"\nconfig_dir = {}\n",
            lit(&abs("abs").join("dir"))
        ));
        assert_eq!(
            resolve_str(&text).unwrap().default_backend().kind,
            BackendKind::ClaudeOauth {
                config_dir: abs("abs").join("dir")
            }
        );
    }

    #[test]
    fn relative_config_dir_is_an_error() {
        let e = err_of(&one(
            "type = \"claude-endpoint\"\ndescription = \"d\"\nbase_url = \"http://h\"\nconfig_dir = \"rel/dir\"\n",
        ));
        assert!(e.contains("`config_dir` must be an absolute path"), "{e}");
        let e = err_of(&one(
            "type = \"claude-oauth\"\ndescription = \"d\"\nconfig_dir = \"\"\n",
        ));
        assert!(e.contains("`config_dir` must not be empty"), "{e}");
    }

    #[test]
    fn home_unknown() {
        let d = ResolveDefaults {
            home: None,
            legacy_agent_config_dir: None,
        };
        let cfg = parse(&one(
            "type = \"claude-oauth\"\ndescription = \"d\"\nconfig_dir = \"~/x\"\n",
        ));
        let e = resolve_with(&cfg, &d).unwrap_err();
        assert!(e.contains("home directory is unknown"), "{e}");
        let cfg = parse(&one("type = \"claude-oauth\"\ndescription = \"d\"\n"));
        let e = resolve_with(&cfg, &d).unwrap_err();
        assert!(e.contains("cannot determine the default config_dir"), "{e}");
        let cfg = parse(
            "[backends.sidekick]\ntype = \"claude-oauth\"\ndescription = \"d\"\ndefault = true\n",
        );
        let e = resolve_with(&cfg, &d).unwrap_err();
        assert!(e.contains("cannot determine the default config_dir"), "{e}");
    }

    #[test]
    fn multiple_errors_reported_together() {
        let e = err_of(
            r#"
[backends.Bad]
type = "command"
description = "d"
command = ["x"]

[backends.ep]
type = "claude-endpoint"
base_url = "localhost"
max_concurrent = 0

[backends.ok]
type = "claude-oauth"
description = "fine"
"#,
        );
        let lines: Vec<&str> = e.lines().collect();
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("backends.Bad: invalid backend name")),
            "{e}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("backends.Bad: `command` must contain")),
            "{e}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("backends.ep: `description` is required")),
            "{e}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("backends.ep: `max_concurrent`")),
            "{e}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("backends.ep: `base_url` must start")),
            "{e}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("backends: no default backend")),
            "{e}"
        );
        assert!(!e.contains("backends.ok"), "{e}");
        assert_eq!(lines.len(), 6, "{e}");
    }

    #[test]
    fn expand_tilde_cases() {
        let home = abs("h");
        let h = Some(home.as_path());
        assert_eq!(expand_tilde(Path::new("~"), h), home);
        assert_eq!(expand_tilde(Path::new("~/"), h), home);
        assert_eq!(expand_tilde(Path::new("~/a/b"), h), home.join("a/b"));
        assert_eq!(expand_tilde(Path::new("~\\a"), h), home.join("a"));
        assert_eq!(
            expand_tilde(Path::new("~user/a"), h),
            PathBuf::from("~user/a")
        );
        assert_eq!(expand_tilde(Path::new("a/~"), h), PathBuf::from("a/~"));
        assert_eq!(expand_tilde(Path::new("~/a"), None), PathBuf::from("~/a"));
    }

    #[test]
    fn normalize_dir_cases() {
        let d = abs("x");
        assert_eq!(
            normalize_dir(&d.join(".").join("y").join("..")),
            normalize_dir(&d)
        );
        assert_eq!(normalize_dir(&d.join("")), normalize_dir(&d));
        assert_ne!(normalize_dir(&d.join("y")), normalize_dir(&d));
    }
}
