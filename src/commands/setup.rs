//! `agent-sidekick setup`: store the backends' secrets and register the MCP server.
//!
//! - Without `--backend`: asks for the OAuth token of every claude-oauth backend whose token
//!   is not stored yet (in name order), prints a hint for claude-endpoint/command backends,
//!   creates the claude-* config dirs, then (re-)registers the MCP server with Claude Code,
//!   keeping the extra `--env` variables of the previous user-scope registration.
//! - With `--backend <name>`: handles only that backend (OAuth token, endpoint API key, the
//!   `secret_env` value of a command backend, or an info line), creates its config dir and
//!   never changes the MCP registration.
//! - With `--register-only`: asks for nothing and only (re-)registers.

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{anyhow, bail, Context, Result};

use super::backends::{load_backends, unknown_backend};
use super::claude_cli;
use crate::backends::{self, BackendKind, Backends};
use crate::locate::{self, Located};
use crate::token::{self, FormatCheck, SecretId, Token};
use crate::{paths, APP_NAME};

#[derive(Debug, clap::Args)]
pub struct SetupArgs {
    /// Read the secret from stdin instead of prompting (for scripting). Only possible when
    /// exactly one secret is asked for (a single claude-oauth backend, or with --backend).
    #[arg(long)]
    pub token_stdin: bool,
    /// Do not register the MCP server with Claude Code.
    #[arg(long)]
    pub no_register: bool,
    /// Explicit path to the `claude` executable (skips auto-detection).
    #[arg(long, value_name = "PATH")]
    pub claude: Option<PathBuf>,
    /// Set up only this backend (token, API key or command secret); the MCP registration is
    /// not changed.
    #[arg(long, value_name = "NAME")]
    pub backend: Option<String>,
    /// Only (re-)register the MCP server with Claude Code; ask for no tokens at all.
    #[arg(long, conflicts_with_all = ["backend", "token_stdin", "no_register"])]
    pub register_only: bool,
    /// Do not start the guided setup (wizard) in an interactive terminal; run the plain
    /// setup instead (ask for the claude-oauth tokens of config.toml, then register).
    #[arg(long)]
    pub no_wizard: bool,
}

impl SetupArgs {
    /// No flag at all: `setup` alone, which starts the wizard in an interactive terminal.
    pub fn is_bare(&self) -> bool {
        !self.token_stdin
            && !self.no_register
            && self.claude.is_none()
            && self.backend.is_none()
            && !self.register_only
            && !self.no_wizard
    }
}

/// The wizard runs for a bare `setup` when both stdin and stdout are terminals; otherwise
/// (piped, scripted, any flag) the non-interactive setup runs unchanged.
pub fn wants_wizard(args: &SetupArgs, stdin_tty: bool, stdout_tty: bool) -> bool {
    args.is_bare() && stdin_tty && stdout_tty
}

/// One thing setup does for one backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// claude-oauth: prompt for the OAuth token, store it, create `config_dir`.
    OauthToken { name: String, config_dir: PathBuf },
    /// claude-oauth in a full setup whose token is already stored (`source`): keep it.
    OauthStored {
        name: String,
        config_dir: PathBuf,
        source: String,
    },
    /// claude-endpoint without `api_key_env`, only with `--backend`: prompt for the API key.
    EndpointKey { name: String, config_dir: PathBuf },
    /// claude-endpoint with `api_key_env`: nothing to store, explain the env var.
    EndpointUsesEnv {
        name: String,
        var: String,
        config_dir: PathBuf,
    },
    /// claude-endpoint without `api_key_env` in a full setup: hint to `setup --backend`.
    EndpointHint { name: String, config_dir: PathBuf },
    /// command with `secret_env`, only with `--backend`: prompt for the secret.
    CommandSecret { name: String, var: String },
    /// command with `secret_env` in a full setup: hint to `setup --backend`.
    CommandSecretHint { name: String, var: String },
    /// command without `secret_env`: nothing to store.
    CommandInfo { name: String },
    /// opencode with a key-based provider, only with `--backend`: prompt for the provider key
    /// (keyring user `backend-<name>`).
    ProviderKey { name: String },
    /// opencode with a key-based provider in a full setup: hint to `setup --backend`.
    ProviderKeyHint { name: String },
    /// opencode with `api_key_env`: nothing to store, explain the env var.
    ProviderUsesEnv { name: String, var: String },
    /// opencode with provider `ollama`: no key.
    ProviderNoKey { name: String },
}

impl Step {
    pub fn asks_secret(&self) -> bool {
        matches!(
            self,
            Step::OauthToken { .. }
                | Step::EndpointKey { .. }
                | Step::CommandSecret { .. }
                | Step::ProviderKey { .. }
        )
    }

    /// The Claude config dir of a claude-* backend, which setup creates.
    pub fn config_dir(&self) -> Option<&Path> {
        match self {
            Step::OauthToken { config_dir, .. }
            | Step::OauthStored { config_dir, .. }
            | Step::EndpointKey { config_dir, .. }
            | Step::EndpointUsesEnv { config_dir, .. }
            | Step::EndpointHint { config_dir, .. } => Some(config_dir),
            Step::CommandSecret { .. }
            | Step::CommandSecretHint { .. }
            | Step::CommandInfo { .. }
            | Step::ProviderKey { .. }
            | Step::ProviderKeyHint { .. }
            | Step::ProviderUsesEnv { .. }
            | Step::ProviderNoKey { .. } => None,
        }
    }
}

