//! Thin wrapper around the `claude` CLI for the management commands
//! (`--version`, `mcp get/list/add/remove`). Never used to run prompts.

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::locate::ENV_CLAUDE;
use crate::APP_NAME;

/// `claude mcp list/get` health-check every server, which can be slow.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Captured result of one CLI invocation.
#[derive(Debug, Clone)]
pub struct CliOutput {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl CliOutput {
    pub fn success(&self) -> bool {
        self.status.success()
    }

    /// stderr (or stdout if stderr is empty), trimmed and redacted, for error messages.
    pub fn diagnostics(&self) -> String {
        let text = if self.stderr.trim().is_empty() {
            &self.stdout
        } else {
            &self.stderr
        };
        crate::redact::redact_all(text.trim())
    }
}

/// Run `program` with `args`, capture both streams and kill it after `timeout`.
/// Note: for a Windows `.cmd` shim only `cmd.exe` is killed, not its node child.
pub fn run<I, S>(program: &Path, args: I, timeout: Duration) -> Result<CliOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting {}", program.display()))?;

    // Drain both pipes on threads so a chatty child cannot block on a full pipe.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let out = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let err = drain(child.stderr.take().map(|p| Box::new(p) as _));

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "{} did not finish within {}s",
                program.display(),
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Ok(CliOutput {
        status,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

/// `claude --version`, e.g. `2.1.296 (Claude Code)`.
pub fn version(claude: &Path) -> Result<String> {
    let out = run(claude, ["--version"], Duration::from_secs(30))?;
    if !out.success() {
        bail!("`claude --version` failed: {}", out.diagnostics());
    }
    Ok(out.stdout.trim().to_owned())
}

/// What `claude mcp get agent-sidekick` reports about the registration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpEntry {
    /// e.g. `User config (available in all your projects)`.
    pub scope: Option<String>,
    /// e.g. `✔ Connected` or `✘ Failed to connect`.
    pub status: Option<String>,
    pub command: Option<String>,
    pub args: Option<String>,
    /// `KEY=VALUE` lines of the `Environment:` block, in order.
    pub env: Vec<(String, String)>,
}

impl McpEntry {
    pub fn is_user_scope(&self) -> bool {
        self.scope_flag() == Some("user")
    }

    pub fn is_connected(&self) -> bool {
        self.status
            .as_deref()
            .is_some_and(|s| s.contains("Connected") && !s.contains("Failed"))
    }

    /// The `-s` value for `claude mcp remove`, derived from the scope label
    /// (`Local config (...)`, `Project config (...)`, `User config (...)`).
    /// `None` for scopes that cannot be removed with `-s` (dynamic, enterprise, ...).
    pub fn scope_flag(&self) -> Option<&'static str> {
        scope_flag(self.scope.as_deref()?)
    }

    /// Value of `AGENT_SIDEKICK_CLAUDE` in the registration, if set.
    pub fn claude_override(&self) -> Option<&str> {
        self.env
            .iter()
            .find(|(k, _)| k == ENV_CLAUDE)
            .map(|(_, v)| v.as_str())
    }
}

/// Map a scope label from `claude mcp get` to the `-s` flag value.
pub fn scope_flag(label: &str) -> Option<&'static str> {
    let label = label.trim().to_ascii_lowercase();
    ["local", "project", "user"]
        .into_iter()
        .find(|s| label.starts_with(s))
}

/// The exact command that removes `entry` (for hints), or a generic instruction
/// when its scope has no `-s` flag.
pub fn remove_hint(entry: &McpEntry) -> String {
    match entry.scope_flag() {
        Some("user") => format!("`claude mcp remove {APP_NAME} -s user`"),
        Some(flag) => format!(
            "`claude mcp remove {APP_NAME} -s {flag}` (run it in the project directory where it is configured)"
        ),
        None => format!(
            "remove it where it is configured ({})",
            entry.scope.as_deref().unwrap_or("unknown scope")
        ),
    }
}

