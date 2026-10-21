//! `agent-sidekick doctor`: check the installation and suggest next steps.

use std::fmt;
use std::path::Path;
use std::process::ExitCode;

use super::backends::{claude_config_dir, parallel_map};
use super::claude_cli;
use crate::backends::{self, Backend, BackendKind, Backends, Reachability};
use crate::config::Config;
use crate::locate::{self, Located};
use crate::{paths, APP_NAME};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Status::Ok => "[OK]  ",
            Status::Warn => "[WARN]",
            Status::Fail => "[FAIL]",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub status: Status,
    pub name: String,
    pub detail: String,
    pub next_step: Option<String>,
}

impl Check {
    fn ok(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            status: Status::Ok,
            name: name.into(),
            detail: detail.into(),
            next_step: None,
        }
    }
    fn warn(name: impl Into<String>, detail: impl Into<String>, next: impl Into<String>) -> Self {
        Self {
            status: Status::Warn,
            name: name.into(),
            detail: detail.into(),
            next_step: Some(next.into()),
        }
    }
    fn fail(name: impl Into<String>, detail: impl Into<String>, next: impl Into<String>) -> Self {
        Self {
            status: Status::Fail,
            ..Self::warn(name, detail, next)
        }
    }
}

#[derive(Debug, Default, clap::Args)]
pub struct DoctorArgs {
    /// Skip the daily update check (no request to GitHub).
    #[arg(long)]
    pub offline: bool,
}

pub fn run(args: DoctorArgs) -> anyhow::Result<ExitCode> {
    let checks = run_checks();
    print!("{}", format_report(&checks));
    // Informational only: never changes the exit code.
    let hint = super::update::doctor_hint(args.offline);
    if !hint.is_empty() {
        print!("\n{hint}");
    }
    Ok(exit_code(&checks))
}

fn run_checks() -> Vec<Check> {
    let mut checks = Vec::new();

    let (cfg_check, cfg) = check_config();
    let cfg_valid = cfg_check.status != Status::Fail;
    let claude = locate::find_claude(cfg.claude_path.as_deref());
    checks.push(check_claude(&claude));
    checks.push(check_config_dir());
    checks.push(cfg_check);
    checks.extend(check_backends(cfg_valid, &cfg, claude.is_ok()));
    checks.push(check_registration(claude.as_ref().ok()));
    checks.push(check_logs_dir());
    checks
}

/// Every check's detail goes through redaction, just in case.
pub fn format_report(checks: &[Check]) -> String {
    let mut out = String::new();
    for c in checks {
        let detail = crate::redact::redact_all(&c.detail);
        out.push_str(&format!("{} {}: {detail}\n", c.status, c.name));
        if let Some(next) = &c.next_step {
            out.push_str(&format!("       next step: {next}\n"));
        }
    }
    let fails = count(checks, Status::Fail);
    let warns = count(checks, Status::Warn);
    out.push_str(&match (fails, warns) {
        (0, 0) => "\nAll checks passed.\n".to_owned(),
        _ => format!("\n{fails} failed, {warns} warning(s).\n"),
    });
    out
}