/// In a full setup, turn the token prompt of every claude-oauth backend whose token is
/// already stored into [`Step::OauthStored`]. `stored`: where the backend's token is
/// stored (`None`: not stored).
pub fn skip_stored(steps: Vec<Step>, stored: &dyn Fn(&str) -> Option<String>) -> Vec<Step> {
    steps
        .into_iter()
        .map(|step| match step {
            Step::OauthToken { name, config_dir } => match stored(&name) {
                Some(source) => Step::OauthStored {
                    name,
                    config_dir,
                    source,
                },
                None => Step::OauthToken { name, config_dir },
            },
            other => other,
        })
        .collect()
}

/// Where the OAuth token of backend `name` is stored, if it is (never the value).
fn stored_oauth_source(name: &str) -> Option<String> {
    token::load_secret(&SecretId::Oauth(name.to_owned()))
        .ok()
        .map(|(_token, source)| source.to_string())
}

/// Which steps setup performs. `only`: the `--backend` value.
pub fn plan(backends: &Backends, only: Option<&str>) -> Result<Vec<Step>, String> {
    let selected: Vec<_> = match only {
        Some(name) => vec![backends
            .get(name)
            .ok_or_else(|| unknown_backend(name, backends))?],
        None => backends.iter().collect(),
    };
    let single = only.is_some();
    Ok(selected
        .into_iter()
        .map(|b| {
            let name = b.name.clone();
            match &b.kind {
                BackendKind::ClaudeOauth { config_dir } => Step::OauthToken {
                    name,
                    config_dir: config_dir.clone(),
                },
                BackendKind::ClaudeEndpoint {
                    api_key_env: Some(var),
                    config_dir,
                    ..
                } => Step::EndpointUsesEnv {
                    name,
                    var: var.clone(),
                    config_dir: config_dir.clone(),
                },
                BackendKind::ClaudeEndpoint { config_dir, .. } if single => Step::EndpointKey {
                    name,
                    config_dir: config_dir.clone(),
                },
                BackendKind::ClaudeEndpoint { config_dir, .. } => Step::EndpointHint {
                    name,
                    config_dir: config_dir.clone(),
                },
                BackendKind::Command {
                    secret_env: Some(var),
                    ..
                } if single => Step::CommandSecret {
                    name,
                    var: var.clone(),
                },
                BackendKind::Command {
                    secret_env: Some(var),
                    ..
                } => Step::CommandSecretHint {
                    name,
                    var: var.clone(),
                },
                BackendKind::Command { .. } => Step::CommandInfo { name },
                BackendKind::OpenCode { provider, .. } if !provider.needs_key() => {
                    Step::ProviderNoKey { name }
                }
                BackendKind::OpenCode {
                    api_key_env: Some(var),
                    ..
                } => Step::ProviderUsesEnv {
                    name,
                    var: var.clone(),
                },
                BackendKind::OpenCode { .. } if single => Step::ProviderKey { name },
                BackendKind::OpenCode { .. } => Step::ProviderKeyHint { name },
            }
        })
        .collect())
}

/// `--token-stdin` provides exactly one secret, so it needs exactly one prompt. Only the
/// steps that actually ask count (a full setup skips tokens that are already stored).
pub fn check_token_stdin(steps: &[Step], token_stdin: bool, single_backend: bool) -> Result<()> {
    if !token_stdin {
        return Ok(());
    }
    let asked: Vec<&str> = steps
        .iter()
        .filter(|s| s.asks_secret())
        .map(step_name)
        .collect();
    let stored: Vec<&str> = steps
        .iter()
        .filter(|s| matches!(s, Step::OauthStored { .. }))
        .map(step_name)
        .collect();
    match asked.len() {
        1 => Ok(()),
        0 if single_backend => bail!(
            "--token-stdin: this backend has no secret to store (see the output without --token-stdin)"
        ),
        0 if !stored.is_empty() => bail!(
            "--token-stdin: the token of every claude-oauth backend is already stored ({}), so \
             there is no token to read; replace one with `{APP_NAME} setup --backend <name> \
             --token-stdin`, or only re-register with `{APP_NAME} setup --register-only`",
            stored.join(", ")
        ),
        0 => bail!("--token-stdin: no claude-oauth backend is configured, so there is no token to read"),
        _ => bail!(
            "--token-stdin can provide only one secret, but setup would ask for {} ({}); \
             store each with `{APP_NAME} setup --backend <name> --token-stdin`, then register \
             with `{APP_NAME} setup --register-only`",
            asked.len(),
            asked.join(", ")
        ),
    }
}

fn step_name(step: &Step) -> &str {
    match step {
        Step::OauthToken { name, .. }
        | Step::OauthStored { name, .. }
        | Step::EndpointKey { name, .. }
        | Step::EndpointUsesEnv { name, .. }
        | Step::EndpointHint { name, .. }
        | Step::CommandSecret { name, .. }
        | Step::CommandSecretHint { name, .. }
        | Step::CommandInfo { name }
        | Step::ProviderKey { name }
        | Step::ProviderKeyHint { name }
        | Step::ProviderUsesEnv { name, .. }
        | Step::ProviderNoKey { name } => name,
    }
}

