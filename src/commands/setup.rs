//! `agent-sidekick setup`: store the agent account's token and register the MCP server.

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Context, Result};

use super::claude_cli;
use crate::config::Config;
use crate::locate::{self, Located};
use crate::token::{self, FormatCheck, Token};
use crate::{paths, APP_NAME};

#[derive(Debug, clap::Args)]
pub struct SetupArgs {
    /// Read the token from stdin instead of prompting (for scripting).
    #[arg(long)]
    pub token_stdin: bool,
    /// Do not register the MCP server with Claude Code.
    #[arg(long)]
    pub no_register: bool,
    /// Explicit path to the `claude` executable (skips auto-detection).
    #[arg(long, value_name = "PATH")]
    pub claude: Option<PathBuf>,
}

pub fn run(args: SetupArgs) -> Result<ExitCode> {
    let self_exe = std::env::current_exe().context("determining the path of this executable")?;
    if looks_like_dev_build(&self_exe) {
        eprintln!(
            "warning: {} looks like a cargo build output; the MCP registration will point there. \
             Install with `cargo install --path .` (or copy the binary somewhere stable) for regular use.",
            self_exe.display()
        );
    }

    let claude = match resolve_claude(args.claude.as_deref()) {
        Ok(found) => {
            let via = match args.claude {
                Some(_) => "--claude".to_owned(),
                None => found.source.to_string(),
            };
            println!("Using claude: {} (via {via})", found.path.display());
            Some(found)
        }
        Err(e) if args.no_register => {
            eprintln!("warning: {e:#}");
            None
        }
        Err(e) => {
            return Err(e.context(
                "Claude Code is required for registration. Next steps: install Claude Code, \
                 or pass --claude <path>, or set AGENT_SIDEKICK_CLAUDE, or use --no-register",
            ))
        }
    };

    let agent_dir = paths::agent_claude_config_dir()?;
    if !args.token_stdin {
        require_terminal(std::io::stdin().is_terminal())?;
        println!("\n{}", token_instructions(&agent_dir));
    }
    let token = read_token(args.token_stdin)?;
    if let FormatCheck::Warn(msg) = token::check_format(token.expose()) {
        eprintln!("warning: {msg}");
    }

    // Before storing the token (the token file lives in the config dir), so that
    // "Created" is reported accurately. Other commands never create these directories.
    for dir in [paths::config_dir()?, paths::logs_dir()?, agent_dir] {
        if !dir.exists() {
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            println!("Created {}", dir.display());
        }
    }

    let source = token::store(&token)?;
    drop(token);
    println!("Token stored in {source}.");

    match &claude {
        Some(found) if !args.no_register => register(&found.path, &self_exe)?,
        _ => println!("Skipping MCP registration (--no-register)."),
    }

    println!(
        "\nDone. Reload VS Code (Developer: Reload Window) and check with /mcp in Claude Code.\n\
         Run `{APP_NAME} doctor` to verify the installation."
    );
    Ok(ExitCode::SUCCESS)
}

/// `--claude` wins over auto-detection.
fn resolve_claude(explicit: Option<&Path>) -> Result<Located> {
    if let Some(p) = explicit {
        let abs = std::path::absolute(p).with_context(|| format!("resolving {}", p.display()))?;
        if !abs.is_file() {
            bail!("--claude {} does not exist or is not a file", abs.display());
        }
        // `LocateSource` has no variant for a CLI flag; the caller labels it `--claude`.
        return Ok(Located {
            path: abs,
            source: locate::LocateSource::Env,
        });
    }
    let cfg = Config::load().unwrap_or_else(|e| {
        eprintln!("warning: ignoring config.toml: {e:#}");
        Config::default()
    });
    Ok(locate::find_claude(cfg.claude_path.as_deref())?)
}

fn register(claude: &Path, self_exe: &Path) -> Result<()> {
    // `mcp get` shows only the entry that wins in this directory (local > project > user),
    // so a user entry may exist behind a local one: always try to remove the user entry.
    let registered = match claude_cli::mcp_get(claude) {
        Ok(entry) => entry.is_some(),
        Err(get_err) => {
            claude_cli::mcp_list_contains(claude).with_context(|| format!("{get_err:#}"))?
        }
    };
    if registered && claude_cli::mcp_remove(claude)? {
        println!("Removed existing {APP_NAME} registration (user scope).");
    }
    claude_cli::mcp_add(claude, claude, self_exe)?;
    println!(
        "Registered MCP server `{APP_NAME}` (user scope): {} serve",
        self_exe.display()
    );
    // A local or project entry would still take precedence over the new one here.
    match claude_cli::mcp_get(claude) {
        Ok(Some(entry)) => {
            if let Some(warning) = shadow_warning(&entry) {
                eprintln!("warning: {warning}");
            }
        }
        Ok(None) => eprintln!(
            "warning: `claude mcp get {APP_NAME}` does not show the new registration; \
             check with `claude mcp list`."
        ),
        Err(e) => eprintln!("warning: could not verify the registration: {e:#}"),
    }
    Ok(())
}

