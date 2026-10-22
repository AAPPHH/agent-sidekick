//! Validation of `[backends]` into [`Backends`].
//!
//! Rules (see also README / NOTES):
//! - no `[backends]` section: the implicit claude-oauth backend `claude` (v0.1 behaviour);
//!   the top-level `default_model` applies only to this implicit backend.
//! - `[backends]` present: at least one backend, exactly one with `default = true`.
//! - `cost` is optional and defaults by type: claude-oauth `quota`, claude-endpoint `paid`,
//!   command `free`.
//! - `models` (all types) is an allowlist of model names; `model` is the default and must be
//!   one of them (without `model` the default is `models[0]`). A command backend needs
//!   `models` exactly when an argument contains `{model}`.
//! - All problems are collected and reported together, one per line, each prefixed with
//!   `backends.<name>: ` (or `backends: ` / `pass_env: ` for cross-backend/global problems).

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use super::{
    command_uses_model, is_primary_oauth_name, Backend, BackendKind, BackendType, Backends,
    ClaudePermissions, CodexAuth, Cost, EndpointAuth, OpencodeProvider, RawAuth, RawBackend,
    IMPLICIT_BACKEND, OLD_IMPLICIT_BACKEND,
};
use crate::config::Config;
use crate::harness::HarnessKind;

/// Inputs that come from the environment, injected for testability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveDefaults {
    /// Home directory for `~` expansion and `~/.claude-sidekick-<name>` defaults.
    pub home: Option<PathBuf>,
    /// Config dir of the implicit/`claude`/`sidekick` claude-oauth backend when none is configured
    /// (`AGENT_SIDEKICK_CLAUDE_CONFIG_DIR` or `~/.claude-sidekick`).
    pub legacy_agent_config_dir: Option<PathBuf>,
    /// agent-sidekick's own config dir (`paths::config_dir`); opencode backends keep their
    /// isolated state in `<config_dir>/opencode/<name>`.
    pub config_dir: Option<PathBuf>,
}