pub fn run(args: SetupArgs) -> Result<ExitCode> {
    // Before loading config.toml: the wizard can repair a broken one (backup + rewrite).
    if wants_wizard(
        &args,
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    ) {
        return super::wizard::run_interactive();
    }
    // The configuration first: a broken config.toml must fail before anything is asked.
    let (cfg, backends) = load_backends().map_err(|e| anyhow!(e))?;
    let single = args.backend.is_some();
    let steps = if args.register_only {
        Vec::new()
    } else {
        let steps = plan(&backends, args.backend.as_deref()).map_err(|e| anyhow!(e))?;
        if single {
            // `--backend` always asks, so a stored token can be replaced.
            steps
        } else {
            skip_stored(steps, &stored_oauth_source)
        }
    };
    check_token_stdin(&steps, args.token_stdin, single)?;
    let prompts = steps.iter().any(Step::asks_secret);
    if prompts && !args.token_stdin {
        require_terminal(std::io::stdin().is_terminal())?;
    }

    let self_exe = std::env::current_exe().context("determining the path of this executable")?;
    let claude = if single {
        // Only for the registration hint; never required here.
        resolve_claude(args.claude.as_deref(), cfg.claude_path.as_deref()).ok()
    } else {
        if looks_like_dev_build(&self_exe) && !args.no_register {
            eprintln!(
                "warning: {} looks like a cargo build output; the MCP registration will point there. \
                 Install with `cargo install --path .` (or copy the binary somewhere stable) for regular use.",
                self_exe.display()
            );
        }
        match resolve_claude(args.claude.as_deref(), cfg.claude_path.as_deref()) {
            Ok(found) => {
                let via = match args.claude {
                    Some(_) => "--claude".to_owned(),
                    None => found.source.to_string(),
                };
                println!("Using claude: {} (via {via})", found.path.display());
                Some(found)
            }
            Err(e) if args.no_register => {
                eprintln!("warning: {e:#}");
                None
            }
            Err(e) => {
                return Err(e.context(
                    "Claude Code is required for registration. Next steps: install Claude Code, \
                     or pass --claude <path>, or set AGENT_SIDEKICK_CLAUDE, or use --no-register",
                ))
            }
        }
    };

    if !args.register_only {
        // Before storing (the file fallback lives in the config dir), so that "Created" is
        // reported accurately. Other commands never create these directories.
        let base = if prompts || !single {
            vec![paths::config_dir()?, paths::logs_dir()?]
        } else {
            Vec::new()
        };
        for dir in dirs_to_create(base, &steps) {
            create_dir(&dir)?;
        }
    }

    for step in &steps {
        run_step(step, &backends, args.token_stdin)?;
    }

    if single {
        if let Some(found) = &claude {
            if let Ok(None) = claude_cli::mcp_get(&found.path) {
                println!(
                    "\nNote: the MCP server `{APP_NAME}` is not registered with Claude Code; \
                     run `{APP_NAME} setup --register-only` once to register it."
                );
            }
        }
        println!(
            "\nDone. Check the backend with `{APP_NAME} backends test {}`.",
            step_name(&steps[0])
        );
        return Ok(ExitCode::SUCCESS);
    }

    match &claude {
        Some(found) if !args.no_register => register(&found.path, &self_exe)?,
        _ => println!("Skipping MCP registration (--no-register)."),
    }

    println!(
        "\nDone. Reload VS Code (Developer: Reload Window) and check with /mcp in Claude Code.\n\
         Run `{APP_NAME} doctor` to verify the installation."
    );
    Ok(ExitCode::SUCCESS)
}

/// `base` plus the Claude config dir of every claude-* step (whether or not it prompts),
/// without duplicates.
fn dirs_to_create(base: Vec<PathBuf>, steps: &[Step]) -> Vec<PathBuf> {
    let mut dirs = base;
    for dir in steps.iter().filter_map(Step::config_dir) {
        if !dirs.iter().any(|d| d == dir) {
            dirs.push(dir.to_owned());
        }
    }
    dirs
}

/// Create `dir` (with parents) if it is missing and say so.
fn create_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        println!("Created {}", dir.display());
    }
    Ok(())
}

fn stored_message(name: &str, source: &str) -> String {
    format!(
        "Token for backend `{name}` already stored ({source}); use `{APP_NAME} setup --backend \
         {name}` to replace it."
    )
}

