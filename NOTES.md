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
- `backends::reach::tests::silent_server_times_out` failed once at the `contains("timed out")` assertion under the same load; the detail text was not captured and it did not reproduce. If it recurs, capture the `Reachability` debug output from the assertion message before changing the test.

## Distribution (release, install, plugin, update)

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

## Distribution (release, install, plugin, update)

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

## Distribution (release, install, plugin, update)

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
