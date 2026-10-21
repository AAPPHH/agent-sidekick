//! The `opencode` harness: the OpenCode CLI (<https://opencode.ai>), run as
//! `opencode run --format json --model <provider>/<model> --title agent-sidekick` with the
//! prompt on stdin.
//!
//! Isolation (see NOTES.md, "Harness adapters (0.3.0)"): every backend gets its own directory
//! `<config-dir>/opencode/<name>/` with `home`, `config`, `data`, `state`, `cache` and `tmp`.
//! `HOME`/`USERPROFILE` (and on Windows `HOMEDRIVE`/`HOMEPATH`, `APPDATA`/`LOCALAPPDATA`),
//! the four `XDG_*_HOME` variables, `TEMP`/`TMP`/`TMPDIR` and `npm_config_cache` point there,
//! so OpenCode reads no user config (it always loads `<home>/.opencode`, whatever the flags
//! say) and writes nothing outside that directory. The generated `config/opencode/opencode.json` depends only on the
//! backend's configuration (provider, models, permissions), so concurrent runs of one backend
//! share it safely; per-run values (`max_turns` as `agent.build.steps`) go through
//! `OPENCODE_CONFIG_CONTENT`. The permission set is also passed as `OPENCODE_PERMISSION`,
//! which OpenCode merges after every other config layer, so no layer can reintroduce "ask"
//! (run mode answers "ask" with an automatic reject, which silently ends the run).
//!
//! The provider key never appears in a file or argument: the config references it as
//! `{env:AGENT_SIDEKICK_PROVIDER_KEY}` and only the child's environment holds it.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use super::{
    batch_safe, create_private_dir, write_if_changed, wrong_harness, CheckDeps, Harness,
    HarnessKind, LaunchInput, Normalized, SecretLoader, VERSION_ENV_KEEP, VERSION_TIMEOUT,
};
use crate::backends::launch::{child_env, eq_name, set_var, PROVIDER_KEY_ENV};
use crate::backends::reach::{ProbeResponse, PROBE_TIMEOUT};
use crate::backends::{Backend, BackendKind, Credential, OpencodeProvider, Reachability};
use crate::redact::Secrets;
use crate::runner::Launch;
use crate::token::{SecretId, Token, TokenError};

/// Official install commands, from opencode.ai/docs (checked 2026-10-21). The npm package
/// installs a real executable (`node_modules/opencode-ai/bin/opencode.exe`); agent-sidekick
/// runs that file, never the `opencode.cmd` shim.
const INSTALL_HINT: &str = "\
npm (all platforms):   npm install -g opencode-ai
Windows (Scoop):       scoop install opencode
Windows (Chocolatey):  choco install opencode
macOS, Linux:          curl -fsSL https://opencode.ai/install | bash
(see https://opencode.ai/docs/)";

/// Provider id in the generated config for openai-compatible and ollama backends.
pub const PROVIDER_ID: &str = "sidekick";
/// Session title passed with `--title` (skips OpenCode's extra title-generation request).
pub const SESSION_TITLE: &str = "agent-sidekick";
/// OpenRouter's model list (public, no key), for the reachability probe.
pub const OPENROUTER_MODELS_URL: &str = "https://openrouter.ai/api/v1/models";

/// Variables set to `1` for every opencode child.
pub const DISABLE_FLAGS: &[&str] = &[
    "OPENCODE_DISABLE_AUTOUPDATE",
    "OPENCODE_DISABLE_PROJECT_CONFIG",
    "OPENCODE_DISABLE_EXTERNAL_SKILLS",
    "OPENCODE_DISABLE_CLAUDE_CODE",
    "OPENCODE_DISABLE_LSP_DOWNLOAD",
    "OPENCODE_DISABLE_MODELS_FETCH",
];

/// Adapter of the `opencode` harness.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpencodeHarness;

/// The per-backend isolation directories below `<config-dir>/opencode/<name>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    pub base: PathBuf,
    /// `HOME`/`USERPROFILE` of the child (empty; OpenCode reads `<home>/.opencode`).
    pub home: PathBuf,
    pub config: PathBuf,
    pub data: PathBuf,
    pub state: PathBuf,
    pub cache: PathBuf,
    pub tmp: PathBuf,
}

