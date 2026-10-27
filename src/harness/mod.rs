//! Harness adapters: the agent CLI a backend runs ("harness") and how agent-sidekick drives
//! it.
//!
//! A backend (`[backends.<name>]`) names its harness with
//! `harness = "claude" | "opencode" | "codex" | "command"` (or, legacy, with
//! `type = "claude-oauth" | "claude-endpoint" | "command"`; opencode and codex have no
//! legacy form). Each harness has one adapter implementing [`Harness`]:
//!
//! - [`claude::ClaudeHarness`]: the official `claude` CLI, in OAuth mode (a Claude account's
//!   token, internal type `claude-oauth`) or endpoint mode (an Anthropic-compatible
//!   `base_url` with an API key, internal type `claude-endpoint`).
//! - [`opencode::OpencodeHarness`]: the OpenCode CLI (`opencode run --format json`) against
//!   an OpenAI-compatible endpoint, Ollama or OpenRouter, isolated per backend.
//! - [`codex::CodexHarness`]: the OpenAI Codex CLI (`codex exec --json`) with a ChatGPT login
//!   or an API key, in a per-backend `CODEX_HOME`.
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
pub mod codex;
pub mod command;
pub mod opencode;

#[cfg(test)]
mod equivalence_tests;

/// How long `<harness> --version` may take before it is killed.
pub const VERSION_TIMEOUT: Duration = Duration::from_secs(15);

/// Inherited variables kept for `<harness> --version` of harnesses that run it in a minimal
/// environment (opencode, codex); everything else is dropped.
pub(crate) const VERSION_ENV_KEEP: &[&str] = &[
    "PATH",
    "PATHEXT",
    "SYSTEMROOT",
    "WINDIR",
    "COMSPEC",
    "LANG",
    "LC_ALL",
];

/// The known harnesses (value of `harness = "..."` in config.toml).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HarnessKind {
    Claude,
    #[serde(rename = "opencode")]
    OpenCode,
    Codex,
    Command,
}

impl HarnessKind {
    /// Every known harness, in the order `harness list` prints them.
    pub const ALL: [HarnessKind; 4] = [
        HarnessKind::Claude,
        HarnessKind::OpenCode,
        HarnessKind::Codex,
        HarnessKind::Command,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            HarnessKind::Claude => "claude",
            HarnessKind::OpenCode => "opencode",
            HarnessKind::Codex => "codex",
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
            BackendType::OpenCode => HarnessKind::OpenCode,
            BackendType::Codex => HarnessKind::Codex,
        }
    }

    /// The adapter of this harness.
    pub fn adapter(self) -> &'static dyn Harness {
        match self {
            HarnessKind::Claude => &claude::ClaudeHarness,
            HarnessKind::OpenCode => &opencode::OpencodeHarness,
            HarnessKind::Codex => &codex::CodexHarness,
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
    /// The executable of a harness backend (opencode: `harness_path` or the lookup) or a
    /// user-facing error.
    pub locate: &'a dyn Fn(&Backend) -> Result<PathBuf, String>,
    /// The stored secret itself (for the authenticated provider probe; never shown).
    pub secret_value: &'a dyn Fn(&SecretId) -> Result<Token, TokenError>,
    /// `GET url` with an optional bearer token (never redirected); see
    /// [`crate::backends::reach::probe_get`].
    pub probe_get: &'a dyn Fn(&str, Option<&str>) -> crate::backends::reach::ProbeResponse,
}

/// The result text of a run from a harness with structured output (see
/// [`Harness::normalize_output`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Normalized {
    /// What goes into the `sidekick_run` result (before the final tail cut).
    pub text: String,
    /// Why a run that exited 0 still counts as failed (e.g. no answer at all); `None`: the
    /// exit code decides.
    pub failure: Option<String>,
    pub rate_limit: Option<RateLimit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimit {
    pub retry_after_s: Option<u64>,
}

