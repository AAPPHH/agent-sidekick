use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::{commands, logging};

#[derive(Debug, Parser)]
#[command(
    name = "agent-sidekick",
    version,
    about = "MCP server that delegates tasks from Claude Code or Codex to a second, headless agent (configured backends)",
    propagate_version = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Store the backends' tokens and register the MCP server with Claude Code
    /// (`--backend <name>`: set up only that backend; `--codex`: register with Codex).
    Setup(commands::setup::SetupArgs),
    /// Run the MCP server over stdio (started by Claude Code or Codex, not by hand).
    Serve,
    /// Check the installation and all backends; print next steps for anything that is missing
    /// (`--offline`: skip the daily update check).
    Doctor(commands::doctor::DoctorArgs),
    /// List or test the configured backends.
    Backends(commands::backends::BackendsArgs),
    /// List the known harnesses (agent CLIs) or print a harness's official install command.
    Harness(commands::harness::HarnessArgs),
    /// Remove the stored tokens/API keys of all backends and the MCP registrations (Claude
    /// Code and Codex).
    Uninstall(commands::uninstall::UninstallArgs),
    /// Check for a newer release and print the command that installs it.
    Update,
    /// Validate or aggregate the data repository (used by its workflows) (used by its workflows).
    Data(commands::data::DataArgs),
}

pub fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Serve => {
            let _guard = logging::init(logging::Mode::Server);
            run_async(crate::server::serve())
        }
        Command::Setup(args) => {
            let _guard = logging::init(logging::Mode::Cli);
            commands::setup::run(args)
        }
        Command::Doctor(args) => {
            let _guard = logging::init(logging::Mode::Cli);
            commands::doctor::run(args)
        }
        Command::Backends(args) => {
            let _guard = logging::init(logging::Mode::Cli);
            commands::backends::run(args)
        }
        Command::Harness(args) => {
            let _guard = logging::init(logging::Mode::Cli);
            commands::harness::run(args)
        }
        Command::Uninstall(args) => {
            let _guard = logging::init(logging::Mode::Cli);
            commands::uninstall::run(args)
        }
        Command::Data(args) => {
            let _guard = logging::init(logging::Mode::Cli);
            commands::data::run(args)
        }
        Command::Update => {
            let _guard = logging::init(logging::Mode::Cli);
            commands::update::run()
        }
    };
    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {}", crate::redact::redact_all(&format!("{err:#}")));
            ExitCode::FAILURE
        }
    }
}

fn run_async<F>(fut: F) -> anyhow::Result<ExitCode>
where
    F: std::future::Future<Output = anyhow::Result<()>>,
{
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = rt.block_on(fut);
    // Dropping the runtime drops every task that is still alive (in-flight runs, whose drop
    // kills their process trees), but a plain drop would wait forever for blocking threads,
    // e.g. tokio's stdin reader stuck in a read after a signal. Bound that wait.
    rt.shutdown_timeout(RUNTIME_SHUTDOWN_GRACE);
    result?;
    Ok(ExitCode::SUCCESS)
}

/// Upper bound for blocking threads (stdin reader, keyring access) at exit.
const RUNTIME_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    fn setup(args: &[&str]) -> Result<commands::setup::SetupArgs, clap::Error> {
        let argv = ["agent-sidekick", "setup"].iter().chain(args);
        match Cli::try_parse_from(argv)?.command {
            Command::Setup(a) => Ok(a),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn register_only_parses_and_conflicts() {
        let a = setup(&["--register-only"]).unwrap();
        assert!(a.register_only);
        assert!(!setup(&[]).unwrap().register_only);
        let a = setup(&["--register-only", "--claude", "/x/claude"]).unwrap();
        assert!(a.register_only && a.claude.is_some());
        for other in [
            &["--backend", "work"][..],
            &["--token-stdin"][..],
            &["--no-register"][..],
            &["--codex"][..],
        ] {
            let mut args = vec!["--register-only"];
            args.extend_from_slice(other);
            let err = setup(&args).unwrap_err();
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{args:?}"
            );
        }
    }

    #[test]
    fn codex_parses_and_conflicts() {
        let a = setup(&["--codex"]).unwrap();
        assert!(a.codex && !a.is_bare());
        let a = setup(&["--codex", "--claude", "/x/claude"]).unwrap();
        assert!(a.codex && a.claude.is_some());
        for other in [
            &["--backend", "work"][..],
            &["--token-stdin"][..],
            &["--no-register"][..],
            &["--register-only"][..],
        ] {
            let mut args = vec!["--codex"];
            args.extend_from_slice(other);
            let err = setup(&args).unwrap_err();
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{args:?}"
            );
        }
    }
}
