//! Everything the wizard does to the machine, behind [`System`] so tests never touch the
//! real config directory, keyring, PATH, npm or Claude Code registration.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use super::npm::{self, Invocation};
use crate::backends::reach::{self, ProbeResponse};
use crate::backends::ResolveDefaults;
use crate::harness::HarnessKind;
use crate::locate::Os;
use crate::token::{self, SecretId, Token};

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
}

pub trait System {
    /// The platform flavour (file names, npm invocation).
    fn os(&self) -> Os;
    /// `config.toml` (its directory is agent-sidekick's config dir).
    fn config_file(&self) -> PathBuf;
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

impl System for RealSystem {
    fn os(&self) -> Os {
        Os::current()
    }

    fn config_file(&self) -> PathBuf {
        crate::paths::config_file().unwrap_or_else(|_| PathBuf::from("config.toml"))
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
        which::which("node").ok().filter(|p| p.is_absolute())
    }

    fn find_opencode(&self) -> Option<PathBuf> {
        crate::harness::opencode::find_opencode(None, &env_var, &is_file).ok()
    }

    fn npm(&self, node: &Path) -> Option<Invocation> {
        let npm_in_path = match Os::current() {
            Os::Unix => which::which("npm").ok(),
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
            .map(|(_, source)| source.to_string())
    }

    fn store_secret(&self, id: &SecretId, secret: &Token) -> Result<String> {
        Ok(token::store_secret(id, secret)?.to_string())
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

    fn doctor(&self) -> String {
        crate::commands::doctor::format_report(&crate::commands::doctor::run_checks())
    }

    fn timestamp(&self) -> String {
        chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
