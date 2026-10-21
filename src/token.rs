//! OAuth token storage: OS keyring first, `<config-dir>/token` (0600) as fallback.
//! INTERFACE STUB: implemented in step 2.

use std::fmt;
use std::path::PathBuf;

/// Keyring service and user.
pub const KEYRING_SERVICE: &str = crate::APP_NAME;
pub const KEYRING_USER: &str = "oauth-token";
/// Highest-priority token source, meant for CI and tests.
pub const ENV_TOKEN: &str = "AGENT_SIDEKICK_TOKEN";
/// When set to `1`, the keyring is never touched (file fallback only).
pub const ENV_DISABLE_KEYRING: &str = "AGENT_SIDEKICK_DISABLE_KEYRING";
pub const EXPECTED_PREFIX: &str = "sk-ant-oat01-";

/// A secret token. `Debug`/`Display` never show the value.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into().trim().to_owned())
    }
    /// The only way to read the secret.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token([REDACTED])")
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSource {
    Env,
    Keyring,
    File(PathBuf),
}

impl fmt::Display for TokenSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenSource::Env => write!(f, "environment ({ENV_TOKEN})"),
            TokenSource::Keyring => write!(f, "OS keyring (service {KEYRING_SERVICE})"),
            TokenSource::File(p) => write!(f, "file {}", p.display()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("no token stored; run `agent-sidekick setup`")]
    Missing,
    #[error("token is empty")]
    Empty,
    #[error("token storage error: {0}")]
    Storage(String),
}

/// Result of a format check: tokens with an unexpected prefix are accepted with a warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatCheck {
    Ok,
    Warn(String),
}

pub fn check_format(_token: &str) -> FormatCheck {
    todo!("step 2")
}

/// Load the token: env, then keyring, then file.
pub fn load() -> Result<(Token, TokenSource), TokenError> {
    todo!("step 2")
}

/// Store the token in the keyring, falling back to the file.
pub fn store(_token: &Token) -> Result<TokenSource, TokenError> {
    todo!("step 2")
}

/// Delete from keyring and file. Returns human-readable descriptions of what was removed.
pub fn delete() -> Result<Vec<String>, TokenError> {
    todo!("step 2")
}
