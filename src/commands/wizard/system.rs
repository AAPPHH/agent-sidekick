//! Everything the wizard does to the machine, behind [`System`] so tests never touch the
//! real config directory, keyring, PATH, npm or Claude Code registration.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use super::npm::{self, Invocation};
use crate::backends::launch::which_in_absolute_path;
use crate::backends::reach::{self, ProbeResponse};
use crate::backends::ResolveDefaults;
use crate::harness::HarnessKind;
use crate::locate::Os;
use crate::token::{self, SecretId, Token, TokenSource};

/// Timeout of the KI:connect key check (`GET {base_url}/models`).
pub const KEY_PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// Timeout of `npm root -g` and `node --version`.
const QUICK_TIMEOUT: Duration = Duration::from_secs(30);

/// Tools the wizard looks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Claude,
    Node,
    OpenCode,
    Codex,
}

pub trait System {
    /// The platform flavour (file names, npm invocation).
    fn os(&self) -> Os;
    /// `config.toml` (its directory is agent-sidekick's config dir).
    fn config_file(&self) -> Result<PathBuf>;
    /// Inputs for resolving the written config (home, config dir).
    fn resolve_defaults(&self) -> ResolveDefaults;
    /// Claude Code (`claude_path`: the value from an existing config.toml).
    fn find_claude(&self, claude_path: Option<&Path>) -> Result<PathBuf, String>;
    fn find_node(&self) -> Option<PathBuf>;
    fn find_opencode(&self) -> Option<PathBuf>;
    /// How to run npm without a shell (see [`npm::npm_base`]).
    fn npm(&self, node: &Path) -> Option<Invocation>;
    /// The Gemini CLI's JavaScript entry point (see [`npm::gemini_entry`]).
    fn find_gemini(&self, node: &Path, npm: Option<&Invocation>) -> Option<PathBuf>;
    /// `<tool> --version` (first line).
    fn version(&self, tool: Tool, path: &Path) -> Result<String, String>;
    /// Run a program visibly (output goes to the terminal); `Ok(true)`: exit code 0.
    fn run_visible(&self, inv: &Invocation) -> Result<bool>;
    /// Where the secret is stored (`None`: not stored). Never the value.
    fn stored_secret(&self, id: &SecretId) -> Option<String>;
    /// Store the secret; returns where (never the value).
    fn store_secret(&self, id: &SecretId, secret: &Token) -> Result<String>;
    /// HTTP GET with an optional bearer token ([`reach::probe_get`]).
    fn probe_get(&self, url: &str, bearer: Option<&str>) -> ProbeResponse;
    /// Create a directory with parents.
    fn create_dir(&self, dir: &Path) -> Result<()>;
    /// (Re-)register the MCP server with Claude Code (prints its own lines).
    fn register(&self, claude: &Path) -> Result<()>;
    /// The Codex CLI, if installed.
    fn find_codex(&self) -> Option<PathBuf>;
    /// Register the MCP server with Codex (`setup --codex`; prints its own lines).
    fn register_codex(&self, claude: &Path) -> Result<()>;
    /// Create a codex backend's `CODEX_HOME` with its generated config; returns the directory.
    fn prepare_codex_home(&self, backend: &crate::backends::Backend) -> Result<PathBuf, String>;
    /// Whether `path` is an existing file (never read).
    fn file_exists(&self, path: &Path) -> bool;
    /// The `doctor` report.
    fn doctor(&self) -> String;
    /// Timestamp for backup file names, e.g. `20261021-142233`.
    fn timestamp(&self) -> String;
}

/// [`System`] on the real machine.
pub struct RealSystem;

fn env_var(k: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(k)
}

fn is_file(p: &Path) -> bool {
    p.is_file()
}

/// `program` in the absolute PATH entries only: never from the current directory through an
/// empty or relative entry (the wizard runs what it finds and writes node's path into
/// config.toml), the same rule as for command backends.
fn path_lookup(program: &str) -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    which_in_absolute_path(program, std::env::var_os("PATH"), &cwd)
}

