//! `agent-sidekick backends list|test`: show the configured backends and check them.
//!
//! Nothing printed here ever contains a secret: credentials are only described by their
//! source (env var name, keyring, file), and every free text goes through redaction.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Result;

use super::claude_cli;
use crate::backends::{self, Backend, BackendKind, Backends, Reachability};
use crate::config::Config;
use crate::redact::redact_all;
use crate::APP_NAME;

/// Width the DESCRIPTION column is truncated to.
pub const DESCRIPTION_WIDTH: usize = 60;
/// Width the MODELS column is truncated to.
pub const MODELS_WIDTH: usize = 40;
/// At most this many reachability checks run at the same time.
const CHECK_PARALLELISM: usize = 8;

#[derive(Debug, clap::Args)]
pub struct BackendsArgs {
    #[command(subcommand)]
    pub command: BackendsCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum BackendsCommand {
    /// List the configured backends (from config.toml).
    List {
        /// Also check every backend's reachability (credential, executable, HTTP probe).
        #[arg(long)]
        check: bool,
    },
    /// Check one backend in detail; exit code 0 if it is reachable, 1 otherwise.
    Test {
        /// Backend name as configured in `[backends.<name>]`.
        name: String,
    },
}

pub fn run(args: BackendsArgs) -> Result<ExitCode> {
    let (cfg, backends) = match load_backends() {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("error: {}", redact_all(&msg));
            return Ok(ExitCode::FAILURE);
        }
    };
    match args.command {
        BackendsCommand::List { check } => {
            let reach = check.then(|| check_all(&backends, cfg.claude_path.as_deref()));
            print!("{}", format_table(&backends, reach.as_deref()));
            if check {
                println!("\nDetails: `{APP_NAME} backends test <name>`.");
            }
            Ok(ExitCode::SUCCESS)
        }
        BackendsCommand::Test { name } => test(&cfg, &backends, &name),
    }
}

/// config.toml (default location) validated into backends. The error is a user-facing
/// message; an invalid config is never replaced by defaults here.
pub(crate) fn load_backends() -> std::result::Result<(Config, Backends), String> {
    let cfg = Config::load().map_err(|e| format!("config.toml: {e:#}"))?;
    let backends = backends::resolve(&cfg)
        .map_err(|e| format!("invalid backend configuration in config.toml:\n{e}"))?;
    Ok((cfg, backends))
}

/// Error for an unknown backend name, listing the configured ones.
pub(crate) fn unknown_backend(name: &str, backends: &Backends) -> String {
    format!(
        "unknown backend `{name}`; configured: {}",
        backends.names().join(", ")
    )
}

/// [`backends::check_backend`] for every backend, at most [`CHECK_PARALLELISM`] at a time.
/// Results are in the order of `backends.iter()`.
pub(crate) fn check_all(backends: &Backends, claude_path: Option<&Path>) -> Vec<Reachability> {
    let list: Vec<&Backend> = backends.iter().collect();
    parallel_map(&list, CHECK_PARALLELISM, |b| {
        backends::check_backend(b, claude_path)
    })
}

/// `f` over `items` on scoped threads, `limit` at a time; keeps the order.
pub(crate) fn parallel_map<T, R, F>(items: &[T], limit: usize, f: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    let mut out = Vec::with_capacity(items.len());
    for chunk in items.chunks(limit.max(1)) {
        std::thread::scope(|s| {
            let handles: Vec<_> = chunk.iter().map(|item| s.spawn(|| f(item))).collect();
            for h in handles {
                match h.join() {
                    Ok(r) => out.push(r),
                    Err(panic) => std::panic::resume_unwind(panic),
                }
            }
        });
    }
    out
}

/// `text` shortened to `width` characters (ending in `...`), on one line.
pub fn truncate(text: &str, width: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= width {
        return one_line;
    }
    let keep = width.saturating_sub(3);
    let mut s: String = one_line.chars().take(keep).collect();
    s.truncate(s.trim_end().len());
    s.push_str("...");
    s
}

