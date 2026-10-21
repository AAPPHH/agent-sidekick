//! OAuth token storage: OS keyring first, `<config-dir>/token` (0600) as fallback.
//!
//! Lookup order for [`load`]: `AGENT_SIDEKICK_TOKEN`, OS keyring, token file.
//! Nothing in here ever logs or formats the secret itself.

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::redact::redact_all;

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

pub fn check_format(token: &str) -> FormatCheck {
    if token.trim().starts_with(EXPECTED_PREFIX) {
        FormatCheck::Ok
    } else {
        FormatCheck::Warn(format!(
            "token does not start with `{EXPECTED_PREFIX}`; it may not be a Claude Code \
             OAuth token (create one with `claude setup-token`)"
        ))
    }
}

/// Which stored secret: the OAuth token of a claude-oauth backend or the API key of a
/// claude-endpoint backend. A claude-oauth backend named `sidekick` uses the v0.1
/// locations (keyring user `oauth-token`, file `token`, env `AGENT_SIDEKICK_TOKEN`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SecretId {
    /// OAuth token of the claude-oauth backend with this name.
    Oauth(String),
    /// API key of the claude-endpoint backend with this name.
    Endpoint(String),
}

impl SecretId {
    /// The v0.1 secret (claude-oauth backend `sidekick`).
    pub fn legacy() -> Self {
        SecretId::Oauth(crate::backends::IMPLICIT_BACKEND.to_owned())
    }

    /// Keyring user name: `oauth-token`, `oauth-token-<name>` or `endpoint-<name>`.
    pub fn keyring_user(&self) -> String {
        todo!("secrets task")
    }

    /// File name in the config dir: `token`, `token-<name>` or `endpoint-<name>.key`.
    pub fn file_name(&self) -> String {
        todo!("secrets task")
    }

    /// Environment override (highest priority): only `AGENT_SIDEKICK_TOKEN` for the legacy
    /// secret; `None` otherwise.
    pub fn env_override(&self) -> Option<&'static str> {
        todo!("secrets task")
    }
}

impl fmt::Display for SecretId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretId::Oauth(n) => write!(f, "OAuth token of backend `{n}`"),
            SecretId::Endpoint(n) => write!(f, "API key of backend `{n}`"),
        }
    }
}

/// [`load`] for any secret (env override, keyring, file).
pub fn load_secret(_id: &SecretId) -> Result<(Token, TokenSource), TokenError> {
    todo!("secrets task")
}

/// [`store`] for any secret.
pub fn store_secret(_id: &SecretId, _token: &Token) -> Result<TokenSource, TokenError> {
    todo!("secrets task")
}

/// [`delete`] for any secret.
pub fn delete_secret(_id: &SecretId) -> Result<Vec<String>, TokenError> {
    todo!("secrets task")
}

/// Load the token: env, then keyring, then file.
pub fn load() -> Result<(Token, TokenSource), TokenError> {
    let keyring = keyring_enabled().then_some(OsKeyring);
    let file = match crate::paths::token_file() {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!("cannot determine token file location: {e:#}");
            None
        }
    };
    load_with(
        |name| std::env::var(name).ok(),
        keyring.as_ref().map(|k| k as &dyn SecretStore),
        file.as_deref(),
    )
}

/// Store the token in the keyring, falling back to the file.
pub fn store(token: &Token) -> Result<TokenSource, TokenError> {
    let keyring = keyring_enabled().then_some(OsKeyring);
    store_with(
        token,
        keyring.as_ref().map(|k| k as &dyn SecretStore),
        &token_file_path()?,
    )
}

/// Delete from keyring and file. Returns human-readable descriptions of what was removed.
pub fn delete() -> Result<Vec<String>, TokenError> {
    let keyring = keyring_enabled().then_some(OsKeyring);
    delete_with(
        keyring.as_ref().map(|k| k as &dyn SecretStore),
        &token_file_path()?,
    )
}

// ---------------------------------------------------------------------------
// Internals (dependency-injected so tests never touch the real env or keyring)
// ---------------------------------------------------------------------------