pub fn exit_code(checks: &[Check]) -> ExitCode {
    if count(checks, Status::Fail) == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn count(checks: &[Check], status: Status) -> usize {
    checks.iter().filter(|c| c.status == status).count()
}

const SETUP: &str = "run `agent-sidekick setup`";
/// Re-registering keeps the stored tokens and the registration's extra variables.
const REREGISTER: &str = "run `agent-sidekick setup --register-only`";

fn check_claude(claude: &Result<Located, locate::LocateError>) -> Check {
    const NAME: &str = "claude executable";
    match claude {
        Err(e) => Check::fail(
            NAME,
            e.to_string(),
            "install Claude Code, or set AGENT_SIDEKICK_CLAUDE / claude_path in config.toml",
        ),
        Ok(found) => match claude_cli::version(&found.path) {
            Ok(v) => Check::ok(
                NAME,
                format!(
                    "{} (via {}), version {v}",
                    found.path.display(),
                    found.source
                ),
            ),
            Err(e) => Check::fail(
                NAME,
                format!(
                    "{} found but `--version` failed: {e:#}",
                    found.path.display()
                ),
                "check that this Claude Code installation works in a terminal",
            ),
        },
    }
}

/// Meaningful because CLI-mode logging never creates the config dir (see `logging`).
fn check_config_dir() -> Check {
    match paths::config_dir() {
        Ok(dir) => check_config_dir_at(&dir),
        Err(e) => Check::fail(
            "config directory",
            format!("{e:#}"),
            "set AGENT_SIDEKICK_CONFIG_DIR",
        ),
    }
}

fn check_config_dir_at(dir: &Path) -> Check {
    const NAME: &str = "config directory";
    if dir.is_dir() {
        Check::ok(NAME, dir.display().to_string())
    } else {
        Check::fail(NAME, format!("{} does not exist", dir.display()), SETUP)
    }
}

/// Returns the check and the config to use for the remaining checks (defaults on error).
fn check_config() -> (Check, Config) {
    match paths::config_file() {
        Ok(path) => check_config_file(&path),
        Err(e) => (
            Check::fail(
                "config.toml",
                format!("{e:#}"),
                "set AGENT_SIDEKICK_CONFIG_DIR",
            ),
            Config::default(),
        ),
    }
}

fn check_config_file(path: &Path) -> (Check, Config) {
    const NAME: &str = "config.toml";
    if !path.exists() {
        return (
            Check::ok(
                NAME,
                format!("{} not present, using defaults", path.display()),
            ),
            Config::default(),
        );
    }
    match Config::load_from(path) {
        Ok(cfg) => (Check::ok(NAME, path.display().to_string()), cfg),
        Err(e) => (
            Check::fail(
                NAME,
                format!("{e:#}"),
                format!("fix or delete {}", path.display()),
            ),
            Config::default(),
        ),
    }
}

const BACKENDS: &str = "backends";

/// The backends summary check plus one check per backend (checked in parallel; each
/// endpoint probe can take up to 2 s). `cfg_valid`: config.toml loaded without error;
/// otherwise `cfg` holds defaults and is not resolved (that would report the implicit
/// backend as if nothing were wrong).
fn check_backends(cfg_valid: bool, cfg: &Config, claude_found: bool) -> Vec<Check> {
    if !cfg_valid {
        return vec![Check::fail(
            BACKENDS,
            "not checked: config.toml could not be loaded",
            "fix config.toml first (see above)",
        )];
    }
    let backends = match backends::resolve(cfg) {
        Ok(b) => b,
        Err(e) => return vec![backends_check(&Err(e))],
    };
    let mut checks = vec![backends_check(&Ok(backends.clone()))];
    let list: Vec<&Backend> = backends.iter().collect();
    let claude_path = cfg.claude_path.as_deref();
    checks.extend(parallel_map(&list, 8, |b| {
        // Only the source description is kept; the credential is dropped right here.
        let credential = backends::load_credential(b).map(|(_credential, source)| source);
        let reach = backends::check_backend(b, claude_path);
        let dir_exists = claude_config_dir(b).map(|d| d.is_dir());
        backend_check(b, claude_found, &credential, &reach, dir_exists)
    }));
    checks
}

/// Summary of the backend configuration (or the validation error).
fn backends_check(resolved: &Result<Backends, String>) -> Check {
    match resolved {
        Err(e) => Check::fail(
            BACKENDS,
            format!("invalid configuration:\n{e}"),
            "fix the [backends] section of config.toml (see the README, section Backends)",
        ),
        Ok(backends) => {
            let names: Vec<String> = backends
                .iter()
                .map(|b| {
                    if b.is_default {
                        format!("{} (default)", b.name)
                    } else {
                        b.name.clone()
                    }
                })
                .collect();
            let implicit = if backends.iter().any(|b| b.implicit) {
                "; implicit backend, no [backends] section in config.toml"
            } else {
                ""
            };
            Check::ok(
                BACKENDS,
                format!(
                    "{} configured: {}{implicit}",
                    backends.len(),
                    names.join(", ")
                ),
            )
        }
    }
}

/// Status of one backend from the results of [`backends::check_backend`] (`reach`) and
/// [`backends::load_credential`] (`credential`: source description or error).
/// - unreachable claude-oauth (no token), missing endpoint key, missing command program,
///   claude not found: FAIL for the default backend, WARN for the others;
/// - claude-endpoint server not answering: always WARN (it may be started on demand);
/// - reachable but the claude-* `config_dir` does not exist: WARN.
fn backend_check(
    b: &Backend,
    claude_found: bool,
    credential: &Result<String, String>,
    reach: &Reachability,
    config_dir_exists: Option<bool>,
) -> Check {
    let name = format!("backend `{}`", b.name);
    let default = if b.is_default { ", default" } else { "" };
    let detail = format!("{}{default}: {}", b.backend_type(), reach.detail);
    let setup = format!("run `{APP_NAME} setup --backend {}`", b.name);
    let severe = |detail: String, next: String| {
        if b.is_default {
            Check::fail(name.clone(), detail, next)
        } else {
            Check::warn(name.clone(), detail, next)
        }
    };

    if reach.reachable {
        if let (Some(false), Some(dir)) = (config_dir_exists, claude_config_dir(b)) {
            return Check::warn(
                name.clone(),
                format!("{detail}; config_dir {} does not exist", dir.display()),
                format!("{setup} (it creates the directory)"),
            );
        }
        return Check::ok(name, detail);
    }
    let claude_based = !matches!(b.kind, BackendKind::Command { .. });
    if claude_based && !claude_found {
        return severe(detail, "fix the claude executable check first".to_owned());
    }
    match &b.kind {
        BackendKind::ClaudeOauth { .. } => severe(detail, setup),
        BackendKind::ClaudeEndpoint {
            api_key_env,
            base_url,
            ..
        } => match (credential, api_key_env) {
            (Err(_), Some(var)) => severe(
                detail,
                format!(
                    "set {var} in the MCP server's environment (e.g. `claude mcp add ... -e {var}=...` \
                     or the OS) and in this terminal, or remove api_key_env and {setup}"
                ),
            ),
            (Err(_), None) => severe(detail, setup),
            (Ok(_), _) => Check::warn(
                name,
                detail,
                format!(
                    "start the server at {base_url} (e.g. `ollama serve` or LM Studio's local \
                     server); ignore this if it is started on demand"
                ),
            ),
        },
        // The program is checked first, so a secret problem is only reported once the
        // program was found.
        BackendKind::Command {
            secret_env: Some(_),
            ..
        } if credential.is_err()
            && (reach.detail.starts_with("no secret stored")
                || reach.detail.starts_with("cannot read the")) =>
        {
            severe(detail, setup)
        }
        BackendKind::Command { command, .. } => severe(
            detail,
            format!(
                "install `{}` or fix PATH / `command` of [backends.{}] in config.toml",
                command.first().map(String::as_str).unwrap_or(""),
                b.name
            ),
        ),
    }
}

fn check_registration(claude: Option<&Located>) -> Check {
    const NAME: &str = "MCP registration";
    let Some(found) = claude else {
        return Check::warn(
            NAME,
            "skipped (claude not found)",
            "fix the claude check first",
        );
    };
    match claude_cli::mcp_get(&found.path) {
        Ok(None) => Check::fail(NAME, format!("{APP_NAME} is not registered"), SETUP),
        Ok(Some(entry)) => registration_check(
            &entry,
            std::env::current_exe().ok().as_deref(),
            Some(&found.path),
        ),
        Err(e) => match claude_cli::mcp_list_contains(&found.path) {
            Ok(true) => Check::ok(NAME, "registered (per `claude mcp list`)"),
            Ok(false) => Check::fail(NAME, format!("{APP_NAME} is not registered"), SETUP),
            Err(_) => Check::warn(
                NAME,
                format!("could not query Claude Code: {e:#}"),
                format!("run `claude mcp get {APP_NAME}` manually"),
            ),
        },
    }
}

/// `found_claude` is the claude doctor located in this terminal; the server instead
/// uses the `AGENT_SIDEKICK_CLAUDE` baked into the registration (no PATH fallback).
fn registration_check(
    entry: &claude_cli::McpEntry,
    self_exe: Option<&Path>,
    found_claude: Option<&Path>,
) -> Check {
    const NAME: &str = "MCP registration";
    let scope = entry.scope.as_deref().unwrap_or("unknown scope");
    let status = entry.status.as_deref().unwrap_or("unknown status");
    let detail = format!("registered ({scope}), {status}");
    if let Some(registered) = entry.claude_override() {
        let registered_path = Path::new(registered);
        if !registered_path.is_file() {
            return Check::fail(
                NAME,
                format!(
                    "{detail}; registered claude path {registered} no longer exists, \
                     so every sidekick_run will fail"
                ),
                REREGISTER.to_owned(),
            );
        }
        if let Some(found) = found_claude {
            if !same_path(registered_path, found) {
                return Check::warn(
                    NAME,
                    format!(
                        "{detail}; the server uses claude at {registered}, \
                         but doctor found {}",
                        found.display()
                    ),
                    format!("{REREGISTER} if the server should use {}", found.display()),
                );
            }
        }
    }
    if let (Some(cmd), Some(exe)) = (entry.command.as_deref(), self_exe) {
        if !same_path(Path::new(cmd), exe) {
            return Check::warn(
                NAME,
                format!(
                    "{detail}; command is {cmd}, but this binary is {}",
                    exe.display()
                ),
                format!("{REREGISTER} with the binary you want Claude Code to use"),
            );
        }
    }
    if entry.status.is_some() && !entry.is_connected() {
        let logs = paths::logs_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "the logs directory".into());
        return Check::warn(NAME, detail, format!("see the server log in {logs}"));
    }
    if !entry.is_user_scope() && entry.scope.is_some() {
        return Check::warn(
            NAME,
            detail,
            format!(
                "this entry takes precedence over a user-scope one here; remove it with {}, \
                 then {REREGISTER} to register in user scope (all projects)",
                claude_cli::remove_hint(entry)
            ),
        );
    }
    Check::ok(NAME, detail)
}

