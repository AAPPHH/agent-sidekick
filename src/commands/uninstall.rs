//! INTERFACE STUB: implemented in step 6.

use std::process::ExitCode;

#[derive(Debug, clap::Args)]
pub struct UninstallArgs {
    /// Also delete the config directory (config.toml, token file, logs).
    #[arg(long)]
    pub purge: bool,
}

pub fn run(_args: UninstallArgs) -> anyhow::Result<ExitCode> {
    anyhow::bail!("not implemented yet")
}
