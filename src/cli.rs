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
    /// Store the agent account's OAuth token and register the MCP server with Claude Code.
    Setup(commands::setup::SetupArgs),
    /// Run the MCP server over stdio (started by Claude Code, not by hand).
    Serve,
    /// Check the installation and print next steps for anything that is missing.
    Doctor,
    /// Remove the stored token and the MCP registration.
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
    rt.block_on(fut)?;
    Ok(ExitCode::SUCCESS)
}
