//! `agent-sidekick update` and the daily update hint of `doctor`.
//!
//! Only checks: queries the GitHub Releases API for the latest release, compares its
//! version with the running binary and prints the platform's install one-liner. Nothing is
//! downloaded or installed here.
//!
//! - Owner/repo come from `repository` in Cargo.toml (`CARGO_PKG_REPOSITORY`, baked in at
//!   compile time).
//! - `AGENT_SIDEKICK_UPDATE_URL` replaces the API base `https://api.github.com` (test hook,
//!   or a GitHub Enterprise / mirror API base such as `https://ghe.example/api/v3`); the
//!   request goes to `<base>/repos/<owner>/<repo>/releases/latest`.
//! - Proxies: ureq honours `ALL_PROXY`/`HTTPS_PROXY`/`HTTP_PROXY` (and `NO_PROXY`);
//!   loopback hosts are always contacted directly.
//! - No credentials are sent (unauthenticated API: 60 requests per hour per IP).
//! - `doctor` checks at most once per 24 h and remembers the result in
//!   `<config-dir>/update-check.json`; it never creates the config directory for that.

use std::cmp::Ordering;
use std::fmt;
use std::net::IpAddr;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{paths, APP_NAME};

/// Replaces the API base URL (default [`DEFAULT_API_BASE`]).
pub const ENV_UPDATE_URL: &str = "AGENT_SIDEKICK_UPDATE_URL";
pub const DEFAULT_API_BASE: &str = "https://api.github.com";
/// Version of the running binary.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// `repository` from Cargo.toml.
pub const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

/// Timeout of the request made by `agent-sidekick update`.
pub const UPDATE_TIMEOUT: Duration = Duration::from_secs(10);
/// Timeout of the request made by `doctor` (kept short: it is only a hint).
pub const DOCTOR_TIMEOUT: Duration = Duration::from_secs(3);
/// File in the config directory that remembers the last check of `doctor`.
pub const CHECK_FILE: &str = "update-check.json";
/// `doctor` checks at most this often.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// After a failed check, `doctor` tries again after this long (e.g. back online).
pub const ERROR_RETRY_INTERVAL: Duration = Duration::from_secs(60 * 60);

// ---------------------------------------------------------------------------
// Versions
// ---------------------------------------------------------------------------

/// A semantic version (`MAJOR.MINOR.PATCH[-PRE][+BUILD]`), compared by SemVer 2.0
/// precedence; build metadata is ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Vec<PreId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreId {
    Num(u64),
    Alpha(String),
}

impl Version {
    /// Parses a version or a release tag: surrounding whitespace and one leading `v`/`V`
    /// are ignored. `None` for anything that is not a semantic version.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let s = s
            .strip_prefix('v')
            .or_else(|| s.strip_prefix('V'))
            .unwrap_or(s);
        let (s, build) = match s.split_once('+') {
            Some((v, b)) => (v, Some(b)),
            None => (s, None),
        };
        if let Some(b) = build {
            if !b.split('.').all(is_ident) {
                return None;
            }
        }
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (s, None),
        };
        let mut nums = core.split('.').map(parse_num);
        let (Some(Some(major)), Some(Some(minor)), Some(Some(patch)), None) =
            (nums.next(), nums.next(), nums.next(), nums.next())
        else {
            return None;
        };
        let pre = match pre {
            None => Vec::new(),
            Some(p) => p
                .split('.')
                .map(|id| {
                    if !is_ident(id) {
                        None
                    } else if id.bytes().all(|b| b.is_ascii_digit()) {
                        parse_num(id).map(PreId::Num)
                    } else {
                        Some(PreId::Alpha(id.to_owned()))
                    }
                })
                .collect::<Option<Vec<_>>>()?,
        };
        Some(Self {
            major,
            minor,
            patch,
            pre,
        })
    }
}

fn is_ident(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Digits only, no leading zero (except `0` itself).
fn parse_num(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) || (s.len() > 1 && s.starts_with('0'))
    {
        return None;
    }
    s.parse().ok()
}