impl ResolveDefaults {
    /// From the real environment (`paths::home_dir`, `paths::agent_claude_config_dir`,
    /// `paths::config_dir`).
    pub fn from_env() -> Self {
        Self {
            home: crate::paths::home_dir().ok(),
            legacy_agent_config_dir: crate::paths::agent_claude_config_dir().ok(),
            config_dir: crate::paths::config_dir().ok(),
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
/// Credentials that must never be passed through to an opencode backend (it gets its own
/// key in `AGENT_SIDEKICK_PROVIDER_KEY`).
const SECRET_PASS_ENV_OPENCODE: &[&str] = &[
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "AGENT_SIDEKICK_TOKEN",
    "AGENT_SIDEKICK_PROVIDER_KEY",
];
/// Credentials and isolation variables that must never be passed through to a codex backend
/// (its key goes through `CODEX_API_KEY`/`AGENT_SIDEKICK_PROVIDER_KEY`, its state through
/// `CODEX_HOME`, all set by the adapter).
const SECRET_PASS_ENV_CODEX: &[&str] = &[
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "AGENT_SIDEKICK_TOKEN",
    "AGENT_SIDEKICK_PROVIDER_KEY",
    "CODEX_API_KEY",
    "CODEX_ACCESS_TOKEN",
    "CODEX_HOME",
    "CODEX_SQLITE_HOME",
    "OPENAI_API_KEY",
];
/// Variables the `env` table of a codex backend may not set (the adapter sets them).
const RESERVED_ENV_CODEX: &[&str] = &[
    "CODEX_HOME",
    "CODEX_SQLITE_HOME",
    "CODEX_API_KEY",
    "CODEX_ACCESS_TOKEN",
];
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
        models: None,
        env: BTreeMap::new(),
        pass_env: Vec::new(),
        implicit: true,
        kind: BackendKind::ClaudeOauth {
            config_dir,
            permissions: ClaudePermissions::default(),
        },
        foreign_secret_env: Vec::new(),
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

    // `claude` and `sidekick` (its name before v0.6) use the same stored token, so only one
    // of them may be a claude-oauth backend.
    let primary: Vec<&str> = list
        .iter()
        .filter(|b| matches!(b.kind, BackendKind::ClaudeOauth { .. }))
        .map(|b| b.name.as_str())
        .filter(|n| is_primary_oauth_name(n))
        .collect();
    if primary.len() > 1 {
        errors.push(format!(
            "backends: `{IMPLICIT_BACKEND}` and `{OLD_IMPLICIT_BACKEND}` are both claude-oauth \
             backends, but they share one stored token; remove \
             `[backends.{OLD_IMPLICIT_BACKEND}]` (or rename one of them)"
        ));
    }

    // A claude-endpoint must not share its CLAUDE_CONFIG_DIR with a claude-oauth backend:
    // a saved claude.ai login there would otherwise stay active.
    for ep in &list {
        let BackendKind::ClaudeEndpoint { config_dir, .. } = &ep.kind else {
            continue;
        };
        let ep_norm = normalize_dir(config_dir);
        for oa in &list {
            if let BackendKind::ClaudeOauth {
                config_dir: oa_dir, ..
            } = &oa.kind
            {
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

    match raw.max_concurrent {
        Some(0) => errs.push("`max_concurrent` must be at least 1".to_owned()),
        Some(n) if n > crate::config::MAX_CONCURRENT_LIMIT => errs.push(format!(
            "`max_concurrent` must be at most {}, got {n}",
            crate::config::MAX_CONCURRENT_LIMIT
        )),
        _ => {}
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

    let kind = match backend_type_of(raw) {
        Ok(k) => k,
        Err(e) => {
            errs.push(e);
            return Err(errs);
        }
    };
    // How the backend's kind was written, for error messages.
    let written_as = match raw.kind {
        Some(t) => format!("type `{t}`"),
        None => harness_mode_label(kind).to_owned(),
    };

    if kind == BackendType::Codex {
        for k in env.keys() {
            if RESERVED_ENV_CODEX.iter().any(|r| r.eq_ignore_ascii_case(k)) {
                errs.push(format!(
                    "env: `{k}` is set by agent-sidekick for harness `codex`; reserved: {}",
                    RESERVED_ENV_CODEX.join(", ")
                ));
            }
        }
    }

    let pass_env = raw.pass_env.clone().unwrap_or_default();
    let denied = match kind {
        BackendType::Command => SECRET_PASS_ENV_COMMAND,
        BackendType::OpenCode => SECRET_PASS_ENV_OPENCODE,
        BackendType::Codex => SECRET_PASS_ENV_CODEX,
        _ => SECRET_PASS_ENV_CLAUDE,
    };
    // Legacy `type` configs keep the v0.3 wording; `harness` configs name what was written.
    let pass_env_scope = match raw.kind {
        Some(_) => format!("a {kind} backend"),
        None => format!("a backend with {written_as}"),
    };
    for e in check_pass_env(&pass_env, denied, &pass_env_scope) {
        errs.push(format!("pass_env: {e}"));
    }

    // Fields that are not allowed for this type.
    let claude: &[BackendType] = &[BackendType::ClaudeOauth, BackendType::ClaudeEndpoint];
    let set_fields: [(&str, bool, &[BackendType]); 17] = [
        (
            "config_dir",
            raw.config_dir.is_some(),
            &[BackendType::ClaudeOauth, BackendType::ClaudeEndpoint],
        ),
        (
            "base_url",
            raw.base_url.is_some(),
            &[
                BackendType::ClaudeEndpoint,
                BackendType::OpenCode,
                BackendType::Codex,
            ],
        ),
        (
            "api_key_env",
            raw.api_key_env.is_some(),
            &[
                BackendType::ClaudeEndpoint,
                BackendType::OpenCode,
                BackendType::Codex,
            ],
        ),
        ("provider", raw.provider.is_some(), &[BackendType::OpenCode]),
        (
            "harness_path",
            raw.harness_path.is_some(),
            &[BackendType::OpenCode, BackendType::Codex],
        ),
        (
            "read_only",
            raw.read_only.is_some(),
            &[BackendType::OpenCode, BackendType::Codex],
        ),
        (
            "request_timeout_s",
            raw.request_timeout_s.is_some(),
            &[BackendType::OpenCode],
        ),
        (
            "model_options",
            raw.model_options.is_some(),
            &[BackendType::OpenCode],
        ),
        (
            "auth",
            raw.auth.is_some(),
            &[BackendType::ClaudeEndpoint, BackendType::Codex],
        ),
        ("wire_api", raw.wire_api.is_some(), &[BackendType::Codex]),
        (
            "windows_sandbox",
            raw.windows_sandbox.is_some(),
            &[BackendType::Codex],
        ),
        ("command", raw.command.is_some(), &[BackendType::Command]),
        (
            "prompt_via_stdin",
            raw.prompt_via_stdin.is_some(),
            &[BackendType::Command],
        ),
        (
            "secret_env",
            raw.secret_env.is_some(),
            &[BackendType::Command],
        ),
        ("permission_mode", raw.permission_mode.is_some(), claude),
        ("allowed_tools", raw.allowed_tools.is_some(), claude),
        ("disallowed_tools", raw.disallowed_tools.is_some(), claude),
    ];
    for (field, is_set, allowed) in set_fields {
        if is_set && !allowed.contains(&kind) {
            errs.push(format!("field `{field}` is not allowed for {written_as}"));
        }
    }

    let (model, models) = resolve_models(raw, &mut errs);

    let backend_kind = match kind {
        BackendType::ClaudeOauth => {
            let dir = config_dir_for(name, raw, defaults, true, &mut errs);
            let permissions = claude_permissions(raw, &mut errs);
            dir.map(|config_dir| BackendKind::ClaudeOauth {
                config_dir,
                permissions,
            })
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
            check_api_key_env(raw, &mut errs);
            let auth = match raw.auth {
                None | Some(RawAuth::XApiKey) => Some(EndpointAuth::XApiKey),
                Some(RawAuth::Bearer) => Some(EndpointAuth::Bearer),
                Some(other) => {
                    errs.push(format!(
                        "`auth = \"{}\"` is not valid for {written_as}; use \"x-api-key\" or \
                         \"bearer\"",
                        other.as_str()
                    ));
                    None
                }
            };
            let dir = config_dir_for(name, raw, defaults, false, &mut errs);
            let permissions = claude_permissions(raw, &mut errs);
            match (base_url, dir, auth) {
                (Some(base_url), Some(config_dir), Some(auth)) => {
                    Some(BackendKind::ClaudeEndpoint {
                        base_url,
                        api_key_env: raw.api_key_env.clone(),
                        auth,
                        config_dir,
                        permissions,
                    })
                }
                _ => None,
            }
        }
        BackendType::Command => {
            let prompt_via_stdin = raw.prompt_via_stdin.unwrap_or(false);
            let secret_ok = match raw.secret_env.as_deref() {
                Some(var) => {
                    let e = check_secret_env(var, &env);
                    let ok = e.is_empty();
                    errs.extend(e);
                    ok
                }
                None => true,
            };
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
                    // command[0] is the program and is never substituted.
                    if PLACEHOLDERS.iter().any(|p| cmd[0].contains(p)) {
                        errs.push(
                            "`command[0]` (the program) must not contain a placeholder \
                             ({prompt}, {cwd}, {max_turns}, {model}); only the arguments are \
                             substituted"
                                .to_owned(),
                        );
                    }
                    // {model} lands in argv: only names from the `models` allowlist.
                    match (command_uses_model(cmd), raw.models.is_some()) {
                        (true, false) => errs.push(
                            "`models` is required when `command` uses `{model}` (the list of \
                             model names a request may put into the command line)"
                                .to_owned(),
                        ),
                        (false, true) => errs.push(
                            "`models` has no effect without `{model}` in `command`; add \
                             `{model}` to an argument or remove `models`"
                                .to_owned(),
                        ),
                        _ => {}
                    }
                    if raw.model.is_some() && !command_uses_model(cmd) {
                        errs.push(
                            "`model` has no effect without `{model}` in `command`; add \
                             `{model}` to an argument (with `models`) or remove `model`"
                                .to_owned(),
                        );
                    }
                    let has_prompt = cmd[1..].iter().any(|a| a.contains("{prompt}"));
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
                    (errs.len() == before && secret_ok).then(|| BackendKind::Command {
                        command: cmd.to_vec(),
                        prompt_via_stdin,
                        secret_env: raw.secret_env.clone(),
                    })
                }
            }
        }
        BackendType::OpenCode => opencode_kind(name, raw, defaults, &mut errs),
        BackendType::Codex => codex_kind(name, raw, defaults, &mut errs),
    };
    // opencode always passes `--model`: it needs a default model, and `models` is the
    // allowlist (a lone `model` is the only allowed one).
    let models = match (kind, &model, models) {
        (BackendType::OpenCode, None, None) => {
            if raw.model.is_none() && raw.models.is_none() {
                errs.push(
                    "`models` (or `model`) is required for harness `opencode`: the model \
                     name(s) the provider serves, e.g. models = [\"gpt-oss-120b\"]"
                        .to_owned(),
                );
            }
            None
        }
        (BackendType::OpenCode, Some(m), None) => Some(vec![m.clone()]),
        (_, _, models) => models,
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
        cost: raw.cost.unwrap_or(match (&kind_val, kind) {
            (_, BackendType::ClaudeOauth) => Cost::Quota,
            (_, BackendType::ClaudeEndpoint) => Cost::Paid,
            (_, BackendType::Command) => Cost::Free,
            (
                BackendKind::OpenCode {
                    provider: OpencodeProvider::Ollama,
                    ..
                },
                _,
            ) => Cost::Free,
            (_, BackendType::OpenCode) => Cost::Paid,
            (
                BackendKind::Codex {
                    auth: CodexAuth::Chatgpt,
                    ..
                },
                _,
            ) => Cost::Quota,
            (_, BackendType::Codex) => Cost::Paid,
        }),
        is_default: raw.default.unwrap_or(false),
        max_concurrent: raw.max_concurrent,
        model,
        models,
        env,
        pass_env,
        implicit: false,
        kind: kind_val,
        foreign_secret_env: Vec::new(),
    })
}

/// The internal [`BackendType`] of a backend from `harness` and/or the legacy `type`:
/// - only `type`: as before (`claude-oauth`, `claude-endpoint`, `command`);
/// - only `harness`: `claude` without `base_url` is claude-oauth, with `base_url`
///   claude-endpoint; `command` is command; `opencode` and `codex` their own types;
/// - both: they must agree (`type` maps onto its harness, and for `claude` the mode implied
///   by `base_url` is checked by the field rules of the type); otherwise a config error.
fn backend_type_of(raw: &RawBackend) -> Result<BackendType, String> {
    match (raw.kind, raw.harness) {
        // The legacy `type` stays limited to the v0.3 values.
        (Some(ty @ (BackendType::OpenCode | BackendType::Codex)), _) => Err(format!(
            "`type = \"{ty}\"` is not supported; write `harness = \"{ty}\"` (the legacy `type` \
             only knows claude-oauth, claude-endpoint and command)"
        )),
        (None, Some(HarnessKind::OpenCode)) => Ok(BackendType::OpenCode),
        (None, Some(HarnessKind::Codex)) => Ok(BackendType::Codex),
        (Some(ty), None) => Ok(ty),
        (None, Some(HarnessKind::Claude)) => Ok(if raw.base_url.is_some() {
            BackendType::ClaudeEndpoint
        } else {
            BackendType::ClaudeOauth
        }),
        (None, Some(HarnessKind::Command)) => Ok(BackendType::Command),
        (Some(ty), Some(h)) if HarnessKind::of_type(ty) == h => Ok(ty),
        (Some(ty), Some(h)) => Err(format!(
            "`type = \"{ty}\"` and `harness = \"{h}\"` disagree (type `{ty}` means \
             harness `{}`); set only `harness`",
            HarnessKind::of_type(ty)
        )),
        // Starts with the v0.3 wording, which tests and users may rely on.
        (None, None) => Err(
            "`type` is required (one of: claude-oauth, claude-endpoint, \
             command), or `harness` (one of: claude, opencode, codex, command)"
                .to_owned(),
        ),
    }
}

/// How a backend configured with `harness` (no `type`) is named in error messages.
fn harness_mode_label(kind: BackendType) -> &'static str {
    match kind {
        BackendType::ClaudeOauth => "harness `claude` without `base_url` (claude-oauth mode)",
        BackendType::ClaudeEndpoint => "harness `claude` with `base_url` (claude-endpoint mode)",
        BackendType::Command => "harness `command`",
        BackendType::OpenCode => "harness `opencode`",
        BackendType::Codex => "harness `codex`",
    }
}

/// `harness_path` of an opencode/codex backend: absolute (or `~`), never empty.
fn harness_path_of(
    raw: &RawBackend,
    defaults: &ResolveDefaults,
    errs: &mut Vec<String>,
) -> Option<PathBuf> {
    match &raw.harness_path {
        None => None,
        Some(p) if p.as_os_str().is_empty() => {
            errs.push("`harness_path` must not be empty".to_owned());
            None
        }
        Some(p) => {
            let expanded = expand_tilde(p, defaults.home.as_deref());
            if expanded.is_absolute() {
                Some(expanded)
            } else {
                errs.push(format!(
                    "`harness_path` must be an absolute path (or start with `~`), got {}",
                    p.display()
                ));
                None
            }
        }
    }
}

/// The [`BackendKind::Codex`] of `[backends.<name>]` with `harness = "codex"`:
/// - `auth`: `chatgpt` (default) or `api-key`;
/// - `base_url`, `api_key_env` and `wire_api` only with `api-key`; `wire_api` must be
///   `responses` (the only wire API Codex 0.163 supports) and only next to `base_url`;
/// - `harness_path` absolute (or `~`); the backend's `CODEX_HOME` is
///   `<config-dir>/codex/<name>`.
fn codex_kind(
    name: &str,
    raw: &RawBackend,
    defaults: &ResolveDefaults,
    errs: &mut Vec<String>,
) -> Option<BackendKind> {
    let before = errs.len();
    let auth = match raw.auth {
        Some(RawAuth::Chatgpt) => Some(CodexAuth::Chatgpt),
        Some(RawAuth::ApiKey) => Some(CodexAuth::ApiKey),
        Some(other) => {
            errs.push(format!(
                "`auth = \"{}\"` is not valid for harness `codex`; use \"chatgpt\" (your \
                 ChatGPT login) or \"api-key\" (an OpenAI API key)",
                other.as_str()
            ));
            None
        }
        // Default: the ChatGPT login (the user's subscription).
        None => Some(CodexAuth::Chatgpt),
    };
    let chatgpt = auth == Some(CodexAuth::Chatgpt);
    for (field, set) in [
        ("base_url", raw.base_url.is_some()),
        ("api_key_env", raw.api_key_env.is_some()),
        ("wire_api", raw.wire_api.is_some()),
    ] {
        if chatgpt && set {
            errs.push(format!(
                "`{field}` is not allowed with `auth = \"chatgpt\"` (the ChatGPT login always \
                 talks to OpenAI); use `auth = \"api-key\"` for another provider"
            ));
        }
    }
    let base_url = match raw.base_url.as_deref() {
        Some(u) if !chatgpt => match normalize_base_url(u) {
            Ok(u) => Some(u),
            Err(e) => {
                errs.push(e);
                None
            }
        },
        _ => None,
    };
    if let Some(w) = raw.wire_api.as_deref() {
        if w != "responses" {
            errs.push(format!(
                "`wire_api = {w:?}` is not supported: Codex only speaks the Responses API \
                 (`wire_api = \"responses\"`); an endpoint that only offers /chat/completions \
                 cannot be used with harness `codex` (use harness `opencode` for it)"
            ));
        } else if raw.base_url.is_none() && !chatgpt {
            errs.push(
                "`wire_api` has no effect without `base_url`; remove it or set `base_url`"
                    .to_owned(),
            );
        }
    }
    check_api_key_env(raw, errs);
    let harness_path = harness_path_of(raw, defaults, errs);
    let base_dir = match &defaults.config_dir {
        Some(dir) => dir.join("codex").join(name),
        None => {
            errs.push(
                "cannot determine agent-sidekick's config directory for the codex state (set \
                 AGENT_SIDEKICK_CONFIG_DIR)"
                    .to_owned(),
            );
            return None;
        }
    };
    if errs.len() != before {
        return None;
    }
    Some(BackendKind::Codex {
        auth: auth?,
        base_url,
        api_key_env: raw.api_key_env.clone(),
        harness_path,
        read_only: raw.read_only.unwrap_or(false),
        windows_sandbox: raw.windows_sandbox.unwrap_or_default(),
        base_dir,
    })
}

fn claude_permissions(raw: &RawBackend, errs: &mut Vec<String>) -> ClaudePermissions {
    for (field, list) in [
        ("allowed_tools", &raw.allowed_tools),
        ("disallowed_tools", &raw.disallowed_tools),
    ] {
        for (i, t) in list.iter().flatten().enumerate() {
            if let Some(p) = tool_rule_problem(t) {
                errs.push(format!("`{field}[{i}]` ({t:?}) {p}"));
            }
        }
    }
    ClaudePermissions {
        mode: raw.permission_mode.unwrap_or_default(),
        allowed_tools: raw.allowed_tools.clone(),
        disallowed_tools: raw.disallowed_tools.clone().unwrap_or_default(),
    }
}

fn tool_rule_problem(rule: &str) -> Option<&'static str> {
    if rule.trim().is_empty() {
        Some("must not be blank")
    } else if rule.chars().any(char::is_control) {
        Some("must not contain control characters")
    } else if rule.trim() != rule {
        Some("must not have leading or trailing whitespace")
    } else if rule.starts_with('-') {
        Some("must not start with `-` (it would be read as a claude option)")
    } else {
        None
    }
}

/// `api_key_env` (claude-endpoint, opencode): a valid name and never one of the agent's own
/// secrets.
fn check_api_key_env(raw: &RawBackend, errs: &mut Vec<String>) {
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
}

/// The [`BackendKind::OpenCode`] of `[backends.<name>]` with `harness = "opencode"`:
/// `provider` required; `base_url` required for openai-compatible/ollama and not allowed for
/// openrouter; `api_key_env` not allowed for ollama (no key); `harness_path` absolute (or
/// `~`); the isolation dir is `<config-dir>/opencode/<name>`.
fn opencode_kind(
    name: &str,
    raw: &RawBackend,
    defaults: &ResolveDefaults,
    errs: &mut Vec<String>,
) -> Option<BackendKind> {
    let before = errs.len();
    let Some(provider) = raw.provider else {
        errs.push(
            "`provider` is required for harness `opencode` (one of: openai-compatible, ollama, \
             openrouter)"
                .to_owned(),
        );
        return None;
    };
    let base_url = match (provider, raw.base_url.as_deref()) {
        (OpencodeProvider::Openrouter, Some(_)) => {
            errs.push(
                "`base_url` is not allowed for provider `openrouter` (OpenCode's built-in \
                 OpenRouter provider is used); use provider `openai-compatible` for another URL"
                    .to_owned(),
            );
            None
        }
        (OpencodeProvider::Openrouter, None) => None,
        (p, None) => {
            errs.push(format!(
                "`base_url` is required for provider `{p}` (e.g. {})",
                match p {
                    OpencodeProvider::Ollama => "http://localhost:11434",
                    _ => "https://chat.kiconnect.nrw/api/v1",
                }
            ));
            None
        }
        (_, Some(u)) => match normalize_base_url(u) {
            Ok(u) => Some(u),
            Err(e) => {
                errs.push(e);
                None
            }
        },
    };
    if raw.api_key_env.is_some() && !provider.needs_key() {
        errs.push(format!(
            "`api_key_env` is not allowed for provider `{provider}` (it needs no key)"
        ));
    }
    check_api_key_env(raw, errs);
    let harness_path = harness_path_of(raw, defaults, errs);
    let base_dir = match &defaults.config_dir {
        Some(dir) => dir.join("opencode").join(name),
        None => {
            errs.push(
                "cannot determine agent-sidekick's config directory for the opencode state \
                 (set AGENT_SIDEKICK_CONFIG_DIR)"
                    .to_owned(),
            );
            return None;
        }
    };
    let request_timeout_s = raw.request_timeout_s;
    if let Some(t) = request_timeout_s.filter(|t| !(1..=3600).contains(t)) {
        errs.push(format!(
            "`request_timeout_s` must be between 1 and 3600 (got {t})"
        ));
    }
    let model_options = raw.model_options.clone().unwrap_or_default();
    if raw.model_options.is_some() && provider == OpencodeProvider::Openrouter {
        errs.push(
            "`model_options` is not allowed for provider `openrouter` (only for              openai-compatible and ollama)"
                .to_owned(),
        );
    }
    for key in ["name", "tool_call"] {
        if model_options.contains_key(key) {
            errs.push(format!(
                "`model_options` must not set `{key}` (agent-sidekick sets it for every model)"
            ));
        }
    }
    for (k, v) in &model_options {
        check_json_value(&format!("model_options.{k}"), v, errs);
    }
    if errs.len() != before {
        return None;
    }
    Some(BackendKind::OpenCode {
        provider,
        base_url,
        api_key_env: raw.api_key_env.clone(),
        harness_path,
        read_only: raw.read_only.unwrap_or(false),
        request_timeout_s,
        model_options,
        base_dir,
    })
}

fn check_json_value(path: &str, v: &serde_json::Value, errs: &mut Vec<String>) {
    match v {
        serde_json::Value::Null => errs.push(format!(
            "`{path}`: not a JSON-compatible value (nan and inf are not allowed)"
        )),
        serde_json::Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                check_json_value(&format!("{path}[{i}]"), item, errs);
            }
        }
        serde_json::Value::Object(map) => {
            if map.contains_key(TOML_DATETIME_FIELD) {
                errs.push(format!(
                    "`{path}`: not a JSON-compatible value (TOML dates and times are not allowed)"
                ));
                return;
            }
            for (k, item) in map {
                check_json_value(&format!("{path}.{k}"), item, errs);
            }
        }
        _ => {}
    }
}

