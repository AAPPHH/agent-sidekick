//! Reachability checks for `sidekick_backends`, `backends test`, `doctor` and the
//! pre-run check of claude-endpoint backends.
//!
//! The HTTP probe (ureq 3, rustls with `ring`, no gzip):
//! - GETs `base_url` exactly as configured (no path appended), never with credentials;
//!   `user:pass@` in the URL is stripped before the request and from every `detail`.
//! - Any HTTP response counts as reachable, also 4xx/5xx and 3xx (redirects are not
//!   followed); connection errors, timeouts and non-HTTP answers do not.
//! - One global timeout covers resolve, connect (incl. TLS) and reading the response head;
//!   the body is never read.
//! - Proxies: loopback hosts (`localhost`, `*.localhost`, `127.0.0.0/8`, `::1`) are always
//!   probed directly. Other hosts use the proxy from `ALL_PROXY`/`HTTPS_PROXY`/`HTTP_PROXY`
//!   (honouring `NO_PROXY`), like the `claude` child would. A proxy that answers for an
//!   unreachable upstream (e.g. 502) therefore makes the endpoint look reachable.

use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ureq::http::Uri;

use super::Backend;
use crate::harness::CheckDeps;
use crate::token::{SecretId, TokenError};

/// Timeout of the HTTP probe of a claude-endpoint `base_url`.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reachability {
    pub reachable: bool,
    /// Human-readable reason; never contains secrets.
    pub detail: String,
}

impl Reachability {
    pub(crate) fn ok(detail: impl Into<String>) -> Self {
        Self {
            reachable: true,
            detail: detail.into(),
        }
    }

    pub(crate) fn unreachable(detail: impl Into<String>) -> Self {
        Self {
            reachable: false,
            detail: detail.into(),
        }
    }
}

/// HTTP GET `base_url` without credentials; any HTTP response (also 4xx/5xx) counts as
/// reachable, a connection error or timeout does not. Blocking.
pub fn probe_http(base_url: &str, timeout: Duration) -> Reachability {
    match probe_get(base_url, None, timeout) {
        ProbeResponse::Status { code, url, millis } => {
            Reachability::ok(format!("HTTP {code} from {url} in {millis} ms"))
        }
        ProbeResponse::Failed(why) => Reachability::unreachable(format!("unreachable: {why}")),
    }
}

/// Outcome of [`probe_get`]. Never contains credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeResponse {
    /// Any HTTP response; `url` without `user:pass@`.
    Status {
        code: u16,
        url: String,
        millis: u128,
    },
    /// No HTTP response (invalid URL, connection error, timeout, TLS, ...).
    Failed(String),
}

/// HTTP `GET url` (same rules as [`probe_http`]: no redirects, one global timeout, loopback
/// without proxy, body never read), optionally with `Authorization: Bearer <bearer>`. The
/// bearer token is only ever sent to `url` itself (redirects are not followed) and never
/// appears in the result. Blocking.
pub fn probe_get(url: &str, bearer: Option<&str>, timeout: Duration) -> ProbeResponse {
    let target = match ProbeTarget::parse(url) {
        Ok(t) => t,
        Err(why) => return ProbeResponse::Failed(why),
    };

    let start = Instant::now();
    loop {
        // The remaining budget, so retries never extend the overall timeout.
        let remaining = timeout.saturating_sub(start.elapsed());
        let mut config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(remaining));
        if target.loopback {
            config = config.proxy(None);
        }
        let agent: ureq::Agent = config.build().into();

        let mut request = agent.get(&target.url);
        if let Some(token) = bearer {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        // `call` returns once the response head is read; the body is dropped unread.
        match request.call() {
            Ok(resp) => {
                return ProbeResponse::Status {
                    code: resp.status().as_u16(),
                    url: target.url,
                    millis: start.elapsed().as_millis(),
                }
            }
            // A signal (e.g. SIGCHLD when a run's child exits) can interrupt a blocking socket
            // call on Unix; ureq does not retry that. A GET is idempotent, so try again.
            Err(ureq::Error::Io(io)) if io.kind() == io::ErrorKind::Interrupted => {
                if start.elapsed() >= timeout {
                    return ProbeResponse::Failed(format!(
                        "timed out after {} ms",
                        timeout.as_millis()
                    ));
                }
            }
            Err(e) => return ProbeResponse::Failed(error_text(&e, timeout)),
        }
    }
}

