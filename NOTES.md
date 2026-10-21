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
- `model` per `claude-*` backend (default `--model`; the `sidekick_run` parameter overrides it). Since 0.3.0 also `models` (allowlist) on every backend type, and `model`/`models` on `command` backends together with the `{model}` placeholder (one backend per provider, model chosen per run). `model` or `models` without `{model}` in the command is a config error (the setting would silently do nothing); `{model}` without `models` too (no unchecked values in argv). A model outside the allowlist is rejected before the run starts, with no fallback; a fallback backend that does not offer the requested model uses its own default, and the result says so. Without `{model}`, a requested `model`/`allowed_tools` is ignored, and the note names only what was actually ignored (e.g. `model ignored (command backend)`, an accepted deviation from the spec's "keep the old note": the old text claimed both were ignored even when only one was given).
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
  Still open for the opencode harness too: see "Step 2 review follow-ups" (Harness adapters) for the
  tool-calling test and the mini run.
- Backend config: `base_url = "http://127.0.0.1:4000"`, `model = "<litellm model_name>"`, API key =
  LiteLLM master key (`setup --backend`). Map Claude Code's aliases to the same model via the backend `env`
  table: `ANTHROPIC_DEFAULT_HAIKU_MODEL` (background tasks), `ANTHROPIC_DEFAULT_SONNET_MODEL`,
  `ANTHROPIC_DEFAULT_OPUS_MODEL` and `CLAUDE_CODE_SUBAGENT_MODEL`, names verified in the Claude Code docs.
  The `env` table is applied after session isolation, so these `CLAUDE_CODE_*` and `ANTHROPIC_*` names
  are allowed there. The provider key stays in the proxy's environment only.
- The proxy is an extra runtime process (Python via uvx). agent-sidekick does not start or manage it.

## Verification status

State as of 2026-10-21, version 0.2.0.

### Verified automatically

- **Windows 11 (MSVC, rustc 1.99.1)**: `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, and
  `cargo test --locked` (267 unit tests plus the integration suites `backends_fallback`, `command_secret`,
  `e2e_fake_api`, `mcp_handshake`, `runner_fake_claude`) all pass. They ran twice in a row to check for flakiness.
- **Linux (WSL Ubuntu, native build, rustc 1.99.1)**: clippy with `-D warnings` and the full test suite pass,
  including the Unix-only tests: `sh` fake scripts, the SIGTERM shutdown test, process-group and subreaper cleanup
  (`runner_linux_setsid`), and 0600/0700 permissions.
- **macOS**: compiled only through CI (`ring` needs the Apple toolchain); not run locally.
- **End-to-end with the real claude CLI (2.1.296) against a local fake Anthropic API** (`AGENT_SIDEKICK_E2E=1 cargo test
  --test e2e_fake_api`, no quota, Windows):
  - implicit claude-oauth: the token arrives as `Bearer sk-ant-oat01-…`, and neither the tool result nor the run
    log contains it;
  - the inherited Claude Code session variables do not reach the child (its user agent no longer says
    `claude-vscode`);
  - claude-endpoint: the key is sent as `x-api-key` (or `Bearer` with `auth = "bearer"`), and no OAuth token or
    inherited `ANTHROPIC_API_KEY`/`ANTHROPIC_CUSTOM_HEADERS`/model override is sent.
- **MCP handshake** over stdio against the built binary: `initialize`, then `tools/list` returns `sidekick_run`,
  `sidekick_status` and `sidekick_backends` with the expected schema. Cancellation (`notifications/cancelled`) and
  progress notifications are covered as well.
- **Reviews**: three multi-agent reviews (v0.1, v0.2 and a final one), each finding checked by two independent
  verifiers. All confirmed findings are fixed, and `git log` records each fix.
- **Binary smoke tests**: `--version`, `--help`, `doctor` (finds `claude.exe` on PATH and reports the missing token
  and registration with next steps), and `backends list` with a claude-oauth + LiteLLM claude-endpoint config.

### Not verified, to be checked manually

1. **VS Code extension with a real token.** Run `CLAUDE_CONFIG_DIR=~/.claude-sidekick claude setup-token` (in
   PowerShell: `$env:CLAUDE_CONFIG_DIR="$HOME\.claude-sidekick"; claude setup-token`), then `agent-sidekick setup`,
   then "Developer: Reload Window". Then:
   - `/mcp` must list `agent-sidekick` as connected;
   - a short `sidekick_run` (e.g. "list the files in cwd", `cwd` = a scratch folder) must return `backend: sidekick
     (claude-oauth)` and exit code 0;
   - `sidekick_status` must show `token (sidekick): present`;
   - pressing Esc during a longer run must stop it, and `sidekick_status` must show running 0 afterwards.
2. **Real OS keyring**: the Windows Credential Manager entry `agent-sidekick` / `oauth-token` (and `cmd-<name>`,
   `endpoint-<name>`) was not written in tests. Check that setup reports "stored in OS keyring", and that
   `uninstall` removes the entry.
3. **aider against KI:connect.nrw**: run it yourself with the kit in `C:\clones\aider-kiconnect-test` (README
   there).
4. **Tool use of the KI:connect models through LiteLLM**: needs a valid key. Run `tooltest.py` /
   `streamtest.py` in `C:\clones\aider-kiconnect-test\litellm`. The proxy translation itself is verified (see above).
5. **GitHub Actions**: `ci.yml` and `release.yml` have never run, because there is no remote yet. Set the
   repository URL (`OWNER/agent-sidekick` placeholders in `install.sh`, `install.ps1` and the README, plus
   `repository` in Cargo.toml), push, then tag `v0.2.0`. The `ubuntu-24.04-arm` runner is free for public repos only.
6. **macOS** runtime behaviour (Keychain, process groups): CI only.

## Known flaky tests (Linux, under heavy load)

- `commands::claude_cli::tests::mcp_add_failure_redacts_values` failed once in WSL while two other builds ran in parallel; not reproducible in 21 later runs. Most likely cause: `ETXTBSY` (executing a freshly written script while another test thread forks with the write handle open, rust-lang/rust#114554). The test now retries that transient error.
- `backends::reach::tests::silent_server_times_out` failed under load with `unreachable: I/O error (operation interrupted)`: a signal (SIGCHLD from other tests' child processes) interrupted the blocking socket read (EINTR), and ureq does not retry it. This was a real bug: the server's reachability probes could fail the same way when a run's child exits during a probe. `probe_get` now retries `Interrupted` within the remaining timeout budget.
- `tests/backends_fallback.rs` `stopped_run_still_names_backend` hit the 30 s `wait_backend_line` deadline once in WSL while a full Windows test suite ran in parallel on the same machine; it passed 3/3 in isolation and in a full rerun. Load flake (the test waits for a line from a child process); test unchanged.

## Distribution (release, install, plugin, update)

### Install scripts and release

Owner decision: `Cargo.toml` has `repository = "https://github.com/AAPPHH/agent-sidekick"` (git user AAPPHH). It is
the only place to change the owner/repository. `scripts/render-installers.sh` reads it (plain `sed`, no cargo
needed) and writes release copies of `install.sh` / `install.ps1` with the placeholder `OWNER/agent-sidekick`
replaced; the release workflow attaches those copies. The source copies keep the placeholder and refuse to run
without `AGENT_SIDEKICK_REPO` or `AGENT_SIDEKICK_INSTALL_BASE_URL`, with an error naming both. This supersedes the
"Open points" item about setting the URL in four places; the README install URLs name AAPPHH directly (docs only).

Targets and asset names (one archive per target, `agent-sidekick-<target-triple>.<ext>`, files at the archive root):

| Asset | Notes |
|---|---|
| `agent-sidekick-x86_64-unknown-linux-musl.tar.gz` | static, `ubuntu-latest` + `musl-tools` |
| `agent-sidekick-aarch64-unknown-linux-musl.tar.gz` | static, native `ubuntu-24.04-arm` runner + `musl-tools` |
| `agent-sidekick-x86_64-pc-windows-msvc.zip` | static CRT (`.cargo/config.toml`), so no VC++ Redistributable is needed |
| `install.sh`, `install.ps1` | repository filled in |
| `SHA256SUMS` | `<sha256>  <file>` for all of the above, LF, no BOM |

- **musl rather than gnu on Linux.** keyring 4.2 on Linux uses `zbus-secret-service-keyring-store`, i.e. `zbus`
  (a pure-Rust D-Bus client) plus RustCrypto (`aes`, `cbc`, `hkdf`, `sha2`). There is no `libdbus-sys` or openssl in
  `Cargo.lock`. The only C code is `ring` (rustls), which builds with `musl-gcc` (`CC_<target>=musl-gcc` is set in
  the workflows). A static binary runs on any distribution and glibc version (old LTS, Alpine, containers). At
  runtime the Secret Service still needs a session D-Bus with a secret service (GNOME Keyring/KWallet); without
  one the existing token-file fallback applies, the same as for a gnu build.
- **macOS** (`aarch64-apple-darwin`): built in a separate non-blocking job (`continue-on-error`, not in the publish
  job's `needs`). It is uploaded only as a workflow artifact, not as a release asset. `install.sh` on Darwin
  stops with a hint to `cargo install --locked --git ...`. To publish it later, add it to the publish job and drop
  the `rm -f ./*apple-darwin*`; `install.sh` would then need the Darwin branch mapping to `<arch>-apple-darwin`.
- The x86_64 Darwin and Windows ARM64 builds are dropped. Windows ARM64 runs the x86_64 build under emulation.
- `release.yml` first checks that the tag equals `v` + the Cargo.toml version (`cargo metadata`) and that the
  repository URL can be rendered, then builds. SHA256SUMS is recomputed in the publish job over the downloaded
  archives plus the rendered scripts.
- Packaging is in `scripts/package.sh` (tar.gz) and `scripts/package.ps1` (zip). Both write
  `agent-sidekick-<target>.<ext>` and add or replace its line in `<out>/SHA256SUMS`. `release.yml` and the CI
  install dry-run call the same scripts.

Install scripts (`install.sh` POSIX sh, `install.ps1` for Windows PowerShell 5.1+):

- Environment: `VERSION` (tag such as `v0.3.0`; `0.3.0` also works; default latest),
  `AGENT_SIDEKICK_INSTALL_DIR` (default `~/.local/bin` or `%LOCALAPPDATA%\Programs\agent-sidekick`),
  `AGENT_SIDEKICK_INSTALL_BASE_URL` (a URL or a local directory that holds the archive and SHA256SUMS; it takes
  precedence over `VERSION`/repo; it is the test hook used by CI), `AGENT_SIDEKICK_REPO` (`owner/name`; the
  former generic `REPO` was renamed because the name is too generic), `AGENT_SIDEKICK_TARGET` (override the detected
  triple; test hook, e.g. to install a gnu build in WSL without musl-gcc), `AGENT_SIDEKICK_NO_SETUP=1`,
  `AGENT_SIDEKICK_NO_MODIFY_PATH=1`.
- Order: fetch the archive and SHA256SUMS → compare hashes (nothing is installed on a mismatch) → unpack → install →
  run `<installed> --version` as a smoke test → PATH → `setup` or a hint.
- `install.sh`: all logic sits in `main()`, called on the last line, so a truncated `curl | sh` download cannot
  run half a script. Questions and `setup` read from `/dev/tty`, because stdin is the script. "Interactive" means
  `/dev/tty` can be opened (it fails without a controlling terminal, e.g. in CI). Linux always uses the musl asset.
  Upgrades copy to `.agent-sidekick.new` and then `mv`, which works atomically even while the binary is running.
- PATH on Linux: rc file from `$SHELL` (bash → `~/.bashrc`, zsh → `${ZDOTDIR:-~}/.zshrc`, fish →
  `~/.config/fish/config.fish` with `fish_add_path`, otherwise `~/.profile`). The script prints the line and
  asks y/N only with a tty and without `NO_MODIFY_PATH`. It never writes unasked and skips the question if the
  line is already there.
- `install.ps1`: the body runs in `& { ... }` so `irm | iex` does not leak `$ErrorActionPreference` or variables
  into the caller's session. Errors are `throw`n, never `exit` (that would close the user's window; with
  `powershell -File` an uncaught throw still gives exit code 1). The PSScriptAnalyzer suppression
  `PSAvoidUsingWriteHost` (with justification) sits on a script-level `param()`. PSSA ignores it on the nested
  script block's `param()`, as tested with PSSA 1.25. A top-level `param()` is accepted by `iex` (tested on 5.1).
- User PATH on Windows: the spec says `[Environment]::SetEnvironmentVariable(...,'User')`. The script instead reads
  and writes the raw `HKCU\Environment\Path` value (REG_EXPAND_SZ, `DoNotExpandEnvironmentNames`), so existing
  `%VAR%` entries are not expanded and frozen. Duplicates are detected after expansion, case-insensitively and
  ignoring trailing `\`. A dummy `SetEnvironmentVariable(...,'User')` set/unset broadcasts WM_SETTINGCHANGE. The
  current session's `$env:Path` is also updated. `NO_MODIFY_PATH=1` skips both.
- Locked exe on Windows: if overwriting fails (a running MCP server), the old exe is renamed to
  `agent-sidekick.exe.old` (Windows allows renaming a running image) and the new one is copied. The script tells
  the user to reload VS Code. A stale `.old` is deleted on the next run when it is no longer locked.
- Interactive on Windows = `[Environment]::UserInteractive -and -not [Console]::IsInputRedirected`. `irm | iex`
  does not redirect stdin, so setup runs in a normal console.

CI (`ci.yml`): `lint-scripts` (shellcheck `--shell=sh` on the source and rendered `install.sh`, plus
`scripts/*.sh`); `lint-powershell` (PSScriptAnalyzer on Windows PowerShell 5.1, fails on any Error/Warning);
`install-dry-run` on ubuntu (musl build, `sh < install.sh` like `curl | sh`, running under dash) and windows
(`Get-Content -Raw | iex` in powershell 5.1). Each installs twice from the local dist dir into `$RUNNER_TEMP`, checks
`--version` against Cargo.toml, checks that the user PATH is unchanged (Windows), and checks that a tampered
SHA256SUMS is rejected and nothing gets installed.

Verified locally (2026-10-21): shellcheck 0.11 (via `shellcheck-py`) is clean on `install.sh` (`-s sh`) and on
`scripts/*.sh`. PSScriptAnalyzer 1.25 reports 0 findings on `install.ps1` and `scripts/package.ps1`.
actionlint 1.7.12 and `yaml.safe_load` pass on both workflows. Windows: release exe built with static CRT (imports
no vcruntime), `package.ps1` run, then `Get-Content -Raw install.ps1 | iex` in PowerShell 5.1 into a temp dir
(install, re-install, an exe held open is renamed to `.old` and the stale `.old` is cleaned up on the next run, a
tampered SHA256SUMS is rejected, the source copy refuses without a base URL, `-File` exits with 1). The user PATH
stayed unchanged. WSL Ubuntu: a `x86_64-unknown-linux-musl` build (static-pie; linked with plain `gcc` as
`CC_x86_64_unknown_linux_musl`, since `musl-gcc` is not installed there) was packaged by `package.sh` and installed with
`sh < install.sh` under dash, with and without a pseudo-tty (`script`). With a throwaway `HOME` the tests covered
answering y (line appended to `.bashrc`), a re-run (line detected, no question), zsh with answer n (unchanged), fish
without a tty (line printed only), the checksum mismatch, a missing target, a truncated script (syntax error,
nothing runs) and a 404 for `VERSION=v0.3.0` (no release yet).
Not verified: the HKCU PATH write (it would touch the real registry), the interactive `setup` hand-off, the
aarch64 musl build, and the workflows on GitHub (no remote yet). `ubuntu-24.04-arm` is free only for public
repositories.

Verifier follow-up (2026-10-21):
- Fixed (blocking): `lint-powershell` passed two paths to `Invoke-ScriptAnalyzer -Path`, which takes a single string
  in PSSA 1.25 (ParameterBindingException before any analysis). It now calls it once per file in a `foreach`. The
  earlier "0 findings" claim was per file only; the exact CI loop now gives 0 findings in Windows PowerShell 5.1
  with PSSA 1.25.
- Fixed: on Windows the installers are rendered in a separate `shell: bash` step (Git Bash, path via `cygpath -u`)
  instead of `bash ...` from pwsh, whose resolution depends on PATH order (could be the WSL launcher).
- Hardening: `install.sh` passes `--proto =https --tlsv1.2` to curl whenever the base URL is https (the default), so
  a redirect cannot downgrade to http. An explicit http `AGENT_SIDEKICK_INSTALL_BASE_URL` (local test server) still
  works. wget gets no https-only flag: `--https-only` is GNU wget >= 1.17 only and busybox wget rejects it; the
  default URLs are https anyway. SHA256SUMS comes from the same release: it detects corruption, not a compromised
  release (as the spec intends; signing is out of scope).
- Not changed: the README install URLs keep AAPPHH literally. Docs cannot be rendered from Cargo.toml without a
  build step, and a placeholder URL would not be copy-pasteable. Changing the owner means `Cargo.toml` plus the
  README URLs; scripts, release and CI derive it from Cargo.toml.


### Repository owner

- Owner decision: `repository = "https://github.com/AAPPHH/agent-sidekick"` in Cargo.toml (git user AAPPHH). This is
  the single place to change it; `update`/`doctor` derive owner/repo from `CARGO_PKG_REPOSITORY` at compile time
  (only `https://github.com/<owner>/<repo>` URLs are accepted, a unit test guards it).

### `agent-sidekick update` and the doctor hint (feat/dist-update)

- `update` only checks: `GET <base>/repos/<owner>/<repo>/releases/latest` (base `https://api.github.com`, or
  `AGENT_SIDEKICK_UPDATE_URL` as test/enterprise/mirror hook), headers `User-Agent: agent-sidekick/<version>`,
  `Accept: application/vnd.github+json`, `X-GitHub-Api-Version: 2022-11-28`, no credentials, 10 s timeout (doctor:
  3 s). Proxies come from ureq's environment handling (`ALL_PROXY`/`HTTPS_PROXY`/`HTTP_PROXY`, `NO_PROXY`); loopback
  hosts bypass the proxy so the tests are not affected by a proxy in the environment. Body limit 1 MiB.
- Version comparison is hand-written SemVer 2.0 precedence (no new dependency, keeps Cargo.lock untouched): one leading
  `v`/`V` stripped, build metadata ignored, pre-release lower than the release, numeric identifiers compared
  numerically. Leading zeros and anything else not SemVer is rejected as a bad response.
- Results and exit codes: newer release -> install one-liner for the platform (`irm .../install.ps1 | iex` on
  Windows, `curl -fsSL .../install.sh | sh` elsewhere, both from `releases/latest/download/`) plus "reload VS Code",
  exit 0. Equal -> "up to date", older (dev/pre-release build) -> "up to date (newer than the latest release)", 404
  -> "no release published yet"; all exit 0. Real errors exit 1 with a `next step:` line: no connection/timeout,
  403/429 (treated as rate limit; unauthenticated limit is 60/h per IP), other HTTP status, JSON without a SemVer
  `tag_name`. Error texts never contain the URL (an overridden one could carry credentials) or response bodies.
- `/releases/latest` ignores drafts and pre-releases, so a published pre-release is never offered.
- doctor: `[INFO]` lines after the report, never counted as warning/failure and never changing the exit code.
  Remembered in `<config-dir>/update-check.json` (`checked_at` Unix seconds, `current`, `result`, `latest`):
  - the spec's "hint at most once per day" is read literally: the result is shown when it is fetched, and later
    doctor runs within 24 h print nothing about updates (no network, no repeated hint; `agent-sidekick update`
    checks on demand); a failed check is shown once and retried after 1 h (silent meanwhile);
  - the record is ignored if the installed version changed, the clock went backwards, or the file is unreadable;
  - like `logging::Mode::Cli`, doctor never creates the config directory: if it does not exist, the check is skipped
    (info line pointing to `agent-sidekick update`), since "once per day" could not be enforced without the record;
  - `doctor --offline` skips it entirely (no request, no file).
- `update` itself does not read or write `update-check.json` (kept simple; doctor checks again within a day at most).
- Changes outside the new module are kept small for merging with feat/harness: `DoctorArgs { offline }` and three
  lines in `doctor::run`, the `Update` subcommand in `cli.rs`, `pub mod update` in `commands/mod.rs`.
- Error texts: ureq hands resolver, connect and TLS failures through as `io::Error`, so `update.rs` classifies them
  itself: DNS (Winsock 11001/11002/11004 on Windows, std's non-localized "failed to lookup address information"
  prefix elsewhere) -> "host not found (offline or DNS problem?)"; `InvalidData` during the request -> "TLS handshake
  failed: <rustls reason>" (during the body read it is a bad, non-UTF-8 answer instead); connection refused/reset and
  timeouts get a note "a proxy variable HTTPS_PROXY/ALL_PROXY/HTTP_PROXY is set" when one is set and the host
  is not loopback (variable names only, never the value). An `AGENT_SIDEKICK_UPDATE_URL` that is not an http(s) URL
  with a host is rejected before any request ("not a valid http(s) URL", next step "fix or unset", value not echoed);
  network errors name the variable in the next step when it is set.
- Redirects: ureq follows them; `https_only` is set for https bases, so a redirect to plain http is refused. http
  bases (loopback test servers, an internal http mirror) keep working.

### Merge notes (feat/dist-update, measured with `git merge-tree --write-tree`)

- Cargo.toml `repository`: feat/dist-update and feat/dist-install add the identical line before `readme`;
  feat/dist-plugin adds it after `readme`. Git merges that without conflict but leaves the key twice, and Cargo then
  refuses the manifest (`cargo metadata` fails with a duplicate-key error). Not moved here, since moving it would
  only shift the duplicate to the dist-install merge: whoever merges feat/dist-plugin after one of the others must
  delete one of the two `repository` lines (or move it before `readme` on feat/dist-plugin first).
- With feat/harness: `src/cli.rs` and `src/commands/doctor.rs` merge cleanly (an earlier prediction of conflicts in
  `run(DoctorArgs)` / `Command::Doctor(DoctorArgs)` was wrong). Real conflicts: `src/commands/mod.rs` (only the
  doc-comment line), README.md, NOTES.md, CHANGELOG.md.
- With feat/dist-install and feat/dist-plugin: only NOTES.md conflicts (README.md auto-merges).
- The install one-liner printed by `update` targets AAPPHH/agent-sidekick; until feat/dist-install fills in the
  repository in released install scripts (its source copy still defaults to `OWNER/agent-sidekick` and refuses to run
  with that placeholder unless `AGENT_SIDEKICK_REPO` is set), it only works once a release exists. No release exists
  yet: the real API answers 404 -> "no release published yet", exit 0.
- Known, pre-existing, not changed here (doctor belongs to feat/harness): with a real `claude` on PATH, `doctor`
  runs `claude mcp list`, which starts the registered server; that inherits `AGENT_SIDEKICK_CONFIG_DIR` and creates
  `<config-dir>/logs/`. So the "config dir missing -> update check skipped" path rarely applies in real use; the update
  hint itself never creates the directory (integration test).


Owner decision: `Cargo.toml` has `repository = "https://github.com/AAPPHH/agent-sidekick"` (git user AAPPHH). It is the source of truth for the owner: code that can read it (scripts, release, update) derives owner/repo from it. Static files cannot derive anything and hardcode `AAPPHH`; changing the owner means the same edit in `.claude-plugin/marketplace.json` (owner name/url), `plugin/.claude-plugin/plugin.json` (author name/url, homepage, repository), `plugin/README.md` (links, install one-liners, marketplace commands) and `README.md` (plugin paragraph). `grep -rn AAPPHH --exclude-dir=target .` finds them all. This supersedes the "`repository` is intentionally left out" bullet under Open points.

### Claude Code plugin + marketplace

Files: `.claude-plugin/marketplace.json` (repo root = marketplace root, marketplace `name` `agent-sidekick`, one entry `agent-sidekick` with `source: "./plugin"`), `plugin/.claude-plugin/plugin.json`, `plugin/.mcp.json`, `plugin/skills/delegate/SKILL.md`, `plugin/README.md`. No `.mcp.json` at the repo root (that would be a project-scope server prompting everyone who opens the repo).

Format sources (checked 2026-10-21): https://code.claude.com/docs/en/plugins-reference (manifest fields, `userConfig`, `${user_config.KEY}` substituted in MCP server config, `${CLAUDE_PLUGIN_ROOT}`/`${CLAUDE_PLUGIN_DATA}`/`${CLAUDE_PROJECT_DIR}` resolve in stdio `command`/`args`/`env`), https://code.claude.com/docs/en/plugin-marketplaces (marketplace.json needs `name`, `owner`, `plugins[]` with `name` + `source`; relative source from the marketplace root, no `..`; entry name must equal plugin.json `name`; install id `<entry>@<marketplace>`), https://code.claude.com/docs/en/mcp (plugin servers start automatically, register as `plugin:<plugin>:<server>`, tools are `mcp__plugin_<plugin>_<server>__<tool>`), https://code.claude.com/docs/en/plugins/components (instructions go in a skill; a CLAUDE.md at the plugin root is not loaded).

Decisions:

- Command: `${user_config.binary}` with `userConfig.binary` default `"agent-sidekick"`, args `["serve"]`. Without configuration the server is started as the bare `agent-sidekick` from Claude Code's PATH; users whose GUI app does not see the PATH set the absolute path via `/plugin configure agent-sidekick@agent-sidekick` (or `claude plugin configure ... --values-stdin`). No absolute default path: install locations differ per OS (`~/.local/bin` vs `%LOCALAPPDATA%\Programs\agent-sidekick`) and there is no documented portable variable for them in plugin MCP configs (`${VAR:-default}` expansion is documented for project/user `.mcp.json`, not for plugins; not relied on).
- No `AGENT_SIDEKICK_CLAUDE` in the plugin's server env: unlike `setup`, the plugin cannot know the path of `claude`. The server falls back to `claude_path` in config.toml, PATH, then well-known locations (`locate.rs`); plugin/README.md tells users to set `claude_path` if runs fail with "claude not found".
- `version: "0.3.0"` in plugin.json pins plugin updates to that string: the release step must bump it together with Cargo.toml (`claude plugin tag` checks plugin.json and the marketplace entry agree).
- Skill `delegate` (`/agent-sidekick:delegate`, also model-invoked) is a backend-agnostic condensation of `examples/CLAUDE.md` (~70-80 tokens always-on per `claude plugin details`; includes the "no fallback for side effects outside the working tree" rule).
- Plugin vs `setup` registration: use one. Plugin users run `agent-sidekick setup --no-register` and remove an old registration with `claude mcp remove agent-sidekick -s user`; `setup` users don't install the plugin.

Verification (claude 2.1.296, temporary `CLAUDE_CONFIG_DIR` in the session scratchpad, `AGENT_SIDEKICK_CONFIG_DIR` temp, `AGENT_SIDEKICK_DISABLE_KEYRING=1`; the real `~/.claude` and `%APPDATA%\agent-sidekick` were not touched):

- `claude plugin validate --strict ./plugin` and `claude plugin validate --strict .`: both "Validation passed" (the marketplace run advises an install line in README.md: `/plugin install agent-sidekick --marketplace <owner>/<repo>`, "one-step form on Claude Code 2.1.275+"; only the CLI form `claude plugin install agent-sidekick --marketplace ...` was seen in `--help` of 2.1.296, so plugin/README.md documents that and claims no version cutoff or slash-command support).
- `claude plugin marketplace add <worktree>` then `claude plugin install agent-sidekick@agent-sidekick` (and bare `claude plugin install agent-sidekick`): installed, enabled; `claude plugin details agent-sidekick` lists Skills (1) delegate and MCP servers (1) agent-sidekick.
- `claude mcp list`: binary not on PATH, default option -> `plugin:agent-sidekick:agent-sidekick: agent-sidekick serve - Failed to connect`; with the debug build on PATH -> `Connected`; with `binary` set to the absolute path and not on PATH -> `Connected`. So the default substitution of `${user_config.binary}` works even while the option shows "not set".
- Duplicates: with a user-scope `agent-sidekick` registration whose command and args are identical to the plugin's, `claude mcp list` showed only the user-scope server (Claude Code skipped the plugin copy). With different commands (setup's absolute path vs the plugin's bare name, the normal case) both were listed and connected: two processes, every tool twice. The dedup rule is observed behaviour, not documented.
- `agent-sidekick doctor` with only the plugin: `[FAIL] MCP registration: agent-sidekick is not registered`, next step `run agent-sidekick setup`, which would create the duplicate.
- `claude plugin install agent-sidekick@agent-sidekick --config "binary=C:\path with space\agent-sidekick.exe"` sets the option at install time, and re-running it on an installed plugin updates the value (`claude mcp list` shows the new command). `claude plugin configure ... --values-stdin < values.json` works from bash; piping JSON from PowerShell 5.1 is rejected by claude and `echo '{"binary":"C:\\..."}'` is not portable (dash's echo turns `\\` into `\`, giving invalid JSON), so plugin/README.md documents `--config`, `/plugin configure` and the file redirect (via `cmd /c` in PowerShell) instead of an echo pipe.
- Not verified live: the plugin tool name `mcp__plugin_agent-sidekick_agent-sidekick__sidekick_run` (from the documented pattern `mcp__plugin_<plugin>_<server>__<tool>`); plugin/README.md says to check `/mcp` if a permission rule does not match.

Open points:

- For feat/harness (doctor/setup owners): `doctor` should recognise the plugin registration (e.g. `claude plugin list` shows `agent-sidekick@...` enabled, or `claude mcp list` has `plugin:agent-sidekick:agent-sidekick`) and then warn about a duplicate user-scope registration instead of failing; `setup` could skip registering (or warn) when the plugin is enabled, and `setup --backend <name>` (single mode, src/commands/setup.rs, the "not registered ... run `setup --register-only`" note) should not suggest registering when the plugin is enabled. plugin/README.md tells plugin users to ignore both the doctor and the setup --backend hint until then. Not changed here to keep this branch out of src/.
- Depends on the scripts step: plugin/README.md and the README pointer use `https://github.com/AAPPHH/agent-sidekick/releases/latest/download/install.{sh,ps1}`, but `install.sh`/`install.ps1` still default `REPO` to `OWNER/agent-sidekick`; until that default is derived from Cargo.toml, the one-liners download the script and then fail to fetch the archive. The root README Quickstart (install one-liners with `OWNER/agent-sidekick`) is left to the scripts step as well, so for now it disagrees with the AAPPHH plugin paragraph; switching it here would only conflict with that step.
- Not tested: install from the GitHub marketplace (`/plugin marketplace add AAPPHH/agent-sidekick`) since the repo is not published; the interactive `/plugin` dialogs and the userConfig prompt on enable; VS Code with a GUI-reduced PATH.

## Harness adapters (0.3.0)

### Step 1: trait + claude/command adapters, behaviour unchanged

- **Layout.** `src/harness/mod.rs` holds `HarnessKind` (`claude`, `command`), the `Harness` trait and the injected-dependency structs `LaunchInput` / `CheckDeps`; `src/harness/claude.rs` and `src/harness/command.rs` are the adapters (stateless unit structs, `&'static dyn Harness` via `HarnessKind::adapter()` or `Backend::harness()`). Trait methods: `name`, `locate` (per backend), `locate_installed` (for `harness list`; `None` = n/a), `version` (`<program> --version`, first line, redacted, killed after 15 s via `claude_cli::run`), `load_credential`, `build_launch`, `normalize_output`, `check` (reachability), `install_hint` (string only, never executed).
- **What stays common** (in `src/backends/`): config validation (`resolve`), session isolation of the child environment (`launch::child_env` and its removal/`pass_env` rules), placeholder substitution, `command[0]` lookup, the HTTP probe. The adapters call these helpers; their code was moved, not rewritten.
- **Dispatchers keep their signatures.** `backends::{build_launch, load_credential, resolve_program, check_backend}` (and the private test seams `load_credential_with`, `check_backend_with`) are unchanged in signature and now dispatch to the backend's adapter, so `server.rs`, the CLI commands and every existing test work as before. Only test-module imports changed (`SecretId`/`TokenError`/`TokenSource` in `launch.rs`, `BackendKind` in `reach.rs`), because the production code no longer needs them.
- **Credential/backend matching stays in the dispatcher** (`backends::build_launch`), before any adapter is called: an `OauthToken` credential is accepted only for a claude-oauth backend, so the Claude OAuth token can never reach the command adapter (or later opencode/codex). Each adapter additionally refuses backends of another harness with an internal error (defence in depth, tested).
- **No new field on `Backend`.** The harness is derived from the internal `BackendKind` (`Backend::harness_kind()`), so `Backend` literals in tests and the `Backends` equality stay as they were. `BackendType` (`claude-oauth`, `claude-endpoint`, `command`) remains the internal discriminant and the user-visible label in results (`backend: <name> (<type>)`), `sidekick_backends.kind`, logs and `backends list`; the harness does not leak into result text in step 1, so MCP tool schemas and output are unchanged.
- **Config shape.** `harness = "claude" | "command"` in `[backends.<name>]`. Under `harness = "claude"` the mode is chosen by `base_url`: absent → OAuth mode (claude-oauth: token from `setup`, own `config_dir`, `cost` default `quota`), present → endpoint mode (claude-endpoint: `base_url`, `api_key_env`, `auth`, `cost` default `paid`). Reasons: no new key; the existing `auth` (`x-api-key`/`bearer`) keeps its meaning; an endpoint always has a `base_url` and OAuth never has one, so the mapping is total and unambiguous. Endpoint-only fields without `base_url` are reported as "field `api_key_env` is not allowed for harness `claude` without `base_url` (claude-oauth mode)", so a forgotten `base_url` next to endpoint settings is a config error. A bare `harness = "claude"` backend without any endpoint field is OAuth mode by design; it needs its own stored token (`setup --backend <name>`) and does not set `ANTHROPIC_BASE_URL` itself (an inherited one is kept with the v0.1 warning, unchanged).
- **Legacy `type`.** Still accepted and resolved exactly as before (same errors, same defaults). `type` and `harness` together: allowed when `type` maps onto that harness (`claude-oauth`/`claude-endpoint` → `claude`, `command` → `command`); otherwise the config error "`type = "<t>"` and `harness = "<h>"` disagree (type `<t>` means harness `<x>`); set only `harness`". With both set, `type` decides the claude mode, and the existing field rules catch a contradicting `base_url` (`type = "claude-oauth"` + `base_url` → not allowed; `type = "claude-endpoint"` without `base_url` → required). Neither set: the v0.3 message "`type` is required (one of: ...)" extended by ", or `harness` (one of: claude, command)" (the existing test asserts the prefix).
- **Unknown harness names** (`opencode`, `codex`) are a TOML parse error (unknown variant) in step 1; the variants are added with their adapters in steps 2 and 3.
- **Secret ids unchanged**: claude-oauth `oauth-token[-<name>]`, claude-endpoint `endpoint-<name>`, command `cmd-<name>`, whether the backend was written with `type` or `harness` (the id follows the internal kind).
- **`normalize_output`** is called on the run's output tail in `server::attempt` right after the run; identity for claude and command (the runner already merges stdout/stderr and redacts). It exists for harnesses with structured output (step 2+).
- **CLI.** `harness list` reads config.toml only for `claude_path` and to show which configured backends use each harness (an invalid config prints a warning and uses defaults); `command` shows `n/a` (generic, the program is per backend; `backends test <name>` checks it). `harness install <name>` prints the official commands; unknown names exit 1. Install commands for Claude Code taken from code.claude.com/docs/en/setup (checked 2026-10-21): `curl -fsSL https://claude.ai/install.sh | bash`, `irm https://claude.ai/install.ps1 | iex`, `curl -fsSL https://claude.ai/install.cmd -o install.cmd && install.cmd && del install.cmd`, `brew install --cask claude-code`, `winget install Anthropic.ClaudeCode`.
- **Deferred to later steps** (deliberately not in step 1, which must not change behaviour): per-backend `timeout_s`/`max_turns` (listed as common fields in the spec), `doctor` checking harnesses per backend beyond today's claude check, `setup`/`uninstall` for `backend-<name>` keys (opencode), README examples rewritten to `harness`.
- **Tests added**: `src/harness/equivalence_tests.rs` resolves one config with six backends (two OAuth, two endpoint incl. `api_key_env` and `auth = "bearer"`, two command incl. `{model}`/`secret_env` and `prompt_via_stdin`) written three ways (legacy `type`, `harness`, both) and asserts identical `Backends` and identical `Launch`es (program, argv, env, stdin, secrets, header) for each backend and several models, plus spot checks of the v0.3 shape, OAuth-token refusal for every non-OAuth backend, adapter cross-harness refusal and the new config errors. `src/harness/mod.rs` and `src/commands/harness.rs` test names, mapping, install hints, identity normalization and the `harness list`/`install` output.

### Step 1 review follow-ups

- **Branch base.** `feat/harness` was forked from 1f147da; it is now rebased onto main (b241303), so main's ETXTBSY retry and the "Known flaky tests" section are kept. The NOTES.md conflict (both sides appended) was resolved by keeping both sections. `stopped_run_still_names_backend` was added to the flaky list (load flake seen during verification). Checks after the rebase: fmt, Windows and Linux clippy clean; Windows tests all green; in WSL the first full run (right after a fresh build) failed the known `silent_server_times_out` once more at the `contains("timed out")` assertion (detail not captured by the filtered output), then 8 lib reruns and a full rerun were green.
- **pass_env wording.** For a backend written with `harness`, the pass_env refusal now names the harness as written ("... must not be passed through to a backend with harness `claude` without `base_url` (claude-oauth mode)"); legacy `type` configs keep the v0.3 text "... to a claude-oauth backend" (tested both ways).
- **`harness list`** now prints a warning on stderr when `[backends]` does not resolve (the "used by" column stays empty) instead of dropping the error silently.
- **README** key table: `type` is "(required, or `harness`)" and `harness` has its own row.
- **Accepted, not changed in step 1 (behaviour identity):** an inherited variable named like a command backend's `secret_env` is not removed from other backends' children, because `foreign_secret_env` only collects `api_key_env` names. Same on main. To be fixed in step 2: collect `secret_env` names too, so a secret variable meant for one backend never reaches another one.
- **Decisions for step 2** (from the step 1 design review):
  - Session isolation becomes mandatory: the dispatcher applies `child_env` and passes the adapter an already isolated env it may only add to; a generic test runs every `HarnessKind::ALL` adapter against the poisoned inherited env (OAuth token, API/auth tokens, CLAUDECODE, AGENT_SIDEKICK_TOKEN, foreign keys).
  - Program lookup becomes generic: `locate`/`locate_installed` and `CheckDeps` get a per-harness program override (`claude_path` for claude, an optional per-backend/per-harness path for the others) and a `find_program(harness)` instead of `find_claude`.
  - Normalization of JSONL output (opencode `--format json`, codex `exec --json`) must tolerate a tail that starts mid-object (skip unparsable leading lines), and the normalized text is redacted again with the launch's `Secrets`, because decoding JSON escapes after redaction could reveal an escaped secret.
  - Generated harness configs go through env/args, not temp files (no prepare/cleanup hook): `OPENCODE_CONFIG_CONTENT` with `{env:AGENT_SIDEKICK_PROVIDER_KEY}` indirection, codex `-c` overrides. If that proves insufficient, a prepare/cleanup hook is added to the trait then.
  - New harnesses get new internal kinds, but the legacy `type` field stays limited to the v0.3 values: `type = "opencode"` / `type = "codex"` are rejected; new harnesses are only reachable via `harness`.
  - `Harness::version` depending on `commands::claude_cli::run` (harness -> commands) is accepted for now; the helper moves to a neutral module if a second user appears.

### Step 2: opencode adapter (`src/harness/opencode.rs`)

Basis: the OpenCode research (opencode-ai 1.19.3 via npm) with its verified corrections, plus my own checks against the real binary (see "Verification (step 2)" below). Decisions:

- **Config shape.** `harness = "opencode"` only; `type = "opencode"` is a config error that points to `harness` (the legacy `type` stays limited to the v0.3 values; `BackendType::OpenCode` exists internally and is the `type` label in results, logs and `sidekick_backends`). Keys: `provider` (required: `openai-compatible`, `ollama`, `openrouter`), `base_url` (required for openai-compatible/ollama, a config error for openrouter, same validation as claude-endpoint), `models`/`model` (at least one is required because `--model` cannot be omitted; a lone `model` becomes the one-entry allowlist), `api_key_env` (alternative to the stored key; not for ollama), `harness_path` (absolute or `~`), `read_only` (default false). Not allowed: `config_dir`, `auth`, `command`, `prompt_via_stdin`, `secret_env`; `provider`/`harness_path`/`read_only` are not allowed for other harnesses. Cost default: `paid`, `free` for ollama. Per-backend `timeout_s`/`max_turns` are still not implemented (deferred again; the global defaults and the request parameters apply).
- **Isolation dir** `<config-dir>/opencode/<name>/` is computed at resolve time (`ResolveDefaults.config_dir`, so unit tests never depend on the process environment) and carried in `BackendKind::OpenCode.base_dir`. Subdirs `home` (child `HOME`, on Windows also `USERPROFILE`, `APPDATA` = `home/AppData/Roaming`, `LOCALAPPDATA` = `home/AppData/Local`), `config`, `data`, `state`, `cache` (with `cache/npm` = `npm_config_cache`), `tmp` (`TEMP`, `TMP`, `TMPDIR`). Created (base 0700 on Unix) by `build_launch`. `HOME` is set on Windows too (harmless; Git Bash inside OpenCode then also uses the isolated home).
- **Generated opencode.json** depends only on the backend config (all models of the allowlist with `tool_call: true`, the default model as `model` and `small_model`, `enabled_providers` = the one provider, permissions, `autoupdate false`, `share "disabled"`, `snapshot false`, `lsp false`, `formatter false`, `plugin []`). Written only when its content changed (temp file + rename), so concurrent runs of one backend (`max_concurrent` > 1) never race on it. Per-run values go through the environment: `OPENCODE_CONFIG_CONTENT = {"agent":{"build":{"steps":<max_turns>}}}` (verified to be applied). The provider `options.timeout` is not set (OpenCode's default per-request timeout applies; the runner's `timeout_s` kill is the hard limit). The step-1 plan to pass the whole config in `OPENCODE_CONFIG_CONTENT` was dropped: the task asks for a regenerated file, and a file can be inspected by the user and by the fake harness.
- **Key handling.** New `SecretId::Backend(name)` (keyring user `backend-<name>`, file `backend-<name>.key`) and `Credential::ProviderKey`. The child gets the key only as `AGENT_SIDEKICK_PROVIDER_KEY`; the config says `{env:AGENT_SIDEKICK_PROVIDER_KEY}` (ollama: the literal `"ollama"`, no key at all). The key is in the launch's `Secrets` (redaction of output, log and result). The credential matrix of `build_launch` accepts only `ProviderKey` (key-based providers) or `None` (ollama) for opencode, so the OAuth token can never reach it; `child_env` additionally strips `CLAUDE_CODE_OAUTH_TOKEN` for opencode.
- **Environment.** On top of the common session isolation, inherited `OPENCODE_*` (OpenCode's own config/permission/plugin switches could inject config), `ANTHROPIC_*`, `OPENAI_API_KEY`, `OPENROUTER_API_KEY`, `AGENT_SIDEKICK_PROVIDER_KEY`, `PWD`, `OLDPWD` and `npm_config_cache` (case-insensitive) are removed. `pass_env` can re-allow only `OPENAI_API_KEY`/`OPENROUTER_API_KEY` of these (tools in the agent's bash may need them); `CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN`, `AGENT_SIDEKICK_TOKEN` and `AGENT_SIDEKICK_PROVIDER_KEY` in an opencode `pass_env` are config errors. Set by the adapter after the backend's `env` table (so the table cannot override them): the isolation variables, `OPENCODE_DISABLE_{AUTOUPDATE,PROJECT_CONFIG,EXTERNAL_SKILLS,CLAUDE_CODE,LSP_DOWNLOAD,MODELS_FETCH}=1`, `OPENCODE_PERMISSION` (the permission JSON, merged by OpenCode after all layers incl. managed config), `OPENCODE_CONFIG_CONTENT`, and on Windows `OPENCODE_GIT_BASH_PATH` (from an inherited `OPENCODE_GIT_BASH_PATH`, `CLAUDE_CODE_GIT_BASH_PATH`, or `<dir of git.exe in PATH>\..\bin\bash.exe`; otherwise a log note that the bash tool falls back to cmd.exe). `OPENCODE_DISABLE_DEFAULT_PLUGINS` is not set: it did not stop project plugins (see below) and its other effects are unknown.
- **secret_env follow-up from step 1 done:** `Backends::new` now collects every `api_key_env` (claude-endpoint and opencode) and `secret_env` into `foreign_secret_env`, so an inherited variable named like another backend's secret is removed from every child (the owner still gets its explicit value).
- **Permissions** (never "ask"): `read` `{"*":"allow","*.env":"deny","*.env.*":"deny","*.env.example":"allow"}` (serde_json sorts keys, which happens to be the needed "last match wins" order; a unit test pins the serialization), `edit`/`bash`/`webfetch` allow (deny with `read_only`), `glob`/`grep`/`list` allow, `websearch`/`task`/`external_directory`/`doom_loop`/`question` deny. `allowed_tools` is not mapped (OpenCode's tool names and patterns differ from Claude Code's); it is ignored with a log note, and `read_only` is the supported restriction.
- **argv**: `run --format json --model <id>/<model> --title agent-sidekick`; provider id `sidekick` (openai-compatible, ollama) or `openrouter`. Prompt on stdin (never an argument), cwd = run cwd. `--title` skips the extra title request (verified: 2 instead of 3 requests).
- **Program lookup** (`find_opencode`, pure and tested): `harness_path` > npm package exe (`<dir of an opencode.cmd in PATH>/node_modules/opencode-ai/bin/opencode.exe`, then `%APPDATA%\npm\...`; Unix `<prefix>/lib/node_modules/opencode-ai/bin/opencode.exe` for a PATH entry `<prefix>/bin`) > `opencode.exe` (Windows, never `.cmd`/`.ps1`) or `opencode` (Unix) in absolute PATH entries > Unix `~/.opencode/bin/opencode`. npm is never spawned. A `.cmd`/`.bat` is accepted only as an explicit `harness_path` (the Windows tests use one), with model names limited to `[A-Za-z0-9-_./:@+]` because cmd.exe re-parses arguments.
- **`--version`** (`harness list`, `doctor`, `backends test`) runs with an allowlisted minimal environment (`PATH`, `PATHEXT`, `SYSTEMROOT`, `WINDIR`, `COMSPEC`, `LANG`, `LC_ALL`) plus isolation variables pointing into a fresh temp directory that is deleted afterwards (otherwise OpenCode creates `~/.config/opencode` etc.). It uses the new `claude_cli::run_in_env` (same timeout/kill logic as `run`).
- **Output normalization.** `Harness::normalize_output` now returns `Option<Normalized { text, failure }>` (`None` = keep the runner's tail; claude/command unchanged) and gets the runner's bounded output buffer (`RunOutcome.output`, the last 64-128 KiB, redacted) instead of the 4000-char tail, because the JSON stream is much larger than the answer. The server then cuts the normalized text to 4000 chars and redacts it again with the run's secrets (JSON unescaping could reveal an escaped key). Text = `text` parts after the last `step_start` (fallback: all text parts), then non-JSON lines under `[opencode output]` (stderr, agent-sidekick's own timeout/cancel notes; a cut-off first line or anything that looks like an event fragment is skipped), `[error] <name>: <message>` per error event, `stopped: permission auto-rejected` (tool error "rejected permission" or a stderr "auto-rejecting" line, only when there is no answer), and `[usage] steps N, tokens in X / out Y, cost Z` (sums over `step_finish`). No answer gives a `failure` ("no answer: permission auto-rejected" / "opencode reported an error" / "opencode produced no text" / "no opencode JSON output"); it is applied only to runs that exited 0 and were not killed. `RunOutcome.failure` makes `outcome_is_error` and `fallback_eligible` true; the result shows `failed: <reason>` after `exit_code`, and the fallback headline says `failed: <reason> (exit_code 0)`.
- **Reachability / pre-run check.** `CheckDeps` got `locate`, `secret_value` and `probe_get`; `reach::probe_get(url, bearer, timeout)` is the old probe with an optional bearer header (no redirects, so the key goes only to the configured URL; never in the detail). Order: installed (detail prefix `opencode not installed`), key (stored or env; the value is read only for the openai-compatible probe), probe: openai-compatible `GET {base_url}/models` with Bearer, ollama `GET {base_url}/api/tags`, openrouter `GET https://openrouter.ai/api/v1/models` (public, no key; key validity is not checked for OpenRouter). Any HTTP status except 401/403 counts as reachable (gateways without a model list answer 404). The same probe runs right before every opencode run (`Harness::pre_run_check`, a new trait method with a default no-op), so a dead provider fails as "not started" and a fallback can run instead of waiting through OpenCode's ~60 s of retries.
- **CLI.** `setup --backend <name>` asks for the provider key (`Step::ProviderKey`; a full setup prints a hint, ollama/`api_key_env` print info); `uninstall` deletes `backend-<name>` for every opencode backend (also ollama, a key may have been stored earlier) and mentions the opencode state dirs (removed only with `--purge`); `doctor` adds one "harness `opencode`" check per distinct located executable (path, version, which backends use it) and opencode-specific next steps per backend; `backends test` prints the opencode path/version and key source; `describe` (backends list/test) shows provider, base_url, harness_path, read_only, key source and state dir; `sidekick_status` lists `provider key (<name>)` sources.
- **MCP tools unchanged** (names, schemas, descriptions; later amended: the review changed the `allowed_tools` description, see "Step 2 review follow-ups"; the only text change at the time was the claude-oauth to claude-oauth fallback error, which now also suggests opencode). `sidekick_backends.type` is `"opencode"` for these backends.
- **Project plugins (correction 5, settled).** In a real run (local stub, see below) a `<cwd>/.opencode/plugin/p.js` WAS executed although `OPENCODE_DISABLE_PROJECT_CONFIG=1`; also with `OPENCODE_PURE=1` and with `OPENCODE_DISABLE_DEFAULT_PLUGINS=1`. Source: besides the gated `ConfigPaths.directories`, 1.19.3 has a second config loader (`Config.entries`) that walks from cwd up to the project root collecting `.opencode` directories and `opencode.json(c)` documents without that gate; its entries feed the `config-plugin` (directory `{plugin,plugins}/*.{ts,js}` and the documents' `plugins`), `config-skill` (`skill(s)` dirs) and `experimental.policies`. The project's `opencode.json` model and permission settings were NOT applied (the stub got our model; `read` worked although the project said `ask`). Not fixable from outside short of refusing such projects; agent-sidekick detects `.opencode/plugin(s)` with `.js`/`.ts` files (since the review also config documents naming plugins, see "Step 2 review follow-ups") between cwd and the first directory containing `.git` and writes a warning into the run log header and the server log. Documented in the README as a known limit ("only run opencode backends in projects whose code you trust", like Claude Code's project hooks).
- **First-run downloads** (verified): in a new backend dir OpenCode fetches its plugin SDK through npm (cache in `cache/npm/_cacache`, files in `config/opencode`) and `ripgrep-15.1.0` into `cache/opencode/bin`; nothing outside the backend dir. Needs network once per backend dir.
- **Leftover from the research run (not created by this step, not deleted):** `%USERPROFILE%\npm-cache\` on the development machine (from the research's un-isolated plugin install).

#### Verification (step 2)

- Unit tests (`src/harness/opencode/tests.rs`): config rules, rendered config per provider, permission sets (no "ask", read pattern order), launch argv/stdin/env against a poisoned inherited env (OAuth token, Claude session vars, Anthropic keys/base URL, `OPENCODE_CONFIG*`/`OPENCODE_PERMISSION`, `PWD`, provider keys, foreign keys, `NPM_CONFIG_CACHE`, real HOME/XDG/TEMP), every isolation path inside the backend dir and created, no file outside it, no key in the generated config, config not rewritten when unchanged, credential matrix (OAuth/ApiKey/CommandSecret/None refused), pass_env limits, batch wrapper rule, credential sources, lookup order (Windows and Unix branches), Git Bash detection, reachability incl. bearer only for openai-compatible, normalization (answer, usage, errors, cut-off line, stderr, rejection, empty), re-redaction, project plugin detection, README and examples/config.toml examples resolve. Plus tests in setup/uninstall/doctor/backends for the new steps, secret ids, checks and `describe`.
- Integration (`tests/opencode_fake.rs`; fake opencode = sh script / .cmd via `harness_path`, provider = `tests/support/fake_api.rs`): isolated, normalized run (argv, stdin, cwd, env names, isolation paths, generated config, new files only in `logs/` and `opencode/oc/`, nothing in the fake real home, no key or OAuth token in recordings/config/log/result, exactly one `GET /api/v1/models` with the right bearer), a key echoed by the agent is redacted, no answer gives an error and the fallback opencode -> command, provider error event, fallback claude -> opencode without the OAuth token reaching opencode, `sidekick_backends`/`sidekick_status` (reachable, not installed, provider key source), run on a missing harness. `tests/mcp_handshake.rs`: tool list and `sidekick_run` schema unchanged with an opencode backend.
- Real OpenCode 1.19.3 (Windows, npm install), driven through `agent-sidekick serve` against a local OpenAI-compatible stub on 127.0.0.1 (no real model, no real key): answer `STUB_FINAL_ANSWER` with usage line; requests carried `Bearer <stub key>`; 2 requests (no title request); the project's `opencode.json` (`model: evil/evil`, `read: ask`) was not applied; an inherited `OPENCODE_CONFIG_CONTENT` with `bash: ask` had no effect; `read_only = true` offered only glob, grep, read, skill, todowrite; `max_turns = 2` injected "CRITICAL - MAXIMUM STEPS REACHED" from the 2nd request on while tools stayed offered, and the run was killed by `timeout_s = 15` (result: timeout note and usage line); no new files in `~/.config`, `~/.local`, `~/.cache`, `~/.opencode`, `~/npm-cache`, `%LOCALAPPDATA%\opencode`, `%TEMP%\opencode`, `%APPDATA%\opencode`; the project plugin executed (see above). Not verified: a real run on Linux (WSL has no Linux OpenCode; only the Windows npm shim is visible through PATH interop, and `find_opencode` on Linux would pick that `opencode` shell shim from a PATH entry, which then runs the Windows exe through interop), OpenRouter and Ollama at runtime, a real KI:connect call (no real model calls allowed). `harness list` and `backends test` were smoke-tested with the built binary against a scratch config dir (opencode 1.19.3 found, version read, temp dir removed, no files in the real home).

#### Status of the step-1 "Decisions for step 2"

- `secret_env` collected into `foreign_secret_env`: **done** (behaviour change: an inherited variable named like a command backend's `secret_env` no longer reaches other backends; `tests/command_secret.rs` updated, README corrected: `pass_env` never re-allows another backend's `api_key_env`/`secret_env`, which was already the behaviour for `api_key_env`).
- JSONL normalization tolerant of a mid-object start, normalized text redacted again: **done**.
- Legacy `type` limited to the v0.3 values (`type = "opencode"` rejected): **done**.
- Generated configs through env/args only (`OPENCODE_CONFIG_CONTENT` with the whole config): **dropped** for opencode; the task asks for a regenerated `opencode.json` (inspectable by users and by the fake harness). Only the per-run step limit goes through `OPENCODE_CONFIG_CONTENT`.
- Session isolation applied by the dispatcher (adapters get an already isolated env) plus one generic poisoned-env test over `HarnessKind::ALL`: **deferred to step 3**. Each adapter still calls `child_env` itself (opencode does, and its poisoned-env test covers it; the step-1 equivalence tests cover claude/command). Doing it now would have touched the claude/command launch paths again in a step that adds a harness.
- Generic program lookup (`find_program(harness)` instead of `find_claude` in `CheckDeps`): **partly**; `CheckDeps.locate` (per backend) was added next to `find_claude`, which the claude adapter still uses. Unifying is deferred to step 3 (codex will need the same per-backend lookup).
- `Harness::version` depending on `commands::claude_cli::run`: a second user exists now (`run_in_env` for opencode), but the helper stays in `claude_cli` for this step to keep the diff small; moving it to a neutral module is deferred to step 3.

### Step 2 review follow-ups

Findings of the step-2 verifier and what was done with them.

- **Merge with main (blocker): done as a merge, not a rebase.** `git merge main` into feat/harness (merge commit) instead of rebasing the four harness commits: one conflict pass instead of four, and main itself takes feature branches as merge commits. Conflicts resolved: CHANGELOG (main's unreleased `update`/doctor-hint/`repository` entries moved into `[0.4.0]`, `[Unreleased]` left empty), NOTES (main's "Distribution" section kept before this section), README (command table: harness rows kept, doctor row combines the harness checks with main's `--offline`/update hint, `update` row added, uninstall row from the harness side), `src/commands/mod.rs` (module doc lists both `harness` and `update`). `plugin/.claude-plugin/plugin.json` bumped to `0.4.0` by hand (nothing checks it against Cargo.toml); the `VERSION` examples in README, install.ps1 and install.sh now say `v0.4.0`. `plugin/README.md` mentions "(0.3.0)" as the version that introduced a behaviour, left as is. The doctor tests (main's update hint, the harness checks) and the whole suite were rerun right after the merge: green.
- **Plugins named in a project config (detection gap): fixed.** `project_plugin_dirs` became `project_plugin_sources`: besides `.opencode/plugin(s)` with `.js`/`.ts` files it now reports `opencode.json`, `opencode.jsonc`, `.opencode/opencode.json` and `.opencode/opencode.jsonc` in every directory from cwd up to the first one containing `.git` when they name plugins. A document serde_json can parse counts when its top-level `plugin` or `plugins` is present and not null or an empty array. A document it cannot parse (JSONC comments, trailing commas, broken JSON) counts when `"plugin":` or `"plugins":` appears anywhere (whitespace ignored). No JSONC parser was added: a false positive is only a warning, a miss would hide code execution. The npm-package case (a plugin given by package name) is detected the same way; whether OpenCode then installs and runs it was not tested (README says so).
- **Plugins get the provider key (doc): fixed.** The run-log warning now reads `warning: OpenCode runs the project plugins from <paths> (not blocked by OPENCODE_DISABLE_PROJECT_CONFIG); they see AGENT_SIDEKICK_PROVIDER_KEY, the provider key`, the server-log warning says the same, and the README entry is now "Project plugins run, and they get the provider key" (exfiltration, not only code execution; use a key you can revoke). The run is still not blocked: refusing every project with a plugin would make the harness unusable in repos that use OpenCode themselves, and the user chooses the cwd.
- **The agent's own tools can read the key (doc): fixed.** README Security paragraph and a new known-limit entry name `AGENT_SIDEKICK_PROVIDER_KEY`. Unavoidable: OpenCode resolves `{env:...}` from its own environment and its bash tool inherits that environment.
- **Unredacted key in OpenCode's session store (doc): fixed.** README isolation bullet: `data/opencode/opencode.db*` holds complete unredacted transcripts; Unix creates the backend directory 0700, on Windows it inherits the ACL of the config directory (the user profile; same as the run logs).
- **`.env` rule covers only the read tool (doc): fixed.** README permissions bullet says so (bash `cat .env` is not blocked; no protection for secrets in the project).
- **HOMEDRIVE/HOMEPATH (defence in depth): done.** On Windows the adapter removes inherited `HOMEDRIVE`, `HOMEPATH` and `HOMESHARE` and sets `HOMEDRIVE`/`HOMEPATH` to the split isolated home (`split_drive`: drive `C:` or UNC `\\server\share`, verbatim prefixes handled, device paths give no pair and leave both unset). The launch test checks that `HOMEDRIVE + HOMEPATH` equals `<base>/home` and that `HOMESHARE` is gone; `split_drive` has its own Windows test. Not re-verified against the real binary (no new real run); the change only narrows what the child can see.
- **Runner log lines (cosmetic): fixed.** `started agent` (with a `program` field) and `agent finished` instead of `started claude` / `claude finished`; no test matched the old text.
- **`allowed_tools` schema doc (cosmetic): fixed.** "Ignored by command and opencode backends (opencode: use the backend's read_only setting)." This changes the `sidekick_run` input schema's description text (names and types unchanged; `tests/mcp_handshake.rs` checks types only), so the step-2 line "MCP tools unchanged (names, schemas, descriptions)" no longer holds for this one description.
- **Isolation of two opencode backends (test gap): fixed.** `two_opencode_backends_are_isolated_from_each_other`: two openai-compatible backends (one with `api_key_env`) resolve to different, non-nested base dirs; launching A does not create B's dir; each launch has its own key in `AGENT_SIDEKICK_PROVIDER_KEY` and the other key nowhere in its env; the `api_key_env` variable reaches neither child; HOME/XDG/TEMP point into the own base; each generated config holds only its own base_url and no key; launching B leaves A's files byte-identical and vice versa. Platform-neutral (runs in WSL too).
- **`max_concurrent` > 1 on one opencode backend: documented as unverified,** not limited. The generated config is safe (written only when changed, via rename), but the runs share OpenCode's `data`/`state` dirs and its SQLite session store; no test or real run covers that. README known limit recommends separate backends for parallel runs; the examples use 1. A hard limit was not added because the failure mode is unknown, not known bad, and a limit would silently override a user's explicit setting.
- **Unix npm candidate with `.exe` suffix: rejected, it is correct.** The npm package names its binary `bin/opencode.exe` on every platform: `package.json` has `"bin": {"opencode": "./bin/opencode.exe"}` and `postinstall.mjs` line 29 is `const targetBinary = path.join(__dirname, "bin", "opencode.exe")` (line 28 picks the platform package's `opencode.exe` on Windows and `opencode` elsewhere as the source it links or copies). Checked in the installed opencode-ai 1.19.3. Still not checked on a real Linux install; the PATH fallback covers other layouts.
- **WSL picks the Windows npm shim (isolation consequence): documented.** The shim runs the Windows exe through interop; `HOME`/`XDG_*` do not cross into a Windows process without `WSLENV`, so such a run would see the Windows user's real OpenCode home and config. README known limit: in WSL install OpenCode natively and set `harness_path`. No code change (detecting interop paths would be a heuristic on `/mnt/<drive>/`; left for step 3 if needed).
- **Spec gap: tool-calling test and mini run NOT done.** The spec's step 2 asks for a tool-calling test against the real provider and, if it passes, a mini run (`max_turns` 3, read-only). Neither was done: this workflow forbids real model calls and has no valid KI:connect key. It is still unknown whether KI:connect's vLLM gateway emits tool calls (it needs `--enable-auto-tool-choice`; without it OpenCode's requests fail with 400, shown as `[error] APIError: ...`). Step 2 is therefore complete only up to the real-provider check; the user runs it:
  1. Add a read-only backend to `config.toml` (`agent-sidekick doctor` prints the path):
     ```toml
     [backends.kiconnect-ro]
     harness = "opencode"
     provider = "openai-compatible"
     base_url = "https://chat.kiconnect.nrw/api/v1"
     models = ["openai-gpt-oss-120b", "qwen-qwen3-8-27b"]
     description = "OpenCode via KI:connect.nrw, read-only (test)"
     cost = "free"
     max_concurrent = 1
     read_only = true
     ```
  2. `agent-sidekick setup --backend kiconnect-ro` (stores the key in the OS keyring), then `agent-sidekick backends test kiconnect-ro` (must report the opencode path and version, the stored key, and reachable).
  3. Tool-calling test (from Claude Code, after reloading the MCP server): `sidekick_run` with `backend = "kiconnect-ro"`, `max_turns = 3`, `cwd` = a scratch folder containing a `hello.txt` with one line, prompt "Read hello.txt with your read tool and answer with its first line only." Pass: the result shows the line and a `[usage]` line with steps >= 2, and the run log contains a `"type":"tool_use"` event. Fail: `[error] APIError` with 400 (no tool calling on the gateway), or text without a tool_use event (the model answered without the tool).
  4. Only if 3 passes, the mini run: the same backend, `max_turns = 3`, `cwd` = a small real repository, prompt "List the top-level files and summarize in three sentences what this project does." Check: answer present, exit code 0, `[usage]` line, no file changed in cwd (`git status` clean), nothing new in `~/.config/opencode` or `~/.opencode`. Repeat with `model = "qwen-qwen3-8-27b"`.
  Record the outcome here (and in the older KI:connect note above).
- **Verified OK / verdict bullets:** no action needed.
- **Checks after these fixes (on the merged branch):** `cargo fmt --check` clean; clippy `-D warnings` clean on Windows and cross for `x86_64-unknown-linux-gnu`; Windows `cargo test --locked`: lib 347 passed, 1 ignored; backends_fallback 14, command_secret 2, e2e_fake_api 5, mcp_handshake 8, opencode_fake 6, runner_fake_claude 9, update_command 9. WSL: lib 345 passed, 1 ignored (two Windows-only lib tests, one of them the new `split_drive` test); backends_fallback 14, command_secret 2, e2e_fake_api 5, mcp_handshake 9, opencode_fake 6, runner_fake_claude 9, runner_linux_setsid 1, update_command 9. No new real OpenCode run was made for these changes.

### Step 3: codex stub (`src/harness/codex.rs`)

User decision: OpenAI/Codex is not wired up yet (model list later), so this step adds a stub only.

- **What the stub does.** `HarnessKind::Codex` (`harness = "codex"`, order in `HarnessKind::ALL`: claude, opencode, codex, command; this order is also the `harness list` order and the "known: ..." list of `harness install`). `CodexHarness` implements `locate_installed` (lookup below), `version` and `install_hint` (`npm (all platforms):   npm install -g @openai/codex`, plus the npm package page npmjs.com/package/@openai/codex; printed only, never run). `harness list` prints an extra line under codex: `status:  not enabled yet (stub); backends with harness = "codex" are rejected`, so `codex: installed` does not invite a config that then fails.
- **No backend can use it.** There is no `BackendType`/`BackendKind` for codex. `resolve::backend_type_of` rejects `harness = "codex"` before any type is assigned, with the config error `codex harness is not enabled yet (stub): agent-sidekick only locates the Codex CLI (`agent-sidekick harness list`) and cannot run it; use harness claude, opencode or command` (prefixed with `backends.<name>: ` like every resolve error). The arm comes before the type/harness agreement check, so `type = "claude-oauth"` + `harness = "codex"` gets the stub message, not "disagree". `type = "codex"` stays a TOML parse error (unknown variant; the legacy `type` remains limited to the v0.3 values). Consequence: `HarnessKind::of_type` stays total and never yields `Codex`, `Backend::harness_kind()` is never `Codex`, and the dispatchers (`build_launch`, `load_credential`, `resolve_program`, `check_backend`) never reach the codex adapter. Its backend methods (`locate`, `load_credential`, `build_launch`, `check`) still refuse with `backend <name>: codex harness is not enabled yet (stub)` (defence in depth): no secret is loaded, no env is read, no launch is built (tested with an OAuth credential and with `CheckDeps` closures that panic if called).
- **Error lists.** The "neither `type` nor `harness`" error keeps `or `harness` (one of: claude, opencode, command)` without codex (deliberately: it lists what a backend can use), while `harness install <unknown>` lists all known harnesses incl. codex (`known: claude, opencode, codex, command`).
- **Setup/uninstall/doctor/status: no change needed.** No codex backend can exist, so there is no secret id for it (when it is wired up: `backend-<name>` like opencode), nothing for `uninstall` to remove, and `doctor::harness_checks` (which only checks opencode executables beyond claude) is correct by construction. MCP tools unchanged.
- **Lookup** (`find_codex`, pure, injected env and `is_file`): Windows: the npm package's native executable `<prefix>\node_modules\@openai\codex\node_modules\@openai\<pkg>\vendor\<triple>\bin\codex.exe` with `<pkg>/<triple>` = `codex-win32-x64`/`x86_64-pc-windows-msvc` and `codex-win32-arm64`/`aarch64-pc-windows-msvc` (the host architecture first), for `<prefix>` = every absolute PATH directory that contains a `codex.cmd` shim, then `%APPDATA%\npm`; then `codex.exe` in an absolute PATH entry. `.cmd`/`.ps1` shims are never returned (the shim runs node, which runs the same exe; running the exe directly avoids cmd.exe argument re-parsing). Unix: `codex` in an absolute PATH entry (must be executable); the Unix npm vendor layout was not verified (no Linux codex install here), so no vendor path is guessed. npm is never spawned. No per-backend `harness_path` yet (no backend exists).
- **`--version`** runs with the allowlisted minimal environment (`VERSION_ENV_KEEP`, moved from opencode.rs to `harness/mod.rs` and shared) plus `CODEX_HOME`, `HOME` and `USERPROFILE` pointing to a fresh `%TEMP%\agent-sidekick-codex-version-<uuid>` that is removed afterwards. Verified with the real codex-cli 0.163.0: `--version` creates `tmp/` inside `CODEX_HOME` even for `--version`, so the real `~/.codex` must not be the target; the smoke test (`harness list` with the built binary and a scratch `AGENT_SIDEKICK_CONFIG_DIR`) printed `codex-cli 0.163.0`, left no temp dir and did not change the entry count of `~/.codex`.
- **Installed layout seen on this machine** (`npm install -g @openai/codex`, 0.163.0, Windows x64): `%APPDATA%\npm\codex`, `codex.cmd`, `codex.ps1` (shims); `...\codex-win32-x64\vendor\x86_64-pc-windows-msvc\bin\codex.exe` and `codex-code-mode-host.exe`; `...\vendor\x86_64-pc-windows-msvc\codex-path\rg.exe` (bundled ripgrep; the 0.163.0 shim `bin/codex.js` does NOT add `codex-path` to PATH, it only spawns `codex.exe` with `CODEX_MANAGED_PACKAGE_ROOT=<package root>` and `CODEX_MANAGED_BY_NPM=1` added to the inherited env, so the exe presumably finds `codex-path` itself; whether running the exe directly without those two variables changes behaviour (e.g. update hints, rg lookup) is not verified and must be checked when the adapter is wired up).
- **Known launch facts for the later adapter** (from the Codex research; not implemented, not re-verified here):
  - argv: `codex exec --sandbox workspace-write -c windows.sandbox="mxc" --skip-git-repo-check --ephemeral --color never --cd {cwd} -` with the prompt on stdin (the trailing `-`).
  - Key: `CODEX_API_KEY` in the child environment (from keyring user `backend-<name>` / `api_key_env`, registered in the launch's `Secrets`; never in argv or a file).
  - `CODEX_HOME` must exist before the run: a per-backend directory (like opencode's `<config-dir>/codex/<name>/`), created by `build_launch`; generated settings via `-c` overrides, not a config file with the key.
  - Still open: model list/allowlist, provider selection (`-c model_provider=...` for OpenAI-compatible gateways), structured output (`exec --json` normalization, tolerant of a mid-object tail and redacted again, per the step-1 decision), reachability probe (`GET {base_url}/models`), `max_turns` mapping (codex has no step limit flag; only the runner timeout), and session isolation of the inherited env (`OPENAI_*`, `CODEX_*`).
- **Step-2 items "deferred to step 3": deferred again** (this step is a stub and adds no launch path): session isolation applied by the dispatcher with a generic poisoned-env test over `HarnessKind::ALL` (codex has no launch to test), unifying the program lookup into `find_program(harness)` (codex has no per-backend lookup yet), moving `claude_cli::run`/`run_in_env` to a neutral module (codex is a third user, still in `claude_cli`). All three belong to the step that wires up codex.
- **Tests.** `harness/codex.rs`: Windows lookup order (shims alone never used, PATH exe last, `%APPDATA%\npm` before PATH, a `codex.cmd` directory's npm prefix before `%APPDATA%`, both architectures) and the real default layout; Unix PATH lookup (relative entries ignored, PATH order); `--version` against a fake codex (`.cmd` on Windows, sh on Unix) that fails if an inherited variable leaks or `CODEX_HOME` does not exist, checking the scratch dir is used and removed (also through the trait's temp dir); a missing program; the stub refusing every backend method without touching secrets/env; install hint. `equivalence_tests.rs`: `harness = "codex"` alone and with `type = "claude-oauth"`/`"command"` gives the stub error (not "disagree"), `type = "codex"` and an unknown harness are parse errors. `commands/harness.rs`: `install codex` output, unknown names list `claude, opencode, codex, command`, the stub status line. `harness/mod.rs`: name round trip and `ALL` order, install hints, identity normalization for codex.
- **Checks (step 3):** `cargo fmt --check` clean; clippy `-D warnings` clean on Windows and cross for `x86_64-unknown-linux-gnu`; Windows `cargo test --locked`: lib 357 passed, 1 ignored; backends_fallback 14, command_secret 2, e2e_fake_api 5, mcp_handshake 8, opencode_fake 6, runner_fake_claude 9, update_command 9. WSL: lib 354 passed, 1 ignored; backends_fallback 14, command_secret 2, e2e_fake_api 5, mcp_handshake 9, opencode_fake 6, runner_fake_claude 9, runner_linux_setsid 1, update_command 9. Smoke test with the built binary and a scratch config dir: `harness list` shows codex installed (native exe path, `codex-cli 0.163.0`, stub status line), `harness install codex` prints the npm command, a `harness = "codex"` backend makes `backends list` exit 1 with the stub error.

## Version for the first release

The harness work was developed as 0.4.0 but merged before anything was tagged. At the user's request, everything (Cargo.toml, CHANGELOG, plugin.json, VERSION examples, NOTES headings) is unified to **0.3.0** for the first public release; the CHANGELOG merges the former [0.4.0] entries into [0.3.0]. 0.4.0 starts after the first release. Entries above that mention "0.4.0" in a merge context are history. Paths that contained the developer's user name were replaced with placeholders (`~`, `%USERPROFILE%`, `%APPDATA%`).

## KI:connect mini runs through the OpenCode harness (2026-10-21)

Setup: agent-sidekick 0.3.0 (opencode 1.19.3), backend `kiconnect` (`harness = "opencode"`, `provider = "openai-compatible"`, `base_url = https://chat.kiconnect.nrw/api/v1`), key in keyring user `backend-kiconnect`. Runs started through `agent-sidekick serve` (MCP `tools/call sidekick_run`). Test project: a copy of the aider test folder (`calc.py` with three defects: `average` divides by `len + 1`, `average([])` does not raise `ValueError`, `is_even` tests for odd) **without** the `# BUG` hint comments, plus the unchanged `test_calc.py`. Each result was checked by hand; the write runs by running `python -m unittest` myself and diffing `test_calc.py`. Tool calling had been confirmed beforehand with a direct probe (non-stream, stream, and a second turn with a tool result) for both models.

| Run | Model | max_turns | Result | Steps | Tokens in / out | Duration |
|---|---|---|---|---|---|---|
| read-only #1 | openai-gpt-oss-120b | 3 | **failed**: first read used the absolute path `/calc.py` (denied as external directory), then `glob`, then the step cap; answer "limit reached", no analysis | 3 | 11,734 / 270 | 5.7 s |
| read-only | qwen-qwen3-8-27b | 3 | **correct**: all three defects with corrected code | 2 | 9,031 / 230 | 330.5 s |
| read-only #2 | openai-gpt-oss-120b | 3 | **failed**: read `calc.py`, step cap before reading the tests | 3 | 11,877 / 220 | 5.1 s |
| read-only #3 (diagnostic) | openai-gpt-oss-120b | 5 | **correct**: all three defects with corrected code | 4 | 15,498 / 240 | 5.4 s |
| write | openai-gpt-oss-120b | 8 | **correct**: fixed `calc.py`, ran the tests itself, tests unchanged, my run: 4 tests OK | 7 | 42,719 / 123 | 7.4 s |
| write | qwen-qwen3-8-27b | 8 | **correct**: same, 4 tests OK | 4 | 28,462 / 320 | 325.6 s |
| write (repeat, `headerTimeout` 120 s) | qwen-qwen3-8-27b | 8 | **correct**, 4 tests OK | 4 | 28,488 / 333 | 397.7 s |

Cost is reported as 0 (KI:connect has no per-token price).

Findings:
- **Both models solve the read and the write task; `kiconnect` counts as usable.** gpt-oss-120b reads one file per step and needs one step for the answer, so `max_turns` 3 is too tight for it; use at least 5 for small tasks (the doc tasks use 20).
- **gpt-oss is fast (5 to 8 s per task), Qwen is slow (5.5 to 6.5 min per task)** because one request per run gets no response headers for about 5 minutes. OpenCode's header timeout (300 s) fires and the retry answers in seconds. With `headerTimeout` 120 s the retry waited again, three times in a row, so this looks like queueing at the provider, not a lost request; a shorter timeout does not help. `request_timeout_s` stays an optional setting (unset by default). Prefer gpt-oss as the default model; use Qwen only when its latency is acceptable.
- **The soft step cap works with both models:** they stop and summarise when the cap is reached instead of looping.
- The OpenCode harness behaves as designed: isolated state dir, key only via `{env:...}`, read-only mode offered only read tools, `test_calc.py` untouched in the write runs.

## Setup wizard

`agent-sidekick setup` without flags in an interactive terminal (stdin and stdout are terminals) starts the wizard in `src/commands/wizard/`. Decisions and assumptions:

- **Language: German** for all wizard text (target audience: universities in NRW, KI:connect.nrw users); the profile labels are exactly the ones from the spec. Output reused from other commands stays English: the registration lines of `setup::register` and the `doctor` report (translating them would change existing commands and tests). The rest of the CLI stays English.
- **Gating.** The wizard runs only for a bare `setup` (no flag at all) with both stdin and stdout on a terminal. It is dispatched before `config.toml` is loaded, so it can repair a broken file (backup + rewrite). Every flag, and a bare `setup` without a terminal, run the old non-interactive code unchanged. New flag `--no-wizard` gets the old flow (token prompts for claude-oauth backends) in a terminal. The installers' `agent-sidekick setup` call now lands in the wizard.
- **Order.** Prerequisites are shown first (Claude Code with path and version, Node.js, OpenCode), then the profiles are chosen, and only then are Node.js, OpenCode and the Gemini CLI required, because Node.js is only needed for what the chosen profiles use (installing OpenCode for KI:connect when it is missing; always for Gemini, whose command runs with node). Claude Code is always required (registration). Missing Node.js when needed: instructions (nodejs.org link; Windows `winget install OpenJS.NodeJS.LTS`; Linux distro package or nvm) and abort before anything is written. All installs happen before the config step, so an abort there leaves no config and no secret behind. Secrets are asked after the new config was built and validated, before it is written.
- **npm without a shell.** Windows: `node <node dir>\node_modules\npm\bin\npm-cli.js install -g <package>` (the file ships with the Node.js installer, winget and nvm-windows); never `npm.cmd` or `cmd /c`. If it is missing, the wizard prints the manual command instead. Unix: `node <prefix>/lib/node_modules/npm/bin/npm-cli.js` (also `/usr/share/nodejs`, `/usr/lib/node_modules`), else `npm` from PATH executed directly (a shebang script). The package names are constants; no user input reaches the command line. A failed install aborts with the manual command as next step (Unix: hint for EACCES, user prefix or nvm). Declining the OpenCode install continues with a hint (config and key are still written; doctor shows OpenCode missing). Declining the Gemini CLI drops the Gemini profile (its command needs the entry path).
- **Gemini backend.** Command backend `gemini`: `[<node>, <entry>, "--approval-mode=yolo", "--skip-trust", "--model={model}", "--prompt={prompt}"]`, `models = ["auto", "pro", "flash"]` (the CLI's aliases, so the config survives model renames; `auto` lets the CLI choose), `secret_env = "GEMINI_API_KEY"` (keyring user `cmd-gemini`), cost `quota`. Node and entry are absolute paths filled in by the wizard (TOML-escaped), so the MCP server does not depend on its PATH. The entry is found through the npm global root: `npm root -g` (through the same shell-free npm invocation), then the `.cmd` shim directories in PATH, `%APPDATA%\npm\node_modules` and the node directory (Windows), or `<prefix>/lib/node_modules` for node's prefix and every PATH `.../bin` (Unix). The file is the `bin.gemini` entry of the package's `package.json` (`bundle/gemini.js` in 0.65.0), with `bundle/gemini.js` as fallback; a `bin` path leaving the package is ignored. The `--flag=value` forms keep a prompt that starts with `-` from being parsed as an option. Flags checked against `gemini --help` of 0.65.0 (`-p/--prompt` is the headless mode, `--approval-mode yolo`, `--skip-trust` trusts the workspace for the session). Not verified: a real Gemini run through agent-sidekick (no Gemini key was used). If the user's own Gemini CLI settings select another auth type (Google login), the CLI may prefer it over `GEMINI_API_KEY`.
- **KI:connect profile** = the README/example opencode backend `kiconnect` (provider openai-compatible, `https://chat.kiconnect.nrw/api/v1`, models `openai-gpt-oss-120b` and `qwen-qwen3-8-27b`, same description). **Claude-Zweitkonto** = claude-oauth backend `sidekick`, so the v0.1 secret locations (keyring user `oauth-token`, file `token`) apply and an existing token keeps working; its config dir (`~/.claude-sidekick`) is created like `setup` does.
- **Existing config.toml.** Options: merge (default; only offered if the file parses), backup + rewrite, cancel. Merge appends the missing profile tables as text to the unchanged file (a missing final line break is added), never a TOML round trip, so existing backends and comments stay byte-identical. Profiles whose backend name exists are kept as they are; the key prompt then uses the existing backend's own `secret_id()` (e.g. the example's aider `kiconnect` reads `cmd-kiconnect`), and only opencode backends are probed. If the file had no `[backends]` (implicit `sidekick`), the sidekick template is appended too, so adding a backend does not remove the implicit one; its stored token keeps working and is not asked for. An existing `default = true` is kept (changing it would edit an existing backend); new backends then get none. If nothing is missing, the file is not touched. Before writing, merge and rewrite copy the old file to `config.toml.bak-<YYYYmmdd-HHMMSS>` (`-2`, `-3`, ... if that exists). The new text is validated with `Config` + `resolve_with` before anything is asked or written; an invalid result (e.g. the old file already had two defaults) aborts with "Sichern und neu schreiben" as next step. Written through a temp file and rename.
- **Default backend.** Asked only when more than one new backend is added and no existing default is kept; suggestion `kiconnect` if chosen, else the first.
- **Keys.** Hidden input (`rpassword`), asked again when empty or with control characters (three tries, then nothing is stored and `setup --backend <name>` is named as next step). A stored secret is kept unless the user chooses to replace it. A Claude token without `sk-ant-oat01-`: warning, stored anyway (as in `setup`). KI:connect: optional online check (asked, default yes) with `GET {base_url}/models` and the key as bearer (`reach::probe_get`, 15 s). 401/403: "Schlüssel abgelehnt (HTTP 401) – erstelle einen neuen auf https://chat.kiconnect.nrw", then the choice to enter a new key or not to store one. No answer: warning, key stored. Secrets are never printed; the tests assert that for the whole output and every error.
- **Registration and doctor.** `setup::register` (keeps the registration's extra `-e` variables) and `doctor::run_checks` + `format_report` (without the update hint) are reused. A failed registration is reported with `setup --register-only` as next step, and the wizard continues with doctor.
- **Testability.** `io::Prompter` (say, confirm, choose_one, choose_many, secret) with the real `Terminal` and a scripted implementation in the tests; `system::System` (locators, versions, visible command runs, secret store, HTTP probe, directory creation, registration, doctor, timestamp) with `RealSystem` and a fake that keeps everything in a temp dir. The tests cover fresh installs per profile and combinations, an empty selection, missing Claude Code / Node.js (Windows and Unix texts), OpenCode and Gemini CLI installs with the exact argv, declined and failed installs, an existing config (cancel, backup + rewrite, merge into `examples/config.toml`, merge into an implicit config, unreadable and invalid files), the default selection, stored secrets, invalid input, 401/403 from a local HTTP server through the real `probe_get`, an unreachable provider, a failed registration and the final doctor call. The templates are resolved alone, combined (every default choice) and merged into `examples/config.toml`. `system::tests::real_locators` (ignored) prints what the real locators find; on the development machine it found claude, node v24, OpenCode, `node.exe npm-cli.js` and the Gemini CLI's `bundle/gemini.js`.
- **Checks (setup wizard):** `cargo fmt --check` clean; clippy `-D warnings` clean on Windows and cross for `x86_64-unknown-linux-gnu`; Windows `cargo test --locked`: lib 402 passed, 3 ignored, all integration tests pass; WSL Ubuntu: lib 399 passed, 3 ignored, all integration tests pass (backends_fallback 14, command_secret 2, e2e_fake_api 5, mcp_handshake 9, opencode_fake 6, runner_fake_claude 9, runner_linux_setsid 1, update_command 9). The wizard itself was not run against the real config dir, keyring or Claude registration (by design of this task); `real_locators` was run read-only.
- The Gemini prompt is passed as one argument (`--prompt={prompt}`), so the Windows command-line limit (about 32 KiB) applies, as for the aider example; `prompt_via_stdin` was not used because `-p` would then append stdin to an empty prompt and the behaviour is less documented.