fn run_step(step: &Step, backends: &Backends, token_stdin: bool) -> Result<()> {
    match step {
        Step::OauthStored { name, source, .. } => println!("{}", stored_message(name, source)),
        Step::OauthToken { name, config_dir } => {
            if !token_stdin {
                println!("\n{}", token_instructions(name, config_dir));
            }
            let token = read_secret(
                token_stdin,
                &format!("Paste the OAuth token for backend `{name}` (input hidden): "),
            )?;
            if let FormatCheck::Warn(msg) = token::check_format(token.expose()) {
                eprintln!("warning: {msg}");
            }
            let source = token::store_secret(&SecretId::Oauth(name.clone()), &token)?;
            drop(token);
            println!("Token for backend `{name}` stored in {source}.");
        }
        Step::EndpointKey { name, .. } => {
            let token = read_secret(
                token_stdin,
                &format!(
                    "API key for backend `{name}` (input hidden; for local servers that ignore \
                     keys enter any value, e.g. ollama): "
                ),
            )?;
            if let FormatCheck::Warn(msg) = token::check_endpoint_key_format(token.expose()) {
                eprintln!("warning: {msg}");
            }
            let source = token::store_secret(&SecretId::Endpoint(name.clone()), &token)?;
            drop(token);
            println!("API key for backend `{name}` stored in {source}.");
        }
        Step::EndpointUsesEnv { name, var, .. } => println!(
            "Backend `{name}` (claude-endpoint): the API key is read from the environment \
             variable {var}; nothing is stored. It must be set in the MCP server's environment, \
             preferably in the OS/user environment Claude Code starts from (an `-e {var}=...` \
             in the `{APP_NAME}` registration also works and is kept when setup re-registers)."
        ),
        Step::EndpointHint { name, .. } => println!(
            "Backend `{name}` (claude-endpoint): run `{APP_NAME} setup --backend {name}` to store \
             its API key."
        ),
        Step::CommandSecret { name, var } => {
            let secret = read_secret(token_stdin, &secret_prompt(name, var))?;
            if let FormatCheck::Warn(msg) = token::check_command_secret_format(secret.expose()) {
                eprintln!("warning: {msg}");
            }
            let source = token::store_secret(&SecretId::Command(name.clone()), &secret)?;
            drop(secret);
            println!("Value for {var} of backend `{name}` stored in {source}.");
        }
        Step::CommandSecretHint { name, var } => println!(
            "Backend `{name}` (command): run `{APP_NAME} setup --backend {name}` to store the \
             value for {var}."
        ),
        Step::ProviderKey { name } => {
            let key = read_secret(
                token_stdin,
                &format!("Provider API key for backend `{name}` (input hidden): "),
            )?;
            if let FormatCheck::Warn(msg) = token::check_command_secret_format(key.expose()) {
                eprintln!("warning: {msg}");
            }
            let source = token::store_secret(&SecretId::Backend(name.clone()), &key)?;
            drop(key);
            println!("Provider key for backend `{name}` stored in {source}.");
        }
        Step::ProviderKeyHint { name } => println!(
            "Backend `{name}` (opencode): run `{APP_NAME} setup --backend {name}` to store its \
             provider API key."
        ),
        Step::ProviderUsesEnv { name, var } => println!(
            "Backend `{name}` (opencode): the provider key is read from the environment \
             variable {var}; nothing is stored. It must be set in the MCP server's environment."
        ),
        Step::ProviderNoKey { name } => println!(
            "Backend `{name}` (opencode, provider ollama): no key needed; nothing to store."
        ),
        Step::CommandInfo { name } => {
            let program = backends
                .get(name)
                .map(|b| backends::resolve_program(b, None));
            match program {
                Some(Ok(p)) => println!(
                    "Backend `{name}` (command): nothing to store; program {}.",
                    p.display()
                ),
                Some(Err(e)) => {
                    println!("Backend `{name}` (command): nothing to store; program not found: {e}")
                }
                None => println!("Backend `{name}` (command): nothing to store."),
            }
        }
    }
    Ok(())
}

/// Prompt for the `secret_env` value of a command backend.
fn secret_prompt(name: &str, var: &str) -> String {
    format!("Value for {var} of backend {name} (input hidden): ")
}

/// `--claude` wins over auto-detection (`config_claude_path`: `claude_path` in config.toml).
fn resolve_claude(explicit: Option<&Path>, config_claude_path: Option<&Path>) -> Result<Located> {
    if let Some(p) = explicit {
        let abs = std::path::absolute(p).with_context(|| format!("resolving {}", p.display()))?;
        if !abs.is_file() {
            bail!("--claude {} does not exist or is not a file", abs.display());
        }
        // `LocateSource` has no variant for a CLI flag; the caller labels it `--claude`.
        return Ok(Located {
            path: abs,
            source: locate::LocateSource::Env,
        });
    }
    Ok(locate::find_claude(config_claude_path)?)
}

pub(crate) fn register(claude: &Path, self_exe: &Path) -> Result<()> {
    // `mcp get` shows only the entry that wins in this directory (local > project > user),
    // so a user entry may exist behind a local one: always try to remove the user entry.
    let (registered, keep) = match claude_cli::mcp_get(claude) {
        Ok(entry) => {
            let keep = entry.as_ref().map(env_to_keep);
            (
                entry.is_some(),
                keep.unwrap_or(EnvToKeep::Known(Vec::new())),
            )
        }
        Err(get_err) => {
            let listed =
                claude_cli::mcp_list_contains(claude).with_context(|| format!("{get_err:#}"))?;
            (listed, EnvToKeep::Unknown)
        }
    };
    let extra_env = match keep {
        EnvToKeep::Known(env) => env,
        EnvToKeep::Unknown => {
            if registered {
                eprintln!("warning: {UNKNOWN_ENV_WARNING}");
            }
            Vec::new()
        }
    };
    if registered && claude_cli::mcp_remove(claude)? {
        println!("Removed existing {APP_NAME} registration (user scope).");
    }
    claude_cli::mcp_add(claude, claude, self_exe, &extra_env)?;
    println!(
        "Registered MCP server `{APP_NAME}` (user scope): {} serve",
        self_exe.display()
    );
    if let Some(note) = kept_env_note(&extra_env) {
        println!("{note}");
    }
    // A local or project entry would still take precedence over the new one here.
    match claude_cli::mcp_get(claude) {
        Ok(Some(entry)) => {
            if let Some(warning) = shadow_warning(&entry) {
                eprintln!("warning: {warning}");
            }
        }
        Ok(None) => eprintln!(
            "warning: `claude mcp get {APP_NAME}` does not show the new registration; \
             check with `claude mcp list`."
        ),
        Err(e) => eprintln!("warning: could not verify the registration: {e:#}"),
    }
    Ok(())
}