fn keyring_enabled() -> bool {
    !std::env::var(ENV_DISABLE_KEYRING)
        .map(|v| v.trim() == "1" || v.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn token_file_path() -> Result<PathBuf, TokenError> {
    crate::paths::token_file().map_err(|e| TokenError::Storage(format!("{e:#}")))
}

/// Error from a secret store. `unavailable` marks "there is no usable store at all"
/// (e.g. no Secret Service on a headless Linux box), which is never fatal.
#[derive(Debug)]
pub(crate) struct StoreError {
    unavailable: bool,
    /// Already redacted.
    message: String,
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// Minimal secret-store abstraction over the OS keyring.
pub(crate) trait SecretStore {
    /// `Ok(None)` if there is no entry.
    fn get(&self) -> Result<Option<String>, StoreError>;
    fn set(&self, secret: &str) -> Result<(), StoreError>;
    /// `Ok(true)` if an entry was removed, `Ok(false)` if none existed.
    fn delete(&self) -> Result<bool, StoreError>;
}

/// The real OS keyring (Credential Manager / Keychain / Secret Service).
pub(crate) struct OsKeyring;

impl OsKeyring {
    fn entry(&self) -> Result<keyring::Entry, StoreError> {
        keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).map_err(store_error)
    }
}

/// Converts a keyring error without ever exposing secret bytes: `Display` of
/// `keyring::Error` omits raw data, and we redact token-like text on top.
fn store_error(e: keyring::Error) -> StoreError {
    let unavailable = matches!(e, keyring::Error::NoDefaultStore);
    StoreError {
        unavailable,
        message: redact_all(&e.to_string()),
    }
}

impl SecretStore for OsKeyring {
    fn get(&self) -> Result<Option<String>, StoreError> {
        match self.entry()?.get_password() {
            Ok(s) => Ok(Some(s)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(store_error(e)),
        }
    }

    fn set(&self, secret: &str) -> Result<(), StoreError> {
        self.entry()?.set_password(secret).map_err(store_error)
    }

    fn delete(&self) -> Result<bool, StoreError> {
        match self.entry()?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(e) => Err(store_error(e)),
        }
    }
}

pub(crate) fn load_with(
    env_lookup: impl Fn(&str) -> Option<String>,
    keyring: Option<&dyn SecretStore>,
    file: Option<&Path>,
) -> Result<(Token, TokenSource), TokenError> {
    if let Some(v) = env_lookup(ENV_TOKEN).filter(|v| !v.trim().is_empty()) {
        return Ok((Token::new(v), TokenSource::Env));
    }
    if let Some(k) = keyring {
        match k.get() {
            Ok(Some(v)) if !v.trim().is_empty() => {
                return Ok((Token::new(v), TokenSource::Keyring))
            }
            Ok(_) => {}
            Err(e) if e.unavailable => tracing::debug!("OS keyring unavailable: {e}"),
            Err(e) => tracing::warn!("reading OS keyring failed, trying token file: {e}"),
        }
    }
    if let Some(path) = file {
        if let Some(t) = read_token_file(path)? {
            return Ok((t, TokenSource::File(path.to_path_buf())));
        }
    }
    Err(TokenError::Missing)
}

pub(crate) fn store_with(
    token: &Token,
    keyring: Option<&dyn SecretStore>,
    file: &Path,
) -> Result<TokenSource, TokenError> {
    if token.expose().is_empty() {
        return Err(TokenError::Empty);
    }
    if let Some(k) = keyring {
        // Verify by reading back: some backends accept writes they cannot return.
        match k.set(token.expose()).and_then(|()| k.get()) {
            Ok(Some(v)) if v.trim() == token.expose() => {
                if let Err(e) = remove_file_if_exists(file) {
                    tracing::warn!("could not remove stale token file {}: {e}", file.display());
                }
                return Ok(TokenSource::Keyring);
            }
            Ok(_) => tracing::warn!("OS keyring did not return the stored token; using file"),
            Err(e) if e.unavailable => tracing::debug!("OS keyring unavailable: {e}"),
            Err(e) => tracing::warn!("storing in OS keyring failed, using token file: {e}"),
        }
        // A leftover keyring entry would shadow the file on load; drop it.
        if let Err(e) = k.delete() {
            if !e.unavailable {
                tracing::warn!("could not remove old OS keyring entry: {e}");
            }
        }
    }
    write_token_file(file, token)?;
    Ok(TokenSource::File(file.to_path_buf()))
}

pub(crate) fn delete_with(
    keyring: Option<&dyn SecretStore>,
    file: &Path,
) -> Result<Vec<String>, TokenError> {
    let mut removed = Vec::new();
    let mut keyring_err = None;
    if let Some(k) = keyring {
        match k.delete() {
            Ok(true) => removed.push("OS keyring entry".to_owned()),
            Ok(false) => {}
            Err(e) if e.unavailable => tracing::debug!("OS keyring unavailable: {e}"),
            Err(e) => keyring_err = Some(e),
        }
    }
    if remove_file_if_exists(file).map_err(|e| io_error("remove", file, e))? {
        removed.push(format!("file {}", file.display()));
    }
    match keyring_err {
        Some(e) => Err(TokenError::Storage(format!(
            "deleting the OS keyring entry failed: {e}"
        ))),
        None => Ok(removed),
    }
}

fn io_error(action: &str, path: &Path, e: io::Error) -> TokenError {
    TokenError::Storage(format!("cannot {action} {}: {e}", path.display()))
}

fn remove_file_if_exists(path: &Path) -> io::Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Reads and trims the token file. `Ok(None)` if it is missing or blank.
pub(crate) fn read_token_file(path: &Path) -> Result<Option<Token>, TokenError> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(Token::new(s)).filter(|t| !t.expose().is_empty())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error("read", path, e)),
    }
}