impl Ord for PreId {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (PreId::Num(a), PreId::Num(b)) => a.cmp(b),
            (PreId::Num(_), PreId::Alpha(_)) => Ordering::Less,
            (PreId::Alpha(_), PreId::Num(_)) => Ordering::Greater,
            (PreId::Alpha(a), PreId::Alpha(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for PreId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                // A pre-release has lower precedence than the release.
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        for (i, id) in self.pre.iter().enumerate() {
            f.write_str(if i == 0 { "-" } else { "." })?;
            match id {
                PreId::Num(n) => write!(f, "{n}")?,
                PreId::Alpha(s) => f.write_str(s)?,
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Repository and install command
// ---------------------------------------------------------------------------

/// `(owner, repo)` of a `https://github.com/<owner>/<repo>` URL.
pub fn parse_github_repo(url: &str) -> Option<(&str, &str)> {
    let rest = url.trim().strip_prefix("https://github.com/")?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, repo) = rest.split_once('/')?;
    let ok = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    (ok(owner) && ok(repo)).then_some((owner, repo))
}

/// Owner and repo this binary was built from.
pub fn repo_slug() -> Result<(&'static str, &'static str), CheckError> {
    parse_github_repo(REPOSITORY).ok_or_else(|| {
        CheckError::Config(format!(
            "this build has no GitHub repository configured (Cargo.toml `repository` = {REPOSITORY:?})"
        ))
    })
}

/// The one-liner that installs the latest release (the install scripts replace the binary).
pub fn install_command(windows: bool, owner: &str, repo: &str) -> String {
    let base = format!("https://github.com/{owner}/{repo}/releases/latest/download");
    if windows {
        format!("irm {base}/install.ps1 | iex")
    } else {
        format!("curl -fsSL {base}/install.sh | sh")
    }
}

const RELOAD_NOTE: &str =
    "Afterwards reload VS Code (Command Palette: \"Developer: Reload Window\") \
or restart Claude Code, so the MCP server is started from the new binary.";

// ---------------------------------------------------------------------------
// The check
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The latest release is the installed version.
    UpToDate { latest: String },
    /// The installed version is newer than the latest release (dev or pre-release build).
    Ahead { latest: String },
    /// A newer release exists.
    Available { latest: String },
    /// The repository has no published release yet (HTTP 404).
    NoRelease,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckError {
    /// No HTTP response (offline, DNS, TLS, proxy, timeout).
    Network(String),
    /// HTTP 403/429: GitHub's rate limit for unauthenticated requests (or access denied).
    RateLimited(u16),
    /// Any other unexpected HTTP status.
    Http(u16),
    /// The answer was not the expected JSON or had no semantic-version `tag_name`.
    BadResponse(String),
    /// The build itself cannot be checked (no repository configured).
    Config(String),
}

impl fmt::Display for CheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckError::Network(why) => write!(f, "could not reach the release server: {why}"),
            CheckError::RateLimited(status) => write!(
                f,
                "the GitHub API refused the request (HTTP {status}), most likely its rate limit for unauthenticated requests"
            ),
            CheckError::Http(status) => write!(f, "unexpected answer from the release server (HTTP {status})"),
            CheckError::BadResponse(why) => write!(f, "unexpected answer from the release server: {why}"),
            CheckError::Config(why) => f.write_str(why),
        }
    }
}

impl CheckError {
    pub fn next_step(&self, owner_repo: Option<(&str, &str)>) -> String {
        let releases = match owner_repo {
            Some((o, r)) => format!("https://github.com/{o}/{r}/releases"),
            None => "the project's releases page".to_owned(),
        };
        match self {
            CheckError::Network(_) => format!(
                "check the internet connection (and HTTPS_PROXY if you need a proxy), then run `{APP_NAME} update` again"
            ),
            CheckError::RateLimited(_) => {
                format!("wait up to an hour and run `{APP_NAME} update` again, or look at {releases}")
            }
            CheckError::Http(_) | CheckError::BadResponse(_) => {
                format!("try again later or look at {releases}")
            }
            CheckError::Config(_) => format!("look at {releases}"),
        }
    }
}

/// API base: [`ENV_UPDATE_URL`] if set and non-empty, else [`DEFAULT_API_BASE`].
pub fn api_base() -> String {
    std::env::var(ENV_UPDATE_URL)
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_owned())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_API_BASE.to_owned())
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
}

