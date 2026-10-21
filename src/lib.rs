//! agent-sidekick: local MCP server (stdio) that delegates prompts to a second,
//! headless Claude Code instance running with its own credentials.

pub mod backends;
pub mod cli;
pub mod commands;
pub mod config;
pub mod locate;
pub mod logging;
pub mod paths;
pub mod redact;
pub mod runner;
pub mod server;
pub mod token;

/// Name used for the binary, the MCP server registration and the keyring service.
pub const APP_NAME: &str = "agent-sidekick";