/// The environment of the user-scope registration that a re-registration carries over.
#[derive(Debug, PartialEq, Eq)]
enum EnvToKeep {
    /// Read from the user-scope entry (without `AGENT_SIDEKICK_CLAUDE`).
    Known(Vec<(String, String)>),
    /// The user-scope entry could not be read (shadowed or `mcp get` failed).
    Unknown,
}

const UNKNOWN_ENV_WARNING: &str = "could not read the environment variables of a user-scope \
    registration, if there is one (another entry shadows it here, or `claude mcp get` failed); variables added \
    to it with `-e` are not carried over. Set them in the OS environment or add them again.";

/// Only a user-scope entry is replaced, so only its variables are carried over; a local or
/// project entry that `mcp get` shows instead hides the user entry's variables.
fn env_to_keep(entry: &claude_cli::McpEntry) -> EnvToKeep {
    if entry.is_user_scope() {
        EnvToKeep::Known(claude_cli::preserved_env(&entry.env))
    } else {
        EnvToKeep::Unknown
    }
}

/// Names (never values) of the variables carried over to the new registration.
fn kept_env_note(env: &[(String, String)]) -> Option<String> {
    if env.is_empty() {
        return None;
    }
    let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
    Some(format!(
        "Kept environment variables of the previous registration: {}",
        names.join(", ")
    ))
}

/// Warning text if `entry` (what `mcp get` reports after registering) is not the
/// user-scope entry, i.e. another entry shadows it in this directory.
fn shadow_warning(entry: &claude_cli::McpEntry) -> Option<String> {
    let scope = entry.scope.as_deref()?;
    if entry.is_user_scope() {
        return None;
    }
    Some(format!(
        "another `{APP_NAME}` entry ({scope}) takes precedence over the user-scope registration \
         in this directory and may point to an old binary. Remove it with {}.",
        claude_cli::remove_hint(entry)
    ))
}

/// The hidden prompt reads from the console, not from stdin; without a terminal it
/// can wait forever (e.g. a token piped in without `--token-stdin`).
fn require_terminal(stdin_is_terminal: bool) -> Result<()> {
    if !stdin_is_terminal {
        bail!(
            "stdin is not a terminal, so the secret cannot be prompted for; \
             pipe it in with `{APP_NAME} setup --token-stdin` (with --backend <name> if more \
             than one secret is needed), or only re-register with `{APP_NAME} setup --register-only`"
        );
    }
    Ok(())
}

fn read_secret(from_stdin: bool, prompt: &str) -> Result<Token> {
    let raw = if from_stdin {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .context("reading the secret from stdin")?;
        s
    } else {
        rpassword::prompt_password(prompt).context("reading the secret from the terminal")?
    };
    let token = Token::new(raw);
    if token.expose().is_empty() {
        bail!("nothing entered");
    }
    Ok(token)
}

/// How to create a long-lived token for the account of backend `name` in its config dir.
fn token_instructions(name: &str, config_dir: &Path) -> String {
    let dir = config_dir.display();
    let mut cmd = format!("  CLAUDE_CONFIG_DIR={dir} claude setup-token");
    if cfg!(windows) {
        cmd = format!(
            "  PowerShell:  $env:CLAUDE_CONFIG_DIR=\"{dir}\"; claude setup-token\n\
             \x20 Git Bash:    CLAUDE_CONFIG_DIR=\"{dir}\" claude setup-token"
        );
    }
    format!(
        "Backend `{name}` (claude-oauth), config_dir {dir}\n\
         Create an OAuth token for THIS backend's account (log in with that account in the browser).\n\
         In a separate terminal run:\n{cmd}\n\
         and copy the printed token (starts with {}).",
        token::EXPECTED_PREFIX
    )
}

/// `.../target/debug/...` or `.../target/release/...`.
fn looks_like_dev_build(exe: &Path) -> bool {
    let parts: Vec<_> = exe.components().map(|c| c.as_os_str()).collect();
    parts
        .windows(2)
        .any(|w| w[0] == "target" && (w[1] == "debug" || w[1] == "release"))
}

#[cfg(test)]
mod tests {
    use super::super::backends::tests::{
        abs, command, command_with_secret, endpoint, oauth, opencode, sample,
    };
    use super::*;
    use crate::backends::OpencodeProvider;

