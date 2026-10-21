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

## Backends: assumptions and additions beyond the spec

Additions (not in the user's spec):

- `auth = "x-api-key" | "bearer"` for `claude-endpoint` (default `x-api-key`, i.e. `ANTHROPIC_API_KEY`; `bearer` sets `ANTHROPIC_AUTH_TOKEN`). Needed for Ollama's cloud and LM Studio with authentication, whose docs use the Bearer form.
- `env = { ... }` per backend (all types): extra non-secret variables, applied after the removal rules, so a configured `CLAUDE_CODE_*` variable (e.g. `CLAUDE_CODE_ATTRIBUTION_HEADER = "0"` for LM Studio) survives. Reserved names (config error): `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_BASE_URL`, `CLAUDE_CODE_OAUTH_TOKEN`, `CLAUDE_CONFIG_DIR`, `AGENT_SIDEKICK_*` (case-insensitive).
- `pass_env` (top level and per backend), justified by the session-isolation finding below: the removal rules are broad (`CLAUDE_CODE_*` prefix), so users need a way to keep e.g. `CLAUDE_CODE_MAX_OUTPUT_TOKENS`. It only matters for names that would be removed; other inherited variables are passed anyway. Never passable: `CLAUDE_CODE_OAUTH_TOKEN`, `AGENT_SIDEKICK_TOKEN`. `ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN` only for `command` backends (config error at top level and for `claude-*`). `CLAUDE_CONFIG_DIR` is silently not passed to `claude-*`, `ANTHROPIC_BASE_URL` not to `claude-endpoint` (both set their own).
- `model` per `claude-*` backend (default `--model`; the `sidekick_run` parameter overrides it). Not allowed for `command` (config error); the run-time `model`/`allowed_tools` parameters are ignored for command backends with a note in the log header.
- `backends list --check`: `list` alone is fast (no probes), `--check` adds reachability.
- `cost` is optional with type defaults: `claude-oauth` `quota`, `claude-endpoint` `paid` (a remote endpoint may bill; set `free` for local servers), `command` `free`.
- `max_concurrent` per backend is acquired before the global slot; a fallback releases the primary's slots before acquiring its own.

Decisions:

- Fallback from a `claude-oauth` backend to another `claude-oauth` backend is rejected with an explanation: it would amount to account switching / rate-limit circumvention, a v0.1 non-goal. Fallback after a non-zero exit is allowed, but the result states that the primary ran and may have changed files in `cwd`, with exit code and log path. Never after timeout or cancellation; single level.
- Config errors: v0.1 ran with default settings when `config.toml` was invalid. Now `sidekick_run` fails with the config problem (no silent rerouting to the implicit backend); `sidekick_backends`/`sidekick_status` show it. Behaviour change, listed under Changed in the CHANGELOG.
- Redaction: a secret is added to the redaction set only with at least 8 characters, so a placeholder key such as `ollama` does not mangle the agent's output. `sk-ant-` patterns are always redacted. `setup --backend <endpoint>` warns when storing a shorter key.
- `setup --backend <name>` only stores that backend's secret; it never touches the MCP registration. Plain `setup` asks for every `claude-oauth` token (one prompt per backend), never for endpoint keys.
- Secret locations: claude-oauth `sidekick` -> keyring user `oauth-token`, file `token`, env `AGENT_SIDEKICK_TOKEN`; other claude-oauth -> `oauth-token-<name>`, `token-<name>`, no env override; claude-endpoint -> `endpoint-<name>`, `endpoint-<name>.key` (or `api_key_env`); command -> none.
- An explicitly declared `claude-oauth` backend named `sidekick` shares the legacy secret and the legacy config dir (`AGENT_SIDEKICK_CLAUDE_CONFIG_DIR` or `~/.claude-sidekick`), so adding `[backends]` to an existing installation needs no new token.
- `config_dir` of a `claude-endpoint` must differ from every `claude-oauth` config dir (compared lexically, case-insensitive on Windows/macOS): a claude.ai login saved there would stay active. Default `~/.claude-sidekick-<name>`; configured dirs must be absolute or start with `~`.
- `api_key_env` may not name `CLAUDE_CODE_OAUTH_TOKEN` or `AGENT_SIDEKICK_TOKEN`; `CLAUDE_CODE_OAUTH_TOKEN` is removed from an endpoint's environment as a last step (defence in depth).
- Command program lookup: a bare `command[0]` is searched only in absolute `PATH` entries (empty or relative entries are ignored, so nothing is picked up from the server's cwd); a relative path with a separator is rejected; `.cmd`/`.bat` require `prompt_via_stdin = true`.
- Reachability probe (new network access): `GET <base_url>` exactly as configured, no credentials (userinfo stripped), no redirects, 2 s global timeout, body never read; any HTTP status counts as reachable. Only for `sidekick_backends`, `backends list --check`/`backends test`, `doctor` and right before a `claude-endpoint` run. Loopback hosts bypass proxies; other hosts honour `ALL_PROXY`/`HTTPS_PROXY`/`HTTP_PROXY`/`NO_PROXY` like `claude` would, so a proxy answering 502 for a dead upstream counts as reachable.
- HTTP client: `ureq` 3 with `rustls` (crypto provider `ring`), no default features, sync (run in `spawn_blocking`). `ring` needs a C compiler for non-host targets: the local Linux cross-clippy on Windows uses `CC_x86_64_unknown_linux_gnu=gcc AR_x86_64_unknown_linux_gnu=ar` (Strawberry Perl's gcc). CI builds natively and needs nothing extra.
- Aider example flags, checked against aider.chat/docs/config/options.html and aider.chat/docs/llms/ollama.html (2026-10-21): `--message` (`-m`, process one message then exit), `--yes-always`, `--no-auto-commits`, `--no-check-update`, `--model ollama_chat/<model>` (`ollama_chat/` is recommended over `ollama/`), `OLLAMA_API_BASE` for the Ollama URL. Aider has no turn-limit flag, so the example does not use `{max_turns}`. Not run locally.
- Endpoint examples from vendor docs, not probed: Ollama >= 0.14 serves the Anthropic Messages API at `http://localhost:11434`, a local server ignores the key ("required but ignored"), Ollama's cloud (`https://ollama.com`) uses `Authorization: Bearer`. LM Studio's Claude Code docs use `http://localhost:1234`, `ANTHROPIC_AUTH_TOKEN` (Bearer) and `CLAUDE_CODE_ATTRIBUTION_HEADER=0`; whether LM Studio accepts `x-api-key` was not checked, hence `auth = "bearer"` in the docs.

## Verified facts (claude 2.1.296)

Probed against a local fake Anthropic API (no quota used):

- Fresh `CLAUDE_CONFIG_DIR` + `ANTHROPIC_BASE_URL` + `ANTHROPIC_API_KEY` in a clean environment: works headless (`-p`); claude sends the key as `x-api-key`.
- Same with `ANTHROPIC_AUTH_TOKEN`: works; sent as `Authorization: Bearer`.
- Same with `CLAUDE_CODE_OAUTH_TOKEN` (+ `ANTHROPIC_BASE_URL`): works; sent as `Authorization: Bearer`.
- With the environment inherited from a Claude Code VS Code session (`CLAUDECODE`, `CLAUDE_CODE_ENTRYPOINT=claude-vscode`, `CLAUDE_CODE_SESSION_ID`, `CLAUDE_CODE_CHILD_SESSION`, `CLAUDE_CODE_SESSION_ATTENDED`, `CLAUDE_CODE_MESSAGING_SOCKET`, `CLAUDE_CODE_MESSAGING_TOKEN`, `CLAUDE_CODE_EXECPATH`, `CLAUDE_PID`, `CLAUDE_AGENT_SDK_VERSION`, `CLAUDE_EFFORT`, `AI_AGENT`, `MCP_CONNECTION_NONBLOCKING`, ...): `ANTHROPIC_API_KEY` mode fails with "Not logged in · Please run /login"; OAuth mode works, but the child identifies as "claude-vscode, agent-sdk" and receives the parent's messaging socket and token. The MCP server inherits these variables from Claude Code, so the child must not get them; hence the removal rules for every backend type. `ANTHROPIC_API_KEY` in `-p` mode therefore only works with a clean environment.
- On startup claude sends `HEAD <base_url>/api/hello` (no credentials) before `POST /v1/messages?beta=true`. (This is claude's own request, not agent-sidekick's probe, which is a `GET <base_url>`.)

## Verification status

TODO: filled in by the orchestrator at the end.
