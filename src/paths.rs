//! Filesystem locations. Every location can be overridden through an environment
//! variable so tests and unusual setups never touch the real user directories.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Overrides the config directory (default: `<os config dir>/agent-sidekick`).
pub const ENV_CONFIG_DIR: &str = "AGENT_SIDEKICK_CONFIG_DIR";
/// Overrides the data directory (default: `<config dir>/data`).
pub const ENV_DATA_DIR: &str = "AGENT_SIDEKICK_DATA_DIR";
/// Overrides the Claude config dir used by the agent (default: `~/.claude-sidekick`).
pub const ENV_CLAUDE_CONFIG_DIR: &str = "AGENT_SIDEKICK_CLAUDE_CONFIG_DIR";

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

pub fn home_dir() -> Result<PathBuf> {
    directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .context("could not determine the home directory")
}

/// Linux: `~/.config/agent-sidekick`, Windows: `%APPDATA%\agent-sidekick`,
/// macOS: `~/Library/Application Support/agent-sidekick`.
pub fn config_dir() -> Result<PathBuf> {
    if let Some(p) = env_path(ENV_CONFIG_DIR) {
        return Ok(p);
    }
    directories::BaseDirs::new()
        .map(|b| b.config_dir().join(crate::APP_NAME))
        .context("could not determine the user config directory")
}

pub fn config_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

pub fn token_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("token"))
}

pub fn data_dir() -> Result<PathBuf> {
    if let Some(p) = env_path(ENV_DATA_DIR) {
        return Ok(p);
    }
    Ok(config_dir()?.join("data"))
}

pub fn logs_dir() -> Result<PathBuf> {
    Ok(config_dir()?.join("logs"))
}

/// `CLAUDE_CONFIG_DIR` handed to the agent's Claude Code process.
pub fn agent_claude_config_dir() -> Result<PathBuf> {
    if let Some(p) = env_path(ENV_CLAUDE_CONFIG_DIR) {
        return Ok(p);
    }
    Ok(home_dir()?.join(".claude-sidekick"))
}