/// Parse the stdout of `claude mcp get <name>`. Returns `None` if the header line
/// `<name>:` is missing.
pub fn parse_mcp_get(stdout: &str, name: &str) -> Option<McpEntry> {
    let header = format!("{name}:");
    let mut lines = stdout.lines().skip_while(|l| l.trim() != header);
    lines.next()?;
    let mut entry = McpEntry::default();
    // Indentation of the `Environment:` line while inside its block. Entries are
    // indented deeper and split on `=`, because values (Windows paths) contain `:`.
    let mut env_indent: Option<usize> = None;
    for line in lines {
        let indent = line.len() - line.trim_start().len();
        if let Some(block) = env_indent {
            if !line.trim().is_empty() && indent > block {
                if let Some((k, v)) = line.trim().split_once('=') {
                    entry.env.push((k.to_owned(), v.to_owned()));
                }
                continue;
            }
            env_indent = None;
        }
        let Some((key, value)) = line.trim().split_once(':') else {
            continue;
        };
        let value = Some(value.trim().to_owned());
        match key.trim() {
            "Scope" => entry.scope = value,
            "Status" => entry.status = value,
            "Command" => entry.command = value,
            "Args" => entry.args = value,
            "Environment" => env_indent = Some(indent),
            _ => {}
        }
    }
    Some(entry)
}

/// True if `claude mcp list` stdout contains a line for exactly `name`
/// (`<name>: <command> ... - ✔ Connected`).
pub fn parse_mcp_list(stdout: &str, name: &str) -> bool {
    let prefix = format!("{name}:");
    stdout.lines().any(|l| l.trim_start().starts_with(&prefix))
}

/// claude prints this (to stderr, exit 1) for `mcp get/remove` of an unknown name.
fn is_not_found(out: &CliOutput) -> bool {
    out.stderr.contains("No MCP server named") || out.stdout.contains("No MCP server named")
}

/// Look up the agent-sidekick registration (any scope).
/// `Ok(None)` = not registered; `Err` = claude could not answer.
pub fn mcp_get(claude: &Path) -> Result<Option<McpEntry>> {
    let out = run(claude, ["mcp", "get", APP_NAME], DEFAULT_TIMEOUT)?;
    if out.success() {
        // Exit 0 is authoritative; tolerate an unexpected output layout.
        return Ok(Some(
            parse_mcp_get(&out.stdout, APP_NAME).unwrap_or_default(),
        ));
    }
    if is_not_found(&out) {
        return Ok(None);
    }
    bail!("`claude mcp get {APP_NAME}` failed: {}", out.diagnostics())
}

/// True if agent-sidekick is registered. Uses `mcp get`, falls back to `mcp list`.
pub fn mcp_is_registered(claude: &Path) -> Result<bool> {
    match mcp_get(claude) {
        Ok(entry) => Ok(entry.is_some()),
        Err(get_err) => mcp_list_contains(claude).with_context(|| format!("{get_err:#}")),
    }
}

/// True if `claude mcp list` shows agent-sidekick.
pub fn mcp_list_contains(claude: &Path) -> Result<bool> {
    let out = run(claude, ["mcp", "list"], DEFAULT_TIMEOUT)?;
    if !out.success() {
        bail!("`claude mcp list` failed: {}", out.diagnostics());
    }
    Ok(parse_mcp_list(&out.stdout, APP_NAME))
}

/// `claude mcp remove agent-sidekick -s user`. Returns `false` if nothing was registered
/// in user scope.
pub fn mcp_remove(claude: &Path) -> Result<bool> {
    let out = run(
        claude,
        ["mcp", "remove", APP_NAME, "-s", "user"],
        DEFAULT_TIMEOUT,
    )?;
    if out.success() {
        Ok(true)
    } else if is_not_found(&out) {
        Ok(false)
    } else {
        bail!(
            "`claude mcp remove {APP_NAME}` failed: {}",
            out.diagnostics()
        )
    }
}

