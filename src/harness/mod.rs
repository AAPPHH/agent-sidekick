//! Harness adapters: the agent CLI a backend runs ("harness") and how agent-sidekick drives
//! it.
//!
//! A backend (`[backends.<name>]`) names its harness with `harness = "claude" | "command"`
//! (or, legacy, with `type = "claude-oauth" | "claude-endpoint" | "command"`). Each harness
//! has one adapter implementing [`Harness`]:
//!
//! - [`claude::ClaudeHarness`]: the official `claude` CLI, in OAuth mode (a Claude account's
//!   token, internal type `claude-oauth`) or endpoint mode (an Anthropic-compatible
//!   `base_url` with an API key, internal type `claude-endpoint`).
//! - [`command::CommandHarness`]: any other agent CLI, configured as an argument array with
//!   placeholders.
//!
//! What is common to every harness stays in [`crate::backends`]: config validation
//! ([`crate::backends::resolve`]), session isolation of the child environment
//! ([`crate::backends::launch::child_env`]), the check that a credential matches its backend
//! (in [`crate::backends::build_launch`], so an OAuth token can never reach another adapter)
//! and the HTTP probe ([`crate::backends::probe_http`]). The public entry points
//! [`crate::backends::build_launch`], [`crate::backends::load_credential`],
//! [`crate::backends::resolve_program`] and [`crate::backends::check_backend`] dispatch to the
//! adapter of the backend's harness.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::backends::{Backend, BackendType, Credential, Reachability};
use crate::runner::{Launch, RunRequest};
use crate::token::{SecretId, Token, TokenError, TokenSource};

pub mod claude;
pub mod command;

#[cfg(test)]
mod equivalence_tests;

/// How long `<harness> --version` may take before it is killed.
pub const VERSION_TIMEOUT: Duration = Duration::from_secs(15);

/// The known harnesses (value of `harness = "..."` in config.toml).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HarnessKind {
    Claude,
    Command,
}

impl HarnessKind {
    /// Every known harness, in the order `harness list` prints them.
    pub const ALL: [HarnessKind; 2] = [HarnessKind::Claude, HarnessKind::Command];

    pub fn as_str(self) -> &'static str {
        match self {
            HarnessKind::Claude => "claude",
            HarnessKind::Command => "command",
        }
    }

    /// Parse a harness name as written in config.toml / on the command line.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|h| h.as_str() == name)
    }

    /// The harness of an internal backend type (also the mapping of the legacy `type`).
    pub fn of_type(ty: BackendType) -> Self {
        match ty {
            BackendType::ClaudeOauth | BackendType::ClaudeEndpoint => HarnessKind::Claude,
            BackendType::Command => HarnessKind::Command,
        }
    }

    /// The adapter of this harness.
    pub fn adapter(self) -> &'static dyn Harness {
        match self {
            HarnessKind::Claude => &claude::ClaudeHarness,
            HarnessKind::Command => &command::CommandHarness,
        }
    }
}

impl fmt::Display for HarnessKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Injected secret store access (keyring/file), see [`crate::token::load_secret`].
pub type SecretLoader<'a> = &'a dyn Fn(&SecretId) -> Result<(Token, TokenSource), TokenError>;

/// Everything an adapter needs to build the [`Launch`] of one run. The dispatcher
/// ([`crate::backends::build_launch`]) has already checked that `credential` matches the
/// backend and resolved the effective `model`.
pub struct LaunchInput<'a> {
    pub backend: &'a Backend,
    pub req: &'a RunRequest,
    pub credential: &'a Credential,
    /// The harness executable (from [`Harness::locate`]).
    pub program: &'a Path,
    /// The inherited environment (session isolation is applied by the adapter through
    /// [`crate::backends::launch::child_env`]).
    pub base: Vec<(OsString, OsString)>,
    pub global_pass_env: &'a [String],
    /// The effective model of the run (`None`: no model is passed).
    pub model: Option<String>,
    /// Log header lines so far (the `backend: ...` line); the adapter appends its own.
    pub header: Vec<String>,
}

/// Injected environment access for [`Harness::check`] (testable without keyring, PATH or
/// network).
pub struct CheckDeps<'a> {
    /// Path of the claude executable or a user-facing error.
    pub find_claude: &'a dyn Fn() -> Result<PathBuf, String>,
    /// The secret's source description (never the value) or the load error.
    pub secret: &'a dyn Fn(&SecretId) -> Result<String, TokenError>,
    /// Environment access (for `api_key_env`).
    pub env_lookup: &'a dyn Fn(&str) -> Option<String>,
    /// `command[0]` -> executable path or a user-facing error.
    pub resolve_program: &'a dyn Fn(&str) -> Result<PathBuf, String>,
    /// The HTTP probe of a `base_url`.
    pub probe: &'a dyn Fn(&str) -> Reachability,
}