/// Same file: equal spelling (case-insensitive on Windows) or same canonical path.
fn same_path(a: &Path, b: &Path) -> bool {
    let equal = |a: &Path, b: &Path| {
        if cfg!(windows) {
            a.to_string_lossy()
                .eq_ignore_ascii_case(&b.to_string_lossy())
        } else {
            a == b
        }
    };
    if equal(a, b) {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => equal(&ca, &cb),
        _ => false,
    }
}

fn check_logs_dir() -> Check {
    const NAME: &str = "logs directory";
    let dir = match paths::logs_dir() {
        Ok(d) => d,
        Err(e) => return Check::fail(NAME, format!("{e:#}"), "set AGENT_SIDEKICK_CONFIG_DIR"),
    };
    check_logs_dir_at(&dir)
}

/// Read-only apart from a probe file: doctor does not create missing directories.
fn check_logs_dir_at(dir: &Path) -> Check {
    const NAME: &str = "logs directory";
    if !dir.is_dir() {
        return Check::warn(
            NAME,
            format!(
                "{} does not exist (created by setup or on the first server start)",
                dir.display()
            ),
            SETUP,
        );
    }
    let log_file = dir.join(crate::logging::LOG_FILE_NAME);
    if log_file.exists() && !log_file.is_file() {
        return Check::fail(
            NAME,
            format!("{} exists but is not a file", log_file.display()),
            format!("remove or rename {}", log_file.display()),
        );
    }
    match probe_writable(dir) {
        Ok(()) => Check::ok(NAME, format!("{} is writable", dir.display())),
        Err(e) => Check::fail(
            NAME,
            format!("{} is not writable: {e}", dir.display()),
            "check permissions or set AGENT_SIDEKICK_CONFIG_DIR",
        ),
    }
}

