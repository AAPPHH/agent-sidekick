//! `agent-sidekick uninstall`: remove the stored secrets of all backends and the MCP
//! registration. Every step runs even if an earlier one fails; all errors are reported at
//! the end.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;

use super::backends::{claude_config_dir, load_backends};
use super::claude_cli;
use crate::backends::{BackendKind, Backends};
use crate::config::Config;
use crate::token::SecretId;
use crate::{locate, paths, token, APP_NAME};

#[derive(Debug, clap::Args)]
pub struct UninstallArgs {
    /// Also delete the config directory (config.toml, token/key files, logs).
    #[arg(long)]
    pub purge: bool,
}

pub fn run(args: UninstallArgs) -> Result<ExitCode> {
    let mut errors = Vec::new();
    let mut step = |name: &str, result: Result<()>| {
        if let Err(e) = result {
            let msg = crate::redact::redact_all(&format!("{name}: {e:#}"));
            eprintln!("error: {msg}");
            errors.push(msg);
        }
    };

    let resolved = load_backends().map(|(_cfg, backends)| backends);
    let (secrets, note) = secrets_to_delete(&resolved);
    if let Some(note) = note {
        eprintln!("warning: {note}");
    }
    let mut removed_any = false;
    for id in &secrets {
        step(
            &format!("removing the {id}"),
            remove_secret(id, &mut removed_any),
        );
    }
    for file in codex_logins(&resolved) {
        step(
            &format!("removing the ChatGPT login {}", file.display()),
            remove_codex_login(&file, &mut removed_any),
        );
    }
    if !removed_any {
        println!("No stored tokens or API keys found.");
    }
    step("removing the MCP registration", remove_registration());
    step(
        "removing the Codex MCP registration",
        remove_codex_registration(),
    );
    if args.purge {
        step("removing the config directory", purge_config_dir());
    }

    if !args.purge {
        for dir in opencode_dirs(&resolved) {
            if dir.exists() {
                println!(
                    "Note: {} holds an opencode backend's sessions, logs and generated config \
                     (no keys); it was left in place. `uninstall --purge` removes it with the \
                     config directory.",
                    dir.display()
                );
            }
        }
        for dir in codex_dirs(&resolved) {
            if dir.exists() {
                println!(
                    "Note: {} is a codex backend's CODEX_HOME (generated config, Codex's state \
                     databases and helper files; its login was removed above); it was left in \
                     place. `uninstall --purge` removes it with the config directory.",
                    dir.display()
                );
            }
        }
    }
    for dir in left_in_place(&resolved) {
        if dir.exists() {
            println!(
                "Note: {} contains a backend's Claude Code settings and login; \
                 it was left in place. Delete it manually if you no longer need it.",
                dir.display()
            );
        }
    }

    if errors.is_empty() {
        println!("Uninstall complete.");
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!("Uninstall finished with {} error(s).", errors.len());
        Ok(ExitCode::FAILURE)
    }
}

/// Every secret that may be stored for the configured backends, plus the legacy (v0.1)
/// token, deduplicated. Endpoint keys are included even with `api_key_env`, command secrets
/// even without `secret_env` (a secret may have been stored before). If the configuration is invalid, only the legacy secret is known;
/// the second value is then a warning saying so.
pub fn secrets_to_delete(resolved: &Result<Backends, String>) -> (Vec<SecretId>, Option<String>) {
    let mut ids = Vec::new();
    let mut note = None;
    match resolved {
        Ok(backends) => {
            for b in backends.iter() {
                ids.push(match b.kind {
                    BackendKind::ClaudeOauth { .. } => SecretId::Oauth(b.name.clone()),
                    BackendKind::ClaudeEndpoint { .. } => SecretId::Endpoint(b.name.clone()),
                    BackendKind::Command { .. } => SecretId::Command(b.name.clone()),
                    BackendKind::OpenCode { .. } | BackendKind::Codex { .. } => {
                        SecretId::Backend(b.name.clone())
                    }
                });
            }
        }
        Err(e) => {
            note = Some(format!(
                "the backend configuration could not be read, so per-backend tokens and API keys \
                 cannot be enumerated; only the default token is removed. Fix config.toml and run \
                 uninstall again to remove the others.\n{e}"
            ))
        }
    }
    ids.push(SecretId::legacy());
    let mut seen = std::collections::HashSet::new();
    ids.retain(|id| seen.insert(id.clone()));
    (ids, note)
}