/// Compare the latest release at `api_base` with `current`. Blocking.
pub fn check(api_base: &str, current: &str, timeout: Duration) -> Result<Outcome, CheckError> {
    let (owner, repo) = repo_slug()?;
    let current = Version::parse(current)
        .ok_or_else(|| CheckError::Config(format!("invalid own version {current:?}")))?;
    let url = format!(
        "{}/repos/{owner}/{repo}/releases/latest",
        api_base.trim_end_matches('/')
    );
    let Some(tag) = fetch_latest_tag(&url, timeout)? else {
        return Ok(Outcome::NoRelease);
    };
    let latest = Version::parse(&tag).ok_or_else(|| {
        CheckError::BadResponse(format!(
            "tag_name {:?} is not a version",
            tag.chars().take(40).collect::<String>()
        ))
    })?;
    let shown = latest.to_string();
    Ok(match latest.cmp(&current) {
        Ordering::Greater => Outcome::Available { latest: shown },
        Ordering::Equal => Outcome::UpToDate { latest: shown },
        Ordering::Less => Outcome::Ahead { latest: shown },
    })
}

/// `Ok(None)` on HTTP 404 (no release yet). Error texts never contain the URL (it may
/// carry credentials when overridden) or response bodies.
fn fetch_latest_tag(url: &str, timeout: Duration) -> Result<Option<String>, CheckError> {
    let mut config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .user_agent(format!("{APP_NAME}/{CURRENT_VERSION}"));
    if url_host(url).is_some_and(is_loopback_host) {
        config = config.proxy(None);
    }
    let agent: ureq::Agent = config.build().into();
    let mut resp = agent
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .map_err(|e| CheckError::Network(error_text(&e)))?;
    match resp.status().as_u16() {
        200 => {}
        404 => return Ok(None),
        s @ (403 | 429) => return Err(CheckError::RateLimited(s)),
        s => return Err(CheckError::Http(s)),
    }
    let body = resp
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_to_string()
        .map_err(|e| CheckError::Network(error_text(&e)))?;
    let release: Release = serde_json::from_str(&body)
        .map_err(|_| CheckError::BadResponse("no JSON object with a tag_name".to_owned()))?;
    Ok(Some(release.tag_name))
}

fn url_host(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if let Some(v6) = host_port.strip_prefix('[') {
        return v6.split_once(']').map(|(h, _)| h);
    }
    Some(host_port.split(':').next().unwrap_or(host_port))
}

fn is_loopback_host(host: &str) -> bool {
    let lower = host.to_ascii_lowercase();
    lower == "localhost"
        || lower.ends_with(".localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn error_text(e: &ureq::Error) -> String {
    use ureq::Error as E;
    match e {
        E::Timeout(_) => "timed out".to_owned(),
        E::HostNotFound => "host not found (offline?)".to_owned(),
        E::ConnectionFailed => "connection failed".to_owned(),
        E::Io(io) => format!("I/O error ({})", io.kind()),
        E::BadUri(_) | E::Http(_) => {
            format!("invalid URL (check {ENV_UPDATE_URL} if it is set)")
        }
        E::Protocol(_) => "no valid HTTP response".to_owned(),
        E::Tls(msg) => format!("TLS error: {msg}"),
        E::Rustls(err) => format!("TLS error: {err}"),
        E::InvalidProxyUrl => "invalid proxy URL in the environment".to_owned(),
        E::ConnectProxyFailed(_) => "proxy connection failed".to_owned(),
        E::BodyExceedsLimit(_) => "response too large".to_owned(),
        _ => "request failed".to_owned(),
    }
}

// ---------------------------------------------------------------------------
// `agent-sidekick update`
// ---------------------------------------------------------------------------

/// Text printed for a successful check.
pub fn format_outcome(outcome: &Outcome, current: &str, windows: bool) -> String {
    match outcome {
        Outcome::UpToDate { .. } => format!("{APP_NAME} {current} is up to date.\n"),
        Outcome::Ahead { latest } => format!(
            "{APP_NAME} {current} is up to date (newer than the latest release {latest}).\n"
        ),
        Outcome::NoRelease => format!(
            "No release of {APP_NAME} has been published yet (installed: {current}); nothing to update.\n"
        ),
        Outcome::Available { latest } => {
            let cmd = match repo_slug() {
                Ok((o, r)) => install_command(windows, o, r),
                Err(e) => return format!("{APP_NAME} {latest} is available ({e}).\n"),
            };
            format!(
                "{APP_NAME} {latest} is available (installed: {current}). To update, run:\n\n    {cmd}\n\n{RELOAD_NOTE}\n"
            )
        }
    }
}

/// Exit code 0 for every answer (also "no release yet"), 1 when the check failed.
pub fn run() -> anyhow::Result<ExitCode> {
    match check(&api_base(), CURRENT_VERSION, UPDATE_TIMEOUT) {
        Ok(outcome) => {
            print!(
                "{}",
                format_outcome(&outcome, CURRENT_VERSION, cfg!(windows))
            );
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            eprintln!(
                "error: update check failed: {}\n  next step: {}",
                crate::redact::redact_all(&e.to_string()),
                e.next_step(repo_slug().ok())
            );
            Ok(ExitCode::FAILURE)
        }
    }
}

// ---------------------------------------------------------------------------
// Daily hint for `doctor`
// ---------------------------------------------------------------------------

/// Content of [`CHECK_FILE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CheckRecord {
    /// Unix seconds of the check.
    checked_at: u64,
    /// Installed version at the time of the check (a different one invalidates it).
    current: String,
    /// `up_to_date`, `ahead`, `available`, `no_release` or `error`.
    result: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    latest: Option<String>,
}

