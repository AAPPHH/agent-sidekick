//! `agent-sidekick setup`: store the backends' secrets and register the MCP server.
//!
//! - Without `--backend`: asks for the OAuth token of every claude-oauth backend (in name
//!   order), prints a hint for claude-endpoint/command backends, then (re-)registers the
//!   MCP server with Claude Code.
//! - With `--backend <name>`: handles only that backend (OAuth token, endpoint API key or
//!   an info line) and never changes the MCP registration.

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
    /// Set up only this backend (token or API key); the MCP registration is not changed.
    #[arg(long, value_name = "NAME")]
    pub backend: Option<String>,
}

/// One thing setup does for one backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// claude-oauth: prompt for the OAuth token, store it, create `config_dir`.
    OauthToken { name: String, config_dir: PathBuf },
    /// claude-endpoint without `api_key_env`, only with `--backend`: prompt for the API key.
    EndpointKey { name: String, config_dir: PathBuf },
    /// claude-endpoint with `api_key_env`: nothing to store, explain the env var.
    EndpointUsesEnv { name: String, var: String },
    /// claude-endpoint without `api_key_env` in a full setup: hint to `setup --backend`.
    EndpointHint { name: String },
    /// command: nothing to store.
    CommandInfo { name: String },
}

impl Step {
    pub fn asks_secret(&self) -> bool {
        matches!(self, Step::OauthToken { .. } | Step::EndpointKey { .. })
    }
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
                    ..
                } => Step::EndpointUsesEnv {
                    name,
                    var: var.clone(),
                },
                BackendKind::ClaudeEndpoint { config_dir, .. } if single => Step::EndpointKey {
                    name,
                    config_dir: config_dir.clone(),
                },
                BackendKind::ClaudeEndpoint { .. } => Step::EndpointHint { name },
                BackendKind::Command { .. } => Step::CommandInfo { name },
            }
        })
        .collect())
}

/// `--token-stdin` provides exactly one secret, so it needs exactly one prompt.
pub fn check_token_stdin(steps: &[Step], token_stdin: bool, single_backend: bool) -> Result<()> {
    if !token_stdin {
        return Ok(());
    }
    let asked: Vec<&str> = steps
        .iter()
        .filter(|s| s.asks_secret())
        .map(step_name)
        .collect();
    match asked.len() {
        1 => Ok(()),
        0 if single_backend => bail!(
            "--token-stdin: this backend has no secret to store (see the output without --token-stdin)"
        ),
        0 => bail!("--token-stdin: no claude-oauth backend is configured, so there is no token to read"),
        _ => bail!(
            "--token-stdin can provide only one secret, but setup would ask for {} ({}); \
             run `{APP_NAME} setup --backend <name> --token-stdin` once per backend",
            asked.len(),
            asked.join(", ")
        ),
    }
}

fn step_name(step: &Step) -> &str {
    match step {
        Step::OauthToken { name, .. }
        | Step::EndpointKey { name, .. }
        | Step::EndpointUsesEnv { name, .. }
        | Step::EndpointHint { name }
        | Step::CommandInfo { name } => name,
    }
}

pub fn run(args: SetupArgs) -> Result<ExitCode> {
    // The configuration first: a broken config.toml must fail before anything is asked.
    let (cfg, backends) = load_backends().map_err(|e| anyhow!(e))?;
    let steps = plan(&backends, args.backend.as_deref()).map_err(|e| anyhow!(e))?;
    let single = args.backend.is_some();
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

    let mut dirs = vec![paths::config_dir()?, paths::logs_dir()?];
    for step in &steps {
        match step {
            Step::OauthToken { config_dir, .. } | Step::EndpointKey { config_dir, .. } => {
                dirs.push(config_dir.clone())
            }
            _ => {}
        }
    }
    if prompts || !single {
        // Before storing (the file fallback lives in the config dir), so that "Created" is
        // reported accurately. Other commands never create these directories.
        for dir in dirs {
            if !dir.exists() {
                std::fs::create_dir_all(&dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
                println!("Created {}", dir.display());
            }
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
                     run `{APP_NAME} setup` once to register it."
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

fn run_step(step: &Step, backends: &Backends, token_stdin: bool) -> Result<()> {
    match step {
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
        Step::EndpointUsesEnv { name, var } => println!(
            "Backend `{name}` (claude-endpoint): the API key is read from the environment \
             variable {var}; nothing is stored. It must be set in the MCP server's environment, \
             e.g. via `claude mcp add ... -e {var}=...` or the OS."
        ),
        Step::EndpointHint { name } => println!(
            "Backend `{name}` (claude-endpoint): run `{APP_NAME} setup --backend {name}` to store \
             its API key."
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

fn register(claude: &Path, self_exe: &Path) -> Result<()> {
    // `mcp get` shows only the entry that wins in this directory (local > project > user),
    // so a user entry may exist behind a local one: always try to remove the user entry.
    let registered = match claude_cli::mcp_get(claude) {
        Ok(entry) => entry.is_some(),
        Err(get_err) => {
            claude_cli::mcp_list_contains(claude).with_context(|| format!("{get_err:#}"))?
        }
    };
    if registered && claude_cli::mcp_remove(claude)? {
        println!("Removed existing {APP_NAME} registration (user scope).");
    }
    claude_cli::mcp_add(claude, claude, self_exe)?;
    println!(
        "Registered MCP server `{APP_NAME}` (user scope): {} serve",
        self_exe.display()
    );
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
             than one secret is needed)"
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
    use super::super::backends::tests::{abs, command, endpoint, oauth, sample};
    use super::*;

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
                    var: "MY_KEY".into()
                },
                Step::EndpointHint {
                    name: "local".into()
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
                var: "MY_KEY".into()
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
    fn explicit_claude_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_claude(Some(&dir.path().join("nope")), None).is_err());
        let f = dir.path().join("claude");
        std::fs::write(&f, "").unwrap();
        assert_eq!(resolve_claude(Some(&f), None).unwrap().path, f);
    }
}
