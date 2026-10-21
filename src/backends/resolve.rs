//! Validation of `[backends]` into [`Backends`].
//! INTERFACE STUB: implemented by the `resolve` task.

use std::path::{Path, PathBuf};

use super::Backends;
use crate::config::Config;

/// Inputs that come from the environment, injected for testability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveDefaults {
    /// Home directory for `~` expansion and `~/.claude-sidekick-<name>` defaults.
    pub home: Option<PathBuf>,
    /// Config dir of the implicit/`sidekick` claude-oauth backend when none is configured
    /// (`AGENT_SIDEKICK_CLAUDE_CONFIG_DIR` or `~/.claude-sidekick`).
    pub legacy_agent_config_dir: Option<PathBuf>,
}

impl ResolveDefaults {
    /// From the real environment (`paths::home_dir`, `paths::agent_claude_config_dir`).
    pub fn from_env() -> Self {
        Self {
            home: crate::paths::home_dir().ok(),
            legacy_agent_config_dir: crate::paths::agent_claude_config_dir().ok(),
        }
    }
}

/// Validate `cfg.backends` (or create the implicit backend) using the real environment.
/// The error is a user-facing message naming the backend and the problem.
pub fn resolve(cfg: &Config) -> Result<Backends, String> {
    resolve_with(cfg, &ResolveDefaults::from_env())
}

/// Testable core of [`resolve`].
pub fn resolve_with(_cfg: &Config, _defaults: &ResolveDefaults) -> Result<Backends, String> {
    todo!("resolve task")
}

/// Expand a leading `~` (`~`, `~/x`, `~\x`) with `home`.
pub fn expand_tilde(_p: &Path, _home: Option<&Path>) -> PathBuf {
    todo!("resolve task")
}