impl CheckRecord {
    fn new(now: u64, current: &str, res: &Result<Outcome, CheckError>) -> Self {
        let (result, latest) = match res {
            Ok(Outcome::UpToDate { latest }) => ("up_to_date", Some(latest.clone())),
            Ok(Outcome::Ahead { latest }) => ("ahead", Some(latest.clone())),
            Ok(Outcome::Available { latest }) => ("available", Some(latest.clone())),
            Ok(Outcome::NoRelease) => ("no_release", None),
            Err(_) => ("error", None),
        };
        Self {
            checked_at: now,
            current: current.to_owned(),
            result: result.to_owned(),
            latest,
        }
    }

    /// The remembered outcome, if still valid at `now`; `None` means "check again".
    fn fresh_outcome(&self, now: u64, current: &str) -> Option<Outcome> {
        if self.current != current || now < self.checked_at {
            return None;
        }
        let age = now - self.checked_at;
        let latest = || {
            self.latest
                .as_deref()
                .and_then(Version::parse)
                .map(|v| v.to_string())
        };
        let outcome = match self.result.as_str() {
            // Nothing to show for a failed check; see `recent_error`.
            "up_to_date" => Outcome::UpToDate { latest: latest()? },
            "ahead" => Outcome::Ahead { latest: latest()? },
            "available" => Outcome::Available { latest: latest()? },
            "no_release" => Outcome::NoRelease,
            _ => return None,
        };
        (age < CHECK_INTERVAL.as_secs()).then_some(outcome)
    }

    fn recent_error(&self, now: u64, current: &str) -> bool {
        self.result == "error"
            && self.current == current
            && now >= self.checked_at
            && now - self.checked_at < ERROR_RETRY_INTERVAL.as_secs()
    }
}

