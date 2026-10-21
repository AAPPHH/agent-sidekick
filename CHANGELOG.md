# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0] - 2026-10-21

### Added

- Harness adapters (step 1): `src/harness/` with a common `Harness` trait (locate, `--version`, credential, launch building, output normalization, reachability check, install hint) and the adapters `claude` and `command`. The launch, credential and reachability logic of `claude-oauth`, `claude-endpoint` and `command` backends moved behind them unchanged (same argv, environment, session isolation, secrets, fallback, results and logs).
- `harness = "claude" | "command"` per backend. `claude` without `base_url` is the claude-oauth mode, with `base_url` the claude-endpoint mode. The legacy `type` keeps working; `type` and `harness` together must agree.
- `agent-sidekick harness list` (installed, path, version, backends using it) and `agent-sidekick harness install <name>` (prints the official install command, never runs it).
- OpenCode harness (step 2): `harness = "opencode"` with `provider = "openai-compatible" | "ollama" | "openrouter"`, `base_url`, `models`/`model` (required), optional `api_key_env`, `harness_path` and `read_only`. Runs `opencode run --format json --model <provider>/<model> --title agent-sidekick` with the prompt on stdin, in a per-backend isolated directory `<config-dir>/opencode/<name>/` (own home, XDG dirs, temp, npm cache), with a generated `opencode.json` that references the key only as `{env:AGENT_SIDEKICK_PROVIDER_KEY}`, a permission set without any "ask" (also passed as `OPENCODE_PERMISSION`), `max_turns` as the build agent's (soft) step limit and the `OPENCODE_DISABLE_*` flags. Inherited `OPENCODE_*`, `ANTHROPIC_*`, `OPENAI_API_KEY`, `OPENROUTER_API_KEY`, `PWD`/`OLDPWD` and the Claude OAuth token never reach it.
- The opencode JSON event stream is translated into the result: final answer, `[error]` lines, `stopped: permission auto-rejected`, and a usage line (steps, tokens in/out, cost). A run that exits 0 without an answer counts as failed (`failed: no answer: ...`) and allows a fallback.
- Provider key per opencode backend: `agent-sidekick setup --backend <name>` (OS keyring user `backend-<name>`, file fallback `backend-<name>.key`; nothing for ollama); `uninstall` removes it; `sidekick_status` shows its source.
- Reachability of opencode backends (`sidekick_backends`, `backends list --check`/`test`, `doctor`, and right before each run): OpenCode installed, key available, and the provider's model list answers (`GET {base_url}/models` with the key as bearer token; ollama `GET /api/tags`; OpenRouter's public list). 401/403 means the key was rejected.
- `doctor` checks the harness executables used by the configured backends (opencode: path and version); `harness list`/`install` know `opencode` (`opencode --version` runs in a temporary isolated home).
- A warning in the run log and the server log when the run's project contains OpenCode plugins (`.opencode/plugin(s)` with `.js`/`.ts` files, or a `plugin`/`plugins` entry in `opencode.json(c)` or `.opencode/opencode.json(c)` up to the repository root), which OpenCode executes regardless of its flags, with the provider key in their environment.
- `agent-sidekick update`: checks the latest GitHub release and prints the platform's install one-liner (no download, no self-install). `AGENT_SIDEKICK_UPDATE_URL` overrides the API base.
- `doctor` shows an update hint at most once a day (checked and shown once, result kept in `<config-dir>/update-check.json`); `doctor --offline` skips it.
- Cargo.toml `repository` (https://github.com/AAPPHH/agent-sidekick).

### Changed

- The `secret_env` of a command backend is now removed from every backend's inherited environment, like an `api_key_env` (the command backend itself still gets its stored value).
- `sidekick_run` treats a harness-reported failure (exit 0 without an answer) as an error and as a fallback trigger; such results contain a `failed: ...` line after `exit_code`.

## [0.3.0] - 2026-10-21

### Added

- Models per backend ("one backend per provider, the model as a parameter"): `models` (all types) lists the models a backend offers and is an allowlist for the `model` parameter of `sidekick_run`; `model` is the default and must be one of `models` (without it the default is the first entry). Names must be unique, non-blank, without leading/trailing whitespace or control characters, and must not start with `-`. Without `models`, claude-* backends accept any model as before.
- `{model}` placeholder for `command` backends, substituted in the same single pass as `{prompt}`, `{cwd}` and `{max_turns}` (never in `command[0]`). A command using `{model}` requires `models`; `models` or `model` without `{model}` is a config error.
- A `model` not offered by the selected backend is rejected before anything runs (the error lists the allowed models; no fallback). A fallback that does not offer the requested model runs with its own default model, and the result notes it.
- `sidekick_backends` returns `models` and `default_model` per backend; `sidekick_status` shows each backend's default model and allowlist; `backends list` has a MODELS column (default first, marked `*`); `backends test` prints the default model and the models.
- README section "Models"; `examples/config.toml` and `examples/CLAUDE.md` configure `kiconnect` as one backend offering `openai-gpt-oss-120b` and `qwen-qwen3-8-27b`.

### Changed

- The first line of the `sidekick_run` result includes the model: `backend: <name> (<type>), model <m>` (omitted when no model was requested and the backend has no default, or a command backend takes no model).
- `model` is now allowed for `command` backends (as the default value of `{model}`).
- The note about ignored parameters of command backends names only what was actually ignored (`model` only when the command has no `{model}`).

## [0.2.0] - 2026-10-21

### Added

- Configurable backends in `config.toml` under `[backends.<name>]` with `type`, `description`, `default`, `cost` and optional `max_concurrent`, `env`, `pass_env`:
  - `claude-oauth`: the `claude` CLI with a Claude account's OAuth token (v0.1 behaviour), optional own `config_dir` and `model`.
  - `claude-endpoint`: the `claude` CLI against an Anthropic-compatible endpoint (e.g. Ollama, LM Studio) with `base_url`, `model`, `auth` (`x-api-key` or `bearer`), the key from `api_key_env` or the keyring/file store, and its own `config_dir` (default `~/.claude-sidekick-<name>`).
  - `command`: any agent CLI as an argument array with `{prompt}`, `{cwd}`, `{max_turns}` placeholders, started without a shell; optional `prompt_via_stdin`.
  - Without `[backends]`: an implicit `claude-oauth` backend `sidekick`, so existing installations keep working unchanged.
- Per-backend secrets: keyring users `oauth-token-<name>` / `endpoint-<name>`, files `token-<name>` / `endpoint-<name>.key`; a `claude-oauth` backend named `sidekick` keeps the v0.1 `oauth-token` / `token` / `AGENT_SIDEKICK_TOKEN`.
- Tool `sidekick_backends`: a JSON object `{config_error, backends: [...]}` with name, type, description, cost, default, reachability, `max_concurrent`, `running` and `queued` of every backend (no secrets).
- `sidekick_run` parameters `backend` and `fallback`; the result names the backend that actually ran. A fallback from one `claude-oauth` backend to another is rejected.
- `sidekick_status` shows running and queued runs per backend; optional per-backend `max_concurrent` in addition to the global limit.
- Reachability probe for `claude-endpoint` backends: HTTP GET of `base_url` without credentials, 2 s timeout.
- CLI: `backends list [--check]`, `backends test <name>`, `setup --backend <name>`; `setup` asks for the token of every `claude-oauth` backend that has none yet; `doctor` checks all backends.
- `setup --register-only`: (re-)registers the MCP server without asking for tokens and without a terminal, e.g. after storing several tokens with `setup --backend <name> --token-stdin`.
- Top-level and per-backend `pass_env` to keep specific inherited variables that session isolation would remove.
- `secret_env` for `command` backends: a secret stored with `setup --backend <name>` (keyring user `cmd-<name>`, file `cmd-<name>.key`) is set in that variable for the backend's runs only, e.g. `OPENAI_API_KEY` for Aider against a hosted OpenAI-compatible API. It is redacted from output and logs and never shown by status, `sidekick_backends`, `backends test` or `doctor`; reachability requires it; `uninstall` removes it.
- `examples/config.toml` and `examples/CLAUDE.md` (routing rules for the calling session), both covering the same four backends (`sidekick`, `local`, `aider` and the optional `kiconnect`).

### Changed

- An invalid `config.toml` now makes `sidekick_run` fail with an error naming the problem instead of silently running with default settings; `sidekick_backends` and `sidekick_status` show the error.
- Secrets shorter than 8 characters (e.g. placeholder keys like `ollama`) are no longer redacted from output; `sk-ant-…` patterns are always redacted.
- The list of variables removed from the child environment is extended for session isolation: every `CLAUDE_CODE_*` and `CLAUDE_AGENT_SDK_*` variable, `CLAUDECODE`, `CLAUDE_PID`, `CLAUDE_EFFORT`, `AI_AGENT`, `MCP_CONNECTION_NONBLOCKING` (in addition to the credentials removed before). An inherited `ANTHROPIC_BASE_URL` is only kept for `claude-oauth` backends. `CLAUDE_CODE_GIT_BASH_PATH` is exempt from the prefix rule and kept (Claude Code on Windows needs it to find Git Bash).
- Further removals from the child environment: `ANTHROPIC_CUSTOM_HEADERS` for `claude-endpoint` and `command` backends, the inherited model overrides (`ANTHROPIC_MODEL`, `ANTHROPIC_SMALL_FAST_MODEL`, `ANTHROPIC_DEFAULT_*_MODEL`) for `claude-endpoint` backends, and every variable named in a backend's `api_key_env` for all other backends. A backend's own `pass_env` re-allows them.
- `default_model` applies only to the implicit backend; explicit backends use their own `model`. `sidekick_status` shows the global `default model` and agent config dir only for the implicit backend; with `[backends]` each `claude-*` backend's line shows its own effective `model` and `config_dir`.
- `setup` skips `claude-oauth` backends that already have a stored token (use `setup --backend <name>` to replace one), and re-registering keeps the extra `-e` variables of the existing user-scope MCP registration (names printed, values never; `CLAUDE_CODE_OAUTH_TOKEN`/`AGENT_SIDEKICK_TOKEN` are dropped with a warning).
- A fallback gets its own full `timeout_s`, so a `sidekick_run` call with `fallback` can take up to about 2 x `timeout_s` plus queue time; progress reports the fallback as queued while it waits for a slot.

### Fixed

- Claude Code session variables inherited by the MCP server (session id, entrypoint, messaging socket and token, ...) were passed to the delegated `claude`, which then identified as part of the parent session and, with an API key, refused to start ("Not logged in").
- Cancelling a `sidekick_run` call was ignored; it now kills the run's whole process tree, and a run cancelled while queued never starts.
- Long runs could hit Claude Code's MCP tool idle timeout; the server now sends progress notifications every 30 s.
- On SIGTERM/SIGINT or client disconnect, running process trees are killed before the server exits.
- Unix: process trees survived when a run ended other than by timeout; timeout and cancellation send SIGTERM, then SIGKILL after 3 s. Linux: the server is a child subreaper and sweeps descendants that left the process group (e.g. `setsid` background commands).
- Very long prompts failed (Windows command line limit of about 32K characters, Linux 128 KiB per argument). Prompts over 8000 characters (and every prompt to a `.cmd` shim) are now passed via stdin; "command line too long" errors are reported clearly.
- `ANTHROPIC_AUTH_TOKEN` and `CLAUDE_CODE_USE_BEDROCK`/`VERTEX`/`FOUNDRY` inherited from the caller could outrank the agent's token; they are removed.
- Very long output lines bypassed the memory cap; secrets are still redacted across the split points.
- Logging no longer panics when the log file cannot be created (stderr only, with a warning).
- `doctor` checks the `claude` path stored in the MCP registration and no longer creates the logs directory; `setup` without `--token-stdin` stops with a hint when stdin is not a terminal; re-registration respects the scope of an existing entry.
- `install.ps1` keeps `%VAR%` references in the user PATH; CI runs clippy and tests with `--locked`.
- A caller's `ANTHROPIC_CUSTOM_HEADERS` (e.g. a gateway credential) was sent to the third-party `base_url` of a `claude-endpoint` backend; it is now removed for `claude-endpoint` and `command` backends.
- The key read through an endpoint's `api_key_env` was inherited by every other backend's child (and not redacted there); it is now removed from all other backends.
- Re-running `setup` dropped variables added to the MCP registration with `-e` (such as an `api_key_env` key); they are now carried over.
- With several `claude-oauth` backends there was no non-interactive way to re-register, and an interactive re-register asked for every token again; see `setup --register-only` and the skipping of backends with a stored token.
- Run logs were created with default permissions; on Unix the logs directory is now 0700 and each run log 0600.
- `{prompt}` only in `command[0]` passed validation although the prompt was never delivered; placeholders in `command[0]` are now a config error and `{prompt}` must be in a later argument.
- `base_url` values the reachability probe cannot parse (non-ASCII host, invalid port) passed validation and then failed every run; they are now config errors.
- `sidekick_status` showed `default model` and `agent claude config dir` values that were not in effect with `[backends]`.
- After a fallback, progress said "agent running" while the fallback was still queued and could exceed its total.
- `CLAUDE_CODE_GIT_BASH_PATH` was removed by session isolation, so `claude-*` backends could fail to start on Windows.

## [0.1.0] - 2026-10-21

### Added

- MCP server over stdio with the tools `sidekick_run` and `sidekick_status`.
- Headless delegation to the official `claude` CLI (`-p`) with a separate config dir and OAuth token.
- Per-run limits: `max_turns` (default 30, max 200), `timeout_s` (default 1800, max 7200), optional `model` and `allowed_tools`.
- Concurrency limit (default 2) with queueing of further runs.
- Process-tree termination on timeout (Windows Job Object, Unix process group).
- Token storage in the OS keyring with a user-only file fallback; token redaction in logs and output.
- `setup`, `doctor` and `uninstall [--purge]` commands.
- Configuration via `config.toml` and `AGENT_SIDEKICK_*` environment variables.
- CI (Linux, Windows, macOS), release workflow for five targets, `install.sh` and `install.ps1`.