impl Dirs {
    pub fn new(base: &Path) -> Self {
        Self {
            base: base.to_path_buf(),
            home: base.join("home"),
            config: base.join("config"),
            data: base.join("data"),
            state: base.join("state"),
            cache: base.join("cache"),
            tmp: base.join("tmp"),
        }
    }

    /// The generated config (`$XDG_CONFIG_HOME/opencode/opencode.json`).
    pub fn config_file(&self) -> PathBuf {
        self.config.join("opencode").join("opencode.json")
    }

    fn npm_cache(&self) -> PathBuf {
        self.cache.join("npm")
    }

    fn appdata(&self) -> PathBuf {
        self.home.join("AppData").join("Roaming")
    }

    fn localappdata(&self) -> PathBuf {
        self.home.join("AppData").join("Local")
    }

    /// Every directory the child expects to exist.
    fn all(&self) -> Vec<PathBuf> {
        let mut dirs = vec![
            self.home.clone(),
            self.config.join("opencode"),
            self.data.clone(),
            self.state.clone(),
            self.cache.clone(),
            self.npm_cache(),
            self.tmp.clone(),
        ];
        if cfg!(windows) {
            dirs.push(self.appdata());
            dirs.push(self.localappdata());
        }
        dirs
    }

    /// Create the directories (Unix: the base dir 0700, it holds sessions and logs).
    pub fn create(&self) -> std::io::Result<()> {
        create_private_dir(&self.base)?;
        for d in self.all() {
            std::fs::create_dir_all(&d)?;
        }
        Ok(())
    }

    /// The isolation variables (no secrets): homes, XDG dirs, temp, npm cache.
    pub fn env_vars(&self) -> Vec<(&'static str, OsString)> {
        let mut vars: Vec<(&'static str, OsString)> = vec![("HOME", self.home.clone().into())];
        if cfg!(windows) {
            vars.push(("USERPROFILE", self.home.clone().into()));
            vars.push(("APPDATA", self.appdata().into()));
            vars.push(("LOCALAPPDATA", self.localappdata().into()));
            // Some tools derive the home from HOMEDRIVE + HOMEPATH instead of USERPROFILE.
            if let Some((drive, path)) = split_drive(&self.home) {
                vars.push(("HOMEDRIVE", drive));
                vars.push(("HOMEPATH", path));
            }
        }
        vars.extend([
            ("XDG_CONFIG_HOME", self.config.clone().into()),
            ("XDG_DATA_HOME", self.data.clone().into()),
            ("XDG_STATE_HOME", self.state.clone().into()),
            ("XDG_CACHE_HOME", self.cache.clone().into()),
            ("TEMP", self.tmp.clone().into()),
            ("TMP", self.tmp.clone().into()),
            ("TMPDIR", self.tmp.clone().into()),
            ("npm_config_cache", self.npm_cache().into()),
        ]);
        vars
    }
}

/// Windows: split an absolute path into `HOMEDRIVE` (`C:` or `\\server\share`) and `HOMEPATH`
/// (`\rest`). `None` for other prefixes (device paths) or a path without prefix.
fn split_drive(path: &Path) -> Option<(OsString, OsString)> {
    use std::path::{Component, Prefix};
    let mut comps = path.components();
    let Some(Component::Prefix(p)) = comps.next() else {
        return None;
    };
    let drive: OsString = match p.kind() {
        Prefix::Disk(d) | Prefix::VerbatimDisk(d) => format!("{}:", d as char).into(),
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
            let mut s = OsString::from("\\\\");
            s.push(server);
            s.push("\\");
            s.push(share);
            s
        }
        _ => return None,
    };
    let mut rest = OsString::new();
    for c in comps {
        match c {
            Component::RootDir | Component::CurDir => {}
            other => {
                rest.push("\\");
                rest.push(other.as_os_str());
            }
        }
    }
    if rest.is_empty() {
        rest.push("\\");
    }
    Some((drive, rest))
}

