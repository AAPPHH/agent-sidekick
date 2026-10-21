//! The `codex` harness: the OpenAI Codex CLI (`@openai/codex`), run as
//! `codex exec --json --ephemeral --skip-git-repo-check --color never --sandbox <mode>
//! --cd <cwd> [-m <model>] [-c windows.sandbox=<impl>] -` with the prompt on stdin.
//!
//! Isolation (see NOTES.md, "Codex (host and harness)"): every backend has its own
//! `CODEX_HOME` = `<config-dir>/codex/<name>/` (also `CODEX_SQLITE_HOME`), so the user's own
//! `~/.codex` (their ChatGPT login, config, MCP servers, sessions) is never read or written.
//! `HOME`/`USERPROFILE` stay the user's: the agent's shell commands need the real profile,
//! and Codex keeps its state in `CODEX_HOME`. Before every run the adapter (re)writes
//! `<CODEX_HOME>/config.toml` (only when it changed; it holds no secret and never
//! `projects.*` or `mcp_servers.*`):
//! - `cli_auth_credentials_store = "file"`, so `codex login` (ChatGPT mode) stores the login
//!   in `<CODEX_HOME>/auth.json` (existence check without reading it; never the OS keyring);
//! - no update check, no history file, no analytics;
//! - `shell_environment_policy`: Codex's default exclusion of `*KEY*`/`*SECRET*`/`*TOKEN*`
//!   names switched on, plus explicit exclusions of `AGENT_SIDEKICK_*`, `CODEX_API_KEY`,
//!   `CODEX_ACCESS_TOKEN`, `OPENAI_API_KEY`, `ANTHROPIC_*` and `CLAUDE*`. Without it, every
//!   shell command the model runs would see the API key (verified with codex 0.163.0: the
//!   default passes `CODEX_API_KEY`, `OPENAI_API_KEY` and a custom provider's key through);
//! - api-key with `base_url`: a custom provider `sidekick` (`wire_api = "responses"`,
//!   `env_key = "AGENT_SIDEKICK_PROVIDER_KEY"`).
//!
//! The key never appears in a file or argument: `CODEX_API_KEY` (built-in `openai` provider)
//! or `AGENT_SIDEKICK_PROVIDER_KEY` (custom provider) exists only in the child's environment.
//! `codex login --with-api-key` is never used (it would write the key into `auth.json`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{
    batch_safe, create_private_dir, write_if_changed, wrong_harness, CheckDeps, Harness,
    HarnessKind, LaunchInput, Normalized, SecretLoader, VERSION_ENV_KEEP, VERSION_TIMEOUT,
};
use crate::backends::launch::{child_env, eq_name, set_var, PROVIDER_KEY_ENV};
use crate::backends::reach::{ProbeResponse, PROBE_TIMEOUT};
use crate::backends::{Backend, BackendKind, CodexAuth, Credential, Reachability};
use crate::redact::Secrets;
use crate::runner::Launch;
use crate::token::{SecretId, Token, TokenError};

