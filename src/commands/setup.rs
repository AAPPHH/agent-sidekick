//! INTERFACE STUB: implemented in step 6.

use std::process::ExitCode;

#[derive(Debug, clap::Args)]
pub struct SetupArgs {}

pub fn run(_args: SetupArgs) -> anyhow::Result<ExitCode> {
    anyhow::bail!("not implemented yet")
}