/// The OpenCode permission set: never "ask" (run mode rejects every ask automatically and
/// ends the run without an answer). `read_only`: edit, bash and webfetch denied (OpenCode
/// then does not offer those tools at all).
pub fn permissions(read_only: bool) -> Value {
    let rw = if read_only { "deny" } else { "allow" };
    // serde_json sorts object keys; the `read` patterns sort into the intended order
    // ("last matching rule wins": `*`, then the `.env` denials, then the example allowance).
    json!({
        "read": {
            "*": "allow",
            "*.env": "deny",
            "*.env.*": "deny",
            "*.env.example": "allow"
        },
        "edit": rw,
        "bash": rw,
        "webfetch": rw,
        "glob": "allow",
        "grep": "allow",
        "list": "allow",
        "websearch": "deny",
        "task": "deny",
        "external_directory": "deny",
        "doom_loop": "deny",
        "question": "deny"
    })
}

/// The provider id a model is addressed with (`<id>/<model>`).
pub fn provider_id(provider: OpencodeProvider) -> &'static str {
    match provider {
        OpencodeProvider::Openrouter => "openrouter",
        OpencodeProvider::OpenaiCompatible | OpencodeProvider::Ollama => PROVIDER_ID,
    }
}

/// The generated opencode.json of a backend. Depends only on the backend's configuration
/// (never on the request), contains no secret (the key is `{env:AGENT_SIDEKICK_PROVIDER_KEY}`).
pub fn render_config(backend: &Backend) -> Result<Value, String> {
    let BackendKind::OpenCode {
        provider,
        base_url,
        read_only,
        request_timeout_s,
        ..
    } = &backend.kind
    else {
        return Err(wrong_harness(&OpencodeHarness, backend));
    };
    let Some(default_model) = backend.model.as_deref() else {
        return Err(format!(
            "backend {}: internal error: opencode backend without a default model",
            backend.name
        ));
    };
    let models: Vec<&str> = match &backend.models {
        Some(list) => list.iter().map(String::as_str).collect(),
        None => vec![default_model],
    };
    let key_ref = format!("{{env:{PROVIDER_KEY_ENV}}}");
    let pid = provider_id(*provider);
    let provider_block = match provider {
        OpencodeProvider::OpenaiCompatible | OpencodeProvider::Ollama => {
            let base = base_url.as_deref().unwrap_or_default();
            let (base_url, api_key) = match provider {
                OpencodeProvider::Ollama => (format!("{base}/v1"), "ollama".to_owned()),
                _ => (base.to_owned(), key_ref),
            };
            let mut model_map = Map::new();
            for m in &models {
                model_map.insert((*m).to_owned(), json!({"name": m, "tool_call": true}));
            }
            json!({
                "npm": "@ai-sdk/openai-compatible",
                "name": format!("agent-sidekick {}", backend.name),
                "options": provider_options(
                    json!({"baseURL": base_url, "apiKey": api_key}),
                    *request_timeout_s
                ),
                "models": model_map
            })
        }
        OpencodeProvider::Openrouter => {
            let mut model_map = Map::new();
            for m in &models {
                model_map.insert((*m).to_owned(), json!({}));
            }
            json!({
                "options": provider_options(json!({"apiKey": key_ref}), *request_timeout_s),
                "models": model_map
            })
        }
    };
    let qualified = format!("{pid}/{default_model}");
    let mut providers = Map::new();
    providers.insert(pid.to_owned(), provider_block);
    Ok(json!({
        "$schema": "https://opencode.ai/config.json",
        "autoupdate": false,
        "share": "disabled",
        "snapshot": false,
        "lsp": false,
        "formatter": false,
        "plugin": [],
        "enabled_providers": [pid],
        "model": qualified,
        "small_model": qualified,
        "provider": providers,
        "permission": permissions(*read_only)
    }))
}

/// Provider options plus `headerTimeout` when the backend sets `request_timeout_s`. Never
/// OpenCode's `timeout`: that is a total request limit without retry.
fn provider_options(mut options: Value, request_timeout_s: Option<u64>) -> Value {
    if let Some(t) = request_timeout_s {
        options["headerTimeout"] = json!(t.saturating_mul(1000));
    }
    options
}

/// Per-run config layer (`OPENCODE_CONFIG_CONTENT`): `max_turns` as the build agent's step
/// limit. A soft cap: OpenCode asks the model to stop, it does not cut tools off; the
/// runner's timeout is the hard limit.
pub fn run_config(max_turns: u32) -> Value {
    json!({"agent": {"build": {"steps": max_turns}}})
}

