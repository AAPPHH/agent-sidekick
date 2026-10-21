//! `agent-sidekick harness list|install`: the known harnesses (agent CLIs) and how to install
//! them. `install` only prints the official command(s); nothing is ever downloaded or run.

use std::process::ExitCode;

use anyhow::Result;

use crate::config::Config;
use crate::harness::{HarnessKind, VERSION_TIMEOUT};
use crate::redact::redact_all;
use crate::APP_NAME;

#[derive(Debug, clap::Args)]
pub struct HarnessArgs {
    #[command(subcommand)]
    pub command: HarnessCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum HarnessCommand {
    /// List the known harnesses: installed or not, path, version, and the backends using them.
    List,
    /// Print the official install command(s) of a harness. Nothing is run.
    Install {
        /// Harness name (`claude` or `command`).
        name: String,
    },
}

pub fn run(args: HarnessArgs) -> Result<ExitCode> {
    match args.command {
        HarnessCommand::List => {
            print!("{}", list());
            Ok(ExitCode::SUCCESS)
        }
        HarnessCommand::Install { name } => match install_text(&name) {
            Ok(text) => {
                print!("{text}");
                Ok(ExitCode::SUCCESS)
            }
            Err(e) => {
                eprintln!("error: {e}");
                Ok(ExitCode::FAILURE)
            }
        },
    }
}

/// One line of `harness list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub harness: HarnessKind,
    /// `None`: not applicable (command harness). `Some(Ok(path))`: installed.
    pub installed: Option<std::result::Result<String, String>>,
    /// `--version` output or why it failed (only when installed).
    pub version: Option<std::result::Result<String, String>>,
    /// Configured backends that use this harness.
    pub used_by: Vec<String>,
}

fn list() -> String {
    let cfg = match Config::load() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!(
                "warning: config.toml: {}; showing defaults",
                redact_all(&format!("{e:#}"))
            );
            None
        }
    };
    let claude_path = cfg.as_ref().and_then(|c| c.claude_path.clone());
    let backends = cfg
        .as_ref()
        .and_then(|c| match crate::backends::resolve(c) {
            Ok(b) => Some(b),
            Err(e) => {
                eprintln!(
                    "warning: [backends] is invalid ({}); \"used by\" is left empty \
                     (run `agent-sidekick backends list` for details)",
                    redact_all(&e)
                );
                None
            }
        });
    let rows: Vec<Row> = HarnessKind::ALL
        .into_iter()
        .map(|h| {
            let adapter = h.adapter();
            let installed = adapter.locate_installed(claude_path.as_deref());
            let version = match &installed {
                Some(Ok(path)) => Some(adapter.version(path)),
                _ => None,
            };
            Row {
                harness: h,
                installed: installed.map(|r| r.map(|p| p.display().to_string())),
                version,
                used_by: backends
                    .iter()
                    .flat_map(|b| b.iter())
                    .filter(|b| b.harness_kind() == h)
                    .map(|b| b.name.clone())
                    .collect(),
            }
        })
        .collect();
    format_list(&rows)
}

/// The `harness list` output (redacted).
pub fn format_list(rows: &[Row]) -> String {
    let mut out = String::new();
    for r in rows {
        let name = r.harness.as_str();
        match &r.installed {
            None => out.push_str(&format!(
                "{name}: n/a (generic; the program is configured per backend in `command[0]`; \
                 `{APP_NAME} backends test <name>` checks it)\n"
            )),
            Some(Ok(path)) => {
                out.push_str(&format!("{name}: installed\n"));
                out.push_str(&format!("  path:    {path}\n"));
                match &r.version {
                    Some(Ok(v)) if !v.is_empty() => out.push_str(&format!("  version: {v}\n")),
                    Some(Ok(_)) | None => out.push_str("  version: unknown\n"),
                    Some(Err(e)) => out.push_str(&format!(
                        "  version: unknown ({e}; timeout {}s)\n",
                        VERSION_TIMEOUT.as_secs()
                    )),
                }
            }
            Some(Err(e)) => {
                out.push_str(&format!("{name}: not installed ({e})\n"));
                out.push_str(&format!(
                    "  install: `{APP_NAME} harness install {name}` shows the official command\n"
                ));
            }
        }
        if !r.used_by.is_empty() {
            out.push_str(&format!("  used by: {}\n", r.used_by.join(", ")));
        }
    }
    redact_all(&out)
}

/// What `harness install <name>` prints, or an error for an unknown name.
pub fn install_text(name: &str) -> std::result::Result<String, String> {
    let Some(h) = HarnessKind::parse(name.trim()) else {
        return Err(format!(
            "unknown harness `{name}`; known: {}",
            HarnessKind::ALL.map(HarnessKind::as_str).join(", ")
        ));
    };
    Ok(match h.adapter().install_hint() {
        Some(hint) => {
            let mut s = format!(
                "Official install command(s) for harness `{h}` ({APP_NAME} does not run them):\n"
            );
            for line in hint.lines() {
                s.push_str("  ");
                s.push_str(line);
                s.push('\n');
            }
            s
        }
        None => format!(
            "Harness `{h}` has nothing to install: it runs the program named in `command[0]` of \
             each command backend; install that program as its vendor describes.\n"
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_text_claude_prints_official_commands() {
        let t = install_text("claude").unwrap();
        assert!(t.contains("does not run them"), "{t}");
        assert!(t.contains("  macOS, Linux, WSL:   curl -fsSL https://claude.ai/install.sh | bash"));
        assert!(t.contains("irm https://claude.ai/install.ps1 | iex"));
    }

    #[test]
    fn install_text_command_is_not_applicable() {
        let t = install_text("command").unwrap();
        assert!(t.contains("nothing to install"), "{t}");
    }

    #[test]
    fn install_text_unknown_harness() {
        let e = install_text("opencode").unwrap_err();
        assert_eq!(e, "unknown harness `opencode`; known: claude, command");
    }

    #[test]
    fn format_list_variants() {
        let rows = vec![
            Row {
                harness: HarnessKind::Claude,
                installed: Some(Ok("/usr/bin/claude".into())),
                version: Some(Ok("2.1.296 (Claude Code)".into())),
                used_by: vec!["sidekick".into(), "local".into()],
            },
            Row {
                harness: HarnessKind::Command,
                installed: None,
                version: None,
                used_by: vec![],
            },
        ];
        let t = format_list(&rows);
        assert!(
            t.contains("claude: installed\n  path:    /usr/bin/claude\n"),
            "{t}"
        );
        assert!(t.contains("  version: 2.1.296 (Claude Code)\n"), "{t}");
        assert!(t.contains("  used by: sidekick, local\n"), "{t}");
        assert!(t.contains("command: n/a (generic"), "{t}");

        let missing = vec![Row {
            harness: HarnessKind::Claude,
            installed: Some(Err("could not find the `claude` executable".into())),
            version: None,
            used_by: vec![],
        }];
        let t = format_list(&missing);
        assert!(t.starts_with("claude: not installed ("), "{t}");
        assert!(t.contains("harness install claude"), "{t}");
    }
}
