//! Reachability checks for `sidekick_backends`, `backends test`, `doctor` and the
//! pre-run check of claude-endpoint backends.
//! INTERFACE STUB: implemented by the `reach` task.

use std::path::Path;
use std::time::Duration;

use super::Backend;

/// Timeout of the HTTP probe of a claude-endpoint `base_url`.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reachability {
    pub reachable: bool,
    /// Human-readable reason; never contains secrets.
    pub detail: String,
}

/// HTTP GET `base_url` without credentials; any HTTP response (also 4xx/5xx) counts as
/// reachable, a connection error or timeout does not. Blocking.
pub fn probe_http(_base_url: &str, _timeout: Duration) -> Reachability {
    todo!("reach task")
}

/// claude-oauth: claude found and a token stored; claude-endpoint: claude found, key
/// available and `base_url` answers within [`PROBE_TIMEOUT`]; command: program found.
/// Blocking (keyring, filesystem, network).
pub fn check_backend(_backend: &Backend, _claude_path: Option<&Path>) -> Reachability {
    todo!("reach task")
}
