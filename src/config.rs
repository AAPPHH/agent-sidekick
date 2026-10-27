//! Optional `config.toml` with default values.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub use crate::backends::RawBackend;

pub const MAX_TURNS_LIMIT: u32 = 200;
pub const MAX_TIMEOUT_S: u64 = 7200;
/// Upper bound of `max_concurrent` (global: clamped with a warning; per backend: a config
/// error). Far below tokio's `Semaphore::MAX_PERMITS`, so building the semaphores never panics.
pub const MAX_CONCURRENT_LIMIT: usize = 1024;

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
    /// Names of inherited environment variables that are passed to every backend even
    /// though they would normally be removed (see `backends::launch`).
    pub pass_env: Vec<String>,
    /// `[backends.<name>]` tables as written. `None`: no `[backends]` section, which means
    /// the implicit claude-oauth backend `claude` (v0.1 behaviour). Validated and turned
    /// into [`crate::backends::Backends`] by [`crate::backends::resolve`].
    pub backends: Option<BTreeMap<String, RawBackend>>,
    /// Git remote of the public code repository that carries `data/` (experience logs).
    pub data_remote: String,
    /// Contributor name = prefix under `data/experiences/`. Unset: nothing is pushed.
    pub data_name: Option<String>,
}

pub const DEFAULT_DATA_REMOTE: &str = "https://github.com/AAPPHH/agent-sidekick.git";

impl Default for Config {
    fn default() -> Self {
        Self {
            max_concurrent: 16,
            default_max_turns: 30,
            default_model: None,
            default_timeout_s: 1800,
            claude_path: None,
            pass_env: Vec::new(),
            backends: None,
            data_remote: DEFAULT_DATA_REMOTE.into(),
            data_name: None,
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
        Self::parse(&text, path)
    }

    pub fn parse(text: &str, path: &Path) -> Result<Self> {
        let mut cfg: Self =
            toml::from_str(text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.clamp();
        Ok(cfg)
    }

    /// Load from the default location (`<config-dir>/config.toml`).
    pub fn load() -> Result<Self> {
        Self::load_from(&crate::paths::config_file()?)
    }

    fn clamp(&mut self) {
        if self.max_concurrent > MAX_CONCURRENT_LIMIT {
            tracing::warn!(
                "max_concurrent = {} is above the limit; using {MAX_CONCURRENT_LIMIT}",
                self.max_concurrent
            );
        }
        self.max_concurrent = self.max_concurrent.clamp(1, MAX_CONCURRENT_LIMIT);
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
        assert_eq!(cfg.max_concurrent, 16);
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
    fn huge_max_concurrent_is_clamped() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "max_concurrent = 4611686018427387904\n").unwrap();
        let cfg = Config::load_from(&p).unwrap();
        assert_eq!(cfg.max_concurrent, MAX_CONCURRENT_LIMIT);
        assert!(cfg.max_concurrent <= tokio::sync::Semaphore::MAX_PERMITS);
        std::fs::write(&p, "max_concurrent = 1024\n").unwrap();
        assert_eq!(Config::load_from(&p).unwrap().max_concurrent, 1024);
    }

    #[test]
    fn unknown_key_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "bogus = 1\n").unwrap();
        assert!(Config::load_from(&p).is_err());
    }
}