/// Where opencode is looked for, in order:
/// 1. `harness_path` of the backend (must exist);
/// 2. the npm package's real executable `<npm prefix>/node_modules/opencode-ai/bin/opencode.exe`
///    (Windows: the directory of an `opencode.cmd` shim found in PATH, then
///    `%APPDATA%\npm`; Unix: `<prefix>/lib/node_modules/...` for a PATH entry `<prefix>/bin`);
/// 3. `opencode` (Windows: `opencode.exe` only, never a `.cmd`/`.ps1` shim) in an absolute
///    PATH entry;
/// 4. Unix: `~/.opencode/bin/opencode` (the curl installer's location).
///
/// `env` reads the (inherited) environment, `is_file` the filesystem; injected for tests.
pub fn find_opencode(
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
    let npm_exe = |prefix: &Path| {
        prefix
            .join("node_modules")
            .join("opencode-ai")
            .join("bin")
            .join("opencode.exe")
    };
    let mut candidates: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        for d in &path_dirs {
            if is_file(&d.join("opencode.cmd")) {
                candidates.push(npm_exe(d));
            }
        }
        if let Some(appdata) = env("APPDATA").filter(|v| !v.is_empty()) {
            candidates.push(npm_exe(&PathBuf::from(appdata).join("npm")));
        }
        candidates.extend(path_dirs.iter().map(|d| d.join("opencode.exe")));
    } else {
        for d in &path_dirs {
            if d.file_name() == Some(OsStr::new("bin")) {
                if let Some(prefix) = d.parent() {
                    candidates.push(npm_exe(&prefix.join("lib")));
                }
            }
        }
        candidates.extend(path_dirs.iter().map(|d| d.join("opencode")));
        if let Some(home) = env("HOME").filter(|v| !v.is_empty()) {
            candidates.push(
                PathBuf::from(home)
                    .join(".opencode")
                    .join("bin")
                    .join("opencode"),
            );
        }
    }
    candidates.into_iter().find(|c| is_file(c)).ok_or_else(|| {
        "opencode not found (npm global package, PATH; `.cmd` shims are not used); install it \
         (`agent-sidekick harness install opencode` shows how) or set `harness_path` in the \
         backend"
            .to_owned()
    })
}

/// [`find_opencode`] on the real environment; on Unix the file must be executable.
fn locate_real(harness_path: Option<&Path>) -> Result<PathBuf, String> {
    let path = find_opencode(harness_path, &|k| std::env::var_os(k), &|p| p.is_file())?;
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

/// Windows: the Git Bash OpenCode's bash tool should use, as `OPENCODE_GIT_BASH_PATH`.
/// From (in order) an inherited `OPENCODE_GIT_BASH_PATH` or `CLAUDE_CODE_GIT_BASH_PATH`, or
/// `<git>\..\bin\bash.exe` next to a `git.exe` in PATH. `None`: none found (the bash tool then
/// falls back to cmd.exe).
pub fn git_bash(
    env: &dyn Fn(&str) -> Option<OsString>,
    is_file: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    for var in ["OPENCODE_GIT_BASH_PATH", "CLAUDE_CODE_GIT_BASH_PATH"] {
        if let Some(p) = env(var).filter(|v| !v.is_empty()).map(PathBuf::from) {
            if is_file(&p) {
                return Some(p);
            }
        }
    }
    let path = env("PATH")?;
    std::env::split_paths(&path)
        .filter(|d| d.is_absolute())
        .filter(|d| is_file(&d.join("git.exe")))
        .filter_map(|d| d.parent().map(|root| root.join("bin").join("bash.exe")))
        .find(|b| is_file(b))
}

/// Sources of project plugins between `cwd` and its project root (the first directory
/// upwards that contains `.git`, else the filesystem root): `.opencode/plugin(s)` directories
/// with `*.js`/`*.ts` files, and the config documents `opencode.json(c)` and
/// `.opencode/opencode.json(c)` that name plugins (`plugin`/`plugins`, which may be file URLs
/// or npm packages). OpenCode 1.19 loads all of these even with
/// `OPENCODE_DISABLE_PROJECT_CONFIG=1` and `OPENCODE_PURE=1` (verified in real runs; its second
/// config loader scans `.opencode` directories and `opencode.json(c)` documents without that
/// gate), so a run in such a project executes that code, with the provider key in its
/// environment.
pub fn project_plugin_sources(cwd: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for dir in cwd.ancestors() {
        for sub in ["plugin", "plugins"] {
            let d = dir.join(".opencode").join(sub);
            let has_code = std::fs::read_dir(&d).is_ok_and(|rd| {
                rd.flatten().any(|e| {
                    let name = e.file_name().to_string_lossy().to_ascii_lowercase();
                    name.ends_with(".js") || name.ends_with(".ts")
                })
            });
            if has_code {
                found.push(d);
            }
        }
        for doc_dir in [dir.to_path_buf(), dir.join(".opencode")] {
            for file in ["opencode.json", "opencode.jsonc"] {
                let f = doc_dir.join(file);
                if std::fs::read_to_string(&f).is_ok_and(|t| names_plugins(&t)) {
                    found.push(f);
                }
            }
        }
        if dir.join(".git").exists() {
            break;
        }
    }
    found
}

/// Does an OpenCode config document name plugins? Plain JSON: a top-level `plugin`/`plugins`
/// that is not an empty array (or null). A document serde_json cannot parse (JSONC with
/// comments or trailing commas, or broken JSON) counts when a `"plugin"`/`"plugins"` key
/// appears anywhere: a false positive is only a log warning, a miss would hide code execution.
fn names_plugins(text: &str) -> bool {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(map)) => ["plugin", "plugins"].iter().any(|k| match map.get(*k) {
            None | Some(Value::Null) => false,
            Some(Value::Array(a)) => !a.is_empty(),
            Some(_) => true,
        }),
        Ok(_) => false,
        Err(_) => {
            let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
            compact.contains("\"plugin\":") || compact.contains("\"plugins\":")
        }
    }
}