const TOML_DATETIME_FIELD: &str = "$__toml_private_datetime";

/// Placeholders of a command template (never allowed in `command[0]`).
const PLACEHOLDERS: [&str; 4] = ["{prompt}", "{cwd}", "{max_turns}", "{model}"];

/// Problem with one model name, if any. Names may contain inner spaces (some gateways use
/// display names as ids) but no control characters, no leading/trailing whitespace and no
/// leading `-` (it could be taken as an option where the name lands in argv).
fn model_name_problem(name: &str) -> Option<&'static str> {
    if name.trim().is_empty() {
        Some("must not be blank")
    } else if name.chars().any(char::is_control) {
        Some("must not contain control characters")
    } else if name.trim() != name {
        Some("must not have leading or trailing whitespace")
    } else if name.starts_with('-') {
        Some("must not start with `-`")
    } else {
        None
    }
}

/// `model` (the default) and `models` (the allowlist) of a backend: every name valid,
/// `models` non-empty without duplicates, `model` one of `models`. Without `model` the default
/// is `models[0]`. (The command-specific `{model}` rules are checked with `command`.)
fn resolve_models(
    raw: &RawBackend,
    errs: &mut Vec<String>,
) -> (Option<String>, Option<Vec<String>>) {
    let before = errs.len();
    // `model` keeps its v0.2 behaviour of being trimmed.
    let model = raw.model.as_deref().map(str::trim);
    if let Some(m) = model {
        if let Some(p) = model_name_problem(m) {
            errs.push(format!("`model` {p}"));
        }
    }
    if let Some(list) = &raw.models {
        if list.is_empty() {
            errs.push("`models` must not be empty (omit it to allow any model)".to_owned());
        }
        for (i, m) in list.iter().enumerate() {
            if let Some(p) = model_name_problem(m) {
                errs.push(format!("`models[{i}]` ({m:?}) {p}"));
            } else if list[..i].contains(m) {
                errs.push(format!("`models` lists {m:?} more than once"));
            }
        }
        if let Some(m) = model {
            if !m.is_empty() && !list.iter().any(|x| x == m) {
                errs.push(format!(
                    "`model` {m:?} (the default) must be one of `models`: {}",
                    list.join(", ")
                ));
            }
        }
    }
    if errs.len() != before {
        return (None, None);
    }
    let models = raw.models.clone();
    let default = model
        .map(str::to_owned)
        .or_else(|| models.as_ref().and_then(|l| l.first().cloned()));
    (default, models)
}