/// Lines `doctor` prints after its report (empty or ending with a newline).
pub fn doctor_hint(offline: bool) -> String {
    if offline {
        return "[INFO] update check: skipped (--offline)\n".to_owned();
    }
    let Ok(dir) = paths::config_dir() else {
        return String::new();
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    doctor_hint_at(&dir, now, CURRENT_VERSION, cfg!(windows), &|| {
        check(&api_base(), CURRENT_VERSION, DOCTOR_TIMEOUT)
    })
}

/// Testable core of [`doctor_hint`]: `dir` is the config directory, `now` Unix seconds,
/// `fetch` the network check. Never creates `dir`; read/write problems of the record only
/// mean that the next run checks again.
pub fn doctor_hint_at(
    dir: &Path,
    now: u64,
    current: &str,
    windows: bool,
    fetch: &dyn Fn() -> Result<Outcome, CheckError>,
) -> String {
    if !dir.is_dir() {
        return format!(
            "[INFO] update check: skipped until `{APP_NAME} setup` has created the config directory; run `{APP_NAME} update` to check now\n"
        );
    }
    let file = dir.join(CHECK_FILE);
    let record = std::fs::read(&file)
        .ok()
        .and_then(|b| serde_json::from_slice::<CheckRecord>(&b).ok());
    if let Some(rec) = &record {
        if let Some(outcome) = rec.fresh_outcome(now, current) {
            return hint_text(&Ok(outcome), current, windows);
        }
        if rec.recent_error(now, current) {
            return String::new();
        }
    }
    let res = fetch();
    let rec = CheckRecord::new(now, current, &res);
    if let Ok(json) = serde_json::to_vec_pretty(&rec) {
        let _ = std::fs::write(&file, json);
    }
    hint_text(&res, current, windows)
}

fn hint_text(res: &Result<Outcome, CheckError>, current: &str, windows: bool) -> String {
    match res {
        Ok(Outcome::Available { latest }) => {
            let cmd = match repo_slug() {
                Ok((o, r)) => install_command(windows, o, r),
                Err(_) => format!("{APP_NAME} update"),
            };
            format!(
                "[INFO] update: {APP_NAME} {latest} is available (installed: {current})\n       \
                 next step: run `{cmd}`, then reload VS Code\n"
            )
        }
        Ok(Outcome::UpToDate { .. }) | Ok(Outcome::Ahead { .. }) => {
            format!("[INFO] update: {APP_NAME} {current} is up to date\n")
        }
        Ok(Outcome::NoRelease) => "[INFO] update: no release published yet\n".to_owned(),
        Err(e) => format!(
            "[INFO] update check failed ({}); run `{APP_NAME} update` later\n",
            crate::redact::redact_all(&e.to_string())
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap_or_else(|| panic!("{s:?} should parse"))
    }

    // ----- versions -----

    #[test]
    fn parses_tags_with_v_prefix_and_whitespace() {
        assert_eq!(v("v1.2.3"), v("1.2.3"));
        assert_eq!(v("V1.2.3"), v("1.2.3"));
        assert_eq!(v(" 1.2.3\n"), v("1.2.3"));
        assert_eq!(v("1.2.3+build.5"), v("1.2.3"));
        assert_eq!(v("v0.4.0-rc.1").to_string(), "0.4.0-rc.1");
        assert_eq!(v("10.20.30").to_string(), "10.20.30");
    }

    #[test]
    fn rejects_garbage() {
        for s in [
            "",
            "v",
            "latest",
            "1",
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.2.x",
            "1.2.3-",
            "1.2.3-a..b",
            "1.2.3-01",
            "1.2.3+",
            "vv1.2.3",
            "1.2.3-ü",
            "-1.2.3",
            "1.2.3 beta",
            "99999999999999999999.0.0",
        ] {
            assert!(Version::parse(s).is_none(), "{s:?} should be rejected");
        }
    }

    #[test]
    fn compares_by_semver_precedence() {
        let ordered = [
            "0.2.9",
            "0.3.0-alpha",
            "0.3.0-alpha.1",
            "0.3.0-alpha.beta",
            "0.3.0-beta",
            "0.3.0-beta.2",
            "0.3.0-beta.11",
            "0.3.0-rc.1",
            "0.3.0",
            "0.3.1",
            "0.10.0",
            "1.0.0",
        ];
        for pair in ordered.windows(2) {
            assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
        }
        assert_eq!(v("v1.0.0+a").cmp(&v("1.0.0+b")), Ordering::Equal);
    }

    #[test]
    fn own_version_parses() {
        assert!(Version::parse(CURRENT_VERSION).is_some());
    }

    // ----- repository / install command -----

    #[test]
    fn repository_is_a_github_repo() {
        let (owner, repo) = repo_slug().expect("Cargo.toml repository");
        assert!(!owner.is_empty() && !repo.is_empty());
    }

    #[test]
    fn parses_github_urls() {
        assert_eq!(
            parse_github_repo("https://github.com/AAPPHH/agent-sidekick"),
            Some(("AAPPHH", "agent-sidekick"))
        );
        assert_eq!(
            parse_github_repo("https://github.com/o/r.git/"),
            Some(("o", "r"))
        );
        for bad in [
            "",
            "https://gitlab.com/o/r",
            "https://github.com/o",
            "https://github.com/o/r/tree/main",
            "https://github.com//r",
        ] {
            assert_eq!(parse_github_repo(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn install_commands_per_platform() {
        assert_eq!(
            install_command(true, "o", "r"),
            "irm https://github.com/o/r/releases/latest/download/install.ps1 | iex"
        );
        assert_eq!(
            install_command(false, "o", "r"),
            "curl -fsSL https://github.com/o/r/releases/latest/download/install.sh | sh"
        );
    }

    #[test]
    fn available_output_has_command_and_reload_note() {
        let out = format_outcome(
            &Outcome::Available {
                latest: "9.0.0".into(),
            },
            "0.3.0",
            false,
        );
        assert!(
            out.contains("9.0.0 is available (installed: 0.3.0)"),
            "{out}"
        );
        assert!(
            out.contains("/releases/latest/download/install.sh | sh"),
            "{out}"
        );
        assert!(out.contains("reload VS Code"), "{out}");
        let out = format_outcome(
            &Outcome::Available {
                latest: "9.0.0".into(),
            },
            "0.3.0",
            true,
        );
        assert!(out.contains("install.ps1 | iex"), "{out}");
    }

    #[test]
    fn url_hosts() {
        assert_eq!(url_host("http://127.0.0.1:8080/x"), Some("127.0.0.1"));
        assert_eq!(url_host("https://u:p@localhost/x"), Some("localhost"));
        assert_eq!(url_host("http://[::1]:9/"), Some("::1"));
        assert_eq!(url_host("https://api.github.com"), Some("api.github.com"));
        assert!(is_loopback_host("::1") && is_loopback_host("a.localhost"));
        assert!(!is_loopback_host("api.github.com"));
    }

    // ----- HTTP against a local server -----

    /// Serves one canned response per connection; the request heads go to the receiver.
    fn serve(status: &'static str, body: &'static str) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        (base, rx)
    }

    fn check_against(status: &'static str, body: &'static str) -> Result<Outcome, CheckError> {
        let (base, _rx) = serve(status, body);
        check(&base, "0.3.0", Duration::from_secs(5))
    }

    #[test]
    fn newer_release_is_available() {
        assert_eq!(
            check_against("200 OK", r#"{"tag_name":"v0.4.0","name":"x"}"#),
            Ok(Outcome::Available {
                latest: "0.4.0".into()
            })
        );
    }

    #[test]
    fn equal_release_is_up_to_date() {
        assert_eq!(
            check_against("200 OK", r#"{"tag_name":"v0.3.0"}"#),
            Ok(Outcome::UpToDate {
                latest: "0.3.0".into()
            })
        );
    }

    #[test]
    fn older_release_means_ahead() {
        assert_eq!(
            check_against("200 OK", r#"{"tag_name":"0.2.1"}"#),
            Ok(Outcome::Ahead {
                latest: "0.2.1".into()
            })
        );
        // A release candidate of the installed version is older than the release.
        assert_eq!(
            check_against("200 OK", r#"{"tag_name":"v0.3.0-rc.1"}"#),
            Ok(Outcome::Ahead {
                latest: "0.3.0-rc.1".into()
            })
        );
    }

    #[test]
    fn not_found_means_no_release() {
        assert_eq!(
            check_against("404 Not Found", r#"{"message":"Not Found"}"#),
            Ok(Outcome::NoRelease)
        );
    }

    #[test]
    fn forbidden_is_rate_limit() {
        assert_eq!(
            check_against("403 Forbidden", r#"{"message":"API rate limit exceeded"}"#),
            Err(CheckError::RateLimited(403))
        );
        assert_eq!(
            check_against("429 Too Many Requests", "{}"),
            Err(CheckError::RateLimited(429))
        );
        assert_eq!(
            check_against("500 Internal Server Error", "{}"),
            Err(CheckError::Http(500))
        );
    }

    #[test]
    fn garbage_answers_are_bad_responses() {
        for body in [
            "<html>",
            "{}",
            r#"{"tag_name":42}"#,
            r#"{"tag_name":"latest"}"#,
        ] {
            let (base, _rx) = serve("200 OK", body);
            let res = check(&base, "0.3.0", Duration::from_secs(5));
            assert!(
                matches!(res, Err(CheckError::BadResponse(_))),
                "{body}: {res:?}"
            );
        }
    }

    #[test]
    fn request_path_and_headers() {
        let (base, rx) = serve("200 OK", r#"{"tag_name":"v0.3.0"}"#);
        check(&format!("{base}/"), "0.3.0", Duration::from_secs(5)).unwrap();
        let head = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (owner, repo) = repo_slug().unwrap();
        assert!(
            head.starts_with(&format!(
                "GET /repos/{owner}/{repo}/releases/latest HTTP/1.1"
            )),
            "{head}"
        );
        let lower = head.to_ascii_lowercase();
        assert!(lower.contains("user-agent: agent-sidekick/"), "{head}");
        assert!(
            lower.contains("accept: application/vnd.github+json"),
            "{head}"
        );
        assert!(!lower.contains("authorization"), "{head}");
    }

    #[test]
    fn unreachable_server_is_a_network_error() {
        // Bind and drop: nothing listens on the port any more.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let res = check(
            &format!("http://127.0.0.1:{port}"),
            "0.3.0",
            Duration::from_secs(5),
        );
        assert!(matches!(res, Err(CheckError::Network(_))), "{res:?}");
    }

    // ----- once per day -----

    const DAY: u64 = 24 * 60 * 60;
    const T0: u64 = 1_800_000_000;

    struct Counting {
        calls: Cell<u32>,
        answer: Result<Outcome, CheckError>,
    }

    impl Counting {
        fn new(answer: Result<Outcome, CheckError>) -> Self {
            Self {
                calls: Cell::new(0),
                answer,
            }
        }
        fn hint(&self, dir: &Path, now: u64, current: &str) -> String {
            doctor_hint_at(dir, now, current, false, &|| {
                self.calls.set(self.calls.get() + 1);
                self.answer.clone()
            })
        }
    }

    fn available() -> Result<Outcome, CheckError> {
        Ok(Outcome::Available {
            latest: "0.4.0".into(),
        })
    }

    #[test]
    fn checks_at_most_once_per_day() {
        let dir = tempfile::tempdir().unwrap();
        let f = Counting::new(available());
        let first = f.hint(dir.path(), T0, "0.3.0");
        assert!(first.contains("0.4.0 is available"), "{first}");
        assert!(first.contains("install.sh | sh"), "{first}");
        assert_eq!(f.calls.get(), 1);
        assert!(dir.path().join(CHECK_FILE).is_file());

        // Within 24 h: the remembered result, no network.
        let again = f.hint(dir.path(), T0 + DAY - 1, "0.3.0");
        assert_eq!(again, first);
        assert_eq!(f.calls.get(), 1);

        // After 24 h: checks again.
        f.hint(dir.path(), T0 + DAY, "0.3.0");
        assert_eq!(f.calls.get(), 2);
    }

    #[test]
    fn rechecks_after_upgrade_clock_skew_or_corrupt_record() {
        let dir = tempfile::tempdir().unwrap();
        let f = Counting::new(Ok(Outcome::UpToDate {
            latest: "0.3.0".into(),
        }));
        let out = f.hint(dir.path(), T0, "0.3.0");
        assert!(out.contains("0.3.0 is up to date"), "{out}");
        assert_eq!(f.calls.get(), 1);
        // Another installed version: the record does not apply.
        f.hint(dir.path(), T0 + 10, "0.4.0");
        assert_eq!(f.calls.get(), 2);
        // Clock went backwards.
        f.hint(dir.path(), T0 - 10, "0.4.0");
        assert_eq!(f.calls.get(), 3);
        // Garbage in the file.
        std::fs::write(dir.path().join(CHECK_FILE), b"not json").unwrap();
        f.hint(dir.path(), T0 - 5, "0.4.0");
        assert_eq!(f.calls.get(), 4);
    }

    #[test]
    fn failures_are_quiet_and_retried_after_an_hour() {
        let dir = tempfile::tempdir().unwrap();
        let f = Counting::new(Err(CheckError::Network("host not found".into())));
        let out = f.hint(dir.path(), T0, "0.3.0");
        assert!(out.starts_with("[INFO] update check failed"), "{out}");
        assert_eq!(out.lines().count(), 1, "{out}");
        assert_eq!(f.calls.get(), 1);
        // Within the hour: nothing, no network.
        assert_eq!(f.hint(dir.path(), T0 + 60, "0.3.0"), "");
        assert_eq!(f.calls.get(), 1);
        f.hint(dir.path(), T0 + 3600, "0.3.0");
        assert_eq!(f.calls.get(), 2);
    }

    #[test]
    fn does_not_create_a_missing_config_dir() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("cfg");
        let f = Counting::new(available());
        let out = f.hint(&cfg, T0, "0.3.0");
        assert!(out.starts_with("[INFO] update check: skipped"), "{out}");
        assert_eq!(f.calls.get(), 0);
        assert!(!cfg.exists());
    }

    #[test]
    fn offline_skips_the_check() {
        assert_eq!(
            doctor_hint(true),
            "[INFO] update check: skipped (--offline)\n"
        );
    }
}