/// Official install commands (npm package `@openai/codex`; standalone installers from the
/// Codex docs). Printed only, never run.
pub const INSTALL_HINT: &str = "\
npm (all platforms):   npm install -g @openai/codex
macOS, Linux:          curl -fsSL https://chatgpt.com/codex/install.sh | sh
Windows (PowerShell):  irm https://chatgpt.com/codex/install.ps1 | iex
(see https://www.npmjs.com/package/@openai/codex)";

/// Provider id of the custom provider (api-key with `base_url`) in the generated config.
pub const PROVIDER_ID: &str = "sidekick";
/// OpenAI's API, probed for an api-key backend without `base_url`.
pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
/// Name of the generated config file in `CODEX_HOME`.
pub const CONFIG_FILE: &str = "config.toml";
/// Where `codex login` (with `cli_auth_credentials_store = "file"`) keeps the ChatGPT login.
pub const AUTH_FILE: &str = "auth.json";

/// npm platform packages of `@openai/codex` on Windows: (package, target triple). The
/// native executable is
/// `<npm prefix>/node_modules/@openai/codex/node_modules/@openai/<package>/vendor/<triple>/bin/codex.exe`.
const WINDOWS_PACKAGES: [(&str, &str); 2] = if cfg!(target_arch = "aarch64") {
    [
        ("codex-win32-arm64", "aarch64-pc-windows-msvc"),
        ("codex-win32-x64", "x86_64-pc-windows-msvc"),
    ]
} else {
    [
        ("codex-win32-x64", "x86_64-pc-windows-msvc"),
        ("codex-win32-arm64", "aarch64-pc-windows-msvc"),
    ]
};

/// Adapter of the `codex` harness.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodexHarness;

/// The native `codex.exe` of the npm package below the npm prefix `prefix` (Windows).
fn npm_native_exe(prefix: &Path, package: &str, triple: &str) -> PathBuf {
    prefix
        .join("node_modules")
        .join("@openai")
        .join("codex")
        .join("node_modules")
        .join("@openai")
        .join(package)
        .join("vendor")
        .join(triple)
        .join("bin")
        .join("codex.exe")
}

/// Where the Codex CLI is, in this order:
/// 0. `harness_path` of the backend (must exist);
/// 1. Windows: the native executable of the npm package (see [`npm_native_exe`]) below the
///    directory of a `codex.cmd` shim found in PATH, then below `%APPDATA%\npm`; then the
///    standalone installer's `%LOCALAPPDATA%\Programs\OpenAI\Codex\bin\codex.exe`; then
///    `codex.exe` in an absolute PATH entry. A `.cmd`/`.ps1` shim is never returned.
/// 2. Unix: `codex` in an absolute PATH entry (the npm bin link or a native install).
///
/// `env` reads the (inherited) environment, `is_file` the filesystem; injected for tests.
pub fn find_codex(
    harness_path: Option<&Path>,
    env: &dyn Fn(&str) -> Option<OsString>,
    is_file: &dyn Fn(&Path) -> bool,
) -> Result<PathBuf, String> {
    if let Some(p) = harness_path {
        return if is_file(p) {
            Ok(p.to_path_buf())
        } else {
            Err(format!(
                "harness_path {} does not exist or is not a file",
                p.display()
            ))
        };
    }
    let path_dirs: Vec<PathBuf> = env("PATH")
        .map(|v| {
            std::env::split_paths(&v)
                .filter(|d| !d.as_os_str().is_empty() && d.is_absolute())
                .collect()
        })
        .unwrap_or_default();
    let mut candidates: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        let mut prefixes: Vec<PathBuf> = path_dirs
            .iter()
            .filter(|d| is_file(&d.join("codex.cmd")))
            .cloned()
            .collect();
        if let Some(appdata) = env("APPDATA").filter(|v| !v.is_empty()) {
            prefixes.push(PathBuf::from(appdata).join("npm"));
        }
        for prefix in &prefixes {
            for (package, triple) in WINDOWS_PACKAGES {
                candidates.push(npm_native_exe(prefix, package, triple));
            }
        }
        if let Some(local) = env("LOCALAPPDATA").filter(|v| !v.is_empty()) {
            candidates.push(
                PathBuf::from(local)
                    .join("Programs")
                    .join("OpenAI")
                    .join("Codex")
                    .join("bin")
                    .join("codex.exe"),
            );
        }
        candidates.extend(path_dirs.iter().map(|d| d.join("codex.exe")));
    } else {
        candidates.extend(path_dirs.iter().map(|d| d.join("codex")));
    }
    candidates.into_iter().find(|c| is_file(c)).ok_or_else(|| {
        "codex not found (npm global package, standalone installer, PATH; `.cmd` shims are \
         not used); `agent-sidekick harness install codex` shows how to install it, or set \
         `harness_path` in the backend"
            .to_owned()
    })
}

/// [`find_codex`] on the real environment; on Unix the file must be executable.
pub fn locate_real(harness_path: Option<&Path>) -> Result<PathBuf, String> {
    let path = find_codex(harness_path, &|k| std::env::var_os(k), &|p| p.is_file())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let exec = std::fs::metadata(&path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0);
        if !exec {
            return Err(format!("{} is not executable (chmod +x)", path.display()));
        }
    }
    Ok(path)
}