/// `config_dir` of a claude-* backend: configured (with `~` expanded, must be absolute) or
/// the default (`claude` or `sidekick` oauth: the legacy dir; else `~/.claude-sidekick-<name>`).
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
        None if oauth && is_primary_oauth_name(name) => {
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

/// Errors for the `secret_env` of a command backend: a portable variable name
/// (`[A-Za-z_][A-Za-z0-9_]*`), not one of the agent's own secrets (they are never handed to
/// a command backend), and not also set by the backend's `env` table. `ANTHROPIC_API_KEY`
/// and the like are allowed (e.g. Aider with Anthropic models).
fn check_secret_env(var: &str, env: &BTreeMap<String, String>) -> Vec<String> {
    let mut errs = Vec::new();
    if !valid_secret_env_name(var) {
        errs.push(format!(
            "`secret_env`: invalid variable name {var:?} (use letters, digits and `_`, not starting with a digit)"
        ));
        return errs;
    }
    let reserved_prefix = var
        .get(..RESERVED_ENV_PREFIX.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(RESERVED_ENV_PREFIX));
    if reserved_prefix
        || SECRET_PASS_ENV_COMMAND
            .iter()
            .any(|f| f.eq_ignore_ascii_case(var))
    {
        errs.push(format!(
            "`secret_env` must not be `{var}` (reserved: {}, {RESERVED_ENV_PREFIX}*)",
            SECRET_PASS_ENV_COMMAND.join(", ")
        ));
    }
    if env.keys().any(|k| k.eq_ignore_ascii_case(var)) {
        errs.push(format!(
            "`secret_env` `{var}` is also set in `env`; remove it there (the stored secret is set by agent-sidekick)"
        ));
    }
    errs
}

/// `^[A-Za-z_][A-Za-z0-9_]*$`
fn valid_secret_env_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && (b[0].is_ascii_alphabetic() || b[0] == b'_')
        && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
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
    // Never echo a URL that may carry credentials (`user:password@host`).
    let shown = if u.contains('@') {
        "(not shown: it contains '@')".to_owned()
    } else {
        format!("{url:?}")
    };
    if u.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!(
            "`base_url` must not contain whitespace, got {shown}"
        ));
    }
    let rest = ["http://", "https://"].iter().find_map(|s| {
        u.get(..s.len())
            .filter(|p| p.eq_ignore_ascii_case(s))
            .map(|_| &u[s.len()..])
    });
    let Some(rest) = rest else {
        return Err(format!(
            "`base_url` must start with http:// or https://, got {shown}"
        ));
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        // Credentials in the URL would end up in CLI output, logs and the probe; keys
        // belong in the keyring (`setup --backend`) or in `api_key_env`.
        return Err(
            "`base_url` must not contain credentials (`user:password@`); store the key with \
             `agent-sidekick setup --backend <name>` or use `api_key_env`"
                .to_owned(),
        );
    }
    if authority.is_empty() || authority.starts_with(':') {
        return Err(format!("`base_url` has no host, got {shown}"));
    }
    if !authority.is_ascii() {
        return Err(format!(
            "`base_url` host must be ASCII; write an internationalized domain name in its \
             punycode form (xn--...), got {shown}"
        ));
    }
    // Host and optional port; an IPv6 literal is bracketed (`[::1]:8080`).
    let port = if let Some(v6) = authority.strip_prefix('[') {
        match v6.split_once(']') {
            Some((_, "")) => None,
            Some((_, rest)) => match rest.strip_prefix(':') {
                Some(p) => Some(p),
                None => return Err(format!("`base_url` has an invalid host, got {shown}")),
            },
            None => return Err(format!("`base_url` has an invalid host, got {shown}")),
        }
    } else {
        authority.split_once(':').map(|(_, p)| p)
    };
    if let Some(p) = port {
        let valid = !p.is_empty()
            && p.bytes().all(|b| b.is_ascii_digit())
            && p.parse::<u16>().is_ok_and(|n| n >= 1);
        if !valid {
            return Err(format!(
                "`base_url` port must be a number from 1 to 65535, got {shown}"
            ));
        }
    }
    // The reachability probe parses the URL with the same parser; reject what it rejects.
    if u.parse::<ureq::http::Uri>().is_err() {
        return Err(format!(
            "`base_url` is not a valid URL (expected http://host[:port][/path]), got {shown}"
        ));
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
            config_dir: Some(abs("config").join("agent-sidekick")),
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
        assert_eq!(s.name, "claude");
        assert!(s.implicit && s.is_default);
        assert_eq!(s.cost, Cost::Quota);
        assert_eq!(s.description, IMPLICIT_DESCRIPTION);
        assert_eq!(s.model.as_deref(), Some("sonnet"));
        assert!(s.env.is_empty() && s.pass_env.is_empty());
        assert_eq!(s.max_concurrent, None);
        assert_eq!(
            s.kind,
            BackendKind::ClaudeOauth {
                config_dir: abs("home").join(".claude-sidekick"),
                permissions: Default::default(),
            }
        );
    }

    #[test]
    fn implicit_backend_needs_legacy_dir() {
        let d = ResolveDefaults {
            home: None,
            legacy_agent_config_dir: None,
            config_dir: None,
        };
        let e = resolve_with(&Config::default(), &d).unwrap_err();
        assert!(e.starts_with("backends.claude: "), "{e}");
    }

    #[test]
    fn declared_claude_and_sidekick_default_to_the_legacy_dir() {
        for name in ["claude", "sidekick"] {
            let b = resolve_str(&format!(
                "[backends.{name}]\ntype = \"claude-oauth\"\ndescription = \"d\"\ndefault = true\n"
            ))
            .unwrap();
            assert_eq!(
                b.get(name).unwrap().kind,
                BackendKind::ClaudeOauth {
                    config_dir: abs("home").join(".claude-sidekick"),
                    permissions: Default::default(),
                },
                "{name}"
            );
        }
    }

    #[test]
    fn claude_and_sidekick_oauth_together_is_an_error() {
        let e = err_of(
            "[backends.claude]\ntype = \"claude-oauth\"\ndescription = \"a\"\ndefault = true\n\
             [backends.sidekick]\ntype = \"claude-oauth\"\ndescription = \"b\"\n\
             config_dir = \"~/.claude-sidekick-old\"\n",
        );
        assert!(
            e.contains("`claude` and `sidekick` are both claude-oauth"),
            "{e}"
        );
        // A non-oauth backend named `sidekick` does not share the token.
        resolve_str(
            "[backends.claude]\ntype = \"claude-oauth\"\ndescription = \"a\"\ndefault = true\n\
             [backends.sidekick]\ntype = \"command\"\ndescription = \"b\"\n\
             command = [\"x\", \"{prompt}\"]\n",
        )
        .unwrap();
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
                config_dir: abs("home").join(".claude-sidekick"),
                permissions: Default::default(),
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
                permissions: Default::default(),
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
                secret_env: None,
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
                config_dir: abs("home").join(".claude-sidekick"),
                permissions: Default::default(),
            }
        );
        assert_eq!(
            b.get("other").unwrap().kind,
            BackendKind::ClaudeOauth {
                config_dir: abs("home").join(".claude-sidekick-other"),
                permissions: Default::default(),
            }
        );
        assert_eq!(
            b.get("ep").unwrap().kind,
            BackendKind::ClaudeEndpoint {
                base_url: "https://example.com/api".into(),
                api_key_env: None,
                auth: EndpointAuth::XApiKey,
                config_dir: abs("home").join(".claude-sidekick-ep"),
                permissions: Default::default(),
            }
        );
        assert_eq!(
            b.get("cmd").unwrap().kind,
            BackendKind::Command {
                command: vec!["tool".into()],
                prompt_via_stdin: true,
                secret_env: None,
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
        // `model` is allowed for command backends, but only together with `{model}`.
        assert!(!e.contains("field `model`"), "{e}");
        assert!(
            e.contains("backends.x: `model` has no effect without `{model}` in `command`"),
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
            normalize_base_url(" https://h/v1/ ").unwrap(),
            "https://h/v1"
        );
        // An '@' later in the path is fine.
        assert!(normalize_base_url("https://h/v1/@x").is_ok());
    }

    #[test]
    fn base_url_port_and_host_must_be_valid() {
        for bad in [
            "http://h:abc",
            "http://h:99999",
            "http://h:0",
            "http://h:",
            "http://h:-1",
        ] {
            let e = normalize_base_url(bad).expect_err(bad);
            assert!(
                e.contains("port must be a number from 1 to 65535"),
                "{bad}: {e}"
            );
        }
        let e = normalize_base_url("https://b\u{fc}cher.example/v1").unwrap_err();
        assert!(e.contains("must be ASCII") && e.contains("xn--"), "{e}");
        assert!(normalize_base_url("http://[::1").is_err());
        assert!(normalize_base_url("http://[::1]x").is_err());
        assert!(normalize_base_url("http://[::1]:99999").is_err());
        // Accepted forms, also by the probe's parser.
        for good in [
            "http://h:65535",
            "http://h:1/v1",
            "https://xn--bcher-kva.example/v1",
            "http://[::1]:8080/v1",
            "http://[::1]",
            "http://127.0.0.1:11434",
        ] {
            assert!(normalize_base_url(good).is_ok(), "{good}");
        }
        // Reported as a config error of the backend.
        let e = err_of(&one(
            "type = \"claude-endpoint\"\ndescription = \"d\"\nbase_url = \"http://h:99999\"\n",
        ));
        assert!(e.starts_with("backends.x: `base_url` port"), "{e}");
    }

    #[test]
    fn base_url_with_credentials_is_rejected_without_echoing_them() {
        for url in [
            "https://user:s3cretpw@proxy.example/v1",
            "http://u@h",
            "https://user:s3cretpw@ bad",
            "user:s3cretpw@host",
        ] {
            let e = normalize_base_url(url).unwrap_err();
            assert!(!e.contains("s3cretpw"), "{url}: {e}");
            assert!(!e.contains("proxy.example"), "{url}: {e}");
        }
        let e = err_of(&one(
            "type = \"claude-endpoint\"\ndescription = \"d\"\nbase_url = \"https://user:s3cretpw@proxy.example\"\n",
        ));
        assert!(e.contains("must not contain credentials"), "{e}");
        assert!(!e.contains("s3cretpw"), "{e}");
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
        // {prompt} only in command[0] does not count (command[0] is never substituted).
        for cmd in ["[\"x{prompt}\"]", "[\"{prompt}\"]"] {
            let e = err_of(&one(&format!(
                "type = \"command\"\ndescription = \"d\"\ncommand = {cmd}\n"
            )));
            assert!(e.contains("must contain `{prompt}`"), "{cmd}: {e}");
            assert!(
                e.contains("`command[0]` (the program) must not contain a placeholder"),
                "{e}"
            );
        }
        for cmd in [
            "[\"{cwd}/x\", \"{prompt}\"]",
            "[\"x{max_turns}\", \"{prompt}\"]",
        ] {
            let e = err_of(&one(&format!(
                "type = \"command\"\ndescription = \"d\"\ncommand = {cmd}\n"
            )));
            assert!(e.contains("must not contain a placeholder"), "{cmd}: {e}");
        }
        // With prompt_via_stdin, {prompt} in command[0] is rejected as a placeholder.
        let e = err_of(&one(
            "type = \"command\"\ndescription = \"d\"\ncommand = [\"x{prompt}\"]\nprompt_via_stdin = true\n",
        ));
        assert!(e.contains("must not contain a placeholder"), "{e}");
        // Placeholder embedded in a longer argument counts.
        assert!(resolve_str(&one(
            "type = \"command\"\ndescription = \"d\"\ncommand = [\"t\", \"--msg={prompt}\"]\n"
        ))
        .is_ok());
    }

    fn cmd_with(extra: &str) -> String {
        one(&format!(
            "type = \"command\"\ndescription = \"d\"\ncommand = [\"aider\", \"{{prompt}}\"]\n{extra}"
        ))
    }

    #[test]
    fn secret_env_on_command_backend() {
        let b = resolve_str(&cmd_with(
            "secret_env = \"OPENAI_API_KEY\"\nenv = { OPENAI_API_BASE = \"https://h/api/v1\" }\n",
        ))
        .unwrap();
        let d = b.default_backend();
        assert_eq!(
            d.kind,
            BackendKind::Command {
                command: vec!["aider".into(), "{prompt}".into()],
                prompt_via_stdin: false,
                secret_env: Some("OPENAI_API_KEY".into()),
            }
        );
        assert_eq!(
            d.secret_id(),
            Some(crate::token::SecretId::Command("x".into()))
        );
        // Without secret_env a command backend has no stored secret.
        let b = resolve_str(&cmd_with("")).unwrap();
        assert_eq!(b.default_backend().secret_id(), None);
        // Anthropic keys are fine for a command backend (e.g. Aider with Claude models).
        for var in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "_X", "a1"] {
            assert!(
                resolve_str(&cmd_with(&format!("secret_env = \"{var}\"\n"))).is_ok(),
                "{var}"
            );
        }
    }

    /// The commented example in the repository stays valid.
    #[test]
    fn example_config_is_valid() {
        let b = resolve_str(include_str!("../../examples/config.toml")).unwrap();
        assert_eq!(b.names(), vec!["aider", "claude", "kiconnect", "local"]);
        assert!(matches!(
            &b.get("kiconnect").unwrap().kind,
            BackendKind::Command { secret_env: Some(v), .. } if v == "OPENAI_API_KEY"
        ));
        let kic = b.get("kiconnect").unwrap();
        assert_eq!(
            kic.models,
            Some(vec![
                "openai-gpt-oss-120b".to_owned(),
                "qwen-qwen3-8-27b".to_owned()
            ])
        );
        assert_eq!(kic.model.as_deref(), Some("openai-gpt-oss-120b"));
        assert!(!b.get("aider").unwrap().takes_model());
    }

    /// The provider-per-backend example in docs/REFERENCE.md resolves.
    #[test]
    fn readme_kiconnect_example_is_valid() {
        let readme = include_str!("../../docs/REFERENCE.md").replace("\r\n", "\n");
        let start = readme.find("```toml\n[backends.kiconnect]").unwrap() + "```toml\n".len();
        let end = start + readme[start..].find("```").unwrap();
        let toml = format!(
            "{}[backends.s]\ntype = \"claude-oauth\"\ndescription = \"d\"\ndefault = true\n",
            &readme[start..end]
        );
        let b = resolve_str(&toml).unwrap();
        let kic = b.get("kiconnect").unwrap();
        assert_eq!(kic.models.as_ref().map(Vec::len), Some(2));
        assert_eq!(kic.model.as_deref(), Some("openai-gpt-oss-120b"));
    }

    #[test]
    fn secret_env_not_allowed_for_claude_types() {
        let e = err_of(&one(
            "type = \"claude-oauth\"\ndescription = \"d\"\nsecret_env = \"OPENAI_API_KEY\"\n",
        ));
        assert_eq!(
            e,
            "backends.x: field `secret_env` is not allowed for type `claude-oauth`"
        );
        let e = err_of(&one(
            "type = \"claude-endpoint\"\ndescription = \"d\"\nbase_url = \"http://h\"\nsecret_env = \"K\"\n",
        ));
        assert_eq!(
            e,
            "backends.x: field `secret_env` is not allowed for type `claude-endpoint`"
        );
    }

    #[test]
    fn secret_env_invalid_names() {
        for var in ["", "A=B", "1ABC", "A B", "A-B", "Ä"] {
            let e = err_of(&cmd_with(&format!("secret_env = \"{var}\"\n")));
            assert!(
                e.contains("`secret_env`: invalid variable name"),
                "{var:?}: {e}"
            );
            assert_eq!(e.lines().count(), 1, "{e}");
        }
    }

    #[test]
    fn secret_env_reserved_names() {
        for var in [
            "CLAUDE_CODE_OAUTH_TOKEN",
            "claude_code_oauth_token",
            "AGENT_SIDEKICK_TOKEN",
            "AGENT_SIDEKICK_ANYTHING",
            "agent_sidekick_x",
        ] {
            let e = err_of(&cmd_with(&format!("secret_env = \"{var}\"\n")));
            assert!(
                e.contains(&format!("`secret_env` must not be `{var}`")),
                "{var}: {e}"
            );
        }
    }

    #[test]
    fn secret_env_conflicts_with_env_table() {
        for key in ["OPENAI_API_KEY", "openai_api_key"] {
            let e = err_of(&cmd_with(&format!(
                "secret_env = \"OPENAI_API_KEY\"\nenv = {{ {key} = \"value-not-shown\" }}\n"
            )));
            assert!(
                e.contains("`secret_env` `OPENAI_API_KEY` is also set in `env`"),
                "{key}: {e}"
            );
            assert!(!e.contains("value-not-shown"), "{e}");
        }
    }

    #[test]
    fn max_concurrent_zero() {
        let e = err_of(&one(
            "type = \"command\"\ndescription = \"d\"\ncommand = [\"x\", \"{prompt}\"]\nmax_concurrent = 0\n",
        ));
        assert_eq!(e, "backends.x: `max_concurrent` must be at least 1");
    }

    #[test]
    fn max_concurrent_too_large() {
        let e = err_of(&one(
            "type = \"command\"\ndescription = \"d\"\ncommand = [\"x\", \"{prompt}\"]\nmax_concurrent = 4611686018427387904\n",
        ));
        assert_eq!(
            e,
            "backends.x: `max_concurrent` must be at most 1024, got 4611686018427387904"
        );
        let ok = resolve_str(&one(
            "type = \"command\"\ndescription = \"d\"\ncommand = [\"x\", \"{prompt}\"]\nmax_concurrent = 1024\n",
        ))
        .unwrap();
        assert_eq!(ok.get("x").unwrap().max_concurrent, Some(1024));
    }

    #[test]
    fn foreign_secret_env_lists_every_api_key_env() {
        let b = resolve_str(&format!(
            "[backends.a]\ntype = \"claude-endpoint\"\ndescription = \"d\"\nbase_url = \"http://h\"\napi_key_env = \"KEY_A\"\nconfig_dir = {}\n\
             [backends.b]\ntype = \"claude-endpoint\"\ndescription = \"d\"\nbase_url = \"http://h\"\napi_key_env = \"KEY_B\"\nconfig_dir = {}\n\
             [backends.c]\ntype = \"command\"\ndescription = \"d\"\ncommand = [\"x\", \"{{prompt}}\"]\ndefault = true\n\
             [backends.d]\ntype = \"claude-endpoint\"\ndescription = \"d\"\nbase_url = \"http://h\"\nconfig_dir = {}\n",
            lit(&abs("ca")),
            lit(&abs("cb")),
            lit(&abs("cd")),
        ))
        .unwrap();
        for name in ["a", "b", "c", "d"] {
            assert_eq!(
                b.get(name).unwrap().foreign_secret_env,
                vec!["KEY_A".to_owned(), "KEY_B".to_owned()],
                "{name}"
            );
        }
        let implicit = resolve_str("").unwrap();
        assert!(implicit.default_backend().foreign_secret_env.is_empty());
    }

    /// A command backend `x` with `command` (TOML array) and `extra` lines.
    fn cmd_models(command: &str, extra: &str) -> String {
        one(&format!(
            "type = \"command\"\ndescription = \"d\"\ncommand = {command}\n{extra}"
        ))
    }

    const MODEL_CMD: &str = r#"["aider", "--model", "openai/{model}", "--message", "{prompt}"]"#;

    #[test]
    fn command_models_and_default() {
        let b = resolve_str(&cmd_models(
            MODEL_CMD,
            "models = [\"openai-gpt-oss-120b\", \"qwen-qwen3-8-27b\", \"Mistral Small 3.2 24B Instruct 2506\"]\n",
        ))
        .unwrap();
        let d = b.default_backend();
        assert_eq!(d.model.as_deref(), Some("openai-gpt-oss-120b"));
        assert_eq!(d.models.as_ref().map(Vec::len), Some(3));
        assert!(d.takes_model());
        // Explicit default from the list.
        let b = resolve_str(&cmd_models(
            MODEL_CMD,
            "models = [\"a\", \"b\"]\nmodel = \" b \"\n",
        ))
        .unwrap();
        assert_eq!(b.default_backend().model.as_deref(), Some("b"));
        // Default not in the list.
        let e = err_of(&cmd_models(
            MODEL_CMD,
            "models = [\"a\", \"b\"]\nmodel = \"c\"\n",
        ));
        assert_eq!(
            e,
            "backends.x: `model` \"c\" (the default) must be one of `models`: a, b"
        );
    }

    #[test]
    fn command_model_placeholder_rules() {
        // {model} without models.
        let e = err_of(&cmd_models(MODEL_CMD, ""));
        assert!(
            e.contains("`models` is required when `command` uses `{model}`"),
            "{e}"
        );
        // models without {model}.
        let e = err_of(&cmd_models(
            "[\"aider\", \"{prompt}\"]",
            "models = [\"a\"]\n",
        ));
        assert!(
            e.contains("`models` has no effect without `{model}`"),
            "{e}"
        );
        // model without {model}.
        let e = err_of(&cmd_models("[\"aider\", \"{prompt}\"]", "model = \"a\"\n"));
        assert!(e.contains("`model` has no effect without `{model}`"), "{e}");
        // {model} in command[0] is never substituted and is an error; it does not count.
        let e = err_of(&cmd_models(
            "[\"agent-{model}\", \"{prompt}\"]",
            "models = [\"a\"]\n",
        ));
        assert!(e.contains("must not contain a placeholder"), "{e}");
        assert!(e.contains("{model}); only the arguments"), "{e}");
        assert!(
            e.contains("`models` has no effect without `{model}`"),
            "{e}"
        );
    }

    #[test]
    fn models_name_rules() {
        let cases: [(&str, &str); 7] = [
            ("[]", "`models` must not be empty"),
            ("[\"a\", \"\"]", "`models[1]` (\"\") must not be blank"),
            ("[\"  \"]", "`models[0]` (\"  \") must not be blank"),
            ("[\" a\"]", "must not have leading or trailing whitespace"),
            ("[\"a \"]", "must not have leading or trailing whitespace"),
            ("[\"-a\"]", "`models[0]` (\"-a\") must not start with `-`"),
            ("[\"a\\nb\"]", "must not contain control characters"),
        ];
        for (list, want) in cases {
            let e = err_of(&cmd_models(MODEL_CMD, &format!("models = {list}\n")));
            assert!(e.contains(want), "{list}: {e}");
        }
        // Duplicates (case-sensitive).
        let e = err_of(&cmd_models(MODEL_CMD, "models = [\"a\", \"b\", \"a\"]\n"));
        assert_eq!(e, "backends.x: `models` lists \"a\" more than once");
        assert!(resolve_str(&cmd_models(MODEL_CMD, "models = [\"a\", \"A\"]\n")).is_ok());
        // The default model follows the same rules.
        let e = err_of(&one(
            "type = \"claude-oauth\"\ndescription = \"d\"\nmodel = \"--dangerously\"\n",
        ));
        assert_eq!(e, "backends.x: `model` must not start with `-`");
        let e = err_of(&one(
            "type = \"claude-oauth\"\ndescription = \"d\"\nmodel = \"a\\tb\"\n",
        ));
        assert!(e.contains("control characters"), "{e}");
    }

    #[test]
    fn claude_models_allowlist_is_optional() {
        let b = resolve_str(&one(
            "type = \"claude-oauth\"\ndescription = \"d\"\nmodels = [\"opus\", \"sonnet\"]\n",
        ))
        .unwrap();
        let d = b.default_backend();
        assert_eq!(d.model.as_deref(), Some("opus"));
        assert_eq!(d.models, Some(vec!["opus".to_owned(), "sonnet".to_owned()]));
        assert!(d.effective_model(Some("haiku")).is_err());
        assert_eq!(
            d.effective_model(Some("sonnet")).unwrap().as_deref(),
            Some("sonnet")
        );
        // Without models any model is accepted, and there may be no default.
        let b = resolve_str(&one("type = \"claude-oauth\"\ndescription = \"d\"\n")).unwrap();
        let d = b.default_backend();
        assert_eq!((d.model.as_deref(), d.models.as_ref()), (None, None));
        assert_eq!(
            d.effective_model(Some("fable")).unwrap().as_deref(),
            Some("fable")
        );
        assert_eq!(d.effective_model(None).unwrap(), None);
        // Endpoint: default must be in the list.
        let e = err_of(&one(
            "type = \"claude-endpoint\"\ndescription = \"d\"\nbase_url = \"http://h\"\n\
             models = [\"qwen3-coder\"]\nmodel = \"other\"\n",
        ));
        assert!(e.contains("must be one of `models`: qwen3-coder"), "{e}");
    }

    #[test]
    fn command_without_model_placeholder_ignores_model() {
        let b = resolve_str(&cmd_with("")).unwrap();
        let d = b.default_backend();
        assert!(!d.takes_model());
        assert_eq!(d.effective_model(Some("anything")).unwrap(), None);
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
                config_dir: abs("home").join("sub/dir"),
                permissions: Default::default(),
            }
        );
        let b = resolve_str(&one(
            "type = \"claude-oauth\"\ndescription = \"d\"\nconfig_dir = \"~\"\n",
        ))
        .unwrap();
        assert_eq!(
            b.default_backend().kind,
            BackendKind::ClaudeOauth {
                config_dir: abs("home"),
                permissions: Default::default(),
            }
        );
        let text = one(&format!(
            "type = \"claude-oauth\"\ndescription = \"d\"\nconfig_dir = {}\n",
            lit(&abs("abs").join("dir"))
        ));
        assert_eq!(
            resolve_str(&text).unwrap().default_backend().kind,
            BackendKind::ClaudeOauth {
                config_dir: abs("abs").join("dir"),
                permissions: Default::default(),
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
            config_dir: None,
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

    #[test]
    fn claude_permission_fields_resolve() {
        let b = resolve_str(
            "[backends.s]\nharness = \"claude\"\ndescription = \"d\"\ndefault = true\n\
             permission_mode = \"acceptEdits\"\nallowed_tools = [\"Read\", \"Bash(git:*)\"]\n\
             disallowed_tools = [\"Bash(git push:*)\"]\n\
             [backends.e]\nharness = \"claude\"\ndescription = \"d\"\n\
             base_url = \"http://localhost:11434\"\npermission_mode = \"plan\"\n",
        )
        .unwrap();
        let s = b.get("s").unwrap().claude_permissions().unwrap();
        assert_eq!(s.mode, crate::backends::PermissionMode::AcceptEdits);
        assert_eq!(
            s.allowed_tools.as_deref(),
            Some(&["Read".to_owned(), "Bash(git:*)".to_owned()][..])
        );
        assert_eq!(s.disallowed_tools, ["Bash(git push:*)"]);
        let e = b.get("e").unwrap().claude_permissions().unwrap();
        assert_eq!(e.mode, crate::backends::PermissionMode::Plan);
        assert_eq!(e.allowed_tools, None);
        assert!(e.disallowed_tools.is_empty());
        let implicit = resolve_str("").unwrap();
        assert_eq!(
            implicit.default_backend().claude_permissions(),
            Some(&ClaudePermissions::default())
        );
    }

    #[test]
    fn claude_permission_fields_are_errors_on_other_harnesses() {
        let others = [
            (
                "harness = \"command\"\ncommand = [\"aider\", \"{prompt}\"]",
                "harness `command`",
            ),
            (
                "harness = \"opencode\"\nprovider = \"ollama\"\n\
                 base_url = \"http://localhost:11434\"\nmodels = [\"m\"]",
                "harness `opencode`",
            ),
            ("harness = \"codex\"", "harness `codex`"),
        ];
        for (kind, label) in others {
            for field in [
                "permission_mode = \"acceptEdits\"",
                "allowed_tools = [\"Read\"]",
                "disallowed_tools = [\"Bash\"]",
            ] {
                let e = err_of(&format!(
                    "[backends.x]\n{kind}\ndescription = \"d\"\ndefault = true\n{field}\n"
                ));
                let key = field.split(' ').next().unwrap();
                assert!(
                    e.contains(&format!(
                        "backends.x: field `{key}` is not allowed for {label}"
                    )),
                    "{e}"
                );
            }
        }
    }

    #[test]
    fn claude_permission_values_are_validated() {
        let base = "[backends.s]\nharness = \"claude\"\ndescription = \"d\"\ndefault = true\n";
        let e = toml::from_str::<Config>(&format!("{base}permission_mode = \"yolo\"\n"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("unknown variant `yolo`"), "{e}");
        assert!(
            e.contains("acceptEdits") && e.contains("bypassPermissions"),
            "{e}"
        );
        let e = err_of(&format!(
            "{base}allowed_tools = [\"Read\", \" \", \"--dangerously-skip-permissions\"]\n\
             disallowed_tools = [\" Bash\", \"a\\nb\"]\n"
        ));
        assert!(
            e.contains("`allowed_tools[1]` (\" \") must not be blank"),
            "{e}"
        );
        assert!(
            e.contains(
                "`allowed_tools[2]` (\"--dangerously-skip-permissions\") must not start with `-`"
            ),
            "{e}"
        );
        assert!(
            e.contains(
                "`disallowed_tools[0]` (\" Bash\") must not have leading or trailing whitespace"
            ),
            "{e}"
        );
        assert!(
            e.contains("`disallowed_tools[1]` (\"a\\nb\") must not contain control characters"),
            "{e}"
        );
        for mode in [
            "default",
            "acceptEdits",
            "auto",
            "dontAsk",
            "plan",
            "bypassPermissions",
        ] {
            let b = resolve_str(&format!("{base}permission_mode = \"{mode}\"\n")).unwrap();
            assert_eq!(
                b.get("s")
                    .unwrap()
                    .claude_permissions()
                    .unwrap()
                    .mode
                    .as_str(),
                mode
            );
        }
    }
}