/// The `backends list` table. With `reach` (same order as `backends.iter()`), REACHABLE and
/// DETAIL columns are added.
pub fn format_table(backends: &Backends, reach: Option<&[Reachability]>) -> String {
    let mut header = vec!["NAME", "TYPE", "COST", "DEFAULT", "MODELS", "DESCRIPTION"];
    if reach.is_some() {
        header.extend(["REACHABLE", "DETAIL"]);
    }
    let mut rows: Vec<Vec<String>> = vec![header.into_iter().map(str::to_owned).collect()];
    for (i, b) in backends.iter().enumerate() {
        let mut row = vec![
            b.name.clone(),
            b.backend_type().to_string(),
            b.cost.to_string(),
            if b.is_default { "*" } else { "" }.to_owned(),
            truncate(&models_cell(b), MODELS_WIDTH),
            truncate(&redact_all(&b.description), DESCRIPTION_WIDTH),
        ];
        if let Some(r) = reach.and_then(|r| r.get(i)) {
            row.push(if r.reachable { "yes" } else { "no" }.to_owned());
            row.push(redact_all(&r.detail).replace(['\r', '\n'], " "));
        }
        rows.push(row);
    }
    render_rows(&rows)
}

/// The MODELS cell: the default model first, marked with `*`, then the rest of the
/// allowlist; `(any)` where a claude-* backend accepts any model; `-` for a command backend
/// that takes no model.
pub fn models_cell(b: &Backend) -> String {
    if !b.takes_model() {
        return "-".to_owned();
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(d) = &b.model {
        parts.push(format!("{d}*"));
    }
    match &b.models {
        Some(list) => parts.extend(
            list.iter()
                .filter(|m| Some(*m) != b.model.as_ref())
                .cloned(),
        ),
        None => parts.push("(any)".to_owned()),
    }
    parts.join(", ")
}

/// Left-aligned columns separated by two spaces; the last column is not padded.
fn render_rows(rows: &[Vec<String>]) -> String {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..cols)
        .map(|c| {
            rows.iter()
                .filter_map(|r| r.get(c))
                .map(|s| s.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for row in rows {
        let mut line = String::new();
        for (c, cell) in row.iter().enumerate() {
            if c + 1 == row.len() {
                line.push_str(cell);
            } else {
                line.push_str(cell);
                let pad = widths[c] - cell.chars().count() + 2;
                line.extend(std::iter::repeat_n(' ', pad));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn test(cfg: &Config, backends: &Backends, name: &str) -> Result<ExitCode> {
    let Some(backend) = backends.get(name) else {
        eprintln!("error: {}", unknown_backend(name, backends));
        return Ok(ExitCode::FAILURE);
    };
    let claude_path = cfg.claude_path.as_deref();
    let mut lines = describe(backend);
    // claude found and credential available: then `check_backend` itself probes base_url.
    let mut prerequisites_ok = true;

    match &backend.kind {
        BackendKind::ClaudeOauth { .. } | BackendKind::ClaudeEndpoint { .. } => {
            lines.push(match backends::resolve_program(backend, claude_path) {
                Ok(claude) => match claude_cli::version(&claude) {
                    Ok(v) => format!("claude:      {} (version {v})", claude.display()),
                    Err(e) => format!(
                        "claude:      {} (`--version` failed: {e:#})",
                        claude.display()
                    ),
                },
                Err(e) => {
                    prerequisites_ok = false;
                    format!("claude:      not found: {e}")
                }
            });
            let what = if matches!(backend.kind, BackendKind::ClaudeOauth { .. }) {
                "token:"
            } else {
                "key source:"
            };
            lines.push(match key_source(backend) {
                Ok(source) => format!("{what:<13}{source}"),
                Err(e) => {
                    prerequisites_ok = false;
                    format!("{what:<13}missing: {e}")
                }
            });
        }
        BackendKind::OpenCode { provider, .. } => {
            lines.push(match backends::resolve_program(backend, None) {
                Ok(p) => match backend.harness().version(&p) {
                    Ok(v) => format!("opencode:    {} (version {v})", p.display()),
                    Err(e) => format!("opencode:    {} (`--version` failed: {e})", p.display()),
                },
                Err(e) => format!("opencode:    not found: {e}"),
            });
            if provider.needs_key() {
                lines.push(match key_source(backend) {
                    Ok(source) => format!("key source:  {source}"),
                    Err(e) => format!("key source:  missing: {e}"),
                });
            }
        }
        BackendKind::Codex { auth, .. } => {
            lines.push(match backends::resolve_program(backend, None) {
                Ok(p) => match backend.harness().version(&p) {
                    Ok(v) => format!("codex:       {} (version {v})", p.display()),
                    Err(e) => format!("codex:       {} (`--version` failed: {e})", p.display()),
                },
                Err(e) => format!("codex:       not found: {e}"),
            });
            let what = match auth {
                backends::CodexAuth::Chatgpt => "login:",
                backends::CodexAuth::ApiKey => "key source:",
            };
            lines.push(match key_source(backend) {
                Ok(source) => format!("{what:<13}{source}"),
                Err(e) => format!("{what:<13}missing: {e}"),
            });
        }
        BackendKind::Command { secret_env, .. } => {
            lines.push(match backends::resolve_program(backend, None) {
                Ok(p) => format!("program:     {}", p.display()),
                Err(e) => format!("program:     not found: {e}"),
            });
            if let Some(var) = secret_env {
                // Only where the value comes from; the value itself is dropped in key_source.
                lines.push(match key_source(backend) {
                    Ok(source) => format!("secret:      {var} from {source}"),
                    Err(e) => format!("secret:      missing: {e}"),
                });
            }
        }
    }
    lines.push(account_line(backend, prerequisites_ok));
    if let (BackendKind::ClaudeEndpoint { base_url, .. }, false) = (&backend.kind, prerequisites_ok)
    {
        // `check_backend` stops before probing; show the server's state anyway.
        let probe = backends::probe_http(base_url, backends::reach::PROBE_TIMEOUT);
        lines.push(format!("probe:       {}", probe.detail));
    }

    let reach = backends::check_backend(backend, claude_path);
    lines.push(format!(
        "result:      {} ({})",
        if reach.reachable {
            "reachable"
        } else {
            "NOT reachable"
        },
        reach.detail
    ));
    for line in lines {
        println!("{}", redact_all(&line));
    }
    Ok(if reach.reachable {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn account_line(backend: &Backend, can_detect: bool) -> String {
    let configured = backend.account.as_deref().unwrap_or("not set");
    if !backend.account_bound() || !can_detect {
        return format!("account:     {configured}");
    }
    match backends::detect_account(backend) {
        Ok(found) if backend.account.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(&found)) => {
            format!("account:     {configured} (matches the detected login)")
        }
        Ok(found) => format!("account:     {configured}; detected {found}"),
        Err(e) => format!("account:     {configured} (not detected: {e})"),
    }
}

/// Where the backend's credential comes from (never the value).
fn key_source(backend: &Backend) -> std::result::Result<String, String> {
    if let BackendKind::ClaudeEndpoint {
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
    } = &backend.kind
    {
        // Doctor/test run in a terminal; the server reads its own environment.
        return match std::env::var_os(var).filter(|v| !v.is_empty()) {
            Some(_) => Ok(format!(
                "environment variable {var} (set in this terminal; the MCP server needs it in its own environment)"
            )),
            None => Err(format!(
                "environment variable {var} (api_key_env) is not set in this terminal; \
                 the MCP server needs it in its own environment"
            )),
        };
    }
    // The credential itself is dropped immediately; only its source is kept.
    backends::load_credential(backend).map(|(_credential, source)| source)
}

/// Static facts about a backend (from config.toml; no secrets, no env values).
pub fn describe(backend: &Backend) -> Vec<String> {
    let mut lines = vec![
        format!(
            "backend:     {} ({}){}",
            backend.name,
            backend.backend_type(),
            if backend.is_default { ", default" } else { "" }
        ),
        format!("description: {}", backend.description),
        format!("cost:        {}", backend.cost),
    ];
    if let Some(n) = backend.max_concurrent {
        lines.push(format!("max_concurrent: {n}"));
    }
    if backend.takes_model() {
        lines.push(match (&backend.model, &backend.kind) {
            (Some(m), _) => format!("model:       {m} (default)"),
            (None, BackendKind::ClaudeEndpoint { .. }) => {
                "model:       the endpoint's default".to_owned()
            }
            (None, BackendKind::Codex { .. }) => "model:       Codex's default".to_owned(),
            (None, _) => "model:       the account's default".to_owned(),
        });
        lines.push(match &backend.models {
            Some(list) => format!("models:      {} (allowlist)", list.join(", ")),
            None => "models:      any (no allowlist)".to_owned(),
        });
    } else {
        lines.push(
            "models:      none (no {model} in command; a requested model is ignored)".to_owned(),
        );
    }
    match &backend.kind {
        BackendKind::ClaudeOauth { config_dir, .. } => {
            lines.push(config_dir_line(config_dir));
        }
        BackendKind::ClaudeEndpoint {
            base_url,
            api_key_env,
            auth,
            config_dir,
            ..
        } => {
            lines.push(config_dir_line(config_dir));
            lines.push(format!("base_url:    {base_url}"));
            let auth_name = match auth {
                backends::EndpointAuth::XApiKey => "x-api-key",
                backends::EndpointAuth::Bearer => "bearer",
            };
            lines.push(format!("auth:        {auth_name} ({})", auth.env_var()));
            lines.push(match api_key_env {
                Some(var) => format!("api key:     from environment variable {var}"),
                None => format!(
                    "api key:     OS keyring entry endpoint-{} (or its key file in the config directory)",
                    backend.name
                ),
            });
        }
        BackendKind::OpenCode {
            provider,
            base_url,
            api_key_env,
            harness_path,
            read_only,
            request_timeout_s,
            model_options,
            limits,
            base_dir,
        } => {
            lines.push(format!("provider:    {provider}"));
            if let Some(u) = base_url {
                lines.push(format!("base_url:    {u}"));
            }
            lines.push(match harness_path {
                Some(p) => format!("harness_path: {}", p.display()),
                None => "harness_path: (looked up: npm global package, PATH)".to_owned(),
            });
            lines.push(format!("read_only:   {read_only}"));
            lines.push(match request_timeout_s {
                Some(t) => format!("request_timeout_s: {t}"),
                None => "request_timeout_s: (OpenCode default, 300)".to_owned(),
            });
            lines.push(format!(
                "limits:      context {} tokens, max output {} tokens",
                crate::harness::opencode::effective_context(limits),
                crate::harness::opencode::effective_output(limits)
            ));
            if !model_options.is_empty() {
                lines.push(format!(
                    "model_options: {}",
                    serde_json::Value::Object(model_options.clone())
                ));
            }
            lines.push(match (provider.needs_key(), api_key_env) {
                (false, _) => "api key:     none needed".to_owned(),
                (true, Some(var)) => format!("api key:     from environment variable {var}"),
                (true, None) => format!(
                    "api key:     OS keyring entry backend-{} (or its key file in the config directory)",
                    backend.name
                ),
            });
            lines.push(format!("state dir:   {}", base_dir.display()));
        }
        BackendKind::Codex {
            auth,
            base_url,
            api_key_env,
            harness_path,
            read_only,
            windows_sandbox,
            base_dir,
        } => {
            lines.push(format!("auth:        {auth}"));
            lines.push(match base_url {
                Some(u) => format!("base_url:    {u} (Responses API)"),
                None => "provider:    OpenAI (Codex's built-in provider)".to_owned(),
            });
            lines.push(match harness_path {
                Some(p) => format!("harness_path: {}", p.display()),
                None => "harness_path: (looked up: npm global package, standalone install, PATH)"
                    .to_owned(),
            });
            lines.push(format!(
                "sandbox:     {}{}",
                if *read_only {
                    "read-only"
                } else {
                    "workspace-write"
                },
                if cfg!(windows) {
                    format!(" (windows_sandbox {})", windows_sandbox.as_str())
                } else {
                    String::new()
                }
            ));
            lines.push(match (auth, api_key_env) {
                (backends::CodexAuth::Chatgpt, _) => format!(
                    "login:       ChatGPT login in {} (`agent-sidekick setup --backend {}` \
                     shows how)",
                    base_dir.join(crate::harness::codex::AUTH_FILE).display(),
                    backend.name
                ),
                (backends::CodexAuth::ApiKey, Some(var)) => {
                    format!("api key:     from environment variable {var}")
                }
                (backends::CodexAuth::ApiKey, None) => format!(
                    "api key:     OS keyring entry backend-{} (or its key file in the config directory)",
                    backend.name
                ),
            });
            lines.push(format!("codex home:  {}", base_dir.display()));
        }
        BackendKind::Command {
            command,
            prompt_via_stdin,
            secret_env,
        } => {
            lines.push(format!("command:     {}", quote_template(command)));
            lines.push(format!("prompt_via_stdin: {prompt_via_stdin}"));
            if let Some(var) = secret_env {
                lines.push(format!(
                    "secret_env:  {var} (value from OS keyring entry cmd-{} or its key file in \
                     the config directory; not shown)",
                    backend.name
                ));
            }
        }
    }
    if let Some(p) = backend.claude_permissions() {
        lines.push(format!("permission_mode: {}", p.mode));
        lines.push(match &p.allowed_tools {
            Some(t) if !t.is_empty() => format!(
                "allowed_tools: {} (when the request gives none)",
                t.join(" ")
            ),
            _ => "allowed_tools: (none; without the request's allowed_tools only read-only \
                  tools run)"
                .to_owned(),
        });
        lines.push(if p.disallowed_tools.is_empty() {
            "disallowed_tools: (none)".to_owned()
        } else {
            format!(
                "disallowed_tools: {} (always)",
                p.disallowed_tools.join(" ")
            )
        });
        if p.mode == backends::PermissionMode::BypassPermissions {
            lines.push(format!("warning:     {}", backends::BYPASS_WARNING));
        }
    }
    if !backend.env.is_empty() {
        let keys: Vec<&str> = backend.env.keys().map(String::as_str).collect();
        lines.push(format!(
            "env:         {} (values not shown)",
            keys.join(", ")
        ));
    }
    if !backend.pass_env.is_empty() {
        lines.push(format!("pass_env:    {}", backend.pass_env.join(", ")));
    }
    lines
}

fn config_dir_line(dir: &Path) -> String {
    let state = if dir.is_dir() {
        ""
    } else {
        " (does not exist yet)"
    };
    format!("config_dir:  {}{state}", dir.display())
}

/// `["aider", "--message", "{prompt}"]` as written in config.toml.
pub fn quote_template(command: &[String]) -> String {
    let parts: Vec<String> = command
        .iter()
        .map(|s| serde_json::to_string(s).unwrap_or_else(|_| format!("{s:?}")))
        .collect();
    format!("[{}]", parts.join(", "))
}

/// Config dir of a claude-* backend.
pub(crate) fn claude_config_dir(backend: &Backend) -> Option<&PathBuf> {
    match &backend.kind {
        BackendKind::ClaudeOauth { config_dir, .. }
        | BackendKind::ClaudeEndpoint { config_dir, .. } => Some(config_dir),
        BackendKind::Command { .. } | BackendKind::OpenCode { .. } | BackendKind::Codex { .. } => {
            None
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::backends::{Cost, EndpointAuth};

    pub(crate) fn backend(name: &str, kind: BackendKind, is_default: bool) -> Backend {
        Backend {
            name: name.to_owned(),
            description: format!("description of {name}"),
            account: None,
            cost: Cost::Free,
            is_default,
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

    pub(crate) fn abs(rel: &str) -> PathBuf {
        std::env::temp_dir()
            .join("agent-sidekick-cmd-tests")
            .join(rel)
    }

    pub(crate) fn oauth(name: &str, is_default: bool) -> Backend {
        backend(
            name,
            BackendKind::ClaudeOauth {
                config_dir: abs(&format!(".claude-{name}")),
                permissions: Default::default(),
            },
            is_default,
        )
    }

    pub(crate) fn endpoint(name: &str, api_key_env: Option<&str>, is_default: bool) -> Backend {
        backend(
            name,
            BackendKind::ClaudeEndpoint {
                base_url: "http://localhost:11434".into(),
                api_key_env: api_key_env.map(Into::into),
                auth: EndpointAuth::XApiKey,
                config_dir: abs(&format!(".claude-sidekick-{name}")),
                permissions: Default::default(),
            },
            is_default,
        )
    }

    pub(crate) fn command(name: &str, is_default: bool) -> Backend {
        backend(
            name,
            BackendKind::Command {
                command: vec!["aider".into(), "--message".into(), "{prompt}".into()],
                prompt_via_stdin: false,
                secret_env: None,
            },
            is_default,
        )
    }

    /// A command backend whose stored secret goes into `var`.
    pub(crate) fn command_with_secret(name: &str, var: &str, is_default: bool) -> Backend {
        backend(
            name,
            BackendKind::Command {
                command: vec!["aider".into(), "--message".into(), "{prompt}".into()],
                prompt_via_stdin: false,
                secret_env: Some(var.into()),
            },
            is_default,
        )
    }

    /// An opencode backend (`provider`, optional `api_key_env`), models `m1`, `m2`.
    pub(crate) fn opencode(
        name: &str,
        provider: crate::backends::OpencodeProvider,
        api_key_env: Option<&str>,
        is_default: bool,
    ) -> Backend {
        let mut b = backend(
            name,
            BackendKind::OpenCode {
                provider,
                base_url: match provider {
                    crate::backends::OpencodeProvider::Openrouter => None,
                    _ => Some("https://llm.example/v1".into()),
                },
                api_key_env: api_key_env.map(Into::into),
                harness_path: None,
                read_only: false,
                request_timeout_s: None,
                model_options: serde_json::Map::new(),
                limits: Default::default(),
                base_dir: abs("cfg").join("opencode").join(name),
            },
            is_default,
        );
        b.model = Some("m1".into());
        b.models = Some(vec!["m1".into(), "m2".into()]);
        b
    }

    #[test]
    fn describe_opencode_backend() {
        let b = opencode(
            "kic",
            crate::backends::OpencodeProvider::OpenaiCompatible,
            None,
            true,
        );
        let text = describe(&b).join("\n");
        assert!(
            text.contains("backend:     kic (opencode), default"),
            "{text}"
        );
        assert!(text.contains("model:       m1 (default)"), "{text}");
        assert!(text.contains("models:      m1, m2 (allowlist)"), "{text}");
        assert!(text.contains("provider:    openai-compatible"), "{text}");
        assert!(
            text.contains("base_url:    https://llm.example/v1"),
            "{text}"
        );
        assert!(text.contains("OS keyring entry backend-kic"), "{text}");
        assert!(text.contains("state dir:   "), "{text}");
        assert!(!text.contains("model_options"), "{text}");
        assert_eq!(claude_config_dir(&b), None);

        let mut b = b;
        if let BackendKind::OpenCode { model_options, .. } = &mut b.kind {
            model_options.insert(
                "interleaved".into(),
                serde_json::json!({"field": "reasoning"}),
            );
        }
        let text = describe(&b).join(
            "
",
        );
        assert!(
            text.contains("model_options: {\"interleaved\":{\"field\":\"reasoning\"}}"),
            "{text}"
        );
    }

    /// sidekick (oauth, default), work (oauth), local (endpoint, keyring), envkey (endpoint,
    /// env var), aider (command).
    pub(crate) fn sample() -> Backends {
        Backends::new(vec![
            oauth("sidekick", true),
            oauth("work", false),
            endpoint("local", None, false),
            endpoint("envkey", Some("MY_KEY"), false),
            command("aider", false),
        ])
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("exactly10!", 10), "exactly10!");
        assert_eq!(truncate("a b c d e f g h", 10), "a b c d...");
        assert_eq!(truncate("line one\nline  two", 40), "line one line two");
        // Multi-byte characters are counted as characters, not bytes.
        let t = truncate(&"ä".repeat(70), 60);
        assert_eq!(t.chars().count(), 60);
        assert!(t.ends_with("..."));
    }

    #[test]
    fn table_lists_backends_sorted_with_default_marker() {
        let table = format_table(&sample(), None);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 6, "{table}");
        assert!(lines[0].starts_with("NAME"));
        assert!(lines[0].ends_with("DESCRIPTION"));
        assert!(!lines[0].contains("REACHABLE"));
        let names: Vec<&str> = lines[1..]
            .iter()
            .map(|l| l.split_whitespace().next().unwrap())
            .collect();
        assert_eq!(names, ["aider", "envkey", "local", "sidekick", "work"]);
        let sidekick = lines.iter().find(|l| l.starts_with("sidekick")).unwrap();
        assert!(sidekick.contains("claude-oauth"));
        assert!(sidekick.contains(" * "), "{sidekick}");
        let work = lines.iter().find(|l| l.starts_with("work")).unwrap();
        assert!(!work.contains(" * "), "{work}");
        // Columns are aligned: DESCRIPTION starts at the same offset everywhere.
        let col = lines[0].find("DESCRIPTION").unwrap();
        assert!(sidekick[col..].starts_with("description of sidekick"));
    }

    #[test]
    fn table_truncates_and_redacts_and_has_no_secrets() {
        let mut b = endpoint("local", Some("MY_KEY"), true);
        b.description = format!(
            "{} sk-ant-oat01-SECRETSECRET {}",
            "x".repeat(10),
            "y".repeat(80)
        );
        b.env
            .insert("SOME_VAR".into(), "env-value-not-shown".into());
        let backends = Backends::new(vec![b]);
        let reach = vec![Reachability {
            reachable: false,
            detail: "unreachable: leaked sk-ant-api03-abcdefgh\nsecond line".into(),
        }];
        let table = format_table(&backends, Some(&reach));
        assert!(!table.contains("SECRETSECRET"), "{table}");
        assert!(!table.contains("abcdefgh"), "{table}");
        assert!(!table.contains("env-value-not-shown"), "{table}");
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "detail newlines must not break the table: {table}"
        );
        assert!(lines[0].contains("REACHABLE") && lines[0].ends_with("DETAIL"));
        assert!(lines[1].contains("...  no "), "{table}");
        assert!(lines[1].ends_with("unreachable: leaked [REDACTED] second line"));
        let col = lines[0].find("DESCRIPTION").unwrap();
        let end = lines[0].find("REACHABLE").unwrap();
        assert!(end - col <= DESCRIPTION_WIDTH + 2);
    }

    #[test]
    fn describe_shows_sources_not_values() {
        let mut b = endpoint("local", Some("OLLAMA_KEY"), false);
        b.env
            .insert("CLAUDE_CODE_ATTRIBUTION_HEADER".into(), "secretish".into());
        let text = describe(&b).join("\n");
        assert!(
            text.contains("base_url:    http://localhost:11434"),
            "{text}"
        );
        assert!(text.contains("x-api-key (ANTHROPIC_API_KEY)"), "{text}");
        assert!(text.contains("environment variable OLLAMA_KEY"), "{text}");
        assert!(text.contains("CLAUDE_CODE_ATTRIBUTION_HEADER (values not shown)"));
        assert!(!text.contains("secretish"), "{text}");

        let text = describe(&endpoint("local", None, false)).join("\n");
        assert!(text.contains("keyring entry endpoint-local"), "{text}");

        let text = describe(&command("aider", true)).join("\n");
        assert!(text.contains("aider (command), default"), "{text}");
        assert!(
            text.contains(r#"command:     ["aider", "--message", "{prompt}"]"#),
            "{text}"
        );
    }

    #[test]
    fn describe_shows_claude_permissions() {
        let text = describe(&oauth("s", true)).join("\n");
        assert!(text.contains("permission_mode: default"), "{text}");
        assert!(text.contains("allowed_tools: (none;"), "{text}");
        assert!(text.contains("disallowed_tools: (none)"), "{text}");
        assert!(!text.contains("warning:"), "{text}");

        let mut b = endpoint("local", None, false);
        if let BackendKind::ClaudeEndpoint { permissions, .. } = &mut b.kind {
            permissions.mode = backends::PermissionMode::BypassPermissions;
            permissions.allowed_tools = Some(vec!["Bash".into(), "Edit".into()]);
            permissions.disallowed_tools = vec!["Bash(git push:*)".into()];
        }
        let text = describe(&b).join("\n");
        assert!(
            text.contains("permission_mode: bypassPermissions"),
            "{text}"
        );
        assert!(
            text.contains("allowed_tools: Bash Edit (when the request gives none)"),
            "{text}"
        );
        assert!(
            text.contains("disallowed_tools: Bash(git push:*) (always)"),
            "{text}"
        );
        assert!(
            text.contains(&format!("warning:     {}", backends::BYPASS_WARNING)),
            "{text}"
        );

        let text = describe(&command("aider", true)).join("\n");
        assert!(!text.contains("permission_mode"), "{text}");
    }

    #[test]
    fn describe_command_secret_env_names_variable_only() {
        let text = describe(&command_with_secret("kic", "OPENAI_API_KEY", false)).join("\n");
        assert!(
            text.contains("secret_env:  OPENAI_API_KEY (value from OS keyring entry cmd-kic"),
            "{text}"
        );
        let text = describe(&command("aider", false)).join("\n");
        assert!(!text.contains("secret_env"), "{text}");
    }

    /// A command backend with `{model}` offering `models`, default `default`.
    fn command_with_models(name: &str, models: &[&str], default: &str) -> Backend {
        let mut b = backend(
            name,
            BackendKind::Command {
                command: vec![
                    "aider".into(),
                    "--model".into(),
                    "openai/{model}".into(),
                    "{prompt}".into(),
                ],
                prompt_via_stdin: false,
                secret_env: None,
            },
            false,
        );
        b.models = Some(models.iter().map(|m| (*m).to_owned()).collect());
        b.model = Some(default.to_owned());
        b
    }

    #[test]
    fn models_column_and_describe() {
        let kic = command_with_models("kic", &["gpt-oss", "qwen", "mistral"], "qwen");
        assert_eq!(models_cell(&kic), "qwen*, gpt-oss, mistral");
        assert_eq!(models_cell(&command("aider", false)), "-");
        assert_eq!(models_cell(&oauth("s", true)), "(any)");
        let mut o = oauth("s", true);
        o.model = Some("opus".into());
        assert_eq!(models_cell(&o), "opus*, (any)");
        o.models = Some(vec!["sonnet".into(), "opus".into()]);
        assert_eq!(models_cell(&o), "opus*, sonnet");

        let table = format_table(&Backends::new(vec![kic.clone(), oauth("s", true)]), None);
        let lines: Vec<&str> = table.lines().collect();
        assert!(lines[0].contains("  MODELS  "), "{table}");
        let col = lines[0].find("MODELS").unwrap();
        let kic_line = lines.iter().find(|l| l.starts_with("kic")).unwrap();
        assert!(
            kic_line[col..].starts_with("qwen*, gpt-oss, mistral"),
            "{table}"
        );
        // Long lists are truncated.
        let many: Vec<String> = (0..20).map(|i| format!("model-number-{i}")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let mut long = command_with_models("long", &refs, "model-number-0");
        long.is_default = true;
        let table = format_table(&Backends::new(vec![long]), None);
        assert!(table.contains("model-number-0*, model-number-1"), "{table}");
        assert!(table.contains("..."), "{table}");
        assert!(!table.contains("model-number-19"), "{table}");

        let text = describe(&kic).join("\n");
        assert!(text.contains("model:       qwen (default)"), "{text}");
        assert!(
            text.contains("models:      gpt-oss, qwen, mistral (allowlist)"),
            "{text}"
        );
        let text = describe(&oauth("s", true)).join("\n");
        assert!(
            text.contains("model:       the account's default"),
            "{text}"
        );
        assert!(text.contains("models:      any (no allowlist)"), "{text}");
        let text = describe(&command("aider", false)).join("\n");
        assert!(
            text.contains("models:      none (no {model} in command"),
            "{text}"
        );
    }

    #[test]
    fn unknown_backend_lists_names() {
        let msg = unknown_backend("nope", &sample());
        assert!(msg.contains("`nope`"));
        assert!(
            msg.contains("aider, envkey, local, sidekick, work"),
            "{msg}"
        );
    }

    #[test]
    fn parallel_map_keeps_order_and_bounds() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let running = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let items: Vec<usize> = (0..20).collect();
        let out = parallel_map(&items, 3, |i| {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(5));
            running.fetch_sub(1, Ordering::SeqCst);
            i * 2
        });
        assert_eq!(out, (0..20).map(|i| i * 2).collect::<Vec<_>>());
        assert!(peak.load(Ordering::SeqCst) <= 3);
    }
}
