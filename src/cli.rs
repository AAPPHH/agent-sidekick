use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::{commands, logging};

#[derive(Debug, Parser)]
#[command(
    name = "agent-sidekick",
    version,
    about = "MCP server that delegates tasks to a second, headless Claude Code instance",
    propagate_version = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Store the backends' tokens and register the MCP server with Claude Code
    /// (`--backend <name>`: set up only that backend).
    Setup(commands::setup::SetupArgs),
    /// Run the MCP server over stdio (started by Claude Code, not by hand).
    Serve,
    /// Check the installation and all backends; print next steps for anything that is missing.
    Doctor,
    /// List or test the configured backends.
    Backends(commands::backends::BackendsArgs),
    /// Remove the stored tokens/API keys of all backends and the MCP registration.
    Uninstall(commands::uninstall::UninstallArgs),
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
        Command::Doctor => {
            let _guard = logging::init(logging::Mode::Cli);
            commands::doctor::run()
        }
        Command::Backends(args) => {
            let _guard = logging::init(logging::Mode::Cli);
            commands::backends::run(args)
        }
        Command::Uninstall(args) => {
            let _guard = logging::init(logging::Mode::Cli);
            commands::uninstall::run(args)
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
}
