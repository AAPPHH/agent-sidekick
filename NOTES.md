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
- `backends::reach::tests::silent_server_times_out` failed once at the `contains("timed out")` assertion under the same load; the detail text was not captured and it did not reproduce. If it recurs, capture the `Reachability` debug output from the assertion message before changing the test. It recurred once more in the first full WSL run right after a fresh build of the harness branch (2026-10-21); next time keep the line after "panicked at" (e.g. `grep -A3`) so the `{r:?}` detail survives.
- `tests/backends_fallback.rs` `stopped_run_still_names_backend` hit the 30 s `wait_backend_line` deadline once in WSL while a full Windows test suite ran in parallel on the same machine; it passed 3/3 in isolation and in a full rerun. Load flake (the test waits for a line from a child process); test unchanged.

## Harness adapters (0.4.0)

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