/// `<program> --version` in a minimal environment (of `inherited`, only
/// [`VERSION_ENV_KEEP`]) whose `CODEX_HOME` (and `HOME`/`USERPROFILE`) is the fresh
/// directory `scratch`, which is removed afterwards: codex 0.163.0 creates `tmp/` in
/// `CODEX_HOME` even for `--version`, and the user's real `~/.codex` must not be touched.
pub(crate) fn version_in(
    program: &Path,
    scratch: &Path,
    inherited: impl Iterator<Item = (OsString, OsString)>,
) -> Result<String, String> {
    std::fs::create_dir_all(scratch).map_err(|e| format!("creating {}: {e}", scratch.display()))?;
    let mut env: Vec<(OsString, OsString)> = inherited
        .filter(|(k, _)| VERSION_ENV_KEEP.iter().any(|n| eq_name(k, n)))
        .collect();
    for var in ["CODEX_HOME", "HOME", "USERPROFILE"] {
        set_var(&mut env, var, scratch.as_os_str());
    }
    let out = crate::commands::claude_cli::run_in_env(
        program,
        ["--version"],
        Some(&env),
        VERSION_TIMEOUT,
    );
    let _ = std::fs::remove_dir_all(scratch);
    let out = out.map_err(|e| format!("{e:#}"))?;
    if !out.success() {
        return Err(format!("`--version` failed: {}", out.diagnostics()));
    }
    let text = crate::redact::redact_all(out.stdout.trim());
    Ok(text.lines().next().unwrap_or_default().trim().to_owned())
}

/// A TOML basic string for `s` (quoted and escaped).
fn toml_str(s: &str) -> String {
    toml::Value::String(s.to_owned()).to_string()
}

/// The generated `<CODEX_HOME>/config.toml` of a codex backend. Depends only on the backend's
/// configuration (never on the request) and contains no secret, so concurrent runs share it.
pub fn render_config(backend: &Backend) -> Result<String, String> {
    let BackendKind::Codex { auth, base_url, .. } = &backend.kind else {
        return Err(wrong_harness(&CodexHarness, backend));
    };
    let mut out = format!(
        "# Generated by agent-sidekick for backend `{}` before every run; changes are \
         overwritten.\n\
         # No secrets: an API key reaches Codex only through the environment. A ChatGPT login\n\
         # (`codex login` with CODEX_HOME set to this directory) is kept next to this file in\n\
         # auth.json.\n\
         cli_auth_credentials_store = \"file\"\n\
         check_for_update_on_startup = false\n",
        backend.name
    );
    let custom = match (auth, base_url) {
        (CodexAuth::ApiKey, Some(url)) => Some(url),
        _ => None,
    };
    if custom.is_some() {
        out.push_str(&format!("model_provider = {}\n", toml_str(PROVIDER_ID)));
    }
    out.push_str(
        "\n[history]\npersistence = \"none\"\n\n\
         [analytics]\nenabled = false\n\n\
         # Shell commands of the agent never see keys or agent-sidekick's variables.\n\
         [shell_environment_policy]\nignore_default_excludes = false\n\n\
         [shell_environment_policy.filters]\n",
    );
    for pattern in SHELL_EXCLUDES {
        out.push_str(&format!("{} = \"exclude\"\n", toml_str(pattern)));
    }
    if let Some(url) = custom {
        out.push_str(&format!(
            "\n[model_providers.{PROVIDER_ID}]\nname = {}\nbase_url = {}\nenv_key = {}\n\
             wire_api = \"responses\"\n",
            toml_str(&format!("agent-sidekick {}", backend.name)),
            toml_str(url),
            toml_str(PROVIDER_KEY_ENV),
        ));
    }
    Ok(out)
}

/// Variable patterns removed from the environment of the agent's shell commands (on top of
/// Codex's default `*KEY*`/`*SECRET*`/`*TOKEN*` exclusion, which the config switches on).
pub const SHELL_EXCLUDES: &[&str] = &[
    "AGENT_SIDEKICK_*",
    "CODEX_API_KEY",
    "CODEX_ACCESS_TOKEN",
    "OPENAI_API_KEY",
    "ANTHROPIC_*",
    "CLAUDE*",
];