/// Exactly: `mcp add agent-sidekick -s user --env AGENT_SIDEKICK_CLAUDE=<claude> -- <exe> serve`.
/// The name precedes the variadic `--env`, and `--` terminates it.
pub fn build_mcp_add_args(claude_path_for_env: &Path, self_exe: &Path) -> Vec<OsString> {
    let mut env = OsString::from(format!("{ENV_CLAUDE}="));
    env.push(claude_path_for_env.as_os_str());
    vec![
        "mcp".into(),
        "add".into(),
        APP_NAME.into(),
        "-s".into(),
        "user".into(),
        "--env".into(),
        env,
        "--".into(),
        self_exe.as_os_str().to_owned(),
        "serve".into(),
    ]
}

/// Register the server in user scope.
pub fn mcp_add(claude: &Path, claude_path_for_env: &Path, self_exe: &Path) -> Result<()> {
    let out = run(
        claude,
        build_mcp_add_args(claude_path_for_env, self_exe),
        DEFAULT_TIMEOUT,
    )?;
    if !out.success() {
        bail!("`claude mcp add {APP_NAME}` failed: {}", out.diagnostics());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn add_args_exact_order() {
        let claude = PathBuf::from("/opt/claude dir/claude");
        let exe = PathBuf::from("/usr/local/bin/agent-sidekick");
        let args = build_mcp_add_args(&claude, &exe);
        let args: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "mcp",
                "add",
                "agent-sidekick",
                "-s",
                "user",
                "--env",
                "AGENT_SIDEKICK_CLAUDE=/opt/claude dir/claude",
                "--",
                "/usr/local/bin/agent-sidekick",
                "serve",
            ]
        );
    }

    #[test]
    fn list_parsing_matches_exact_name_only() {
        let out = "Checking MCP server health…\n\n\
                   context7: cmd /c npx -y @upstash/context7-mcp - ✔ Connected\n\
                   agent-sidekick-wftest: C:\\x.exe serve - ✔ Connected\n";
        assert!(!parse_mcp_list(out, "agent-sidekick"));
        let out = format!(
            "{out}agent-sidekick: C:\\bin\\agent-sidekick.exe serve - ✘ Failed to connect\n"
        );
        assert!(parse_mcp_list(&out, "agent-sidekick"));
        assert!(!parse_mcp_list("", "agent-sidekick"));
    }

    #[test]
    fn get_parsing() {
        let out = "agent-sidekick:\n  Scope: User config (available in all your projects)\n  \
                   Status: ✔ Connected\n  Type: stdio\n  Command: C:\\bin\\agent-sidekick.exe\n  \
                   Args: serve\n  Environment:\n    AGENT_SIDEKICK_CLAUDE=C:\\claude.exe\n\n\
                   To remove this server, run: claude mcp remove agent-sidekick -s user\n";
        let e = parse_mcp_get(out, "agent-sidekick").unwrap();
        assert!(e.is_user_scope());
        assert!(e.is_connected());
        assert_eq!(e.command.as_deref(), Some("C:\\bin\\agent-sidekick.exe"));
        assert_eq!(e.args.as_deref(), Some("serve"));
        assert_eq!(
            e.env,
            [(
                "AGENT_SIDEKICK_CLAUDE".to_owned(),
                "C:\\claude.exe".to_owned()
            )]
        );
        assert_eq!(e.claude_override(), Some("C:\\claude.exe"));

        let failed = "agent-sidekick:\n  Scope: Local config\n  Status: ✘ Failed to connect\n";
        let e = parse_mcp_get(failed, "agent-sidekick").unwrap();
        assert!(!e.is_user_scope());
        assert!(!e.is_connected());

        assert!(parse_mcp_get("other:\n  Scope: User\n", "agent-sidekick").is_none());
        assert_eq!(e.claude_override(), None);
    }

    /// Layout as printed by Claude Code 2.x: `Environment:` block followed by more keys.
    #[test]
    fn get_parsing_environment_block() {
        let out = "agent-sidekick:\n  Scope: Local config (private to you in this project)\n  \
                   Status: ✘ Failed to connect\n  Type: stdio\n  Command: /usr/bin/agent-sidekick\n  \
                   Args: serve\n  Environment:\n    FOO=a=b\n    AGENT_SIDEKICK_CLAUDE=/opt/claude: x/claude\n  \
                   Timeout: 5000ms\n\n\
                   To remove this server, run: claude mcp remove agent-sidekick -s local\n";
        let e = parse_mcp_get(out, "agent-sidekick").unwrap();
        assert_eq!(
            e.env,
            [
                ("FOO".to_owned(), "a=b".to_owned()),
                (
                    "AGENT_SIDEKICK_CLAUDE".to_owned(),
                    "/opt/claude: x/claude".to_owned()
                ),
            ]
        );
        assert_eq!(e.claude_override(), Some("/opt/claude: x/claude"));
        assert_eq!(e.scope_flag(), Some("local"));
        assert_eq!(e.command.as_deref(), Some("/usr/bin/agent-sidekick"));

        // CRLF output and an Environment block at the very end.
        let crlf = "agent-sidekick:\r\n  Scope: User config\r\n  Environment:\r\n    \
                    AGENT_SIDEKICK_CLAUDE=C:\\c.exe\r\n";
        let e = parse_mcp_get(crlf, "agent-sidekick").unwrap();
        assert_eq!(e.claude_override(), Some("C:\\c.exe"));
        assert!(e.is_user_scope());
    }

    #[test]
    fn scope_mapping() {
        let cases = [
            (
                "Local config (private to you in this project)",
                Some("local"),
            ),
            ("Project config (shared via .mcp.json)", Some("project")),
            ("User config (available in all your projects)", Some("user")),
            ("Dynamic config (from command line)", None),
            ("Enterprise config (managed by your organization)", None),
        ];
        for (label, flag) in cases {
            assert_eq!(scope_flag(label), flag, "{label}");
            let entry = McpEntry {
                scope: Some(label.into()),
                ..Default::default()
            };
            assert_eq!(entry.scope_flag(), flag);
            assert_eq!(entry.is_user_scope(), flag == Some("user"));
            let hint = remove_hint(&entry);
            match flag {
                Some(f) => assert!(
                    hint.contains(&format!("claude mcp remove agent-sidekick -s {f}`")),
                    "{hint}"
                ),
                None => assert!(hint.contains(label), "{hint}"),
            }
        }
        assert_eq!(McpEntry::default().scope_flag(), None);
    }

    #[cfg(windows)]
    fn shell(script: &str) -> (PathBuf, Vec<String>) {
        (PathBuf::from("cmd.exe"), vec!["/C".into(), script.into()])
    }
    #[cfg(unix)]
    fn shell(script: &str) -> (PathBuf, Vec<String>) {
        (PathBuf::from("sh"), vec!["-c".into(), script.into()])
    }

    #[test]
    fn run_captures_streams_and_status() {
        let (prog, args) = shell("echo out && echo err 1>&2 && exit 3");
        let out = run(&prog, &args, Duration::from_secs(20)).unwrap();
        assert_eq!(out.stdout.trim(), "out");
        assert_eq!(out.stderr.trim(), "err");
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(out.diagnostics(), "err");
    }

    #[test]
    fn run_times_out() {
        #[cfg(windows)]
        let (prog, args) = shell("ping -n 30 127.0.0.1 >NUL");
        #[cfg(unix)]
        let (prog, args) = shell("sleep 30");
        let start = Instant::now();
        let err = run(&prog, &args, Duration::from_millis(300)).unwrap_err();
        assert!(err.to_string().contains("did not finish"));
        assert!(start.elapsed() < Duration::from_secs(20));
    }

    /// Read-only smoke test against a real Claude Code (no prompt, no quota):
    /// `AGENT_SIDEKICK_TEST_CLAUDE=<path> cargo test -- --ignored real_claude`.
    #[test]
    #[ignore]
    fn real_claude_read_only() {
        let claude = PathBuf::from(std::env::var_os("AGENT_SIDEKICK_TEST_CLAUDE").unwrap());
        assert!(!version(&claude).unwrap().is_empty());
        mcp_get(&claude).unwrap();
        mcp_list_contains(&claude).unwrap();
    }
}
