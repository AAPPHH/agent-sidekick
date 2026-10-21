//! `agent-sidekick doctor`: check the installation and suggest next steps.

use std::fmt;
use std::path::Path;
use std::process::ExitCode;

use super::claude_cli;
use crate::config::Config;
use crate::locate::{self, Located};
use crate::{paths, token, APP_NAME};

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
    pub name: &'static str,
    pub detail: String,
    pub next_step: Option<String>,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            status: Status::Ok,
            name,
            detail: detail.into(),
            next_step: None,
        }
    }
    fn warn(name: &'static str, detail: impl Into<String>, next: impl Into<String>) -> Self {
        Self {
            status: Status::Warn,
            name,
            detail: detail.into(),
            next_step: Some(next.into()),
        }
    }
    fn fail(name: &'static str, detail: impl Into<String>, next: impl Into<String>) -> Self {
        Self {
            status: Status::Fail,
            ..Self::warn(name, detail, next)
        }
    }
}

pub fn run() -> anyhow::Result<ExitCode> {
    let checks = run_checks();
    print!("{}", format_report(&checks));
    Ok(exit_code(&checks))
}

fn run_checks() -> Vec<Check> {
    let mut checks = Vec::new();

    let (cfg_check, cfg) = check_config();
    let claude = locate::find_claude(cfg.claude_path.as_deref());
    checks.push(check_claude(&claude));
    checks.push(check_token());
    checks.push(check_config_dir());
    checks.push(check_agent_dir());
    checks.push(cfg_check);
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

fn check_token() -> Check {
    const NAME: &str = "OAuth token";
    match token::load() {
        Ok((_, source)) => Check::ok(NAME, format!("found in {source}")),
        Err(token::TokenError::Missing) => Check::fail(NAME, "no token stored", SETUP),
        Err(e) => Check::fail(NAME, e.to_string(), SETUP),
    }
}

fn check_config_dir() -> Check {
    const NAME: &str = "config directory";
    match paths::config_dir() {
        Ok(dir) if dir.is_dir() => Check::ok(NAME, dir.display().to_string()),
        Ok(dir) => Check::fail(NAME, format!("{} does not exist", dir.display()), SETUP),
        Err(e) => Check::fail(NAME, format!("{e:#}"), "set AGENT_SIDEKICK_CONFIG_DIR"),
    }
}

fn check_agent_dir() -> Check {
    const NAME: &str = "agent Claude config dir";
    match paths::agent_claude_config_dir() {
        Ok(dir) if dir.is_dir() => Check::ok(NAME, dir.display().to_string()),
        Ok(dir) => Check::warn(
            NAME,
            format!("{} does not exist", dir.display()),
            format!("{SETUP} (it creates the directory)"),
        ),
        Err(e) => Check::warn(
            NAME,
            format!("{e:#}"),
            "set AGENT_SIDEKICK_CLAUDE_CONFIG_DIR",
        ),
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
        Ok(Some(entry)) => registration_check(&entry, std::env::current_exe().ok().as_deref()),
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

fn registration_check(entry: &claude_cli::McpEntry, self_exe: Option<&Path>) -> Check {
    const NAME: &str = "MCP registration";
    let scope = entry.scope.as_deref().unwrap_or("unknown scope");
    let status = entry.status.as_deref().unwrap_or("unknown status");
    let detail = format!("registered ({scope}), {status}");
    if let (Some(cmd), Some(exe)) = (entry.command.as_deref(), self_exe) {
        if !same_path(Path::new(cmd), exe) {
            return Check::warn(
                NAME,
                format!(
                    "{detail}; command is {cmd}, but this binary is {}",
                    exe.display()
                ),
                format!("{SETUP} again with the binary you want Claude Code to use"),
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
            format!("{SETUP} to register in user scope (all projects)"),
        );
    }
    Check::ok(NAME, detail)
}

fn same_path(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
    } else {
        a == b
    }
}

fn check_logs_dir() -> Check {
    const NAME: &str = "logs directory";
    let dir = match paths::logs_dir() {
        Ok(d) => d,
        Err(e) => return Check::fail(NAME, format!("{e:#}"), "set AGENT_SIDEKICK_CONFIG_DIR"),
    };
    match probe_writable(&dir) {
        Ok(()) => Check::ok(NAME, format!("{} is writable", dir.display())),
        Err(e) => Check::fail(
            NAME,
            format!("{} is not writable: {e}", dir.display()),
            "check permissions or set AGENT_SIDEKICK_CONFIG_DIR",
        ),
    }
}

fn probe_writable(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let probe = dir.join(format!(".doctor-probe-{}", uuid::Uuid::new_v4()));
    std::fs::write(&probe, b"probe")?;
    std::fs::remove_file(&probe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_cli::McpEntry;

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
        };
        assert_eq!(registration_check(&e, Some(exe)).status, Status::Ok);
        e.command = Some("/old/agent-sidekick".into());
        assert_eq!(registration_check(&e, Some(exe)).status, Status::Warn);
        e.command = Some("/bin/agent-sidekick".into());
        e.status = Some("✘ Failed to connect".into());
        assert_eq!(registration_check(&e, Some(exe)).status, Status::Warn);
    }

    #[test]
    fn probe_creates_dir_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        probe_writable(&logs).unwrap();
        assert_eq!(std::fs::read_dir(&logs).unwrap().count(), 0);
    }
}