/// Write the backend's `CODEX_HOME` and its config.toml (created when missing, rewritten only
/// when it changed). Returns the config file's path. Also used by `setup --backend`, so a
/// ChatGPT login already sees the file credential store.
pub fn prepare_home(backend: &Backend) -> Result<PathBuf, String> {
    let BackendKind::Codex { base_dir, .. } = &backend.kind else {
        return Err(wrong_harness(&CodexHarness, backend));
    };
    let name = &backend.name;
    create_private_dir(base_dir)
        .map_err(|e| format!("backend {name}: creating {}: {e}", base_dir.display()))?;
    let text = render_config(backend)?;
    let file = base_dir.join(CONFIG_FILE);
    write_if_changed(&file, &text)
        .map_err(|e| format!("backend {name}: writing {}: {e}", file.display()))?;
    Ok(file)
}

/// The ChatGPT login file of a codex backend (`<CODEX_HOME>/auth.json`).
pub fn auth_file(backend: &Backend) -> Option<PathBuf> {
    match &backend.kind {
        BackendKind::Codex { base_dir, .. } => Some(base_dir.join(AUTH_FILE)),
        _ => None,
    }
}

/// The exact commands that log a ChatGPT account into this backend's `CODEX_HOME` (one per
/// line, for the platform this binary runs on). `codex` is the user's `codex` command (the
/// npm shim or the standalone install).
pub fn login_commands(home: &Path) -> String {
    let dir = home.display();
    if cfg!(windows) {
        format!(
            "  PowerShell:  $env:CODEX_HOME = \"{dir}\"; codex login; Remove-Item Env:CODEX_HOME\n\
             \x20 Git Bash:    CODEX_HOME=\"{dir}\" codex login\n\
             \x20 (no browser on this machine: `codex login --device-auth` instead of `codex login`)"
        )
    } else {
        format!(
            "  CODEX_HOME=\"{dir}\" codex login\n\
             \x20 (no browser on this machine: `codex login --device-auth` instead of `codex login`)"
        )
    }
}

/// Why a ChatGPT-mode backend cannot run: not logged in (no `auth.json`), with the commands
/// that fix it. `None`: logged in (the file exists; its content is never read).
pub fn login_problem(backend: &Backend, is_file: &dyn Fn(&Path) -> bool) -> Option<String> {
    let BackendKind::Codex {
        auth: CodexAuth::Chatgpt,
        base_dir,
        ..
    } = &backend.kind
    else {
        return None;
    };
    let file = base_dir.join(AUTH_FILE);
    (!is_file(&file)).then(|| {
        format!(
            "No ChatGPT login for backend '{}' ({} is missing). Run \
             'agent-sidekick setup --backend {}' in a terminal and follow the login steps \
             (`codex login` with CODEX_HOME={}).",
            backend.name,
            file.display(),
            backend.name,
            base_dir.display()
        )
    })
}

/// The provider's model list for the reachability probe: `{base_url}/models`, or OpenAI's
/// own for an api-key backend without `base_url`. `None`: ChatGPT mode (no probe; Codex
/// talks to ChatGPT's backend with the login) or another harness.
pub fn probe_url(backend: &Backend) -> Option<String> {
    match &backend.kind {
        BackendKind::Codex {
            auth: CodexAuth::ApiKey,
            base_url,
            ..
        } => Some(format!(
            "{}/models",
            base_url.as_deref().unwrap_or(OPENAI_BASE_URL)
        )),
        _ => None,
    }
}

/// `GET` [`probe_url`] with the key as bearer token: any HTTP answer except 401/403 counts
/// as reachable; 401/403 means the key was rejected. The key never appears in the detail.
pub fn provider_probe(
    backend: &Backend,
    key: Option<&str>,
    probe_get: &dyn Fn(&str, Option<&str>) -> ProbeResponse,
) -> Reachability {
    let Some(url) = probe_url(backend) else {
        return Reachability::ok("ChatGPT login (no provider probe)");
    };
    match probe_get(&url, key) {
        ProbeResponse::Status { code, url, millis } if code == 401 || code == 403 => {
            Reachability::unreachable(format!(
                "the provider rejected the key: HTTP {code} from {url} in {millis} ms"
            ))
        }
        ProbeResponse::Status { code, url, millis } => {
            Reachability::ok(format!("HTTP {code} from {url} in {millis} ms"))
        }
        ProbeResponse::Failed(why) => Reachability::unreachable(format!("unreachable: {why}")),
    }
}

/// Characters a path argument may contain when the program is a `.cmd`/`.bat` wrapper.
fn batch_safe_path(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:@+\\ ".contains(c))
}