/// Writes the token to `path` via a temp file + rename, so a crash never leaves a
/// half-written token. The temp file is restricted to the current user *before*
/// the secret is written to it.
pub(crate) fn write_token_file(path: &Path, token: &Token) -> Result<(), TokenError> {
    if token.expose().is_empty() {
        return Err(TokenError::Empty);
    }
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(|e| io_error("create directory", dir, e))?;

    let tmp = dir.join(format!(".token.{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut f = create_private(&tmp).map_err(|e| io_error("create", &tmp, e))?;
        restrict_permissions(&tmp);
        f.write_all(token.expose().as_bytes())
            .and_then(|()| f.write_all(b"\n"))
            .and_then(|()| f.sync_all())
            .map_err(|e| io_error("write", &tmp, e))?;
        drop(f);
        fs::rename(&tmp, path).map_err(|e| io_error("rename temp file to", path, e))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(unix)]
fn create_private(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// unix: enforce 0600 regardless of umask.
#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
        tracing::warn!("could not set 0600 on {}: {e}", path.display());
    }
}

/// Windows: drop inherited ACEs and grant full control to the current user only.
/// Failure is not fatal: the file then lives under the user's profile, which is
/// already private to that user by default.
#[cfg(windows)]
fn restrict_permissions(path: &Path) {
    use std::process::{Command, Stdio};
    let Ok(user) = std::env::var("USERNAME") else {
        tracing::warn!(
            "USERNAME not set; token file {} is protected only by the user profile directory",
            path.display()
        );
        return;
    };
    let principal = match std::env::var("USERDOMAIN") {
        Ok(d) if !d.is_empty() => format!("{d}\\{user}"),
        _ => user,
    };
    let status = Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("{principal}:F"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if !matches!(status, Ok(s) if s.success()) {
        tracing::warn!(
            "could not restrict ACL of {} (icacls failed); it is protected only by the \
             user profile directory",
            path.display()
        );
    }
}

#[cfg(not(any(unix, windows)))]
fn restrict_permissions(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    const SAMPLE: &str = "sk-ant-oat01-SECRETSECRET";

    /// In-memory keyring for tests.
    #[derive(Default)]
    struct FakeStore {
        value: RefCell<Option<String>>,
        fail: bool,
    }

    impl FakeStore {
        fn failing() -> Self {
            Self {
                value: RefCell::default(),
                fail: true,
            }
        }
        fn err() -> StoreError {
            StoreError {
                unavailable: false,
                message: "boom".into(),
            }
        }
    }

    impl SecretStore for FakeStore {
        fn get(&self) -> Result<Option<String>, StoreError> {
            if self.fail {
                return Err(Self::err());
            }
            Ok(self.value.borrow().clone())
        }
        fn set(&self, secret: &str) -> Result<(), StoreError> {
            if self.fail {
                return Err(Self::err());
            }
            *self.value.borrow_mut() = Some(secret.to_owned());
            Ok(())
        }
        fn delete(&self) -> Result<bool, StoreError> {
            if self.fail {
                return Err(Self::err());
            }
            Ok(self.value.borrow_mut().take().is_some())
        }
    }

    fn env(map: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = map
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn format_check() {
        assert_eq!(check_format(SAMPLE), FormatCheck::Ok);
        match check_format("sk-ant-api03-SECRETSECRET") {
            FormatCheck::Warn(msg) => assert!(!msg.contains("SECRETSECRET")),
            FormatCheck::Ok => panic!("expected warning"),
        }
        assert!(matches!(check_format("garbage"), FormatCheck::Warn(_)));
    }

    #[test]
    fn file_roundtrip_trims_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("token");
        assert_eq!(read_token_file(&path).unwrap(), None);
        write_token_file(&path, &Token::new(format!("  {SAMPLE}\r\n"))).unwrap();
        assert_eq!(read_token_file(&path).unwrap().unwrap().expose(), SAMPLE);

        // Overwrite works and leaves no temp files behind.
        write_token_file(&path, &Token::new("sk-ant-oat01-other")).unwrap();
        assert_eq!(
            read_token_file(&path).unwrap().unwrap().expose(),
            "sk-ant-oat01-other"
        );
        let entries: Vec<_> = fs::read_dir(path.parent().unwrap()).unwrap().collect();
        assert_eq!(entries.len(), 1);

        // Whitespace around a hand-edited token is trimmed; blank means "no token".
        fs::write(&path, format!("\n {SAMPLE} \n\n")).unwrap();
        assert_eq!(read_token_file(&path).unwrap().unwrap().expose(), SAMPLE);
        fs::write(&path, " \n\t").unwrap();
        assert_eq!(read_token_file(&path).unwrap(), None);
    }

    #[test]
    fn write_rejects_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        assert!(matches!(
            write_token_file(&path, &Token::new("  \n")),
            Err(TokenError::Empty)
        ));
        assert!(matches!(
            store_with(&Token::new(""), None, &path),
            Err(TokenError::Empty)
        ));
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn file_permissions_are_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        write_token_file(&path, &Token::new(SAMPLE)).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// The ACL set on the temp file must survive the rename: no inherited ACEs.
    #[cfg(windows)]
    #[test]
    fn file_acl_has_no_inherited_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        write_token_file(&path, &Token::new(SAMPLE)).unwrap();
        let out = std::process::Command::new("icacls")
            .arg(&path)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success());
        assert!(!text.contains("(I)"), "inherited ACEs present: {text}");
    }

    #[test]
    fn load_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        write_token_file(&path, &Token::new("sk-ant-oat01-file")).unwrap();
        let kr = FakeStore::default();
        kr.set("sk-ant-oat01-keyring").unwrap();

        // env wins over everything
        let (t, src) = load_with(
            env(&[(ENV_TOKEN, "sk-ant-oat01-env")]),
            Some(&kr),
            Some(&path),
        )
        .unwrap();
        assert_eq!((t.expose(), src), ("sk-ant-oat01-env", TokenSource::Env));

        // blank env is ignored; keyring beats file
        let (t, src) = load_with(env(&[(ENV_TOKEN, "  ")]), Some(&kr), Some(&path)).unwrap();
        assert_eq!(
            (t.expose(), src),
            ("sk-ant-oat01-keyring", TokenSource::Keyring)
        );

        // keyring disabled -> file
        let (t, src) = load_with(env(&[]), None, Some(&path)).unwrap();
        assert_eq!(
            (t.expose(), src),
            ("sk-ant-oat01-file", TokenSource::File(path.clone()))
        );

        // keyring error falls through to file
        let (_, src) = load_with(env(&[]), Some(&FakeStore::failing()), Some(&path)).unwrap();
        assert_eq!(src, TokenSource::File(path.clone()));

        // nothing anywhere
        fs::remove_file(&path).unwrap();
        assert!(matches!(
            load_with(env(&[]), Some(&FakeStore::default()), Some(&path)),
            Err(TokenError::Missing)
        ));
        assert!(matches!(
            load_with(env(&[]), None, None),
            Err(TokenError::Missing)
        ));
    }

    #[test]
    fn store_prefers_keyring_and_removes_stale_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        write_token_file(&path, &Token::new("sk-ant-oat01-old")).unwrap();
        let kr = FakeStore::default();
        let src = store_with(&Token::new(SAMPLE), Some(&kr), &path).unwrap();
        assert_eq!(src, TokenSource::Keyring);
        assert_eq!(kr.value.borrow().as_deref(), Some(SAMPLE));
        assert!(!path.exists());
    }

    #[test]
    fn store_falls_back_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        let src = store_with(&Token::new(SAMPLE), Some(&FakeStore::failing()), &path).unwrap();
        assert_eq!(src, TokenSource::File(path.clone()));
        assert_eq!(read_token_file(&path).unwrap().unwrap().expose(), SAMPLE);

        let src = store_with(&Token::new(SAMPLE), None, &path).unwrap();
        assert_eq!(src, TokenSource::File(path));
    }

    #[test]
    fn delete_reports_what_was_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        let kr = FakeStore::default();
        assert!(delete_with(Some(&kr), &path).unwrap().is_empty());
        assert!(delete_with(None, &path).unwrap().is_empty());

        kr.set(SAMPLE).unwrap();
        write_token_file(&path, &Token::new(SAMPLE)).unwrap();
        let removed = delete_with(Some(&kr), &path).unwrap();
        assert_eq!(removed.len(), 2);
        assert_eq!(removed[0], "OS keyring entry");
        assert!(removed[1].starts_with("file "));
        assert!(!path.exists());
        assert!(kr.value.borrow().is_none());

        // A real keyring failure is reported, but the file is still removed.
        write_token_file(&path, &Token::new(SAMPLE)).unwrap();
        assert!(matches!(
            delete_with(Some(&FakeStore::failing()), &path),
            Err(TokenError::Storage(_))
        ));
        assert!(!path.exists());
    }

    #[test]
    fn nothing_leaks_the_secret() {
        let t = Token::new(SAMPLE);
        let texts = [
            format!("{t:?}"),
            t.to_string(),
            format!("{:?}", Some(t.clone())),
            format!("{:?}", TokenSource::Env),
            TokenSource::Env.to_string(),
            format!("{:?}", TokenSource::Keyring),
            TokenSource::Keyring.to_string(),
            format!("{:?}", TokenSource::File(PathBuf::from("/x/token"))),
            TokenSource::File(PathBuf::from("/x/token")).to_string(),
            format!("{:?}", TokenError::Missing),
            TokenError::Missing.to_string(),
            format!("{:?}", TokenError::Empty),
            TokenError::Empty.to_string(),
            format!("{:?}", TokenError::Storage("disk full".into())),
            TokenError::Storage("disk full".into()).to_string(),
            format!("{:?}", check_format("SECRETSECRET")),
            store_error(keyring::Error::BadEncoding(SAMPLE.into())).to_string(),
            store_error(keyring::Error::BadStoreFormat(SAMPLE.into())).to_string(),
        ];
        for s in texts {
            assert!(!s.contains("SECRETSECRET"), "leaked in: {s}");
        }
    }
}
