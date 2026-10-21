# Notes

## Assumptions

- Release archives contain the binary, `README.md`, `LICENSE-MIT` and `LICENSE-APACHE` at the archive root (no top-level directory); `install.sh`/`install.ps1` rely on that (`install.ps1` also searches recursively).
- `SHA256SUMS` lists bare file names (`<hash>  <file>`); both install scripts look up the line for their archive instead of running `sha256sum -c` on the whole file.
- Windows on ARM64 uses the x86_64 build via emulation; no `aarch64-pc-windows-msvc` build is published.
- `rust-version = "1.89"`: the highest `rust-version` among locked dependencies (`uuid` 1.28, `aes`). Local toolchain at the time: rustc 1.99.1. Declaring an MSRV enables clippy's `incompatible_msrv` lint, which will flag std APIs newer than 1.89 in our code; if that happens, bump `rust-version` rather than silencing the lint.
- Linux keyring backend is pure Rust (`zbus`, `zbus-secret-service-keyring-store`): `Cargo.lock` contains no `dbus`/`libdbus-sys`, so CI and release builds need no system packages.
- License: dual MIT OR Apache-2.0; copyright holder "The agent-sidekick contributors".

## Verified flag names

Against claude 2.1.296:

- `-p/--print` is a boolean flag; the prompt is positional.
- `--max-turns <n>` is accepted but hidden from `--help`.
- `--model <m>`.
- `--allowedTools` / `--allowed-tools` is variadic.
- `--` is required before a prompt starting with `-` (verified: `claude -p "-x hi"` -> "unknown option").
- `-p` reads the prompt from stdin when no positional prompt is given.
- `claude mcp add [options] <name> <commandOrUrl> [args...]` with `-s/--scope <local|user|project>` and `-e/--env <env...>` (variadic).
- `claude mcp remove [-s scope] <name>`.
- `claude setup-token` subcommand exists.

Claude Code MCP timeouts (code.claude.com/docs/en/env-vars, checked 2026-10-21):

- `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT`: idle window in ms; a tool call aborts when the server sends neither a response nor a progress notification for that long. Default 1800000 (30 min) for stdio servers; stdio servers are subject to it since v2.1.203. Hence the 30 s progress notifications.
- `MCP_TOOL_TIMEOUT`: overall tool-call timeout in ms, default 100000000 (about 28 h), above the 7200 s `timeout_s` maximum.

## Design decisions

- keyring 4.2 with its default v1 API (`Entry::new` / `set_password` / `get_password` / `delete_credential`).
- rmcp 3.6 (server, transport-io, macros) for the MCP stdio server.
- Manual release matrix instead of cargo-dist: `cargo dist init` could not be run or verified offline in this environment, and the manual workflow is small, transparent and covers exactly the five targets (x86_64/aarch64 Linux, x86_64 Windows, x86_64/aarch64 macOS). `aarch64-unknown-linux-gnu` is built natively on `ubuntu-24.04-arm` (no cross toolchain); `x86_64-apple-darwin` is cross-compiled on the arm64 `macos-latest` runner.
- On Windows, when `claude` resolves to a `.cmd` shim, the prompt is passed via stdin to avoid `cmd.exe` argument quoting/injection problems.
- CI sets `AGENT_SIDEKICK_DISABLE_KEYRING=1` at job level so tests never touch a runner's keyring.

## Open points

- The repository URL must be set in: `install.sh` (`REPO` default), `install.ps1` (`$Repo` default), `README.md` (install URLs), and `Cargo.toml` (`repository` is intentionally left out until known). Search for `OWNER/agent-sidekick`.
- `ubuntu-24.04-arm` runners are free only for public repositories; for a private repo the aarch64-linux job needs a paid/larger runner or a switch to cross-compilation (e.g. `cross`).
- Windows token-file ACL: the user-only ACL is set with `icacls <file> /inheritance:r /grant:r USERDOMAIN\USERNAME:F` (`src/token.rs`, tested). If that fails (`USERNAME` unset, a principal `icacls` cannot resolve, e.g. some Azure AD/Microsoft accounts, or `icacls` missing), only a warning is logged and the file keeps the inherited user-profile ACL. A native `SetNamedSecurityInfo` implementation based on the current user's SID would remove the dependency on `icacls` and on name resolution.
- `shellcheck` was not available locally; `install.sh` was only checked with `sh -n`.
- The release workflow has not been run yet (needs a pushed `v*` tag on GitHub).

## Verification status

TODO: filled in by the orchestrator at the end.