/// Warning text if `entry` (what `mcp get` reports after registering) is not the
/// user-scope entry, i.e. another entry shadows it in this directory.
fn shadow_warning(entry: &claude_cli::McpEntry) -> Option<String> {
    let scope = entry.scope.as_deref()?;
    if entry.is_user_scope() {
        return None;
    }
    Some(format!(
        "another `{APP_NAME}` entry ({scope}) takes precedence over the user-scope registration \
         in this directory and may point to an old binary. Remove it with {}.",
        claude_cli::remove_hint(entry)
    ))
}

/// The hidden prompt reads from the console, not from stdin; without a terminal it
/// can wait forever (e.g. a token piped in without `--token-stdin`).
fn require_terminal(stdin_is_terminal: bool) -> Result<()> {
    if !stdin_is_terminal {
        bail!(
            "stdin is not a terminal, so the token cannot be prompted for; \
             pipe it in with `{APP_NAME} setup --token-stdin`"
        );
    }
    Ok(())
}

fn read_token(from_stdin: bool) -> Result<Token> {
    let raw = if from_stdin {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .context("reading the token from stdin")?;
        s
    } else {
        rpassword::prompt_password("Paste the agent account's OAuth token (input hidden): ")
            .context("reading the token from the terminal")?
    };
    let token = Token::new(raw);
    if token.expose().is_empty() {
        bail!("no token entered");
    }
    Ok(token)
}

/// How to create a long-lived token for the agent account in its own config dir.
fn token_instructions(agent_dir: &Path) -> String {
    let cmd = if cfg!(windows) {
        format!(
            "  $env:CLAUDE_CONFIG_DIR=\"{}\"; claude setup-token",
            agent_dir.display()
        )
    } else {
        format!(
            "  CLAUDE_CONFIG_DIR={} claude setup-token",
            agent_dir.display()
        )
    };
    format!(
        "Create an OAuth token for the agent account (log in with THAT account in the browser).\n\
         In a separate terminal run:\n{cmd}\n\
         and copy the printed token (starts with {}).",
        token::EXPECTED_PREFIX
    )
}

/// `.../target/debug/...` or `.../target/release/...`.
fn looks_like_dev_build(exe: &Path) -> bool {
    let parts: Vec<_> = exe.components().map(|c| c.as_os_str()).collect();
    parts
        .windows(2)
        .any(|w| w[0] == "target" && (w[1] == "debug" || w[1] == "release"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_build_detection() {
        assert!(looks_like_dev_build(Path::new(
            "/home/u/agent-sidekick/target/debug/agent-sidekick"
        )));
        assert!(looks_like_dev_build(Path::new(
            "/home/u/x/target/release/agent-sidekick"
        )));
        assert!(!looks_like_dev_build(Path::new(
            "/home/u/.cargo/bin/agent-sidekick"
        )));
    }

    #[test]
    fn instructions_mention_dir_and_command() {
        let text = token_instructions(Path::new("/home/u/.claude-sidekick"));
        assert!(text.contains("claude setup-token"));
        assert!(text.contains(".claude-sidekick"));
        assert!(text.contains("CLAUDE_CONFIG_DIR"));
    }

    #[test]
    fn non_terminal_stdin_points_to_token_stdin() {
        assert!(require_terminal(true).is_ok());
        let err = require_terminal(false).unwrap_err().to_string();
        assert!(err.contains("--token-stdin"), "{err}");
    }

    #[test]
    fn shadow_warning_names_the_scope_specific_command() {
        let entry = |scope: Option<&str>| claude_cli::McpEntry {
            scope: scope.map(Into::into),
            ..Default::default()
        };
        let user = entry(Some("User config (available in all your projects)"));
        assert_eq!(shadow_warning(&user), None);
        assert_eq!(shadow_warning(&entry(None)), None);
        let w = shadow_warning(&entry(Some(
            "Local config (private to you in this project)",
        )));
        let w = w.unwrap();
        assert!(
            w.contains("claude mcp remove agent-sidekick -s local`"),
            "{w}"
        );
        let w = shadow_warning(&entry(Some("Project config (shared via .mcp.json)"))).unwrap();
        assert!(
            w.contains("claude mcp remove agent-sidekick -s project`"),
            "{w}"
        );
    }

    #[test]
    fn explicit_claude_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_claude(Some(&dir.path().join("nope"))).is_err());
        let f = dir.path().join("claude");
        std::fs::write(&f, "").unwrap();
        assert_eq!(resolve_claude(Some(&f)).unwrap().path, f);
    }
}