impl Harness for OpencodeHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::OpenCode
    }

    fn locate(&self, backend: &Backend, _claude_path: Option<&Path>) -> Result<PathBuf, String> {
        let BackendKind::OpenCode { harness_path, .. } = &backend.kind else {
            return Err(wrong_harness(self, backend));
        };
        locate_real(harness_path.as_deref())
    }

    fn locate_installed(&self, _claude_path: Option<&Path>) -> Option<Result<PathBuf, String>> {
        Some(locate_real(None))
    }

    /// `opencode --version` with a minimal environment whose home, XDG and temp dirs are a
    /// fresh temporary directory (removed afterwards): without that, OpenCode creates its
    /// directories in the user's real home even for `--version`.
    fn version(&self, program: &Path) -> Result<String, String> {
        let scratch = std::env::temp_dir().join(format!(
            "agent-sidekick-opencode-version-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let dirs = Dirs::new(&scratch);
        dirs.create()
            .map_err(|e| format!("creating {}: {e}", scratch.display()))?;
        let mut env: Vec<(OsString, OsString)> = std::env::vars_os()
            .filter(|(k, _)| VERSION_ENV_KEEP.iter().any(|n| eq_name(k, n)))
            .collect();
        for (k, v) in dirs.env_vars() {
            set_var(&mut env, k, v);
        }
        for flag in DISABLE_FLAGS {
            set_var(&mut env, flag, "1");
        }
        let out = crate::commands::claude_cli::run_in_env(
            program,
            ["--version"],
            Some(&env),
            VERSION_TIMEOUT,
        );
        let _ = std::fs::remove_dir_all(&scratch);
        let out = out.map_err(|e| format!("{e:#}"))?;
        if !out.success() {
            return Err(format!("`--version` failed: {}", out.diagnostics()));
        }
        let text = crate::redact::redact_all(out.stdout.trim());
        Ok(text.lines().next().unwrap_or_default().trim().to_owned())
    }

    fn load_credential(
        &self,
        backend: &Backend,
        env_lookup: &dyn Fn(&str) -> Option<String>,
        secret_loader: SecretLoader<'_>,
    ) -> Result<(Credential, String), String> {
        let name = &backend.name;
        let BackendKind::OpenCode {
            provider,
            api_key_env,
            ..
        } = &backend.kind
        else {
            return Err(wrong_harness(self, backend));
        };
        if !provider.needs_key() {
            return Ok((Credential::None, "none needed".to_owned()));
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
                "No provider key stored for backend '{name}'. Run \
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

    /// Creates the backend's isolation directories and (re)writes its opencode.json, then
    /// builds argv/env/stdin. The environment starts from the common session isolation
    /// ([`child_env`]: Claude session variables, OAuth token, competing keys, other backends'
    /// keys, inherited `OPENCODE_*`/`ANTHROPIC_*`/`PWD` removed) and adds the isolation
    /// variables, the disable flags, the permission set and the per-run config layer.
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
        let BackendKind::OpenCode {
            provider,
            base_url,
            read_only,
            base_dir,
            ..
        } = &backend.kind
        else {
            return Err(wrong_harness(self, backend));
        };
        let name = &backend.name;
        let Some(model) = model else {
            return Err(format!(
                "backend {name}: internal error: no model for an opencode run"
            ));
        };
        if crate::locate::is_batch_script(program) && !batch_safe(&model) {
            return Err(format!(
                "backend {name}: harness_path is a .cmd/.bat script and the model name \
                 {model:?} contains characters cmd.exe would re-parse; point harness_path at \
                 opencode.exe"
            ));
        }

        let dirs = Dirs::new(base_dir);
        dirs.create()
            .map_err(|e| format!("backend {name}: creating {}: {e}", base_dir.display()))?;
        let config = render_config(backend)?;
        let text = serde_json::to_string_pretty(&config)
            .map_err(|e| format!("backend {name}: serializing opencode.json: {e}"))?;
        let config_file = dirs.config_file();
        write_if_changed(&config_file, &text)
            .map_err(|e| format!("backend {name}: writing {}: {e}", config_file.display()))?;

        // Inherited values needed after `child_env` removed `OPENCODE_*`.
        let inherited_git_bash = if cfg!(windows) {
            let lookup = |k: &str| {
                base.iter()
                    .find(|(n, _)| eq_name(n, k))
                    .map(|(_, v)| v.clone())
            };
            git_bash(&lookup, &|p| p.is_file())
        } else {
            None
        };

        let mut env = child_env(backend, base, global_pass_env, credential);
        if cfg!(windows) {
            // The real profile must not stay reachable through these (replaced below when the
            // isolated home has a drive or UNC prefix).
            env.retain(|(k, _)| {
                !["HOMEDRIVE", "HOMEPATH", "HOMESHARE"]
                    .iter()
                    .any(|n| eq_name(k, n))
            });
        }
        for (k, v) in dirs.env_vars() {
            set_var(&mut env, k, v);
        }
        for flag in DISABLE_FLAGS {
            set_var(&mut env, flag, "1");
        }
        set_var(
            &mut env,
            "OPENCODE_PERMISSION",
            permissions(*read_only).to_string(),
        );
        set_var(
            &mut env,
            "OPENCODE_CONFIG_CONTENT",
            run_config(req.max_turns).to_string(),
        );
        if let Some(bash) = &inherited_git_bash {
            set_var(&mut env, "OPENCODE_GIT_BASH_PATH", bash.as_os_str());
        }

        let qualified = format!("{}/{model}", provider_id(*provider));
        let args: Vec<OsString> = [
            "run",
            "--format",
            "json",
            "--model",
            qualified.as_str(),
            "--title",
            SESSION_TITLE,
        ]
        .into_iter()
        .map(OsString::from)
        .collect();

        header.push(format!("program: {}", program.display()));
        header.push(match base_url {
            Some(u) => format!("provider: {provider} ({u})"),
            None => format!("provider: {provider}"),
        });
        header.push(format!("opencode state: {}", base_dir.display()));
        header.push("prompt delivery: stdin".to_owned());
        header.push(format!(
            "max_turns: {} as agent.build.steps (soft cap; timeout_s is the hard limit)",
            req.max_turns
        ));
        if *read_only {
            header.push("read_only: edit, bash and webfetch denied".to_owned());
        }
        if req
            .allowed_tools
            .iter()
            .flatten()
            .any(|t| !t.trim().is_empty())
        {
            header.push(
                "note: allowed_tools ignored by the opencode harness (use read_only)".to_owned(),
            );
        }
        let plugins = project_plugin_sources(&req.cwd);
        if !plugins.is_empty() {
            let list: Vec<String> = plugins.iter().map(|p| p.display().to_string()).collect();
            tracing::warn!(
                backend = %name,
                "OpenCode will run the project plugins from {}, with {PROVIDER_KEY_ENV} in \
                 their environment",
                list.join(", ")
            );
            header.push(format!(
                "warning: OpenCode runs the project plugins from {} (not blocked by \
                 OPENCODE_DISABLE_PROJECT_CONFIG); they see {PROVIDER_KEY_ENV}, the provider key",
                list.join(", ")
            ));
        }
        if cfg!(windows) && inherited_git_bash.is_none() {
            header.push(
                "note: no Git Bash found; OpenCode's bash tool falls back to cmd.exe".to_owned(),
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

    /// The provider answers (and accepts the key) right before the run, so a dead endpoint
    /// fails fast and a fallback can take over (OpenCode itself retries connection errors
    /// for about a minute).
    fn pre_run_check(&self, backend: &Backend, credential: &Credential) -> Result<(), String> {
        let r = provider_probe(backend, credential.secret().map(Token::expose), &|u, b| {
            crate::backends::reach::probe_get(u, b, PROBE_TIMEOUT)
        });
        if r.reachable {
            Ok(())
        } else {
            Err(format!("provider check failed: {}", r.detail))
        }
    }

    /// opencode installed, key available (unless ollama), provider answers.
    fn check(&self, backend: &Backend, deps: &CheckDeps<'_>) -> Reachability {
        let BackendKind::OpenCode {
            provider,
            api_key_env,
            ..
        } = &backend.kind
        else {
            return Reachability::unreachable(wrong_harness(self, backend));
        };
        let program = match (deps.locate)(backend) {
            Ok(p) => p,
            Err(e) => return Reachability::unreachable(format!("opencode not installed: {e}")),
        };
        let (key, key_detail) = if !provider.needs_key() {
            (None, "no key needed".to_owned())
        } else if let Some(var) = api_key_env {
            match (deps.env_lookup)(var).filter(|v| !v.trim().is_empty()) {
                Some(v) => (Some(v), format!("key from env var {var}")),
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
                        "no provider key stored; run `agent-sidekick setup --backend {}`",
                        backend.name
                    ))
                }
                Err(e) => return Reachability::unreachable(format!("cannot read the {id}: {e}")),
            };
            match (deps.secret_value)(&id) {
                Ok(t) => (Some(t.expose().to_owned()), format!("key from {source}")),
                Err(e) => return Reachability::unreachable(format!("cannot read the {id}: {e}")),
            }
        };
        let r = provider_probe(backend, key.as_deref(), deps.probe_get);
        let detail = format!("opencode {}; {key_detail}; {}", program.display(), r.detail);
        Reachability {
            reachable: r.reachable,
            detail,
        }
    }

    fn install_hint(&self) -> Option<&'static str> {
        Some(INSTALL_HINT)
    }
}

/// The provider's cheap reachability endpoint: openai-compatible `GET {base_url}/models`
/// (with the key as bearer token), ollama `GET {base_url}/api/tags`, openrouter its public
/// model list. Never a model call.
pub fn probe_url(backend: &Backend) -> Option<String> {
    let BackendKind::OpenCode {
        provider, base_url, ..
    } = &backend.kind
    else {
        return None;
    };
    Some(match provider {
        OpencodeProvider::OpenaiCompatible => {
            format!("{}/models", base_url.as_deref().unwrap_or_default())
        }
        OpencodeProvider::Ollama => format!("{}/api/tags", base_url.as_deref().unwrap_or_default()),
        OpencodeProvider::Openrouter => OPENROUTER_MODELS_URL.to_owned(),
    })
}

/// Probe [`probe_url`]: any HTTP answer except 401/403 counts as reachable (some gateways
/// have no model list); 401/403 means the key was rejected. The key is sent only to an
/// openai-compatible `base_url` (and never appears in the detail).
pub fn provider_probe(
    backend: &Backend,
    key: Option<&str>,
    probe_get: &dyn Fn(&str, Option<&str>) -> ProbeResponse,
) -> Reachability {
    let Some(url) = probe_url(backend) else {
        return Reachability::unreachable(wrong_harness(&OpencodeHarness, backend));
    };
    let bearer = match &backend.kind {
        BackendKind::OpenCode {
            provider: OpencodeProvider::OpenaiCompatible,
            ..
        } => key,
        _ => None,
    };
    match probe_get(&url, bearer) {
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

/// Usage summed over `step_finish` events.
#[derive(Debug, Default, Clone, PartialEq)]
struct Usage {
    steps: u64,
    input: u64,
    output: u64,
    cost: f64,
}

/// Turns `opencode run --format json` output (one JSON event per line, possibly mixed with
/// plain stderr lines and agent-sidekick's own notes) into the result text:
/// - the `text` parts after the last `step_start` (the final answer; all text parts if none
///   follow it);
/// - every `error` event as `[error] <name>: <message>`;
/// - `stopped: permission auto-rejected` when a permission was rejected and no answer came;
/// - non-JSON lines as they are (a line that looks like a cut-off event is skipped: the
///   buffer may start mid-line);
/// - a usage line (steps, tokens in/out, cost) from the `step_finish` events.
///
/// No answer at all is a failure even with exit code 0.
pub fn normalize_json_lines(raw: &str) -> Normalized {
    let mut all_text: Vec<String> = Vec::new();
    let mut final_text: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut other: Vec<String> = Vec::new();
    let mut rejected = false;
    let mut usage = Usage::default();
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
            if cut_off || line.contains("\"sessionID\"") || line.trim_start().starts_with('{') {
                // A cut-off event (the buffer starts mid-line) or a malformed one.
                continue;
            }
            if line.contains("auto-rejecting") {
                rejected = true;
            }
            other.push(line.to_owned());
            continue;
        };
        saw_event = true;
        let part = &ev["part"];
        match ev["type"].as_str().unwrap_or_default() {
            "step_start" => final_text.clear(),
            "text" => {
                if let Some(t) = part["text"].as_str() {
                    let t = t.trim();
                    if !t.is_empty() {
                        all_text.push(t.to_owned());
                        final_text.push(t.to_owned());
                    }
                }
            }
            "tool_use" => {
                let state = &part["state"];
                if state["status"].as_str() == Some("error") {
                    let err = state["error"].as_str().unwrap_or_default();
                    if err.contains("rejected permission") {
                        rejected = true;
                    }
                }
            }
            "step_finish" => {
                usage.steps += 1;
                let tokens = &part["tokens"];
                usage.input += tokens["input"].as_u64().unwrap_or(0);
                usage.output += tokens["output"].as_u64().unwrap_or(0);
                usage.cost += part["cost"].as_f64().unwrap_or(0.0);
            }
            "error" => {
                let e = &ev["error"];
                let name = e["name"].as_str().unwrap_or("error");
                let message = e["data"]["message"]
                    .as_str()
                    .or_else(|| e["message"].as_str())
                    .unwrap_or("(no message)");
                errors.push(format!("[error] {name}: {message}"));
            }
            _ => {}
        }
    }

    let answer = if final_text.is_empty() {
        all_text.join("\n\n")
    } else {
        final_text.join("\n\n")
    };
    let mut out = String::new();
    let mut push = |s: &str| {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(s);
    };
    if !answer.is_empty() {
        push(&answer);
    }
    if !other.is_empty() {
        push("[opencode output]");
        for l in &other {
            push(l);
        }
    }
    for e in &errors {
        push(e);
    }
    if rejected && answer.is_empty() {
        push(
            "stopped: permission auto-rejected (OpenCode ended the run because a tool needed a \
             permission that was not granted)",
        );
    }
    if usage.steps > 0 {
        push(&format!(
            "[usage] steps {}, tokens in {} / out {}, cost {:.4}",
            usage.steps, usage.input, usage.output, usage.cost
        ));
    }
    let failure = answer.is_empty().then(|| {
        if rejected {
            "no answer: permission auto-rejected".to_owned()
        } else if !errors.is_empty() {
            "no answer: opencode reported an error".to_owned()
        } else if saw_event {
            "no answer: opencode produced no text".to_owned()
        } else {
            "no answer: no opencode JSON output".to_owned()
        }
    });
    Normalized { text: out, failure }
}

#[cfg(test)]
mod tests;