/// Isolation dirs of the opencode backends (`<config-dir>/opencode/<name>`).
fn opencode_dirs(resolved: &Result<Backends, String>) -> Vec<PathBuf> {
    match resolved {
        Ok(backends) => backends
            .iter()
            .filter_map(|b| match &b.kind {
                BackendKind::OpenCode { base_dir, .. } => Some(base_dir.clone()),
                _ => None,
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// `CODEX_HOME`s of the codex backends (`<config-dir>/codex/<name>`).
fn codex_dirs(resolved: &Result<Backends, String>) -> Vec<PathBuf> {
    match resolved {
        Ok(backends) => backends
            .iter()
            .filter_map(|b| match &b.kind {
                BackendKind::Codex { base_dir, .. } => Some(base_dir.clone()),
                _ => None,
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// The ChatGPT login files (`auth.json`) of every codex backend, also of api-key backends
/// (a login may have been made before the backend was switched).
fn codex_logins(resolved: &Result<Backends, String>) -> Vec<PathBuf> {
    codex_dirs(resolved)
        .into_iter()
        .map(|d| d.join(crate::harness::codex::AUTH_FILE))
        .collect()
}

/// Delete a codex backend's `auth.json` if it exists (never read).
fn remove_codex_login(file: &std::path::Path, removed_any: &mut bool) -> Result<()> {
    match std::fs::remove_file(file) {
        Ok(()) => {
            *removed_any = true;
            println!("Removed ChatGPT login {}", file.display());
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::anyhow!("deleting {}: {e}", file.display())),
    }
}

/// Claude config dirs that uninstall leaves in place (reported if they exist).
fn left_in_place(resolved: &Result<Backends, String>) -> Vec<PathBuf> {
    match resolved {
        Ok(backends) => backends
            .iter()
            .filter_map(claude_config_dir)
            .cloned()
            .collect(),
        Err(_) => paths::agent_claude_config_dir().into_iter().collect(),
    }
}

fn remove_secret(id: &SecretId, removed_any: &mut bool) -> Result<()> {
    let removed = token::delete_secret(id)?;
    for what in removed {
        *removed_any = true;
        println!("Removed {id}: {what}");
    }
    Ok(())
}

fn remove_registration() -> Result<()> {
    // Registration removal must work even with a broken config.toml.
    let cfg = Config::load().unwrap_or_default();
    let found = match locate::find_claude(cfg.claude_path.as_deref()) {
        Ok(f) => f,
        Err(e) => {
            println!(
                "Skipping MCP deregistration ({e}). If needed run: claude mcp remove {APP_NAME} -s user"
            );
            return Ok(());
        }
    };
    if claude_cli::mcp_remove(&found.path)? {
        println!("Removed MCP server `{APP_NAME}` from Claude Code (user scope).");
    } else {
        println!("MCP server `{APP_NAME}` was not registered in user scope.");
    }
    // Entries in local/project scope (for the current directory) are left alone.
    if let Ok(Some(entry)) = claude_cli::mcp_get(&found.path) {
        if !entry.is_user_scope() {
            println!(
                "Note: `{APP_NAME}` is still registered here ({}). Remove it with {}.",
                entry.scope.as_deref().unwrap_or("unknown scope"),
                claude_cli::remove_hint(&entry)
            );
        }
    }
    Ok(())
}

/// Remove `[mcp_servers.agent-sidekick]` from the user's Codex config (`$CODEX_HOME` or
/// `~/.codex`), after a backup; the rest of the file stays as it is.
fn remove_codex_registration() -> Result<()> {
    use super::codex_host::{self, Written};
    let Some(path) = codex_host::real_config_path() else {
        println!("Skipping the Codex registration (home directory unknown).");
        return Ok(());
    };
    match codex_host::unregister(&path, &codex_host::timestamp()).map_err(anyhow::Error::msg)? {
        None => println!(
            "MCP server `{APP_NAME}` was not registered in Codex ({}).",
            path.display()
        ),
        Some(Written::Changed { backup }) => {
            println!(
                "Removed MCP server `{APP_NAME}` from Codex ({}).",
                path.display()
            );
            if let Some(b) = backup {
                println!("Backup of the previous Codex config: {}", b.display());
            }
        }
        Some(Written::Unchanged) => {}
    }
    Ok(())
}

fn purge_config_dir() -> Result<()> {
    let dir = paths::config_dir()?;
    if !dir.exists() {
        println!("Config directory {} does not exist.", dir.display());
        return Ok(());
    }
    remove_dir(&dir)?;
    println!("Removed config directory {}", dir.display());
    Ok(())
}

fn remove_dir(dir: &std::path::Path) -> Result<()> {
    use anyhow::Context;
    std::fs::remove_dir_all(dir).with_context(|| format!("deleting {}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::super::backends::tests::{
        abs, command, command_with_secret, endpoint, oauth, sample,
    };
    use super::*;

    #[test]
    fn remove_dir_is_recursive() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("cfg");
        std::fs::create_dir_all(cfg.join("logs")).unwrap();
        std::fs::write(cfg.join("logs").join("x.log"), "x").unwrap();
        remove_dir(&cfg).unwrap();
        assert!(!cfg.exists());
    }

    #[test]
    fn secrets_of_all_backends_plus_legacy() {
        let (ids, note) = secrets_to_delete(&Ok(sample()));
        assert_eq!(note, None);
        assert_eq!(
            ids,
            vec![
                // aider's (command) secret is removed although it has no secret_env, and
                // envkey's key although it uses an env var.
                SecretId::Command("aider".into()),
                SecretId::Endpoint("envkey".into()),
                SecretId::Endpoint("local".into()),
                SecretId::Oauth("sidekick".into()),
                SecretId::Oauth("work".into()),
            ],
            "sidekick is the legacy secret and must appear once"
        );
        assert!(ids.contains(&SecretId::legacy()));
    }

    #[test]
    fn opencode_provider_keys_are_removed() {
        use crate::backends::OpencodeProvider;
        let backends = Backends::new(vec![
            oauth("sidekick", true),
            super::super::backends::tests::opencode(
                "kic",
                OpencodeProvider::OpenaiCompatible,
                None,
                false,
            ),
            // Also without a key-based provider: a key may have been stored earlier.
            super::super::backends::tests::opencode("local", OpencodeProvider::Ollama, None, false),
        ]);
        let resolved = Ok(backends);
        let (ids, _) = secrets_to_delete(&resolved);
        assert!(ids.contains(&SecretId::Backend("kic".into())));
        assert!(ids.contains(&SecretId::Backend("local".into())));
        assert_eq!(opencode_dirs(&resolved).len(), 2);
        assert!(left_in_place(&resolved)
            .iter()
            .all(|d| !d.to_string_lossy().contains("opencode")));
    }

    #[test]
    fn legacy_secret_is_added_when_no_backend_is_named_sidekick() {
        let backends = Backends::new(vec![oauth("main", true), command("aider", false)]);
        let (ids, _) = secrets_to_delete(&Ok(backends));
        assert_eq!(
            ids,
            vec![
                SecretId::Command("aider".into()),
                SecretId::Oauth("main".into()),
                SecretId::legacy()
            ]
        );

        let backends = Backends::new(vec![endpoint("sidekick", None, true)]);
        let (ids, _) = secrets_to_delete(&Ok(backends));
        assert_eq!(
            ids,
            vec![SecretId::Endpoint("sidekick".into()), SecretId::legacy()]
        );
    }

    #[test]
    fn command_secrets_are_removed() {
        let backends = Backends::new(vec![
            oauth("sidekick", true),
            command_with_secret("kic", "OPENAI_API_KEY", false),
        ]);
        let (ids, _) = secrets_to_delete(&Ok(backends));
        assert_eq!(
            ids,
            vec![SecretId::Command("kic".into()), SecretId::legacy()]
        );
    }

    #[test]
    fn invalid_config_still_removes_the_legacy_secret() {
        let (ids, note) = secrets_to_delete(&Err("backends.x: bad".into()));
        assert_eq!(ids, vec![SecretId::legacy()]);
        let note = note.unwrap();
        assert!(note.contains("cannot be enumerated"), "{note}");
        assert!(note.contains("backends.x: bad"), "{note}");
    }

    #[test]
    fn claude_config_dirs_are_reported() {
        let dirs = left_in_place(&Ok(sample()));
        assert_eq!(
            dirs,
            vec![
                abs(".claude-sidekick-envkey"),
                abs(".claude-sidekick-local"),
                abs(".claude-sidekick"),
                abs(".claude-work"),
            ]
        );
    }
}