impl System for RealSystem {
    fn os(&self) -> Os {
        Os::current()
    }

    fn config_file(&self) -> Result<PathBuf> {
        crate::paths::config_file()
    }

    fn resolve_defaults(&self) -> ResolveDefaults {
        ResolveDefaults::from_env()
    }

    fn find_claude(&self, claude_path: Option<&Path>) -> Result<PathBuf, String> {
        crate::locate::find_claude(claude_path)
            .map(|l| l.path)
            .map_err(|e| e.to_string())
    }

    fn find_node(&self) -> Option<PathBuf> {
        path_lookup("node")
    }

    fn find_opencode(&self) -> Option<PathBuf> {
        crate::harness::opencode::find_opencode(None, &env_var, &is_file).ok()
    }

    fn npm(&self, node: &Path) -> Option<Invocation> {
        let npm_in_path = match Os::current() {
            Os::Unix => path_lookup("npm"),
            Os::Windows => None,
        };
        npm::npm_base(node, Os::current(), &is_file, npm_in_path.as_deref())
    }

    fn find_gemini(&self, node: &Path, npm_base: Option<&Invocation>) -> Option<PathBuf> {
        let root = npm_base.and_then(|base| {
            let inv = npm::root_global(base);
            let out =
                crate::commands::claude_cli::run(&inv.program, &inv.args, QUICK_TIMEOUT).ok()?;
            out.success()
                .then(|| PathBuf::from(out.stdout.trim()))
                .filter(|p| p.is_absolute())
        });
        let roots = npm::global_roots(
            root.as_deref(),
            Some(node),
            Os::current(),
            &env_var,
            &is_file,
        );
        npm::gemini_entry(&roots, &is_file, &|p| std::fs::read_to_string(p).ok())
    }

    fn version(&self, tool: Tool, path: &Path) -> Result<String, String> {
        match tool {
            Tool::Claude => HarnessKind::Claude.adapter().version(path),
            Tool::OpenCode => HarnessKind::OpenCode.adapter().version(path),
            Tool::Codex => HarnessKind::Codex.adapter().version(path),
            Tool::Node => {
                let out = crate::commands::claude_cli::run(path, ["--version"], QUICK_TIMEOUT)
                    .map_err(|e| format!("{e:#}"))?;
                if !out.success() {
                    return Err(format!("`--version` failed: {}", out.diagnostics()));
                }
                Ok(out.stdout.trim().to_owned())
            }
        }
    }

    fn run_visible(&self, inv: &Invocation) -> Result<bool> {
        let status = std::process::Command::new(&inv.program)
            .args(&inv.args)
            .status()
            .with_context(|| format!("starting {}", inv.program.display()))?;
        Ok(status.success())
    }

    fn stored_secret(&self, id: &SecretId) -> Option<String> {
        token::load_secret(id)
            .ok()
            .map(|(_, source)| source_label(&source))
    }

    fn store_secret(&self, id: &SecretId, secret: &Token) -> Result<String> {
        Ok(source_label(&token::store_secret(id, secret)?))
    }

    fn probe_get(&self, url: &str, bearer: Option<&str>) -> ProbeResponse {
        reach::probe_get(url, bearer, KEY_PROBE_TIMEOUT)
    }