    #[test]
    fn opencode_steps() {
        let backends = Backends::new(vec![
            oauth("sidekick", true),
            opencode("kic", OpencodeProvider::OpenaiCompatible, None, false),
            opencode("or", OpencodeProvider::Openrouter, Some("OR_KEY"), false),
            opencode("local", OpencodeProvider::Ollama, None, false),
        ]);
        let steps = plan(&backends, None).unwrap();
        assert!(steps.contains(&Step::ProviderKeyHint { name: "kic".into() }));
        assert!(steps.contains(&Step::ProviderUsesEnv {
            name: "or".into(),
            var: "OR_KEY".into()
        }));
        assert!(steps.contains(&Step::ProviderNoKey {
            name: "local".into()
        }));
        // Only the OAuth token is asked for in a full setup.
        assert_eq!(steps.iter().filter(|s| s.asks_secret()).count(), 1);

        let single = plan(&backends, Some("kic")).unwrap();
        assert_eq!(single, vec![Step::ProviderKey { name: "kic".into() }]);
        assert!(single[0].asks_secret());
        assert_eq!(single[0].config_dir(), None);
        check_token_stdin(&single, true, true).unwrap();
        let local = plan(&backends, Some("local")).unwrap();
        assert_eq!(
            local,
            vec![Step::ProviderNoKey {
                name: "local".into()
            }]
        );
        assert!(check_token_stdin(&local, true, true).is_err());
    }

    #[test]
    fn wizard_only_for_bare_setup_in_a_terminal() {
        let bare = SetupArgs {
            token_stdin: false,
            no_register: false,
            claude: None,
            backend: None,
            register_only: false,
            no_wizard: false,
        };
        assert!(wants_wizard(&bare, true, true));
        assert!(!wants_wizard(&bare, false, true));
        assert!(!wants_wizard(&bare, true, false));
        let variants: [fn(&mut SetupArgs); 6] = [
            |a| a.token_stdin = true,
            |a| a.no_register = true,
            |a| a.claude = Some("/x/claude".into()),
            |a| a.backend = Some("kiconnect".into()),
            |a| a.register_only = true,
            |a| a.no_wizard = true,
        ];
        for set in variants {
            let mut args = bare_clone(&bare);
            set(&mut args);
            assert!(!wants_wizard(&args, true, true), "{args:?}");
        }
    }

    fn bare_clone(a: &SetupArgs) -> SetupArgs {
        SetupArgs {
            token_stdin: a.token_stdin,
            no_register: a.no_register,
            claude: a.claude.clone(),
            backend: a.backend.clone(),
            register_only: a.register_only,
            no_wizard: a.no_wizard,
        }
    }

    #[test]
    fn dev_build_detection() {
        assert!(looks_like_dev_build(Path::new(
            "/home/u/agent-sidekick/target/debug/agent-sidekick"
        )));
        assert!(looks_like_dev_build(Path::new(
            "/home/u/x/target/release/agent-sidekick"
        )));
        assert!(!looks_like_dev_build(Path::new(
            "/home/u/.cargo/bin/agent-sidekick"
        )));
    }

    #[test]
    fn instructions_name_backend_dir_and_command() {
        let dir = abs(".claude-work");
        let text = token_instructions("work", &dir);
        assert!(text.contains("Backend `work`"), "{text}");
        assert!(text.contains("claude setup-token"));
        assert!(text.contains(&dir.display().to_string()), "{text}");
        assert!(text.contains("CLAUDE_CONFIG_DIR="), "{text}");
        if cfg!(windows) {
            assert!(text.contains("$env:CLAUDE_CONFIG_DIR"), "{text}");
        }
    }

    #[test]
    fn non_terminal_stdin_points_to_token_stdin() {
        assert!(require_terminal(true).is_ok());
        let err = require_terminal(false).unwrap_err().to_string();
        assert!(err.contains("--token-stdin"), "{err}");
    }

    #[test]
    fn full_setup_plan_prompts_only_for_oauth_backends_in_name_order() {
        let steps = plan(&sample(), None).unwrap();
        assert_eq!(
            steps,
            vec![
                Step::CommandInfo {
                    name: "aider".into()
                },
                Step::EndpointUsesEnv {
                    name: "envkey".into(),
                    var: "MY_KEY".into(),
                    config_dir: abs(".claude-sidekick-envkey")
                },
                Step::EndpointHint {
                    name: "local".into(),
                    config_dir: abs(".claude-sidekick-local")
                },
                Step::OauthToken {
                    name: "sidekick".into(),
                    config_dir: abs(".claude-sidekick")
                },
                Step::OauthToken {
                    name: "work".into(),
                    config_dir: abs(".claude-work")
                },
            ]
        );
        let asked: Vec<&str> = steps
            .iter()
            .filter(|s| s.asks_secret())
            .map(step_name)
            .collect();
        assert_eq!(asked, ["sidekick", "work"]);
    }

    #[test]
    fn single_backend_plans() {
        let b = sample();
        assert_eq!(
            plan(&b, Some("local")).unwrap(),
            vec![Step::EndpointKey {
                name: "local".into(),
                config_dir: abs(".claude-sidekick-local")
            }]
        );
        assert_eq!(
            plan(&b, Some("envkey")).unwrap(),
            vec![Step::EndpointUsesEnv {
                name: "envkey".into(),
                var: "MY_KEY".into(),
                config_dir: abs(".claude-sidekick-envkey")
            }]
        );
        assert_eq!(
            plan(&b, Some("work")).unwrap(),
            vec![Step::OauthToken {
                name: "work".into(),
                config_dir: abs(".claude-work")
            }]
        );
        assert_eq!(
            plan(&b, Some("aider")).unwrap(),
            vec![Step::CommandInfo {
                name: "aider".into()
            }]
        );
        let err = plan(&b, Some("nope")).unwrap_err();
        assert!(err.contains("unknown backend `nope`"), "{err}");
        assert!(err.contains("sidekick"), "{err}");
    }

