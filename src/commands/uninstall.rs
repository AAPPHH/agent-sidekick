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
    if !removed_any {
        println!("No stored tokens or API keys found.");
    }
    step("removing the MCP registration", remove_registration());
    if args.purge {
        step("removing the config directory", purge_config_dir());
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
/// token, deduplicated. Endpoint keys are included even with `api_key_env` (a key may have
/// been stored before). If the configuration is invalid, only the legacy secret is known;
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
                    BackendKind::Command { .. } => continue,
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
    use super::super::backends::tests::{abs, command, endpoint, oauth, sample};
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
                // aider (command) has none; envkey's key is removed although it uses an env var.
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
    fn legacy_secret_is_added_when_no_backend_is_named_sidekick() {
        let backends = Backends::new(vec![oauth("main", true), command("aider", false)]);
        let (ids, _) = secrets_to_delete(&Ok(backends));
        assert_eq!(
            ids,
            vec![SecretId::Oauth("main".into()), SecretId::legacy()]
        );

        let backends = Backends::new(vec![endpoint("sidekick", None, true)]);
        let (ids, _) = secrets_to_delete(&Ok(backends));
        assert_eq!(
            ids,
            vec![SecretId::Endpoint("sidekick".into()), SecretId::legacy()]
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
