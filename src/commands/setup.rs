//! `agent-sidekick setup`: store the agent account's token and register the MCP server.

use std::io::Read;
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
        println!("\n{}", token_instructions(&agent_dir));
    }
    let token = read_token(args.token_stdin)?;
    if let FormatCheck::Warn(msg) = token::check_format(token.expose()) {
        eprintln!("warning: {msg}");
    }
    let source = token::store(&token)?;
    drop(token);
    println!("Token stored in {source}.");

    for dir in [paths::config_dir()?, agent_dir] {
        if !dir.exists() {
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            println!("Created {}", dir.display());
        }
    }

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
    if claude_cli::mcp_is_registered(claude)? {
        println!("Removing existing {APP_NAME} registration...");
        if !claude_cli::mcp_remove(claude)? {
            eprintln!(
                "warning: {APP_NAME} is registered in a non-user scope (local/project); \
                 that entry takes precedence there. Remove it with `claude mcp remove {APP_NAME} -s local`."
            );
        }
    }
    claude_cli::mcp_add(claude, claude, self_exe)?;
    println!(
        "Registered MCP server `{APP_NAME}` (user scope): {} serve",
        self_exe.display()
    );
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
    fn explicit_claude_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_claude(Some(&dir.path().join("nope"))).is_err());
        let f = dir.path().join("claude");
        std::fs::write(&f, "").unwrap();
        assert_eq!(resolve_claude(Some(&f)).unwrap().path, f);
    }
}