    #[test]
    fn token_stdin_needs_exactly_one_secret() {
        let b = sample();
        let full = plan(&b, None).unwrap();
        assert!(check_token_stdin(&full, false, false).is_ok());
        let err = check_token_stdin(&full, true, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--backend"), "{err}");
        assert!(err.contains("sidekick, work"), "{err}");

        for name in ["sidekick", "local"] {
            let one = plan(&b, Some(name)).unwrap();
            assert!(check_token_stdin(&one, true, true).is_ok(), "{name}");
        }
        for name in ["envkey", "aider"] {
            let none = plan(&b, Some(name)).unwrap();
            assert!(check_token_stdin(&none, true, true).is_err(), "{name}");
            assert!(check_token_stdin(&none, false, true).is_ok(), "{name}");
        }

        // A single claude-oauth backend (e.g. the implicit one): fine without --backend.
        let single = Backends::new(vec![oauth("sidekick", true), command("aider", false)]);
        let steps = plan(&single, None).unwrap();
        assert!(check_token_stdin(&steps, true, false).is_ok());

        // No claude-oauth backend at all: nothing to read.
        let none = Backends::new(vec![endpoint("local", None, true)]);
        let steps = plan(&none, None).unwrap();
        assert!(!steps.iter().any(Step::asks_secret));
        assert!(check_token_stdin(&steps, true, false).is_err());
    }

    #[test]
    fn shadow_warning_names_the_scope_specific_command() {
        let entry = |scope: Option<&str>| claude_cli::McpEntry {
            scope: scope.map(Into::into),
            ..Default::default()
        };
        let user = entry(Some("User config (available in all your projects)"));
        assert_eq!(shadow_warning(&user), None);
        assert_eq!(shadow_warning(&entry(None)), None);
        let w = shadow_warning(&entry(Some(
            "Local config (private to you in this project)",
        )));
        let w = w.unwrap();
        assert!(
            w.contains("claude mcp remove agent-sidekick -s local`"),
            "{w}"
        );
        let w = shadow_warning(&entry(Some("Project config (shared via .mcp.json)"))).unwrap();
        assert!(
            w.contains("claude mcp remove agent-sidekick -s project`"),
            "{w}"
        );
    }

    #[test]
    fn command_secret_steps() {
        let b = Backends::new(vec![
            oauth("sidekick", true),
            command_with_secret("kic", "OPENAI_API_KEY", false),
            command("plain", false),
        ]);
        // Full setup: only a hint for the command secret, no prompt.
        let full = plan(&b, None).unwrap();
        assert_eq!(
            full,
            vec![
                Step::CommandSecretHint {
                    name: "kic".into(),
                    var: "OPENAI_API_KEY".into()
                },
                Step::CommandInfo {
                    name: "plain".into()
                },
                Step::OauthToken {
                    name: "sidekick".into(),
                    config_dir: abs(".claude-sidekick")
                },
            ]
        );
        let asked: Vec<&str> = full
            .iter()
            .filter(|s| s.asks_secret())
            .map(step_name)
            .collect();
        assert_eq!(asked, ["sidekick"]);
        // --token-stdin in a full setup still means the single oauth token.
        assert!(check_token_stdin(&full, true, false).is_ok());

        // --backend kic: prompt for the value; --token-stdin works.
        let one = plan(&b, Some("kic")).unwrap();
        assert_eq!(
            one,
            vec![Step::CommandSecret {
                name: "kic".into(),
                var: "OPENAI_API_KEY".into()
            }]
        );
        assert!(one[0].asks_secret());
        assert!(check_token_stdin(&one, true, true).is_ok());
        // A command backend without secret_env still has nothing to store.
        let none = plan(&b, Some("plain")).unwrap();
        assert!(check_token_stdin(&none, true, true).is_err());
    }

    fn stored_only(names: &'static [&'static str]) -> impl Fn(&str) -> Option<String> {
        move |n: &str| names.contains(&n).then(|| "file /x/token".to_owned())
    }

