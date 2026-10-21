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
- Further removal rules added after the review: `ANTHROPIC_CUSTOM_HEADERS` for `claude-endpoint` and `command` (a caller's gateway credential must not reach a third-party `base_url`; kept for `claude-oauth`); the model overrides `ANTHROPIC_MODEL`, `ANTHROPIC_SMALL_FAST_MODEL`, `ANTHROPIC_DEFAULT_*_MODEL` for `claude-endpoint` (the backend has its own `model`; set them in `env` if needed, as in the LiteLLM section below); every `api_key_env` name of any backend for all other backends (case-insensitive on Windows). All re-allowable through the backend's own `pass_env`. `ANTHROPIC_*` is deliberately not blanket-dropped for `command` backends (Aider may use it). `CLAUDE_CODE_GIT_BASH_PATH` is exempt from the `CLAUDE_CODE_*` prefix rule: it is host configuration that Claude Code on Windows needs to find Git Bash, not session state or a credential.
- `model` per `claude-*` backend (default `--model`; the `sidekick_run` parameter overrides it). Not allowed for `command` (config error); the run-time `model`/`allowed_tools` parameters are ignored for command backends with a note in the log header.
- `backends list --check`: `list` alone is fast (no probes), `--check` adds reachability.
- `cost` is optional with type defaults: `claude-oauth` `quota`, `claude-endpoint` `paid` (a remote endpoint may bill; set `free` for local servers), `command` `free`.
- `max_concurrent` per backend is acquired before the global slot; a fallback releases the primary's slots before acquiring its own.

Decisions:

- Fallback from a `claude-oauth` backend to another `claude-oauth` backend is rejected with an explanation: it would amount to account switching / rate-limit circumvention, a v0.1 non-goal. Fallback after a non-zero exit is allowed, but the result states that the primary ran and may have changed files in `cwd`, with exit code and log path. Never after timeout or cancellation; single level.
- Config errors: v0.1 ran with default settings when `config.toml` was invalid. Now `sidekick_run` fails with the config problem (no silent rerouting to the implicit backend); `sidekick_backends`/`sidekick_status` show it. Behaviour change, listed under Changed in the CHANGELOG.
- Redaction: a secret is added to the redaction set only with at least 8 characters, so a placeholder key such as `ollama` does not mangle the agent's output. `sk-ant-` patterns are always redacted. `setup --backend <endpoint>` warns when storing a shorter key.
- `setup --backend <name>` only stores that backend's secret; it never touches the MCP registration. Plain `setup` asks for the token of every `claude-oauth` backend that has none stored yet (one prompt per backend), never for endpoint keys. `setup --register-only` skips all secret steps and the terminal requirement and only re-registers, so several `claude-oauth` backends can be set up non-interactively (`setup --backend <name> --token-stdin` per backend, then `setup --register-only`).
- Re-registration (`claude mcp remove` + `claude mcp add`) carries over the extra `-e` variables of the existing user-scope entry (read from `claude mcp get`), except `AGENT_SIDEKICK_CLAUDE` (set anew) and `CLAUDE_CODE_OAUTH_TOKEN`/`AGENT_SIDEKICK_TOKEN` (dropped with a warning). Only names are printed. A local- or project-scope entry that shadows the user entry is not copied.
- Run logs: on Unix the logs directory is created with mode 0700 and each run log with 0600 (they contain the prompt and the complete output, and secrets shorter than 8 characters are not redacted). Windows relies on the user-profile ACL.
- A fallback attempt gets its own full `timeout_s` (worst case about 2 x `timeout_s` plus queue time); the progress ticker tracks each attempt separately and reports the fallback as queued while it waits.
- `sidekick_status`: the global `default model` and `agent claude config dir` lines only for the implicit backend; with `[backends]` each `claude-*` backend line shows its effective `model` and `config_dir`.
- Secret locations: claude-oauth `sidekick` -> keyring user `oauth-token`, file `token`, env `AGENT_SIDEKICK_TOKEN`; other claude-oauth -> `oauth-token-<name>`, `token-<name>`, no env override; claude-endpoint -> `endpoint-<name>`, `endpoint-<name>.key` (or `api_key_env`); command with `secret_env` -> `cmd-<name>`, `cmd-<name>.key`, no env override; command without `secret_env` -> none.
- `secret_env` (command backends, requested by the user after wave 2):
  - Validation: a portable name `[A-Za-z_][A-Za-z0-9_]*` (stricter than the `env`/`pass_env` check, which only rejects empty names, `=` and NUL); not `CLAUDE_CODE_OAUTH_TOKEN`, `AGENT_SIDEKICK_TOKEN` or `AGENT_SIDEKICK_*` (case-insensitive); not also a key of the backend's `env` table (case-insensitive, because Windows variable names are). `ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN` are allowed (Aider with Claude models); the inherited caller key is still removed and replaced by the stored secret. Naming the same variable in `pass_env` is not an error; the stored secret wins.
  - The variable is set after the removal rules, `pass_env` and `env`, so it replaces any inherited value (case-insensitive on Windows). A credential that does not match the backend (no secret for a backend with `secret_env`, a command secret for a backend without it, or any claude credential) puts nothing into the environment and `build_launch` refuses it.
  - No environment override (unlike the legacy token): the value is read only from the keyring/file store, so a stray variable in the MCP server's environment can never be mistaken for it.
  - The run log header names the variable only (`secret_env: VAR (stored secret, value not shown)`); `build_launch` does not know whether the value came from the keyring or the file. Status, `sidekick_backends`, `backends list --check`/`test` and `doctor` show only the source.
  - Values shorter than 8 characters are passed but not redacted (same rule as endpoint keys); `setup` warns.
  - Plain `setup` never prompts for command secrets; it prints a hint to `setup --backend <name>`. `--token-stdin` works with `setup --backend <name>`.
  - `uninstall` deletes `cmd-<name>` for every configured command backend, also those without `secret_env` (a secret may have been stored before `secret_env` was removed), like endpoint keys with `api_key_env`.
  - Reachability: the program is checked first, then the secret; `doctor` suggests `setup --backend <name>` only when the program was found and the secret is missing.
  - The black-box test (`tests/command_secret.rs`) ran on Windows only; the Unix variant (sh script) was compiled via the Linux cross-clippy but not executed.
- An explicitly declared `claude-oauth` backend named `sidekick` shares the legacy secret and the legacy config dir (`AGENT_SIDEKICK_CLAUDE_CONFIG_DIR` or `~/.claude-sidekick`), so adding `[backends]` to an existing installation needs no new token.
- `config_dir` of a `claude-endpoint` must differ from every `claude-oauth` config dir (compared lexically, case-insensitive on Windows/macOS): a claude.ai login saved there would stay active. Default `~/.claude-sidekick-<name>`; configured dirs must be absolute or start with `~`.
- `api_key_env` may not name `CLAUDE_CODE_OAUTH_TOKEN` or `AGENT_SIDEKICK_TOKEN`; `CLAUDE_CODE_OAUTH_TOKEN` is removed from an endpoint's environment as a last step (defence in depth).
- Command program lookup: a bare `command[0]` is searched only in absolute `PATH` entries (empty or relative entries are ignored, so nothing is picked up from the server's cwd); a relative path with a separator is rejected; `.cmd`/`.bat` require `prompt_via_stdin = true`. Placeholders are substituted only after `command[0]`, so `{prompt}` must be in a later argument and any placeholder in `command[0]` is a config error.
- `base_url` validation: credentials (`user:password@`) are a config error, and the URL is checked with the same parsing as the reachability probe (ASCII host, so IDN hosts must be written in `xn--` form; numeric port in range), so a URL the probe cannot handle fails at config load instead of at every run.
- Reachability probe (new network access): `GET <base_url>` exactly as configured, no credentials (config validation rejects userinfo in `base_url`; the probe would strip it as well), no redirects, 2 s global timeout, body never read; any HTTP status counts as reachable. Only for `sidekick_backends`, `backends list --check`/`backends test`, `doctor` and right before a `claude-endpoint` run. Loopback hosts bypass proxies; other hosts honour `ALL_PROXY`/`HTTPS_PROXY`/`HTTP_PROXY`/`NO_PROXY` like `claude` would, so a proxy answering 502 for a dead upstream counts as reachable.
- HTTP client: `ureq` 3 with `rustls` (crypto provider `ring`), no default features, sync (run in `spawn_blocking`). `ring` needs a C compiler for non-host targets: the local Linux cross-clippy on Windows uses `CC_x86_64_unknown_linux_gnu=gcc AR_x86_64_unknown_linux_gnu=ar` (Strawberry Perl's gcc). CI and release jobs build natively, except `x86_64-apple-darwin`, which is cross-compiled on the arm64 macOS runner (Apple clang handles `-arch x86_64`, so `ring` should build there via `cc`); not exercised yet, the release workflow has never run.
- Aider example flags, checked against aider.chat/docs/config/options.html and aider.chat/docs/llms/ollama.html (2026-10-21): `--message` (`-m`, process one message then exit), `--yes-always`, `--no-auto-commits`, `--no-check-update`, `--model ollama_chat/<model>` (`ollama_chat/` is recommended over `ollama/`), `OLLAMA_API_BASE` for the Ollama URL. Aider has no turn-limit flag, so the example does not use `{max_turns}`. Not run locally.
- Endpoint examples from vendor docs, not probed: Ollama >= 0.14 serves the Anthropic Messages API at `http://localhost:11434`, a local server ignores the key ("required but ignored"), Ollama's cloud (`https://ollama.com`) uses `Authorization: Bearer`. LM Studio's Claude Code docs use `http://localhost:1234`, `ANTHROPIC_AUTH_TOKEN` (Bearer) and `CLAUDE_CODE_ATTRIBUTION_HEADER=0`; whether LM Studio accepts `x-api-key` was not checked, hence `auth = "bearer"` in the docs.

## Verified facts (claude 2.1.296)

Probed against a local fake Anthropic API (no quota used):

- Fresh `CLAUDE_CONFIG_DIR` + `ANTHROPIC_BASE_URL` + `ANTHROPIC_API_KEY` in a clean environment: works headless (`-p`); claude sends the key as `x-api-key`.
- Same with `ANTHROPIC_AUTH_TOKEN`: works; sent as `Authorization: Bearer`.
- Same with `CLAUDE_CODE_OAUTH_TOKEN` (+ `ANTHROPIC_BASE_URL`): works; sent as `Authorization: Bearer`.
- With the environment inherited from a Claude Code VS Code session (`CLAUDECODE`, `CLAUDE_CODE_ENTRYPOINT=claude-vscode`, `CLAUDE_CODE_SESSION_ID`, `CLAUDE_CODE_CHILD_SESSION`, `CLAUDE_CODE_SESSION_ATTENDED`, `CLAUDE_CODE_MESSAGING_SOCKET`, `CLAUDE_CODE_MESSAGING_TOKEN`, `CLAUDE_CODE_EXECPATH`, `CLAUDE_PID`, `CLAUDE_AGENT_SDK_VERSION`, `CLAUDE_EFFORT`, `AI_AGENT`, `MCP_CONNECTION_NONBLOCKING`, ...): `ANTHROPIC_API_KEY` mode fails with "Not logged in · Please run /login"; OAuth mode works, but the child identifies as "claude-vscode, agent-sdk" and receives the parent's messaging socket and token. The MCP server inherits these variables from Claude Code, so the child must not get them; hence the removal rules for every backend type. `ANTHROPIC_API_KEY` in `-p` mode therefore only works with a clean environment.
- On startup claude sends `HEAD <base_url>/api/hello` (no credentials) before `POST /v1/messages?beta=true`. (This is claude's own request, not agent-sidekick's probe, which is a `GET <base_url>`.)

## OpenAI-compatible providers (KI:connect.nrw) and the LiteLLM proxy

Probed on 2026-10-21 (no agent runs).

**Test kit:** `C:\clones\aider-kiconnect-test` (local, not part of this repository) holds the manual
tests for KI:connect: a small aider exercise (`README.md`, `calc.py`, `test_calc.py`) for the command
backend, and in `litellm\` the LiteLLM proxy config (`litellm_config.yaml`, `README.md`) and the probe scripts `tooltest.py` / `streamtest.py` for tool use (non-streaming and streaming). They need
a valid KI:connect key and are run by the user.

**KI:connect.nrw** (`https://chat.kiconnect.nrw/api/v1`, vLLM 0.31 behind a gateway):
- OpenAI-compatible only: `GET /models` and `POST /chat/completions` work with `Authorization: Bearer`.
- No Anthropic Messages API: `POST …/messages` returns the web app's HTML 404 with Bearer, and 401
  "No API key provided" with `x-api-key`. So it can NOT be used directly as a `claude-endpoint` backend.
- Models seen: `openai-gpt-oss-120b`, `qwen-qwen3-8-27b`, `Mistral Small 3.2 24B Instruct 2506`
  (id contains spaces), plus embedding models.
- gpt-oss is a reasoning model: a 40-token answer was used up by reasoning (`finish_reason: length`),
  so budgets must be generous (the requested `max_tokens 2000` for probes).

**Option A: `command` backend with aider.** aider 0.86.2 via `uvx --python 3.12 --from aider-chat aider`
works (`--model openai/<id>`, `OPENAI_API_BASE`, `OPENAI_API_KEY`). On Windows it crashes on cp1252
consoles unless `PYTHONUTF8=1` is set. A full aider run against KI:connect was not verified here
(the harness refused to start an autonomous agent with `--yes-always`). The user runs it manually
(see the test kit in `C:\clones\aider-kiconnect-test`).

**Option B: `claude-endpoint` via a LiteLLM proxy** (`uvx --from 'litellm[proxy]' litellm`):
- Use the provider prefix `hosted_vllm/<model>`, NOT `openai/<model>`: with `openai/`, LiteLLM forwards
  Anthropic `/v1/messages` to the OpenAI **Responses API** (`/v1/responses`), which an OpenAI-compatible
  vLLM gateway may not offer. With `hosted_vllm/` it uses `/chat/completions`.
- Verified: the proxy starts (about 20 s on first `uvx` download, then a few seconds). `POST /v1/messages` with
  `max_tokens 2000` is routed to KI:connect. With the rotated (invalid) key the upstream answered 401
  `invalid_auth_header`, passed through as an Anthropic `authentication_error`.
- **Tool use through the proxy: works (verified against a local fake OpenAI upstream)**, both
  non-streaming and streaming (Claude Code always streams):
  - Anthropic `tools` become OpenAI `tools` (`type: function`), and `max_tokens` is passed through.
  - The upstream's `tool_calls`, also when streamed in argument fragments, become one `tool_use` block
    with correct `input`, and `stop_reason: tool_use`. The SSE sequence is `message_start`,
    `content_block_start`, `content_block_delta`×n (`input_json_delta`), `content_block_stop`,
    `message_delta`, `message_stop`.
  - The follow-up `tool_result` becomes a `role: tool` message, and the final text ends with `end_turn`.
- **Not verified: whether KI:connect's models actually emit tool calls.** vLLM only does that when the
  operator enables `--enable-auto-tool-choice` with a tool-call parser. Checking it needs a valid key,
  so the user runs `tooltest.py` / `streamtest.py` from `C:\clones\aider-kiconnect-test\litellm`.
- Backend config: `base_url = "http://127.0.0.1:4000"`, `model = "<litellm model_name>"`, API key =
  LiteLLM master key (`setup --backend`). Map Claude Code's aliases to the same model via the backend `env`
  table: `ANTHROPIC_DEFAULT_HAIKU_MODEL` (background tasks), `ANTHROPIC_DEFAULT_SONNET_MODEL`,
  `ANTHROPIC_DEFAULT_OPUS_MODEL` and `CLAUDE_CODE_SUBAGENT_MODEL`, names verified in the Claude Code docs.
  The `env` table is applied after session isolation, so these `CLAUDE_CODE_*` and `ANTHROPIC_*` names
  are allowed there. The provider key stays in the proxy's environment only.
- The proxy is an extra runtime process (Python via uvx). agent-sidekick does not start or manage it.

## Verification status

TODO: filled in by the orchestrator at the end.
