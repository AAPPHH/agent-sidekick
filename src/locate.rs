//! Finding the `claude` executable. MCP hosts (e.g. the VS Code extension) often start
//! servers with a reduced PATH, so well-known install locations are searched too.
//! INTERFACE STUB: implemented in step 3.

use std::fmt;
use std::path::{Path, PathBuf};

pub const ENV_CLAUDE: &str = "AGENT_SIDEKICK_CLAUDE";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocateSource {
    Env,
    Config,
    Path,
    WellKnown,
}

impl fmt::Display for LocateSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LocateSource::Env => "AGENT_SIDEKICK_CLAUDE",
            LocateSource::Config => "config.toml claude_path",
            LocateSource::Path => "PATH",
            LocateSource::WellKnown => "well-known install location",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    /// Absolute path to the executable (`claude`, `claude.exe` or `claude.cmd`).
    pub path: PathBuf,
    pub source: LocateSource,
}

#[derive(Debug, thiserror::Error)]
pub enum LocateError {
    #[error("{var} points to {path}, which does not exist or is not a file")]
    BadOverride { var: String, path: PathBuf },
    #[error("could not find the `claude` executable (searched: {searched}); install Claude Code or set AGENT_SIDEKICK_CLAUDE to its absolute path")]
    NotFound { searched: String },
}

/// Order: `AGENT_SIDEKICK_CLAUDE`, `config_path` (config.toml), PATH, well-known locations.
pub fn find_claude(_config_path: Option<&Path>) -> Result<Located, LocateError> {
    todo!("step 3")
}

/// True if the file must be started through `cmd.exe` (Windows `.cmd`/`.bat` shims).
pub fn is_batch_script(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"))
}
