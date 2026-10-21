//! `agent-sidekick uninstall`: remove the token and the MCP registration.
//! Every step runs even if an earlier one fails; all errors are reported at the end.

use std::process::ExitCode;

use anyhow::Result;

use super::claude_cli;
use crate::config::Config;
use crate::{locate, paths, token, APP_NAME};

#[derive(Debug, clap::Args)]
pub struct UninstallArgs {
    /// Also delete the config directory (config.toml, token file, logs).
    #[arg(long)]
    pub purge: bool,
}

pub fn run(args: UninstallArgs) -> Result<ExitCode> {
    let mut errors = Vec::new();
    let mut step = |name: &str, result: Result<()>| {
        if let Err(e) = result {
            let msg = crate::redact::redact_all(&format!("{name}: {e:#}"));
            eprintln!("error: {msg}");
            errors.push(msg);
        }
    };

    step("removing the token", remove_token());
    step("removing the MCP registration", remove_registration());
    if args.purge {
        step("removing the config directory", purge_config_dir());
    }

    if let Ok(agent_dir) = paths::agent_claude_config_dir() {
        if agent_dir.exists() {
            println!(
                "Note: {} contains the agent account's Claude Code settings and login; \
                 it was left in place. Delete it manually if you no longer need it.",
                agent_dir.display()
            );
        }
    }

    if errors.is_empty() {
        println!("Uninstall complete.");
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!("Uninstall finished with {} error(s).", errors.len());
        Ok(ExitCode::FAILURE)
    }
}

fn remove_token() -> Result<()> {
    let removed = token::delete()?;
    if removed.is_empty() {
        println!("No stored token found.");
    }
    for what in removed {
        println!("Removed token: {what}");
    }
    Ok(())
}

fn remove_registration() -> Result<()> {
    let cfg = Config::load().unwrap_or_default();
    let found = match locate::find_claude(cfg.claude_path.as_deref()) {
        Ok(f) => f,
        Err(e) => {
            println!(
                "Skipping MCP deregistration ({e}). If needed run: claude mcp remove {APP_NAME} -s user"
            );
            return Ok(());
        }
    };
    if claude_cli::mcp_remove(&found.path)? {
        println!("Removed MCP server `{APP_NAME}` from Claude Code (user scope).");
    } else {
        println!("MCP server `{APP_NAME}` was not registered in user scope.");
    }
    // Entries in local/project scope (for the current directory) are left alone.
    if let Ok(Some(entry)) = claude_cli::mcp_get(&found.path) {
        if !entry.is_user_scope() {
            println!(
                "Note: `{APP_NAME}` is still registered here ({}). Remove it with {}.",
                entry.scope.as_deref().unwrap_or("unknown scope"),
                claude_cli::remove_hint(&entry)
            );
        }
    }
    Ok(())
}

fn purge_config_dir() -> Result<()> {
    let dir = paths::config_dir()?;
    if !dir.exists() {
        println!("Config directory {} does not exist.", dir.display());
        return Ok(());
    }
    remove_dir(&dir)?;
    println!("Removed config directory {}", dir.display());
    Ok(())
}

fn remove_dir(dir: &std::path::Path) -> Result<()> {
    use anyhow::Context;
    std::fs::remove_dir_all(dir).with_context(|| format!("deleting {}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_dir_is_recursive() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("cfg");
        std::fs::create_dir_all(cfg.join("logs")).unwrap();
        std::fs::write(cfg.join("logs").join("x.log"), "x").unwrap();
        remove_dir(&cfg).unwrap();
        assert!(!cfg.exists());
    }
}
