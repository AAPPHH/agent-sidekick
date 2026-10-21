//! tracing setup. Logs go to stderr and to `<config-dir>/logs/agent-sidekick.log`,
//! never to stdout (stdout carries the MCP protocol in `serve` mode).

use std::path::Path;

use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// File name of the log inside `<config-dir>/logs`.
pub const LOG_FILE_NAME: &str = "agent-sidekick.log";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `serve`: info level by default; creates the logs directory if needed.
    Server,
    /// Interactive commands: warnings only by default, so normal output stays readable.
    /// Logs to the file only if the logs directory already exists, so `doctor` and
    /// `uninstall` never create the config directory as a side effect.
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
        .and_then(|dir| open_log_file(&dir, mode))
    {
        Some((writer, guard)) => (
            Some(fmt::layer().with_writer(writer).with_ansi(false)),
            Some(guard),
        ),
        None => (None, None),
    };

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .try_init();
    guard
}

/// Open `<dir>/agent-sidekick.log` for appending. `None` means stderr-only logging:
/// in `Cli` mode when `dir` does not exist, in `Server` mode when it cannot be
/// created, and in both modes when the file cannot be opened (with a warning).
fn open_log_file(dir: &Path, mode: Mode) -> Option<(NonBlocking, WorkerGuard)> {
    match mode {
        Mode::Cli if !dir.is_dir() => return None,
        Mode::Cli => {}
        Mode::Server => {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!(
                    "warning: cannot create log directory {}: {e}; logging to stderr only",
                    dir.display()
                );
                return None;
            }
        }
    }
    match build_appender(dir) {
        Ok(appender) => Some(tracing_appender::non_blocking(appender)),
        Err(e) => {
            eprintln!(
                "warning: cannot open log file {}: {e}; logging to stderr only",
                dir.join(LOG_FILE_NAME).display()
            );
            None
        }
    }
}

/// Unlike `tracing_appender::rolling::never`, this returns an error instead of panicking.
fn build_appender(dir: &Path) -> Result<RollingFileAppender, tracing_appender::rolling::InitError> {
    RollingFileAppender::builder()
        .rotation(Rotation::NEVER)
        .filename_prefix(LOG_FILE_NAME)
        .build(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn unopenable_log_file_falls_back_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the log file should be: opening it as a file fails.
        std::fs::create_dir(dir.path().join(LOG_FILE_NAME)).unwrap();
        assert!(build_appender(dir.path()).is_err());
        assert!(open_log_file(dir.path(), Mode::Server).is_none());
        assert!(open_log_file(dir.path(), Mode::Cli).is_none());
    }

    #[test]
    fn appender_writes_to_the_expected_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut appender = build_appender(dir.path()).unwrap();
        appender.write_all(b"hello\n").unwrap();
        appender.flush().unwrap();
        drop(appender);
        let text = std::fs::read_to_string(dir.path().join(LOG_FILE_NAME)).unwrap();
        assert_eq!(text, "hello\n");
    }

    #[test]
    fn cli_mode_does_not_create_the_logs_dir() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("cfg").join("logs");
        assert!(open_log_file(&logs, Mode::Cli).is_none());
        assert!(!dir.path().join("cfg").exists());

        let (_writer, guard) = open_log_file(&logs, Mode::Server).unwrap();
        drop(guard);
        assert!(logs.is_dir());

        let (_writer, guard) = open_log_file(&logs, Mode::Cli).unwrap();
        drop(guard);
        assert!(logs.join(LOG_FILE_NAME).is_file());
    }
}