impl Harness for CodexHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Codex
    }

    fn locate(&self, backend: &Backend, _claude_path: Option<&Path>) -> Result<PathBuf, String> {
        let BackendKind::Codex { harness_path, .. } = &backend.kind else {
            return Err(wrong_harness(self, backend));
        };
        locate_real(harness_path.as_deref())
    }

    fn locate_installed(&self, _claude_path: Option<&Path>) -> Option<Result<PathBuf, String>> {
        Some(locate_real(None))
    }

    fn version(&self, program: &Path) -> Result<String, String> {
        let scratch = std::env::temp_dir().join(format!(
            "agent-sidekick-codex-version-{}",
            uuid::Uuid::new_v4().simple()
        ));
        version_in(program, &scratch, std::env::vars_os())
    }

    /// chatgpt: no secret (Codex reads its own login), but `auth.json` must exist; api-key:
    /// the key from `api_key_env` or the keyring user `backend-<name>`.
    fn load_credential(
        &self,
        backend: &Backend,
        env_lookup: &dyn Fn(&str) -> Option<String>,
        secret_loader: SecretLoader<'_>,
    ) -> Result<(Credential, String), String> {
        let name = &backend.name;
        let BackendKind::Codex {
            auth, api_key_env, ..
        } = &backend.kind
        else {
            return Err(wrong_harness(self, backend));
        };
        if *auth == CodexAuth::Chatgpt {
            if let Some(problem) = login_problem(backend, &|p| p.is_file()) {
                return Err(problem);
            }
            let file = auth_file(backend).unwrap_or_default();
            return Ok((
                Credential::None,
                format!("ChatGPT login in {}", file.display()),
            ));
        }
        if let Some(var) = api_key_env {
            return match env_lookup(var).map(Token::new) {
                Some(t) if !t.expose().is_empty() => Ok((
                    Credential::ProviderKey(t),
                    format!("environment variable {var}"),
                )),
                _ => Err(format!(
                    "Environment variable {var} (api_key_env of backend '{name}') is not set in \
                     the MCP server's environment; set it there or remove api_key_env and store \
                     the key with 'agent-sidekick setup --backend {name}'"
                )),
            };
        }
        let id = SecretId::Backend(name.clone());
        match secret_loader(&id) {
            Ok((key, source)) => Ok((Credential::ProviderKey(key), source.to_string())),
            Err(TokenError::Missing) | Err(TokenError::Empty) => Err(format!(
                "No API key stored for backend '{name}'. Run \
                 'agent-sidekick setup --backend {name}' in a terminal."
            )),
            Err(TokenError::Invalid) => Err(format!(
                "The stored {id} is invalid (it contains line breaks or control characters). \
                 Run 'agent-sidekick setup --backend {name}' to store it again."
            )),
            Err(TokenError::Storage(e)) => Err(format!(
                "Cannot read the {id}: {e}. Run 'agent-sidekick doctor' or \
                 'agent-sidekick setup --backend {name}' in a terminal."
            )),
        }
    }

    /// Writes the backend's `CODEX_HOME` and config.toml, then builds argv/env/stdin. The
    /// environment starts from the common session isolation ([`child_env`]: Claude and Codex
    /// session variables, the OAuth token, competing keys, other backends' keys, inherited
    /// `OPENAI_*` credentials removed) and gets `CODEX_HOME`/`CODEX_SQLITE_HOME` and the key.
    fn build_launch(&self, input: LaunchInput<'_>) -> Result<Launch, String> {
        let LaunchInput {
            backend,
            req,
            credential,
            program,
            base,
            global_pass_env,
            model,
            mut header,
        } = input;
        let BackendKind::Codex {
            auth,
            base_url,
            read_only,
            windows_sandbox,
            base_dir,
            ..
        } = &backend.kind
        else {
            return Err(wrong_harness(self, backend));
        };
        let name = &backend.name;
        let cwd = req.cwd.to_string_lossy().into_owned();
        if crate::locate::is_batch_script(program) {
            if let Some(m) = model.as_deref().filter(|m| !batch_safe(m)) {
                return Err(format!(
                    "backend {name}: harness_path is a .cmd/.bat script and the model name \
                     {m:?} contains characters cmd.exe would re-parse; point harness_path at \
                     codex.exe"
                ));
            }
            if !batch_safe_path(&cwd) {
                return Err(format!(
                    "backend {name}: harness_path is a .cmd/.bat script and the working \
                     directory contains characters cmd.exe would re-parse; point harness_path \
                     at codex.exe"
                ));
            }
        }
        prepare_home(backend)?;

        let env = child_env(backend, base, global_pass_env, credential);
        let sandbox = if *read_only {
            "read-only"
        } else {
            "workspace-write"
        };
        let mut args: Vec<OsString> = [
            "exec",
            "--json",
            "--ephemeral",
            "--skip-git-repo-check",
            "--color",
            "never",
            "--sandbox",
            sandbox,
            "--cd",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        args.push(req.cwd.clone().into_os_string());
        if let Some(m) = &model {
            args.push("-m".into());
            args.push(m.into());
        }
        if cfg!(windows) {
            args.push("-c".into());
            args.push(format!("windows.sandbox={}", windows_sandbox.as_str()).into());
        }
        args.push("-".into());

        header.push(format!("program: {}", program.display()));
        header.push(match (auth, base_url) {
            (CodexAuth::Chatgpt, _) => "auth: chatgpt (login in CODEX_HOME/auth.json)".to_owned(),
            (CodexAuth::ApiKey, Some(u)) => {
                format!("auth: api-key; provider {PROVIDER_ID} ({u}, Responses API)")
            }
            (CodexAuth::ApiKey, None) => "auth: api-key; provider openai".to_owned(),
        });
        header.push(format!("codex home: {}", base_dir.display()));
        header.push("prompt delivery: stdin".to_owned());
        header.push(if cfg!(windows) {
            format!(
                "sandbox: {sandbox} (windows.sandbox={})",
                windows_sandbox.as_str()
            )
        } else {
            format!("sandbox: {sandbox}")
        });
        header.push(format!(
            "max_turns: {} ignored (codex has no turn limit; timeout_s is the hard limit)",
            req.max_turns
        ));
        if model.is_none() {
            header.push("model: codex default".to_owned());
        }
        if req
            .allowed_tools
            .iter()
            .flatten()
            .any(|t| !t.trim().is_empty())
        {
            header.push(
                "note: allowed_tools ignored by the codex harness (use read_only)".to_owned(),
            );
        }
        let secrets = match credential.secret() {
            Some(t) => Secrets::new([t.expose()]),
            None => Secrets::none(),
        };
        Ok(Launch {
            program: program.to_path_buf(),
            args,
            env,
            stdin: Some(req.prompt.clone()),
            secrets,
            header,
        })
    }

    fn normalize_output(&self, raw: &str) -> Option<Normalized> {
        Some(normalize_json_lines(raw))
    }

    /// api-key: the provider answers and accepts the key right before the run, so a dead
    /// endpoint or a revoked key fails fast and a fallback can take over. chatgpt: the login
    /// was checked when the credential was loaded.
    fn pre_run_check(&self, backend: &Backend, credential: &Credential) -> Result<(), String> {
        if probe_url(backend).is_none() {
            return Ok(());
        }
        let r = provider_probe(backend, credential.secret().map(Token::expose), &|u, b| {
            crate::backends::reach::probe_get(u, b, PROBE_TIMEOUT)
        });
        if r.reachable {
            Ok(())
        } else {
            Err(format!("provider check failed: {}", r.detail))
        }
    }

    /// codex installed; chatgpt: logged in (`auth.json` exists); api-key: key available and
    /// the provider answers.
    fn check(&self, backend: &Backend, deps: &CheckDeps<'_>) -> Reachability {
        let BackendKind::Codex {
            auth, api_key_env, ..
        } = &backend.kind
        else {
            return Reachability::unreachable(wrong_harness(self, backend));
        };
        let program = match (deps.locate)(backend) {
            Ok(p) => p,
            Err(e) => return Reachability::unreachable(format!("codex not installed: {e}")),
        };
        if *auth == CodexAuth::Chatgpt {
            return match login_problem(backend, &|p| p.is_file()) {
                Some(_) => Reachability::unreachable(format!(
                    "not logged in: no ChatGPT login in this backend's CODEX_HOME; run \
                     `agent-sidekick setup --backend {}`",
                    backend.name
                )),
                None => Reachability::ok(format!(
                    "codex {}; ChatGPT login present (auth.json)",
                    program.display()
                )),
            };
        }
        let (key, key_detail) = if let Some(var) = api_key_env {
            match (deps.env_lookup)(var).filter(|v| !v.trim().is_empty()) {
                Some(v) => (v, format!("key from env var {var}")),
                None => {
                    return Reachability::unreachable(format!(
                        "env var {var} (api_key_env) is not set"
                    ))
                }
            }
        } else {
            let id = SecretId::Backend(backend.name.clone());
            let source = match (deps.secret)(&id) {
                Ok(source) => source,
                Err(TokenError::Missing) | Err(TokenError::Empty) => {
                    return Reachability::unreachable(format!(
                        "no API key stored; run `agent-sidekick setup --backend {}`",
                        backend.name
                    ))
                }
                Err(e) => return Reachability::unreachable(format!("cannot read the {id}: {e}")),
            };
            match (deps.secret_value)(&id) {
                Ok(t) => (t.expose().to_owned(), format!("key from {source}")),
                Err(e) => return Reachability::unreachable(format!("cannot read the {id}: {e}")),
            }
        };
        let r = provider_probe(backend, Some(&key), deps.probe_get);
        Reachability {
            reachable: r.reachable,
            detail: format!("codex {}; {key_detail}; {}", program.display(), r.detail),
        }
    }

    fn install_hint(&self) -> Option<&'static str> {
        Some(INSTALL_HINT)
    }
}