    #[test]
    fn full_setup_skips_stored_oauth_tokens() {
        let b = sample();
        let steps = skip_stored(plan(&b, None).unwrap(), &stored_only(&["sidekick"]));
        assert!(steps.contains(&Step::OauthStored {
            name: "sidekick".into(),
            config_dir: abs(".claude-sidekick"),
            source: "file /x/token".into()
        }));
        let asked: Vec<&str> = steps
            .iter()
            .filter(|s| s.asks_secret())
            .map(step_name)
            .collect();
        assert_eq!(asked, ["work"]);
        // Only `work` is asked, so --token-stdin is fine now.
        assert!(check_token_stdin(&steps, true, false).is_ok());

        // Both stored: nothing is asked, no terminal needed, --token-stdin has nothing to read.
        let all = skip_stored(plan(&b, None).unwrap(), &stored_only(&["sidekick", "work"]));
        assert!(!all.iter().any(Step::asks_secret));
        assert!(check_token_stdin(&all, false, false).is_ok());
        let err = check_token_stdin(&all, true, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already stored (sidekick, work)"), "{err}");
        assert!(err.contains("--register-only"), "{err}");
        assert!(
            err.contains("setup --backend <name> --token-stdin"),
            "{err}"
        );

        // Nothing stored: unchanged plan.
        assert_eq!(
            skip_stored(plan(&b, None).unwrap(), &stored_only(&[])),
            plan(&b, None).unwrap()
        );
    }

    #[test]
    fn stored_message_names_source_and_replacement() {
        let msg = stored_message("work", "OS keyring (service agent-sidekick)");
        assert!(msg.contains("`work` already stored (OS keyring"), "{msg}");
        assert!(
            msg.contains("use `agent-sidekick setup --backend work` to replace it"),
            "{msg}"
        );
    }

    #[test]
    fn multi_secret_token_stdin_error_points_to_register_only() {
        let err = check_token_stdin(&plan(&sample(), None).unwrap(), true, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("setup --register-only"), "{err}");
        assert!(
            err.contains("setup --backend <name> --token-stdin"),
            "{err}"
        );
        let err = require_terminal(false).unwrap_err().to_string();
        assert!(err.contains("--register-only"), "{err}");
    }

    #[test]
    fn config_dirs_are_created_for_every_claude_backend() {
        let b = sample();
        // `setup --backend envkey` (api_key_env): no prompt, but its config dir is created,
        // as doctor's next step says.
        let one = plan(&b, Some("envkey")).unwrap();
        assert!(!one.iter().any(Step::asks_secret));
        assert_eq!(
            dirs_to_create(Vec::new(), &one),
            [abs(".claude-sidekick-envkey")]
        );
        // Full setup: every claude-* backend, including endpoint hints and stored tokens.
        let full = skip_stored(plan(&b, None).unwrap(), &stored_only(&["work"]));
        let base = vec![abs("cfg"), abs("logs")];
        let dirs = dirs_to_create(base, &full);
        for d in [
            "cfg",
            "logs",
            ".claude-sidekick-envkey",
            ".claude-sidekick-local",
            ".claude-sidekick",
            ".claude-work",
        ] {
            assert!(dirs.contains(&abs(d)), "{d}: {dirs:?}");
        }
        assert_eq!(dirs.len(), 6);
        // A command backend has none.
        assert!(dirs_to_create(Vec::new(), &plan(&b, Some("aider")).unwrap()).is_empty());

        // The directory is really created (with parents), and an existing one is fine.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a").join("cd-envk");
        let mut b2 = endpoint("envk", Some("MY_TEST_KEY"), true);
        if let BackendKind::ClaudeEndpoint { config_dir, .. } = &mut b2.kind {
            *config_dir = dir.clone();
        }
        let steps = plan(&Backends::new(vec![b2]), Some("envk")).unwrap();
        for d in dirs_to_create(Vec::new(), &steps) {
            create_dir(&d).unwrap();
        }
        assert!(dir.is_dir());
        create_dir(&dir).unwrap();
    }

    fn entry_with(scope: Option<&str>, env: &[(&str, &str)]) -> claude_cli::McpEntry {
        claude_cli::McpEntry {
            scope: scope.map(Into::into),
            env: env
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn reregistration_keeps_user_scope_env_except_claude_path() {
        let user = entry_with(
            Some("User config (available in all your projects)"),
            &[
                ("AGENT_SIDEKICK_CLAUDE", "/old/claude"),
                ("MY_TEST_KEY", "abc=def ghi"),
                ("OTHER", "x"),
            ],
        );
        let EnvToKeep::Known(kept) = env_to_keep(&user) else {
            panic!("user scope must be known");
        };
        assert_eq!(
            kept,
            [
                ("MY_TEST_KEY".to_owned(), "abc=def ghi".to_owned()),
                ("OTHER".to_owned(), "x".to_owned())
            ]
        );
        let note = kept_env_note(&kept).unwrap();
        assert_eq!(
            note,
            "Kept environment variables of the previous registration: MY_TEST_KEY, OTHER"
        );
        assert!(!note.contains("abc=def"), "{note}");
        assert_eq!(kept_env_note(&[]), None);

        // A shadowing local entry (or an unknown layout) must not leak into user scope.
        let local = entry_with(
            Some("Local config (private to you in this project)"),
            &[("LOCAL_ONLY", "v")],
        );
        assert_eq!(env_to_keep(&local), EnvToKeep::Unknown);
        assert_eq!(env_to_keep(&entry_with(None, &[])), EnvToKeep::Unknown);
        assert!(UNKNOWN_ENV_WARNING.contains("not carried over"));
    }

    #[test]
    fn command_secret_prompt_names_variable_and_backend() {
        assert_eq!(
            secret_prompt("kic", "OPENAI_API_KEY"),
            "Value for OPENAI_API_KEY of backend kic (input hidden): "
        );
    }

    #[test]
    fn explicit_claude_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_claude(Some(&dir.path().join("nope")), None).is_err());
        let f = dir.path().join("claude");
        std::fs::write(&f, "").unwrap();
        assert_eq!(resolve_claude(Some(&f), None).unwrap().path, f);
    }
}
