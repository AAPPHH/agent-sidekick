# Development Guide

## 1. Prerequisites

- Rust **>= 1.89** (as specified in `Cargo.toml`).
- On Windows use the **MSVC** toolchain.
- Optional: WSL Ubuntu to run the Linux tests on a Windows machine.

## 2. Build and run

```bash
cargo build
cargo run -- serve      # starts the MCP server on stdio
cargo install --path . --locked
```

## 3. Checks (CI)

```bash
# formatting
cargo fmt --all -- --check

# linting
cargo clippy --locked --all-targets -- -D warnings

# tests (Linux, Windows, macOS)
cargo test --locked --all-targets
```

The CI also runs:

- `shellcheck` on `install.sh` and all `scripts/*.sh`
- `PSScriptAnalyzer` (`Invoke-ScriptAnalyzer`) on `install.ps1` and `scripts/package.ps1`
- An **install dry-run** that builds the release binary for:
  - `x86_64-unknown-linux-musl`
  - `x86_64-pc-windows-msvc`
  then packages it with `scripts/package.sh` / `scripts/package.ps1` and installs from the local artifact directory.

## 4. Tests

Unit tests live next to the code in `src/`.  
When running any tests set the environment variable so the real OS keyring is never touched:

```powershell
$env:AGENT_SIDEKICK_DISABLE_KEYRING = "1"
```

```bash
export AGENT_SIDEKICK_DISABLE_KEYRING=1
```

The end-to-end test `tests/e2e_fake_api.rs` only runs when:

```bash
export AGENT_SIDEKICK_E2E=1
```
and requires a real `claude` CLI.

### Test suite overview

| File | Description |
|------|-------------|
| `tests/backends_fallback.rs` | `sidekick_run` with configured backends: backend selection, fallback rules, model allowlists, placeholder substitution. |
| `tests/command_secret.rs` | `secret_env` of command backends: stored with `setup --backend <name> --token-stdin`, handed to the program in that variable, never shown in results, status or logs. |
| `tests/e2e_fake_api.rs` | End-to-end: the real `claude` CLI, launched through `agent-sidekick serve`, talks to a local fake Anthropic API (opt-in, `AGENT_SIDEKICK_E2E=1`). |
| `tests/mcp_handshake.rs` | Speaks raw newline-delimited JSON-RPC to `agent-sidekick serve` over stdio. |
| `tests/opencode_fake.rs` | The opencode harness through `agent-sidekick serve` with a fake `opencode`: argv, environment, config isolation, output, redaction. |
| `tests/runner_fake_claude.rs` | Runs `agent_sidekick::runner::run` against a generated fake `claude` script. |
| `tests/runner_linux_setsid.rs` | Linux only: processes that leave the agent's process group with `setsid` are still killed, on timeout and after a normal exit. |
| `tests/update_command.rs` | `agent-sidekick update` and the doctor update hint, run as a binary against a fake release API. |

## 5. Project layout

| File | Description |
|------|-------------|
| `src/cli.rs` | command-line definitions (clap) for serve, setup, doctor, uninstall, backends, harness, update |
| `src/config.rs` | Optional `config.toml` with default values. |
| `src/lib.rs` | Library root: the local MCP server (stdio) that delegates prompts to a second, headless agent. |
| `src/locate.rs` | Finding the `claude` executable, also when the MCP host starts the server with a reduced PATH. |
| `src/logging.rs` | Tracing setup: logs go to stderr and to `<config-dir>/logs/agent-sidekick.log`, never to stdout. |
| `src/main.rs` | binary entry point |
| `src/paths.rs` | Filesystem locations; every location can be overridden through an environment variable. |
| `src/redact.rs` | Removes secrets from text before it reaches logs, errors or tool responses. |
| `src/runner.rs` | Runs the agent CLI as a child process with timeout, cancellation and kill-tree. |
| `src/server.rs` | MCP server (rmcp, stdio) exposing `sidekick_run`, `sidekick_backends` and `sidekick_status`. |
| `src/token.rs` | Secret storage (OAuth tokens, API keys): OS keyring first, a 0600 file in the config directory as fallback. |
| `src/backends/mod.rs` | Configurable backends: where a `sidekick_run` is executed. |
| `src/commands/mod.rs` | Interactive subcommands: setup, doctor, uninstall, backends, harness, update. |
| `src/harness/mod.rs` | Harness adapters (claude, opencode, command; codex stub): how agent-sidekick drives each agent CLI. |

## 6. Further reading

- `README.md` – installation and first steps for users (German); `docs/REFERENCE.md` – technical reference (backends, configuration, tools, security).  
- `NOTES.md` – design decisions and verification notes.  
- `docs/RELEASING.md` – release process.