impl RateLimit {
    pub fn line(&self) -> String {
        match self.retry_after_s {
            Some(n) => format!("rate_limited: yes (retry after {n}s)"),
            None => "rate_limited: yes".to_owned(),
        }
    }
}

pub fn detect_rate_limit(text: &str) -> Option<RateLimit> {
    let lower = text.to_ascii_lowercase();
    let has_429 = lower.match_indices("429").any(|(i, _)| {
        let b = lower.as_bytes();
        (i == 0 || !b[i - 1].is_ascii_digit()) && b.get(i + 3).is_none_or(|c| !c.is_ascii_digit())
    });
    let marker = ["rate limit", "rate_limit", "too many requests"]
        .iter()
        .any(|m| lower.contains(m));
    let matched = (has_429 && marker)
        || lower.contains("rate_limit_error")
        || lower.contains("rate limit reached");
    rate_limit_of(&lower, matched)
}

pub fn detect_rate_limit_strict(text: &str) -> Option<RateLimit> {
    let lower = text.to_ascii_lowercase();
    let key = "too many requests";
    let after_429 = lower.match_indices(key).any(|(i, _)| {
        let before = lower[..i].trim_end_matches([' ', ':', '-']);
        before.ends_with("429")
            && !before[..before.len() - 3]
                .bytes()
                .next_back()
                .is_some_and(|c| c.is_ascii_digit())
    });
    let matched =
        after_429 || lower.contains("rate_limit_error") || lower.contains("rate limit reached");
    rate_limit_of(&lower, matched)
}

fn rate_limit_of(lower: &str, matched: bool) -> Option<RateLimit> {
    if matched {
        return Some(RateLimit {
            retry_after_s: retry_after(lower),
        });
    }
    (lower.contains("request too large") && lower.contains("tokens per minute")).then_some(
        RateLimit {
            retry_after_s: None,
        },
    )
}

fn retry_after(lower: &str) -> Option<u64> {
    for key in ["retry-after", "retry after"] {
        for (i, _) in lower.match_indices(key) {
            let rest = lower[i + key.len()..].trim_start_matches([':', ' ', '"', '=', '\\']);
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(n) = digits.parse::<u64>() {
                return Some(n.max(1));
            }
        }
    }
    let key = "try again in ";
    lower
        .match_indices(key)
        .find_map(|(i, _)| parse_duration_s(&lower[i + key.len()..]))
}

