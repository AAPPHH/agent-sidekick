//! Optional `config.toml` with default values.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const MAX_TURNS_LIMIT: u32 = 200;
pub const MAX_TIMEOUT_S: u64 = 7200;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Maximum number of `claude` runs at the same time.
    pub max_concurrent: usize,
    pub default_max_turns: u32,
    pub default_model: Option<String>,
    pub default_timeout_s: u64,
    /// Explicit path to the `claude` executable (checked after `AGENT_SIDEKICK_CLAUDE`).
    pub claude_path: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_concurrent: 2,
            default_max_turns: 30,
            default_model: None,
            default_timeout_s: 1800,
            claude_path: None,
        }
    }
}

impl Config {
    /// Load from `path`; a missing file yields the defaults.
    pub fn load_from(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mut cfg: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.clamp();
        Ok(cfg)
    }

    /// Load from the default location (`<config-dir>/config.toml`).
    pub fn load() -> Result<Self> {
        Self::load_from(&crate::paths::config_file()?)
    }

    fn clamp(&mut self) {
        self.max_concurrent = self.max_concurrent.max(1);
        self.default_max_turns = self.default_max_turns.clamp(1, MAX_TURNS_LIMIT);
        self.default_timeout_s = self.default_timeout_s.clamp(1, MAX_TIMEOUT_S);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load_from(&dir.path().join("nope.toml")).unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn partial_file_and_clamping() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "max_concurrent = 0\ndefault_max_turns = 999\n").unwrap();
        let cfg = Config::load_from(&p).unwrap();
        assert_eq!(cfg.max_concurrent, 1);
        assert_eq!(cfg.default_max_turns, MAX_TURNS_LIMIT);
        assert_eq!(cfg.default_timeout_s, 1800);
    }

    #[test]
    fn unknown_key_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "bogus = 1\n").unwrap();
        assert!(Config::load_from(&p).is_err());
    }
}