fn probe_writable(dir: &Path) -> std::io::Result<()> {
    let probe = dir.join(format!(".doctor-probe-{}", uuid::Uuid::new_v4()));
    std::fs::write(&probe, b"probe")?;
    std::fs::remove_file(&probe)
}

#[cfg(test)]
mod tests {
    use super::super::backends::tests::{command, command_with_secret, endpoint, oauth, sample};
    use super::*;
    use claude_cli::McpEntry;

    fn reach(reachable: bool, detail: &str) -> Reachability {
        Reachability {
            reachable,
            detail: detail.to_owned(),
        }
    }

    #[test]
    fn backends_summary() {
        let c = backends_check(&Ok(sample()));
        assert_eq!(c.status, Status::Ok);
        assert_eq!(c.name, "backends");
        assert!(
            c.detail
                .contains("5 configured: aider, envkey, local, sidekick (default), work"),
            "{}",
            c.detail
        );
        let c = backends_check(&Err("backends: two defaults".into()));
        assert_eq!(c.status, Status::Fail);
        assert!(c.detail.contains("two defaults"));
        assert!(c.next_step.unwrap().contains("[backends]"));
    }

    #[test]
    fn invalid_config_file_is_not_resolved_to_defaults() {
        let checks = check_backends(false, &Config::default(), true);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Fail);
        assert!(
            checks[0].detail.contains("not checked"),
            "{}",
            checks[0].detail
        );
    }

    #[test]
    fn oauth_backend_status() {
        let ok = Ok("OS keyring".to_owned());
        let missing = Err("No token stored".to_owned());
        let dflt = oauth("sidekick", true);
        let other = oauth("work", false);

        let c = backend_check(
            &dflt,
            true,
            &ok,
            &reach(true, "token from keyring"),
            Some(true),
        );
        assert_eq!(c.status, Status::Ok);
        assert_eq!(c.name, "backend `sidekick`");
        assert!(
            c.detail.starts_with("claude-oauth, default: "),
            "{}",
            c.detail
        );

        let no_token = reach(false, "no token stored; run setup");
        let c = backend_check(&dflt, true, &missing, &no_token, Some(true));
        assert_eq!(c.status, Status::Fail);
        assert!(c
            .next_step
            .unwrap()
            .contains("agent-sidekick setup --backend sidekick"));
        let c = backend_check(&other, true, &missing, &no_token, Some(true));
        assert_eq!(c.status, Status::Warn);
        assert!(c.next_step.unwrap().contains("setup --backend work"));

        // Reachable, but the config dir is missing: WARN with setup hint.
        let c = backend_check(&other, true, &ok, &reach(true, "ok"), Some(false));
        assert_eq!(c.status, Status::Warn);
        assert!(c.detail.contains("does not exist"), "{}", c.detail);
        assert!(c.next_step.unwrap().contains("creates the directory"));

        // claude missing: points to the claude check.
        let c = backend_check(&dflt, false, &ok, &reach(false, "claude not found"), None);
        assert_eq!(c.status, Status::Fail);
        assert!(c.next_step.unwrap().contains("claude executable"));
    }

    /// An api_key_env endpoint whose config dir is missing: the hint names
    /// `setup --backend`, which (since setup creates the dir for every claude-* backend)
    /// actually clears the warning.
    #[test]
    fn api_key_env_endpoint_missing_config_dir_points_to_setup_backend() {
        let b = endpoint("envkey", Some("MY_KEY"), false);
        let ok = Ok("environment variable MY_KEY".to_owned());
        let c = backend_check(&b, true, &ok, &reach(true, "ok"), Some(false));
        assert_eq!(c.status, Status::Warn);
        let next = c.next_step.unwrap();
        assert!(
            next.contains("agent-sidekick setup --backend envkey` (it creates the directory)"),
            "{next}"
        );
    }

    #[test]
    fn endpoint_backend_status() {
        let ok = Ok("OS keyring".to_owned());
        let missing = Err("No API key stored".to_owned());
        let server_down = reach(false, "unreachable: connection refused");

        // Server down: WARN even for the default backend.
        for is_default in [true, false] {
            let b = endpoint("local", None, is_default);
            let c = backend_check(&b, true, &ok, &server_down, Some(true));
            assert_eq!(c.status, Status::Warn);
            let next = c.next_step.unwrap();
            assert!(next.contains("http://localhost:11434"), "{next}");
            assert!(next.contains("on demand"), "{next}");
        }

        // Key missing: FAIL for the default, WARN otherwise.
        let no_key = reach(false, "no API key stored; run setup");
        let c = backend_check(
            &endpoint("local", None, true),
            true,
            &missing,
            &no_key,
            None,
        );
        assert_eq!(c.status, Status::Fail);
        assert!(c.next_step.unwrap().contains("setup --backend local"));
        let c = backend_check(
            &endpoint("local", None, false),
            true,
            &missing,
            &no_key,
            None,
        );
        assert_eq!(c.status, Status::Warn);

        // api_key_env not set: the hint names the variable.
        let b = endpoint("ollama", Some("OLLAMA_KEY"), false);
        let c = backend_check(&b, true, &missing, &reach(false, "env var not set"), None);
        assert_eq!(c.status, Status::Warn);
        assert!(c.next_step.unwrap().contains("OLLAMA_KEY"));

        let c = backend_check(&b, true, &ok, &reach(true, "HTTP 200"), Some(true));
        assert_eq!(c.status, Status::Ok);
    }

    #[test]
    fn command_backend_status() {
        let ok = Ok("none needed".to_owned());
        let missing = reach(false, "aider not found in PATH");
        let c = backend_check(&command("aider", false), false, &ok, &missing, None);
        assert_eq!(c.status, Status::Warn);
        let next = c.next_step.unwrap();
        assert!(next.contains("install `aider`"), "{next}");
        assert!(next.contains("[backends.aider]"), "{next}");
        let c = backend_check(&command("aider", true), true, &ok, &missing, None);
        assert_eq!(c.status, Status::Fail);
        // claude is irrelevant for command backends.
        let c = backend_check(
            &command("aider", true),
            false,
            &ok,
            &reach(true, "program"),
            None,
        );
        assert_eq!(c.status, Status::Ok);
    }

    #[test]
    fn command_backend_with_secret_env_status() {
        let b = command_with_secret("kic", "OPENAI_API_KEY", false);
        let stored = Ok("OS keyring (service agent-sidekick)".to_owned());
        let c = backend_check(
            &b,
            false,
            &stored,
            &reach(
                true,
                "program /usr/bin/aider; secret_env OPENAI_API_KEY from OS keyring",
            ),
            None,
        );
        assert_eq!(c.status, Status::Ok);

        // Program found, secret missing: setup hint.
        let missing = Err("No secret stored for backend 'kic'".to_owned());
        let no_secret = reach(
            false,
            "no secret stored for secret_env OPENAI_API_KEY; run `agent-sidekick setup --backend kic`",
        );
        let c = backend_check(&b, false, &missing, &no_secret, None);
        assert_eq!(c.status, Status::Warn);
        let next = c.next_step.unwrap();
        assert!(
            next.contains("agent-sidekick setup --backend kic"),
            "{next}"
        );

        // Program missing (checked first): install hint even if the secret is missing too.
        let c = backend_check(
            &b,
            false,
            &missing,
            &reach(false, "backend kic: program 'aider' not found in PATH"),
            None,
        );
        assert!(c.next_step.unwrap().contains("install `aider`"));
    }

    #[test]
    fn backend_details_are_redacted_in_the_report() {
        let c = backend_check(
            &oauth("work", false),
            true,
            &Ok("x".into()),
            &reach(true, "token sk-ant-oat01-SECRETVALUE1234"),
            Some(true),
        );
        let report = format_report(&[c]);
        assert!(!report.contains("SECRETVALUE"), "{report}");
    }

    #[test]
    fn report_formatting_and_exit_code() {
        let checks = vec![
            Check::ok(
                "claude executable",
                "/usr/bin/claude (via PATH), version 2.1",
            ),
            Check::warn("agent Claude config dir", "missing", "run setup"),
            Check::fail("OAuth token", "leaked sk-ant-oat01-abc123", "run setup"),
        ];
        let report = format_report(&checks);
        let expected = "[OK]   claude executable: /usr/bin/claude (via PATH), version 2.1\n\
                        [WARN] agent Claude config dir: missing\n       next step: run setup\n\
                        [FAIL] OAuth token: leaked [REDACTED]\n       next step: run setup\n\
                        \n1 failed, 1 warning(s).\n";
        assert_eq!(report, expected);
        assert_eq!(exit_code(&checks), ExitCode::FAILURE);

        let good = vec![Check::ok("a", "b")];
        assert!(format_report(&good).ends_with("All checks passed.\n"));
        assert_eq!(exit_code(&good), ExitCode::SUCCESS);
        let warn_only = vec![Check::warn("a", "b", "c")];
        assert_eq!(exit_code(&warn_only), ExitCode::SUCCESS);
    }

    #[test]
    fn config_file_check() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        assert_eq!(check_config_file(&p).0.status, Status::Ok);
        std::fs::write(&p, "max_concurrent = 3\n").unwrap();
        let (c, cfg) = check_config_file(&p);
        assert_eq!((c.status, cfg.max_concurrent), (Status::Ok, 3));
        std::fs::write(&p, "bogus = 1\n").unwrap();
        let (c, cfg) = check_config_file(&p);
        assert_eq!(c.status, Status::Fail);
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn registration_states() {
        let exe = Path::new("/bin/agent-sidekick");
        let mut e = McpEntry {
            scope: Some("User config (available in all your projects)".into()),
            status: Some("✔ Connected".into()),
            command: Some("/bin/agent-sidekick".into()),
            args: Some("serve".into()),
            env: Vec::new(),
        };
        assert_eq!(registration_check(&e, Some(exe), None).status, Status::Ok);
        e.command = Some("/old/agent-sidekick".into());
        assert_eq!(registration_check(&e, Some(exe), None).status, Status::Warn);
        e.command = Some("/bin/agent-sidekick".into());
        e.status = Some("✘ Failed to connect".into());
        assert_eq!(registration_check(&e, Some(exe), None).status, Status::Warn);

        e.status = Some("✔ Connected".into());
        e.scope = Some("Project config (shared via .mcp.json)".into());
        let c = registration_check(&e, Some(exe), None);
        assert_eq!(c.status, Status::Warn);
        assert!(c
            .next_step
            .unwrap()
            .contains("claude mcp remove agent-sidekick -s project`"));
    }

    #[test]
    fn registered_claude_path_is_checked() {
        let dir = tempfile::tempdir().unwrap();
        let registered = dir.path().join("claude.exe");
        let other = dir.path().join("other-claude.exe");
        std::fs::write(&other, "").unwrap();
        let exe = Path::new("/bin/agent-sidekick");
        let out = format!(
            "agent-sidekick:\n  Scope: User config (available in all your projects)\n  \
             Status: ✔ Connected\n  Type: stdio\n  Command: /bin/agent-sidekick\n  \
             Args: serve\n  Environment:\n    AGENT_SIDEKICK_CLAUDE={}\n",
            registered.display()
        );
        let e = claude_cli::parse_mcp_get(&out, "agent-sidekick").unwrap();

        // Registered path missing: FAIL, even if doctor found a working claude.
        let c = registration_check(&e, Some(exe), Some(&other));
        assert_eq!(c.status, Status::Fail);
        assert!(c.detail.contains("no longer exists"), "{}", c.detail);
        assert!(c.next_step.unwrap().contains("setup --register-only`"));

        std::fs::write(&registered, "").unwrap();
        // Exists and is the one doctor found: OK.
        let c = registration_check(&e, Some(exe), Some(&registered));
        assert_eq!(c.status, Status::Ok, "{}", c.detail);
        assert_eq!(registration_check(&e, Some(exe), None).status, Status::Ok);
        // Exists but differs from the one doctor found: WARN.
        let c = registration_check(&e, Some(exe), Some(&other));
        assert_eq!(c.status, Status::Warn);
        assert!(c.detail.contains("doctor found"), "{}", c.detail);

        // A directory is not a usable claude.
        std::fs::remove_file(&registered).unwrap();
        std::fs::create_dir(&registered).unwrap();
        let c = registration_check(&e, Some(exe), Some(&other));
        assert_eq!(c.status, Status::Fail);
    }

    #[test]
    fn config_dir_check_fails_for_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let c = check_config_dir_at(&missing);
        assert_eq!(c.status, Status::Fail);
        assert!(c.next_step.unwrap().contains("setup"));
        assert!(!missing.exists());
        assert_eq!(check_config_dir_at(dir.path()).status, Status::Ok);
    }

    #[test]
    fn logs_dir_check_is_read_only_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        assert_eq!(check_logs_dir_at(&logs).status, Status::Warn);
        assert!(!logs.exists());

        std::fs::create_dir(&logs).unwrap();
        assert_eq!(check_logs_dir_at(&logs).status, Status::Ok);
        assert_eq!(std::fs::read_dir(&logs).unwrap().count(), 0);

        std::fs::create_dir(logs.join(crate::logging::LOG_FILE_NAME)).unwrap();
        assert_eq!(check_logs_dir_at(&logs).status, Status::Fail);
    }
}