/// claude-oauth: claude found and a token stored; claude-endpoint: claude found, key
/// available and `base_url` answers within [`PROBE_TIMEOUT`]; command: program found (and,
/// with `secret_env`, its secret stored).
/// Blocking (keyring, filesystem, network).
pub fn check_backend(backend: &Backend, claude_path: Option<&Path>) -> Reachability {
    check_backend_with(
        backend,
        &|| {
            crate::locate::find_claude(claude_path)
                .map(|l| l.path)
                .map_err(|e| e.to_string())
        },
        &|id| crate::token::load_secret(id).map(|(_token, source)| source.to_string()),
        &|name| std::env::var(name).ok(),
        &|program| {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            super::launch::resolve_command_program(
                &backend.name,
                program,
                std::env::var_os("PATH"),
                &cwd,
            )
        },
        &|url| probe_http(url, PROBE_TIMEOUT),
    )
}

/// Testable core of [`check_backend`]: dispatches to the backend's harness adapter
/// ([`crate::harness::Harness::check`]). Checks run in order and the first failure wins.
/// - `find_claude`: path of the claude executable or a user-facing error.
/// - `secret`: the secret's source description (never the value) or the load error.
/// - `env_lookup`: environment access (for `api_key_env`).
/// - `resolve_program`: `command[0]` -> executable path or a user-facing error.
/// - `probe`: the HTTP probe of a `base_url`.
fn check_backend_with(
    backend: &Backend,
    find_claude: &dyn Fn() -> Result<PathBuf, String>,
    secret: &dyn Fn(&SecretId) -> Result<String, TokenError>,
    env_lookup: &dyn Fn(&str) -> Option<String>,
    resolve_program: &dyn Fn(&str) -> Result<PathBuf, String>,
    probe: &dyn Fn(&str) -> Reachability,
) -> Reachability {
    backend.harness().check(
        backend,
        &CheckDeps {
            find_claude,
            secret,
            env_lookup,
            resolve_program,
            probe,
            locate: &|b| b.harness().locate(b, None),
            secret_value: &|id| crate::token::load_secret(id).map(|(token, _source)| token),
            probe_get: &|url, bearer| probe_get(url, bearer, PROBE_TIMEOUT),
        },
    )
}

/// A parsed, credential-free probe URL.
struct ProbeTarget {
    /// `base_url` without `user:pass@`; otherwise exactly as configured.
    url: String,
    loopback: bool,
}

impl ProbeTarget {
    fn parse(base_url: &str) -> Result<Self, String> {
        let trimmed = base_url.trim();
        let uri: Uri = trimmed
            .parse()
            .map_err(|_| "invalid URL (expected http://host[:port][/path])".to_owned())?;
        match uri.scheme_str() {
            Some("http") | Some("https") => {}
            _ => return Err("invalid URL (scheme must be http:// or https://)".to_owned()),
        }
        let authority = uri
            .authority()
            .ok_or_else(|| "invalid URL (no host)".to_owned())?;
        if authority.host().is_empty() {
            return Err("invalid URL (no host)".to_owned());
        }
        let url = match authority.as_str().rfind('@') {
            // Rebuild only when credentials must be dropped, so the URL otherwise stays
            // byte-for-byte as configured.
            Some(at) => {
                let scheme_end = trimmed.find("://").map(|i| i + 3).unwrap_or(0);
                let auth_start = trimmed[scheme_end..]
                    .find(authority.as_str())
                    .map(|i| i + scheme_end)
                    .unwrap_or(scheme_end);
                format!(
                    "{}{}",
                    &trimmed[..auth_start],
                    &trimmed[auth_start + at + 1..]
                )
            }
            None => trimmed.to_owned(),
        };
        Ok(Self {
            url,
            loopback: is_loopback_host(authority.host()),
        })
    }
}

fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let lower = host.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") {
        return true;
    }
    host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Short, secret-free description of a probe failure (no headers, bodies or URLs).
fn error_text(e: &ureq::Error, timeout: Duration) -> String {
    use ureq::Error as E;
    match e {
        E::Timeout(_) => format!("timed out after {} ms", timeout.as_millis()),
        E::HostNotFound => "host not found".to_owned(),
        E::ConnectionFailed => "connection failed".to_owned(),
        E::Io(io) => io_text(io),
        E::BadUri(_) | E::Http(_) => "invalid URL".to_owned(),
        E::Protocol(_) => "no valid HTTP response (not an HTTP server?)".to_owned(),
        E::Tls(msg) => format!("TLS error: {msg}"),
        E::Rustls(err) => format!("TLS error: {err}"),
        E::InvalidProxyUrl => "invalid proxy URL in the environment".to_owned(),
        E::ConnectProxyFailed(_) => "proxy connection failed".to_owned(),
        _ => "request failed".to_owned(),
    }
}

fn io_text(e: &io::Error) -> String {
    use io::ErrorKind as K;
    match e.kind() {
        K::ConnectionRefused => "connection refused".to_owned(),
        K::ConnectionReset => "connection reset".to_owned(),
        K::ConnectionAborted => "connection aborted".to_owned(),
        K::TimedOut | K::WouldBlock => "timed out".to_owned(),
        K::UnexpectedEof => "connection closed without a response".to_owned(),
        K::AddrNotAvailable => "address not available".to_owned(),
        kind => format!("I/O error ({kind})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::BackendKind;
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;

    // ----- probe_http -----

    /// What the fake server does with an accepted connection.
    enum Answer {
        Raw(&'static str),
        /// Accept, read the request, never answer (until the test ends).
        Silent,
    }

    struct FakeServer {
        url: String,
        requests: mpsc::Receiver<String>,
        _stop: mpsc::Sender<()>,
    }

    fn read_head(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    fn fake_server(answer: Answer) -> FakeServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (req_tx, req_rx) = mpsc::channel();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        thread::spawn(move || {
            // One connection is enough; the thread ends with the test process at worst.
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let head = read_head(&mut stream);
            let _ = req_tx.send(head);
            match answer {
                Answer::Raw(resp) => {
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.flush();
                }
                Answer::Silent => {
                    // Hold the connection open until the test drops its sender.
                    let _ = stop_rx.recv_timeout(Duration::from_secs(10));
                }
            }
        });
        FakeServer {
            url: format!("http://127.0.0.1:{port}"),
            requests: req_rx,
            _stop: stop_tx,
        }
    }

    fn assert_no_credentials(request: &str) {
        let lower = request.to_ascii_lowercase();
        assert!(!lower.contains("authorization:"), "{request}");
        assert!(!lower.contains("x-api-key:"), "{request}");
        assert!(!lower.contains("cookie:"), "{request}");
    }

    const T: Duration = Duration::from_millis(500);
    /// Generous upper bound for slow CI runners.
    const SLACK: Duration = Duration::from_millis(2500);

    #[test]
    fn http_404_is_reachable_without_credentials() {
        let srv = fake_server(Answer::Raw(
            "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found",
        ));
        let r = probe_http(&srv.url, Duration::from_secs(5));
        assert!(r.reachable, "{r:?}");
        assert!(
            r.detail
                .starts_with(&format!("HTTP 404 from {} in ", srv.url)),
            "{r:?}"
        );
        assert!(r.detail.ends_with(" ms"), "{r:?}");
        assert!(!r.detail.contains("not found"), "body leaked: {r:?}");
        let req = srv.requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(req.starts_with("GET / HTTP/1.1\r\n"), "{req}");
        assert_no_credentials(&req);
    }

    #[test]
    fn http_200_is_reachable() {
        let srv = fake_server(Answer::Raw(
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nX-Secret: hdr\r\nConnection: close\r\n\r\nok",
        ));
        let r = probe_http(&srv.url, Duration::from_secs(5));
        assert!(r.reachable, "{r:?}");
        assert!(r.detail.starts_with("HTTP 200 from "), "{r:?}");
        assert!(!r.detail.contains("hdr"), "header leaked: {r:?}");
        assert_no_credentials(&srv.requests.recv_timeout(Duration::from_secs(5)).unwrap());
    }

    #[test]
    fn http_500_is_reachable() {
        let srv = fake_server(Answer::Raw(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ));
        let r = probe_http(&srv.url, Duration::from_secs(5));
        assert!(r.reachable, "{r:?}");
        assert!(r.detail.starts_with("HTTP 500 from "), "{r:?}");
    }

    #[test]
    fn redirect_is_a_response_and_not_followed() {
        let srv = fake_server(Answer::Raw(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ));
        let r = probe_http(&srv.url, Duration::from_secs(5));
        assert!(r.reachable, "{r:?}");
        assert!(r.detail.starts_with("HTTP 302 from "), "{r:?}");
    }

    #[test]
    fn path_is_sent_exactly_as_configured() {
        let srv = fake_server(Answer::Raw(
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ));
        let url = format!("{}/api/v1", srv.url);
        let r = probe_http(&url, Duration::from_secs(5));
        assert!(r.reachable, "{r:?}");
        let req = srv.requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(req.starts_with("GET /api/v1 HTTP/1.1\r\n"), "{req}");
    }

    #[test]
    fn userinfo_is_neither_sent_nor_shown() {
        let srv = fake_server(Answer::Raw(
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ));
        let with_creds = srv.url.replace("http://", "http://user:hunter2secret@");
        let r = probe_http(&with_creds, Duration::from_secs(5));
        assert!(r.reachable, "{r:?}");
        assert!(!r.detail.contains("hunter2"), "{r:?}");
        assert!(!r.detail.contains("user"), "{r:?}");
        assert!(r.detail.contains(&srv.url), "{r:?}");
        let req = srv.requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_no_credentials(&req);
        assert!(!req.contains("hunter2"), "{req}");
    }

    #[test]
    fn silent_server_times_out() {
        let srv = fake_server(Answer::Silent);
        let start = Instant::now();
        let r = probe_http(&srv.url, T);
        let elapsed = start.elapsed();
        assert!(!r.reachable, "{r:?}");
        assert!(r.detail.starts_with("unreachable: "), "{r:?}");
        assert!(r.detail.contains("timed out"), "{r:?}");
        assert!(elapsed >= T - Duration::from_millis(50), "{elapsed:?}");
        assert!(elapsed < T + SLACK, "{elapsed:?}");
        // The request reached the server and carried no credentials.
        assert_no_credentials(&srv.requests.recv_timeout(Duration::from_secs(5)).unwrap());
    }

    #[test]
    fn non_http_answer_is_unreachable() {
        let srv = fake_server(Answer::Raw("SSH-2.0-OpenSSH_9.0\r\n\r\n"));
        let r = probe_http(&srv.url, Duration::from_secs(5));
        assert!(!r.reachable, "{r:?}");
        assert!(r.detail.starts_with("unreachable: "), "{r:?}");
    }

    #[test]
    fn closed_port_is_unreachable() {
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let start = Instant::now();
        let r = probe_http(&format!("http://127.0.0.1:{port}"), T);
        assert!(!r.reachable, "{r:?}");
        assert!(r.detail.starts_with("unreachable: "), "{r:?}");
        // Windows retries refused loopback connects (~2 s), so the timeout may hit first.
        assert!(start.elapsed() < T + SLACK, "{:?}", start.elapsed());
    }

    #[test]
    fn invalid_urls_are_unreachable() {
        for url in [
            "not a url",
            "",
            "localhost:11434",
            "ftp://localhost/",
            "http://",
            "/just/a/path",
        ] {
            let start = Instant::now();
            let r = probe_http(url, T);
            assert!(!r.reachable, "{url}: {r:?}");
            assert!(r.detail.starts_with("unreachable: "), "{url}: {r:?}");
            assert!(r.detail.contains("invalid URL"), "{url}: {r:?}");
            assert!(start.elapsed() < T + SLACK);
        }
    }

    #[test]
    fn loopback_detection() {
        for host in [
            "localhost",
            "LOCALHOST",
            "foo.localhost",
            "127.0.0.1",
            "127.1.2.3",
            "[::1]",
        ] {
            assert!(is_loopback_host(host), "{host}");
        }
        for host in ["example.com", "10.0.0.1", "[::2]", "localhost.example.com"] {
            assert!(!is_loopback_host(host), "{host}");
        }
    }

    #[test]
    fn probe_target_strips_only_userinfo() {
        let t = ProbeTarget::parse("http://localhost:11434").unwrap();
        assert_eq!(t.url, "http://localhost:11434");
        assert!(t.loopback);
        let t = ProbeTarget::parse("https://u:p@api.example.com/v1/").unwrap();
        assert_eq!(t.url, "https://api.example.com/v1/");
        assert!(!t.loopback);
        let t = ProbeTarget::parse("http://tok@[::1]:8080").unwrap();
        assert_eq!(t.url, "http://[::1]:8080");
        assert!(t.loopback);
    }

    #[test]
    fn io_errors_have_short_texts() {
        assert_eq!(
            io_text(&io::Error::from(io::ErrorKind::ConnectionRefused)),
            "connection refused"
        );
        assert!(io_text(&io::Error::other("secret-ish text")).starts_with("I/O error ("));
        assert!(!io_text(&io::Error::other("secret-ish text")).contains("secret"));
    }

    // ----- check_backend_with -----

    fn backend(name: &str, kind: BackendKind) -> Backend {
        Backend {
            name: name.to_owned(),
            description: "test".to_owned(),
            cost: super::super::Cost::Free,
            is_default: true,
            max_concurrent: None,
            model: None,
            models: None,
            env: BTreeMap::new(),
            pass_env: Vec::new(),
            implicit: false,
            kind,
            foreign_secret_env: Vec::new(),
        }
    }

    fn oauth(name: &str) -> Backend {
        backend(
            name,
            BackendKind::ClaudeOauth {
                config_dir: PathBuf::from("cfg"),
            },
        )
    }

    fn endpoint(name: &str, api_key_env: Option<&str>) -> Backend {
        backend(
            name,
            BackendKind::ClaudeEndpoint {
                base_url: "http://localhost:11434".to_owned(),
                api_key_env: api_key_env.map(str::to_owned),
                auth: super::super::EndpointAuth::XApiKey,
                config_dir: PathBuf::from("cfg"),
            },
        )
    }

    fn command(cmd: &[&str]) -> Backend {
        backend(
            "cmd",
            BackendKind::Command {
                command: cmd.iter().map(|s| s.to_string()).collect(),
                prompt_via_stdin: false,
                secret_env: None,
            },
        )
    }

    fn command_with_secret(var: &str) -> Backend {
        backend(
            "kic",
            BackendKind::Command {
                command: vec!["aider".into(), "{prompt}".into()],
                prompt_via_stdin: false,
                secret_env: Some(var.to_owned()),
            },
        )
    }

    fn claude_ok() -> Result<PathBuf, String> {
        Ok(PathBuf::from("/opt/claude"))
    }
    fn claude_missing() -> Result<PathBuf, String> {
        Err("could not find the `claude` executable".to_owned())
    }
    fn secret_keyring(_: &SecretId) -> Result<String, TokenError> {
        Ok("OS keyring".to_owned())
    }
    fn secret_missing(_: &SecretId) -> Result<String, TokenError> {
        Err(TokenError::Missing)
    }
    fn secret_broken(_: &SecretId) -> Result<String, TokenError> {
        Err(TokenError::Storage("keyring locked".to_owned()))
    }
    fn no_env(_: &str) -> Option<String> {
        None
    }
    fn no_program(_: &str) -> Result<PathBuf, String> {
        panic!("resolve_program must not be called")
    }
    fn no_probe(_: &str) -> Reachability {
        panic!("probe must not be called")
    }
    fn probe_up(url: &str) -> Reachability {
        Reachability::ok(format!("HTTP 404 from {url} in 3 ms"))
    }
    fn probe_down(_: &str) -> Reachability {
        Reachability::unreachable("unreachable: connection refused")
    }

    #[test]
    fn oauth_reachable() {
        let seen = std::cell::RefCell::new(None);
        let secret = |id: &SecretId| {
            *seen.borrow_mut() = Some(id.clone());
            Ok("OS keyring (service agent-sidekick)".to_owned())
        };
        let r = check_backend_with(
            &oauth("work"),
            &claude_ok,
            &secret,
            &no_env,
            &no_program,
            &no_probe,
        );
        assert!(r.reachable, "{r:?}");
        assert_eq!(
            r.detail,
            format!(
                "claude {}; token from OS keyring (service agent-sidekick)",
                Path::new("/opt/claude").display()
            )
        );
        assert_eq!(seen.into_inner(), Some(SecretId::Oauth("work".to_owned())));
    }

    #[test]
    fn oauth_claude_missing() {
        let r = check_backend_with(
            &oauth("work"),
            &claude_missing,
            &secret_keyring,
            &no_env,
            &no_program,
            &no_probe,
        );
        assert!(!r.reachable);
        assert!(
            r.detail.starts_with("claude not found: could not find"),
            "{r:?}"
        );
    }

    #[test]
    fn oauth_token_missing() {
        let r = check_backend_with(
            &oauth("work"),
            &claude_ok,
            &secret_missing,
            &no_env,
            &no_program,
            &no_probe,
        );
        assert!(!r.reachable);
        assert_eq!(
            r.detail,
            "no token stored; run `agent-sidekick setup --backend work`"
        );
    }

    #[test]
    fn oauth_token_storage_error() {
        let r = check_backend_with(
            &oauth("work"),
            &claude_ok,
            &secret_broken,
            &no_env,
            &no_program,
            &no_probe,
        );
        assert!(!r.reachable);
        assert!(r.detail.contains("keyring locked"), "{r:?}");
        assert!(r.detail.contains("OAuth token of backend `work`"), "{r:?}");
    }

    #[test]
    fn endpoint_reachable_with_stored_key() {
        let probed = std::cell::RefCell::new(Vec::new());
        let probe = |url: &str| {
            probed.borrow_mut().push(url.to_owned());
            probe_up(url)
        };
        let seen = std::cell::RefCell::new(None);
        let secret = |id: &SecretId| {
            *seen.borrow_mut() = Some(id.clone());
            Ok("OS keyring".to_owned())
        };
        let r = check_backend_with(
            &endpoint("local", None),
            &claude_ok,
            &secret,
            &no_env,
            &no_program,
            &probe,
        );
        assert!(r.reachable, "{r:?}");
        assert_eq!(r.detail, "HTTP 404 from http://localhost:11434 in 3 ms");
        assert_eq!(probed.into_inner(), vec!["http://localhost:11434"]);
        assert_eq!(
            seen.into_inner(),
            Some(SecretId::Endpoint("local".to_owned()))
        );
    }

    #[test]
    fn endpoint_reachable_with_env_key_does_not_touch_store() {
        let env = |k: &str| (k == "OLLAMA_API_KEY").then(|| "ollama".to_owned());
        let secret = |_: &SecretId| -> Result<String, TokenError> {
            panic!("secret store must not be used with api_key_env")
        };
        let r = check_backend_with(
            &endpoint("local", Some("OLLAMA_API_KEY")),
            &claude_ok,
            &secret,
            &env,
            &no_program,
            &probe_up,
        );
        assert!(r.reachable, "{r:?}");
    }

    #[test]
    fn endpoint_env_key_unset_or_empty() {
        let empty = |_: &str| Some("  ".to_owned());
        for env in [&no_env as &dyn Fn(&str) -> Option<String>, &empty] {
            let r = check_backend_with(
                &endpoint("local", Some("OLLAMA_API_KEY")),
                &claude_ok,
                &secret_keyring,
                env,
                &no_program,
                &no_probe,
            );
            assert!(!r.reachable);
            assert_eq!(r.detail, "env var OLLAMA_API_KEY (api_key_env) is not set");
        }
    }

    #[test]
    fn endpoint_key_missing() {
        let r = check_backend_with(
            &endpoint("local", None),
            &claude_ok,
            &secret_missing,
            &no_env,
            &no_program,
            &no_probe,
        );
        assert!(!r.reachable);
        assert!(
            r.detail
                .starts_with("no API key stored; run `agent-sidekick setup --backend local`"),
            "{r:?}"
        );
    }

    #[test]
    fn endpoint_key_storage_error() {
        let r = check_backend_with(
            &endpoint("local", None),
            &claude_ok,
            &secret_broken,
            &no_env,
            &no_program,
            &no_probe,
        );
        assert!(!r.reachable);
        assert!(r.detail.contains("API key of backend `local`"), "{r:?}");
    }

    #[test]
    fn endpoint_claude_missing_comes_first() {
        let r = check_backend_with(
            &endpoint("local", None),
            &claude_missing,
            &secret_missing,
            &no_env,
            &no_program,
            &no_probe,
        );
        assert!(!r.reachable);
        assert!(r.detail.starts_with("claude not found: "), "{r:?}");
    }

    #[test]
    fn endpoint_probe_fails() {
        let r = check_backend_with(
            &endpoint("local", None),
            &claude_ok,
            &secret_keyring,
            &no_env,
            &no_program,
            &probe_down,
        );
        assert!(!r.reachable);
        assert_eq!(r.detail, "unreachable: connection refused");
    }

    #[test]
    fn command_reachable_and_not_found() {
        let resolve = |p: &str| -> Result<PathBuf, String> {
            if p == "aider" {
                Ok(PathBuf::from("/usr/bin/aider"))
            } else {
                Err(format!("program `{p}` not found in PATH"))
            }
        };
        let claude = || -> Result<PathBuf, String> { panic!("claude not needed") };
        let secret = |_: &SecretId| -> Result<String, TokenError> { panic!("no secret") };
        let r = check_backend_with(
            &command(&["aider", "--message", "{prompt}"]),
            &claude,
            &secret,
            &no_env,
            &resolve,
            &no_probe,
        );
        assert!(r.reachable, "{r:?}");
        assert_eq!(
            r.detail,
            format!("program {}", Path::new("/usr/bin/aider").display())
        );
        let r = check_backend_with(
            &command(&["nope", "{prompt}"]),
            &claude,
            &secret,
            &no_env,
            &resolve,
            &no_probe,
        );
        assert!(!r.reachable);
        assert_eq!(r.detail, "program `nope` not found in PATH");
        let r = check_backend_with(
            &command(&[]),
            &claude,
            &secret,
            &no_env,
            &resolve,
            &no_probe,
        );
        assert!(!r.reachable);
    }

    #[test]
    fn command_batch_script_with_argument_prompt_is_unreachable() {
        let resolve = |_: &str| -> Result<PathBuf, String> { Ok(PathBuf::from("/x/agent.cmd")) };
        let claude = || -> Result<PathBuf, String> { panic!("claude not needed") };
        let secret = |_: &SecretId| -> Result<String, TokenError> { panic!("no secret") };
        let r = check_backend_with(
            &command(&["agent", "{prompt}"]),
            &claude,
            &secret,
            &no_env,
            &resolve,
            &no_probe,
        );
        assert!(!r.reachable, "{r:?}");
        let expected =
            super::super::launch::batch_argument_error("cmd", Path::new("/x/agent.cmd"), false)
                .unwrap();
        assert_eq!(r.detail, expected);
        assert!(r.detail.contains("prompt_via_stdin = true"), "{r:?}");

        // Via stdin a .cmd is fine.
        let b = backend(
            "cmd",
            BackendKind::Command {
                command: vec!["agent".into()],
                prompt_via_stdin: true,
                secret_env: None,
            },
        );
        let r = check_backend_with(&b, &claude, &secret, &no_env, &resolve, &no_probe);
        assert!(r.reachable, "{r:?}");
    }

    #[test]
    fn command_with_secret_env() {
        let resolve = |p: &str| -> Result<PathBuf, String> {
            if p == "aider" {
                Ok(PathBuf::from("/usr/bin/aider"))
            } else {
                Err(format!("program `{p}` not found in PATH"))
            }
        };
        let claude = || -> Result<PathBuf, String> { panic!("claude not needed") };
        let seen = std::cell::RefCell::new(None);
        let secret = |id: &SecretId| -> Result<String, TokenError> {
            *seen.borrow_mut() = Some(id.clone());
            Ok("OS keyring (service agent-sidekick)".to_owned())
        };
        let b = command_with_secret("OPENAI_API_KEY");
        let r = check_backend_with(&b, &claude, &secret, &no_env, &resolve, &no_probe);
        assert!(r.reachable, "{r:?}");
        assert_eq!(*seen.borrow(), Some(SecretId::Command("kic".into())));
        assert_eq!(
            r.detail,
            format!(
                "program {}; secret_env OPENAI_API_KEY from OS keyring (service agent-sidekick)",
                Path::new("/usr/bin/aider").display()
            )
        );

        // Program found, secret missing.
        let r = check_backend_with(&b, &claude, &secret_missing, &no_env, &resolve, &no_probe);
        assert!(!r.reachable);
        assert_eq!(
            r.detail,
            "no secret stored for secret_env OPENAI_API_KEY; run `agent-sidekick setup --backend kic`"
        );

        // Storage error.
        let r = check_backend_with(&b, &claude, &secret_broken, &no_env, &resolve, &no_probe);
        assert!(!r.reachable);
        assert!(
            r.detail.contains("cannot read the secret of backend `kic`"),
            "{r:?}"
        );
        assert!(r.detail.contains("keyring locked"), "{r:?}");

        // The program check comes first; the store is not consulted then.
        let mut missing_prog = command_with_secret("OPENAI_API_KEY");
        if let BackendKind::Command { command, .. } = &mut missing_prog.kind {
            command[0] = "nope".into();
        }
        let no_secret = |_: &SecretId| -> Result<String, TokenError> { panic!("no secret") };
        let r = check_backend_with(
            &missing_prog,
            &claude,
            &no_secret,
            &no_env,
            &resolve,
            &no_probe,
        );
        assert!(!r.reachable);
        assert_eq!(r.detail, "program `nope` not found in PATH");

        // The environment variable itself is never consulted.
        let env = |_: &str| -> Option<String> { panic!("environment must not be read") };
        let r = check_backend_with(&b, &claude, &secret_missing, &env, &resolve, &no_probe);
        assert!(!r.reachable);
    }
}