/// Usage of the turn (`turn.completed.usage`, summed by Codex over all model requests).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Usage {
    input: u64,
    cached: u64,
    output: u64,
    reasoning: u64,
}

/// Activity lines kept for the result (the most recent ones).
const MAX_ACTIVITY_LINES: usize = 12;
/// Warning lines kept for the result.
const MAX_WARNINGS: usize = 5;
/// Plain (non-JSON) output lines kept for the result.
const MAX_PLAIN_LINES: usize = 12;
/// Longest command or message shown in one result line (characters).
const MAX_LINE_CHARS: usize = 200;

/// The stderr warning codex 0.163 prints when `CODEX_HOME` is under the temp directory.
const PATH_ALIAS_WARNING: &str = "could not create PATH aliases";

fn shorten(s: &str) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= MAX_LINE_CHARS {
        one_line
    } else {
        let cut: String = one_line.chars().take(MAX_LINE_CHARS).collect();
        format!("{cut}...")
    }
}

/// Turns `codex exec --json` output (one JSON event per line, mixed with codex's stderr log
/// lines and agent-sidekick's own notes) into the result text:
/// - a compact activity summary (commands with exit code, MCP tool calls, file changes),
///   placed before the answer so the result's tail keeps the answer;
/// - warnings: top-level `error` events and `error` items (codex emits them for transient
///   reconnects and fallbacks in runs that succeed, so they never fail a run);
/// - non-JSON lines (codex's stderr), without the temp-dir PATH alias warning;
/// - the final answer: the `text` of the last completed `agent_message` item;
/// - `[error] turn failed: <message>` for `turn.failed`;
/// - a usage line from the last `turn.completed`.
///
/// No answer at all is a failure even with exit code 0.
pub fn normalize_json_lines(raw: &str) -> Normalized {
    let mut answer: Option<String> = None;
    let mut activity: Vec<String> = Vec::new();
    let mut commands = 0usize;
    let mut tool_calls = 0usize;
    let mut warnings: Vec<String> = Vec::new();
    let mut plain: Vec<String> = Vec::new();
    let mut failed: Option<String> = None;
    let mut usage: Option<Usage> = None;
    let mut saw_event = false;

    let mut first = true;
    for line in raw.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        // The buffer may start mid-line: a first line that is the tail of a JSON event.
        let cut_off = first && line.trim_end().ends_with('}') && line.contains("\":");
        first = false;
        let event = line
            .trim_start()
            .starts_with('{')
            .then(|| serde_json::from_str::<Value>(line.trim()).ok())
            .flatten()
            .filter(|v| v.get("type").and_then(Value::as_str).is_some());
        let Some(ev) = event else {
            if cut_off || line.trim_start().starts_with('{') || line.contains(PATH_ALIAS_WARNING) {
                continue;
            }
            plain.push(line.to_owned());
            continue;
        };
        saw_event = true;
        let item = &ev["item"];
        match ev["type"].as_str().unwrap_or_default() {
            "item.completed" => match item["type"].as_str().unwrap_or_default() {
                "agent_message" => {
                    if let Some(t) = item["text"].as_str() {
                        if !t.trim().is_empty() {
                            answer = Some(t.trim().to_owned());
                        }
                    }
                }
                "command_execution" => {
                    commands += 1;
                    let cmd = shorten(item["command"].as_str().unwrap_or("?"));
                    let status = item["status"].as_str().unwrap_or("?");
                    activity.push(match item["exit_code"].as_i64() {
                        Some(code) => format!("[command] {cmd} -> exit {code} ({status})"),
                        None => format!("[command] {cmd} ({status})"),
                    });
                }
                "mcp_tool_call" => {
                    tool_calls += 1;
                    let server = item["server"].as_str().unwrap_or("?");
                    let tool = item["tool"].as_str().unwrap_or("?");
                    let status = item["status"].as_str().unwrap_or("?");
                    let mut l = format!("[tool] {server}/{tool} ({status})");
                    if let Some(e) = item["error"]["message"].as_str() {
                        l.push_str(&format!(": {}", shorten(e)));
                    }
                    activity.push(l);
                }
                "file_change" => {
                    let paths: Vec<&str> = item["changes"]
                        .as_array()
                        .map(|a| a.iter().filter_map(|c| c["path"].as_str()).collect())
                        .unwrap_or_default();
                    activity.push(if paths.is_empty() {
                        "[files] changed".to_owned()
                    } else {
                        format!("[files] {}", shorten(&paths.join(", ")))
                    });
                }
                "error" => {
                    if let Some(m) = item["message"].as_str() {
                        warnings.push(shorten(m));
                    }
                }
                _ => {}
            },
            "error" => {
                if let Some(m) = ev["message"].as_str() {
                    warnings.push(shorten(m));
                }
            }
            "turn.failed" => {
                failed = Some(
                    ev["error"]["message"]
                        .as_str()
                        .unwrap_or("(no message)")
                        .to_owned(),
                );
            }
            "turn.completed" => {
                let u = &ev["usage"];
                usage = Some(Usage {
                    input: u["input_tokens"].as_u64().unwrap_or(0),
                    cached: u["cached_input_tokens"].as_u64().unwrap_or(0),
                    output: u["output_tokens"].as_u64().unwrap_or(0),
                    reasoning: u["reasoning_output_tokens"].as_u64().unwrap_or(0),
                });
            }
            _ => {}
        }
    }

    let mut out = String::new();
    let mut push = |s: &str| {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(s);
    };
    if commands + tool_calls > 0 {
        push(&format!(
            "[codex activity] {commands} command(s), {tool_calls} MCP tool call(s){}",
            if activity.len() > MAX_ACTIVITY_LINES {
                format!(" (last {MAX_ACTIVITY_LINES} shown)")
            } else {
                String::new()
            }
        ));
    }
    let skip = activity.len().saturating_sub(MAX_ACTIVITY_LINES);
    for l in &activity[skip..] {
        push(l);
    }
    warnings.dedup();
    for w in warnings.iter().take(MAX_WARNINGS) {
        push(&format!("[warning] {w}"));
    }
    if !plain.is_empty() {
        push("[codex output]");
        let skip = plain.len().saturating_sub(MAX_PLAIN_LINES);
        for l in &plain[skip..] {
            push(l);
        }
    }
    if let Some(a) = &answer {
        push(a);
    }
    if let Some(f) = &failed {
        push(&format!("[error] turn failed: {f}"));
    }
    if let Some(u) = &usage {
        push(&format!(
            "[usage] tokens in {} (cached {}) / out {} (reasoning {})",
            u.input, u.cached, u.output, u.reasoning
        ));
    }
    let failure = match (&answer, &failed) {
        (_, Some(f)) => Some(format!("turn failed: {}", shorten(f))),
        (Some(_), None) => None,
        (None, None) if saw_event => Some("no answer: codex produced no final message".to_owned()),
        (None, None) => Some("no answer: no codex JSON output".to_owned()),
    };
    Normalized { text: out, failure }
}

#[cfg(test)]
mod tests;