/// One harness adapter. Implementations are stateless unit structs; get one with
/// [`HarnessKind::adapter`] or [`Backend::harness`].
pub trait Harness: Sync {
    fn kind(&self) -> HarnessKind;

    /// Name as used in config.toml (`harness = "<name>"`).
    fn name(&self) -> &'static str {
        self.kind().as_str()
    }

    /// The executable that runs `backend`.
    fn locate(&self, backend: &Backend, claude_path: Option<&Path>) -> Result<PathBuf, String>;

    /// Whether the harness is installed, independent of a backend (for `harness list`).
    /// `None`: not applicable (the program is configured per backend).
    fn locate_installed(&self, claude_path: Option<&Path>) -> Option<Result<PathBuf, String>>;

    /// `<program> --version` (first line, trimmed), killed after [`VERSION_TIMEOUT`].
    fn version(&self, program: &Path) -> Result<String, String> {
        let out = crate::commands::claude_cli::run(program, ["--version"], VERSION_TIMEOUT)
            .map_err(|e| format!("{e:#}"))?;
        if !out.success() {
            return Err(format!("`--version` failed: {}", out.diagnostics()));
        }
        let text = crate::redact::redact_all(out.stdout.trim());
        Ok(text.lines().next().unwrap_or_default().trim().to_owned())
    }

    /// The credential `backend` needs for one run, with a description of its source
    /// (never the value). The error is a user-facing message with a next step.
    fn load_credential(
        &self,
        backend: &Backend,
        env_lookup: &dyn Fn(&str) -> Option<String>,
        secret_loader: SecretLoader<'_>,
    ) -> Result<(Credential, String), String>;

    /// The [`Launch`] (program, argv, environment, stdin, secrets, log header) of one run.
    fn build_launch(&self, input: LaunchInput<'_>) -> Result<Launch, String>;

    /// Text of the run's output (stdout and stderr, merged and redacted by the runner) as it
    /// goes into the `sidekick_run` result. Identity unless a harness emits a structured
    /// format that needs translating.
    fn normalize_output(&self, raw: &str) -> String {
        raw.to_owned()
    }

    /// Reachability of `backend`: harness installed, credential available and, where the
    /// harness talks to a configured endpoint, the endpoint answers. Checks run in order and
    /// the first failure wins.
    fn check(&self, backend: &Backend, deps: &CheckDeps<'_>) -> Reachability;

    /// The official install command(s) of the harness, for printing only (agent-sidekick
    /// never runs them). `None`: nothing to install (generic command harness).
    fn install_hint(&self) -> Option<&'static str>;
}

impl Backend {
    /// The harness this backend runs.
    pub fn harness_kind(&self) -> HarnessKind {
        HarnessKind::of_type(self.backend_type())
    }

    /// The adapter of this backend's harness.
    pub fn harness(&self) -> &'static dyn Harness {
        self.harness_kind().adapter()
    }
}

/// Internal error for an adapter called with a backend of another harness (the dispatchers
/// never do that).
pub(crate) fn wrong_harness(adapter: &dyn Harness, backend: &Backend) -> String {
    format!(
        "backend {}: internal error: harness {} called for a {} backend",
        backend.name,
        adapter.name(),
        backend.backend_type()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for h in HarnessKind::ALL {
            assert_eq!(HarnessKind::parse(h.as_str()), Some(h));
            assert_eq!(h.adapter().kind(), h);
            assert_eq!(h.adapter().name(), h.as_str());
        }
        assert_eq!(HarnessKind::parse("opencode"), None);
        assert_eq!(HarnessKind::parse("Claude"), None);
    }

    #[test]
    fn legacy_types_map_onto_harnesses() {
        assert_eq!(
            HarnessKind::of_type(BackendType::ClaudeOauth),
            HarnessKind::Claude
        );
        assert_eq!(
            HarnessKind::of_type(BackendType::ClaudeEndpoint),
            HarnessKind::Claude
        );
        assert_eq!(
            HarnessKind::of_type(BackendType::Command),
            HarnessKind::Command
        );
    }

    #[test]
    fn install_hints() {
        let claude = HarnessKind::Claude.adapter().install_hint().unwrap();
        assert!(claude.contains("curl -fsSL https://claude.ai/install.sh | bash"));
        assert!(claude.contains("irm https://claude.ai/install.ps1 | iex"));
        assert!(HarnessKind::Command.adapter().install_hint().is_none());
    }

    #[test]
    fn normalize_output_is_identity_for_claude_and_command() {
        let raw = "line 1\n\u{1b}[31mred\u{1b}[0m\n{\"json\": true}\n";
        for h in HarnessKind::ALL {
            assert_eq!(h.adapter().normalize_output(raw), raw);
        }
    }
}