fn parse_duration_s(s: &str) -> Option<u64> {
    const UNITS: [(&str, f64); 15] = [
        ("hours", 3600.0),
        ("hour", 3600.0),
        ("hrs", 3600.0),
        ("h", 3600.0),
        ("minutes", 60.0),
        ("minute", 60.0),
        ("mins", 60.0),
        ("min", 60.0),
        ("ms", 0.001),
        ("m", 60.0),
        ("seconds", 1.0),
        ("second", 1.0),
        ("secs", 1.0),
        ("sec", 1.0),
        ("s", 1.0),
    ];
    let mut rest = s;
    let mut total = 0.0f64;
    let mut any = false;
    loop {
        let len = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let Ok(n) = rest[..len].trim_end_matches('.').parse::<f64>() else {
            break;
        };
        let after = rest[len..].strip_prefix(' ').unwrap_or(&rest[len..]);
        let Some((unit, factor)) = UNITS.iter().find(|(u, _)| {
            after.starts_with(u)
                && after[u.len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_ascii_alphabetic())
        }) else {
            break;
        };
        total += n * factor;
        any = true;
        rest = &after[unit.len()..];
    }
    any.then(|| (total.ceil() as u64).max(1))
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

    /// Text of the run's output as it goes into the `sidekick_run` result. `raw` is the
    /// bounded output buffer of the runner (stdout and stderr merged, redacted, at most the
    /// last 64-128 KiB, so it may start mid-line). `None` (the default): no translation, the
    /// runner's tail is used unchanged (claude, command). Harnesses with a structured output
    /// format (opencode JSON lines) return the readable text and, where the exit code is not
    /// enough, a failure reason.
    fn normalize_output(&self, _raw: &str) -> Option<Normalized> {
        None
    }

    /// Checks right before a run, after the credential was loaded and the program found
    /// (e.g. the provider endpoint answers). A failure means the run is not started (a
    /// fallback may be used). Default: nothing to check.
    fn pre_run_check(&self, _backend: &Backend, _credential: &Credential) -> Result<(), String> {
        Ok(())
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

/// Create `dir` with parents (Unix: mode 0700, it holds sessions, logs and possibly a login).
#[cfg(unix)]
pub(crate) fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    Ok(())
}

/// Create `dir` with parents.
#[cfg(not(unix))]
pub(crate) fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Write `contents` to `path` unless it already holds exactly that (then nothing is
/// written, so concurrent runs of one backend never race on an identical file). Written
/// through a temp file in the same directory and renamed into place.
pub(crate) fn write_if_changed(path: &Path, contents: &str) -> std::io::Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|old| old == contents) {
        return Ok(());
    }
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_extension(format!(
        "{ext}.tmp-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&tmp, contents)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            // Another run may have written the same content meanwhile.
            if std::fs::read_to_string(path).is_ok_and(|old| old == contents) {
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

/// Characters an argument may contain when the program is a `.cmd`/`.bat` wrapper (cmd.exe
/// re-parses arguments): letters, digits and `-_./:@+`.
pub(crate) fn batch_safe(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:@+".contains(c))
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
        assert_eq!(HarnessKind::parse("opencode"), Some(HarnessKind::OpenCode));
        assert_eq!(HarnessKind::parse("codex"), Some(HarnessKind::Codex));
        assert_eq!(HarnessKind::parse("aider"), None);
        assert_eq!(
            HarnessKind::ALL.map(HarnessKind::as_str),
            ["claude", "opencode", "codex", "command"]
        );
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
        assert_eq!(
            HarnessKind::of_type(BackendType::OpenCode),
            HarnessKind::OpenCode
        );
        assert_eq!(HarnessKind::of_type(BackendType::Codex), HarnessKind::Codex);
    }

    #[test]
    fn install_hints() {
        let claude = HarnessKind::Claude.adapter().install_hint().unwrap();
        assert!(claude.contains("curl -fsSL https://claude.ai/install.sh | bash"));
        assert!(claude.contains("irm https://claude.ai/install.ps1 | iex"));
        assert!(HarnessKind::Command.adapter().install_hint().is_none());
        let opencode = HarnessKind::OpenCode.adapter().install_hint().unwrap();
        assert!(opencode.contains("npm install -g opencode-ai"));
        let codex = HarnessKind::Codex.adapter().install_hint().unwrap();
        assert!(codex.contains("npm install -g @openai/codex"));
    }

    #[test]
    fn normalize_output_is_identity_for_claude_and_command() {
        let raw = "line 1\n\u{1b}[31mred\u{1b}[0m\n{\"json\": true}\n{\"type\":\"result\",\"is_error\":true,\"subtype\":\"error_during_execution\"}\n";
        for h in [HarnessKind::Claude, HarnessKind::Command] {
            assert_eq!(h.adapter().normalize_output(raw), None);
        }
        // opencode and codex translate their JSON lines.
        assert!(HarnessKind::Codex.adapter().normalize_output(raw).is_some());
    }

    fn retry(text: &str) -> Option<Option<u64>> {
        detect_rate_limit(text).map(|r| r.retry_after_s)
    }

    #[test]
    fn detects_rate_limits() {
        let groq = "Rate limit reached for model `openai/gpt-oss-120b` in organization `org_x` \
                    service tier `on_demand` on tokens per minute (TPM): Limit 8000, Used 7000, \
                    Requested 4000. Please try again in 22.5s.";
        assert_eq!(retry(groq), Some(Some(23)));
        assert_eq!(retry("Error: 429 Too Many Requests"), Some(None));
        assert_eq!(
            retry(r#"API Error: {"type":"error","error":{"type":"rate_limit_error"}}"#),
            Some(None)
        );
        assert_eq!(
            retry("HTTP 429 Too Many Requests\nretry-after: 30"),
            Some(Some(30))
        );
        assert_eq!(
            retry("429 rate limit exceeded, please try again in 1m2.3s"),
            Some(Some(63))
        );
        assert_eq!(
            retry("status 429: rate_limit_exceeded. Try again in 450ms."),
            Some(Some(1))
        );
        assert_eq!(
            retry("429 too many requests, retry after 5 seconds"),
            Some(Some(5))
        );
        assert_eq!(
            detect_rate_limit("429 too many requests").unwrap().line(),
            "rate_limited: yes"
        );
        assert_eq!(
            detect_rate_limit("rate limit reached, try again in 7.66s")
                .unwrap()
                .line(),
            "rate_limited: yes (retry after 8s)"
        );
    }

    #[test]
    fn no_rate_limit_without_markers() {
        assert_eq!(detect_rate_limit("429 lines changed"), None);
        assert_eq!(
            detect_rate_limit("we should add a rate limit to the API"),
            None
        );
        assert_eq!(detect_rate_limit("14290 rate limit tokens"), None);
        assert_eq!(detect_rate_limit(""), None);
    }

    #[test]
    fn groq_request_too_large_is_a_rate_limit_without_retry_time() {
        let groq = "ContextOverflowError: Request too large for model `qwen/qwen3.8-27b` in                     organization `org_x` service tier `on_demand` on input tokens per minute                     (ITPM): Limit 7000, Requested 7210, please reduce your message size";
        assert_eq!(retry(groq), Some(None));
        assert_eq!(
            detect_rate_limit_strict(groq).map(|r| r.retry_after_s),
            Some(None)
        );
        let tpm = "Request too large for model `openai/gpt-oss-20b` on tokens per minute (TPM):                    Limit 8000, Requested 9100";
        assert_eq!(retry(tpm), Some(None));
        let other = "413 Request Too Large: body exceeds 10 MB";
        assert_eq!(retry(other), None);
        assert_eq!(detect_rate_limit_strict(other), None);
    }

    #[test]
    fn strict_detection_needs_unambiguous_markers() {
        let strict = |t: &str| detect_rate_limit_strict(t).map(|r| r.retry_after_s);
        assert_eq!(strict("Error: 429 Too Many Requests"), Some(None));
        assert_eq!(strict("HTTP 429: too many requests"), Some(None));
        assert_eq!(
            strict(r#"API Error: {"type":"error","error":{"type":"rate_limit_error"}}"#),
            Some(None)
        );
        assert_eq!(
            strict("Rate limit reached, try again in 2 seconds"),
            Some(Some(2))
        );
        assert_eq!(strict("test_rate_limit_429 failed"), None);
        assert_eq!(strict("429 rate limit exceeded"), None);
        assert_eq!(strict("too many requests"), None);
        assert_eq!(strict("1429 too many requests"), None);
        assert_eq!(strict("429 tests passed, too many requests mocked"), None);
        assert_eq!(strict(""), None);
    }

    #[test]
    fn parses_duration_formats() {
        assert_eq!(parse_duration_s("5 minutes"), Some(300));
        assert_eq!(parse_duration_s("2 seconds."), Some(2));
        assert_eq!(parse_duration_s("1 hour"), Some(3600));
        assert_eq!(parse_duration_s("3 min"), Some(180));
        assert_eq!(parse_duration_s("10 sec"), Some(10));
        assert_eq!(parse_duration_s("1m2.3s"), Some(63));
        assert_eq!(parse_duration_s("7.66s"), Some(8));
        assert_eq!(parse_duration_s("450ms."), Some(1));
        assert_eq!(parse_duration_s("5 more tries"), None);
        assert_eq!(parse_duration_s("a while"), None);
        assert_eq!(
            retry("Rate limit reached. Please try again in 5 minutes."),
            Some(Some(300))
        );
    }
}