    fn create_dir(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))
    }

    fn register(&self, claude: &Path) -> Result<()> {
        let self_exe =
            std::env::current_exe().context("determining the path of this executable")?;
        crate::commands::setup::register(claude, &self_exe)
    }

    fn find_codex(&self) -> Option<PathBuf> {
        crate::harness::codex::locate_real(None).ok()
    }

    fn register_codex(&self, claude: &Path) -> Result<()> {
        let self_exe =
            std::env::current_exe().context("determining the path of this executable")?;
        let path = crate::commands::codex_host::real_config_path()
            .context("cannot determine the Codex config (set CODEX_HOME)")?;
        let cfg = crate::config::Config::load().ok();
        crate::commands::setup::register_codex(&path, &self_exe, Some(claude), cfg.as_ref())
    }

    fn prepare_codex_home(&self, backend: &crate::backends::Backend) -> Result<PathBuf, String> {
        crate::harness::codex::prepare_home(backend)
            .map(|file| file.parent().map(Path::to_path_buf).unwrap_or(file))
    }

    fn file_exists(&self, path: &Path) -> bool {
        path.is_file()
    }

    fn doctor(&self) -> String {
        crate::commands::doctor::format_report(&crate::commands::doctor::run_checks())
    }

    fn timestamp(&self) -> String {
        chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
    }
}

/// German description of where a secret is stored (the `Display` of [`TokenSource`] is
/// the English one of `setup` and `doctor`).
fn source_label(source: &TokenSource) -> String {
    match source {
        TokenSource::Env => format!("in der Umgebungsvariable {}", token::ENV_TOKEN),
        TokenSource::Keyring => format!(
            "im Schlüsselbund des Betriebssystems (Dienst {})",
            token::KEYRING_SERVICE
        ),
        TokenSource::File(p) => format!("in der Datei {}", p.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_labels_are_german() {
        assert_eq!(
            source_label(&TokenSource::File(PathBuf::from("x").join("token"))),
            format!(
                "in der Datei {}",
                PathBuf::from("x").join("token").display()
            )
        );
        assert!(source_label(&TokenSource::Keyring).starts_with("im Schlüsselbund"));
        assert!(source_label(&TokenSource::Env).starts_with("in der Umgebungsvariable"));
    }

    /// node/npm are looked up like command programs: an executable `node` in the current
    /// directory is not found through an empty or relative PATH entry (on Unix, `which`
    /// would resolve those against the current directory).
    #[test]
    fn node_is_never_found_in_cwd_through_empty_path_entries() {
        let cwd = tempfile::TempDir::new().unwrap();
        let bin = tempfile::TempDir::new().unwrap();
        let name = if cfg!(windows) { "node.exe" } else { "node" };
        let make = |dir: &Path| {
            let p = dir.join(name);
            std::fs::write(&p, "").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            p
        };
        make(cwd.path());
        let sep = if cfg!(windows) { ";" } else { ":" };
        let bin_dir = bin.path().display().to_string();
        for path in [
            String::new(),
            ".".to_owned(),
            format!("{bin_dir}{sep}"),
            format!("{sep}{bin_dir}"),
            format!("{bin_dir}{sep}.{sep}{sep}"),
        ] {
            assert_eq!(
                which_in_absolute_path("node", Some(path.clone().into()), cwd.path()),
                None,
                "{path:?}"
            );
        }
        // A node in an absolute PATH entry is found.
        let real = make(bin.path());
        let found = which_in_absolute_path(
            "node",
            Some(format!("{sep}{bin_dir}{sep}").into()),
            cwd.path(),
        )
        .unwrap();
        assert_eq!(
            std::fs::canonicalize(found).unwrap(),
            std::fs::canonicalize(real).unwrap()
        );
    }

    /// Read-only look at the real machine (runs `npm root -g` and `--version`s); run by
    /// hand with `cargo test real_locators -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_locators() {
        let s = RealSystem;
        println!("claude: {:?}", s.find_claude(None));
        let node = s.find_node();
        println!("node: {node:?}");
        println!("opencode: {:?}", s.find_opencode());
        if let Some(node) = node {
            println!("node version: {:?}", s.version(Tool::Node, &node));
            let npm = s.npm(&node);
            println!("npm: {:?}", npm.as_ref().map(Invocation::display));
            println!("gemini: {:?}", s.find_gemini(&node, npm.as_ref()));
        }
    }
}
