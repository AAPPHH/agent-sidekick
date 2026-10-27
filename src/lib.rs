//! agent-sidekick: local MCP server (stdio) that delegates prompts to a second, headless
//! agent running with its own credentials (a configured backend: Claude Code, OpenCode or
//! another agent CLI).

pub mod backends;
pub mod cli;
pub mod commands;
pub mod config;
pub mod harness;
pub mod locate;
pub mod logging;
pub mod paths;
pub mod redact;
pub mod runner;
pub mod server;
pub mod token;
pub mod trust;

/// Name used for the binary, the MCP server registration and the keyring service.
pub const APP_NAME: &str = "agent-sidekick";
