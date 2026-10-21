//! tracing setup. Logs go to stderr and to `<config-dir>/logs/agent-sidekick.log`,
//! never to stdout (stdout carries the MCP protocol in `serve` mode).

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `serve`: info level by default.
    Server,
    /// Interactive commands: warnings only by default, so normal output stays readable.
    Cli,
}

/// Initialise logging. Keep the returned guard alive for the program's lifetime.
/// Failing to open the log file is not fatal; logging then goes to stderr only.
pub fn init(mode: Mode) -> Option<WorkerGuard> {
    let default = match mode {
        Mode::Server => "info",
        Mode::Cli => "warn",
    };
    let filter =
        EnvFilter::try_from_env("AGENT_SIDEKICK_LOG").unwrap_or_else(|_| EnvFilter::new(default));

    let stderr_layer = fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(false);

    let (file_layer, guard) = match crate::paths::logs_dir()
        .ok()
        .and_then(|dir| std::fs::create_dir_all(&dir).ok().map(|_| dir))
    {
        Some(dir) => {
            let appender = tracing_appender::rolling::never(dir, "agent-sidekick.log");
            let (writer, guard) = tracing_appender::non_blocking(appender);
            (
                Some(fmt::layer().with_writer(writer).with_ansi(false)),
                Some(guard),
            )
        }
        None => (None, None),
    };

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .try_init();
    guard
}
