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
