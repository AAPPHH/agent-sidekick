# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Outcome `reset` (owners only, no job_id; `data validate` rejects it from other prefixes): trust of the model restarts from the start values at the reset time, only earlier blocked events are re-applied. The Steckbrief shows "zurückgesetzt am <date>". Schema version stays 1 (readers accept the new value, old files unchanged).
- Optional profile columns in models.parquet (strengths, weaknesses, task_scope, dims_prior, benchmarks, sources, profile_as_of) and a Steckbrief section "Profil" that never influences trust. Old files without the columns still read and validate.

### Changed

- claude-oauth model aliases (opus, sonnet, haiku, fable) resolve to full ids for the model_id and the CLI call. Dispatch runs claude with `--output-format json`, compares the reported model with the expected id and warns on mismatch; Gold is applied only with a verified model, otherwise returned as branch/patch.

## [0.8.0] - 2026-10-27

### Added

- Job-bound verdicts: every `sidekick_dispatch` appends one event to the own session log (outcome `note`, or the blocked/aborted event) with job_id, model_id, dimension, scope, repo, duration and the time the result was returned. `sidekick_record_experience` takes `job_id`: model, dimension, scope, repo, steps and duration come from that event, `review_s` is measured, the caller supplies only `accepted|rejected`, `conclusion` and `visibility`. One verdict per job, none for blocked jobs. The free-form path stays for seeds and notes.
- Proposal mode: write-tests and write-code jobs below the write threshold still get no write tools, but are asked for the complete change as one unified diff between `<<<SIDEKICK-PATCH` and `SIDEKICK-PATCH>>>`. The sidekick checks (`git apply --check`) and applies it only in the job worktree, runs the same mechanical diff check and returns branch and patch for review, never applied to the target. No or invalid patch gives the new status `NoPatch` with the reason and no trust change.

### Changed

- `sidekick_run` on a backend and model with a registry row requires `dimension` (optional `scope`) and runs the dispatch path. Unregistered backends run as before, without trust effect.
- `record_experience` says what was pushed (numbers only for private events); `sidekick_sync` likewise. The Steckbrief shows the last block as a UTC date, never prints `-0s`, and marks missing write right as "nein (Vorschlagsmodus)" for write-code and write-tests.

## [0.7.1] - 2026-10-27

### Fixed

- OpenCode context and limits: the generated config declared custom models without a token limit, so OpenCode never compacted and sent the whole history, including all tool outputs, on every step (more than 1M tokens per step to a 256k model). The models now get `limit.context` and `limit.output` (sent as max_tokens), compaction with pruning of old tool outputs is enabled and tool outputs are capped. New backend keys `context_tokens` and `max_output_tokens`; dispatched jobs use the registry's `context_budget_tokens`, `tool_output_lines` and `max_steps`, and OpenCode no longer reports them as unsupported.
- Data repository: owners may change every file at the repository root and `.github/**`, not only the rule files; other prefixes and `aggregate/` stay forbidden for everyone.
- Help text of `agent-sidekick data` no longer repeats "(used by its workflows)".

### Changed

- Start data: the registry has no Codex-via-KI:connect row any more; the Qwen3-Coder-Next row has `context_tokens` 262144.

## [0.7.0] - 2026-10-27

### Added

- Trust and Steckbriefe: every model (provider, model, checkpoint, quant, harness) has a trust value per dimension and scope, computed from experience events; Opus 5.5 is Gold. Trust decides write and decisions rights and the review share.
- Parquet data: `models`, `protected` and per-session experience files with schema versions; `sidekick_query` runs SQL over them.
- Gated dispatch: `sidekick_dispatch` runs a job in its own git worktree, derives the harness tools and limits from trust, checks the diff against protected paths and terms and records blocked and aborted outcomes.
- MCP tools `sidekick_steckbrief`, `sidekick_dispatch`, `sidekick_record_experience`, `sidekick_query` and `sidekick_sync`.
- Data sync with the private repository `AAPPHH/sidekick-data` (pseudonymised user and machine IDs, private texts stay local) and the subcommands `agent-sidekick data validate` and `agent-sidekick data aggregate` for its workflows.

### Upgrading

- New config keys `data_repo` (default `AAPPHH/sidekick-data`; `owner/name`, a full git URL or a local path) and `data_name` (your prefix under `experiences/`; without it nothing is pushed). The data repository is private, so git needs access to it; without access the sidekick keeps working on its local data.

## [0.6.0] - 2026-10-23

### Changed

- The second Claude account's backend is now called `claude` (was `sidekick`): the implicit backend of a config.toml without `[backends]`, the wizard's Claude profile, its template and the examples. Use `agent-sidekick setup --backend claude` and `backend: "claude"` in `sidekick_run`.
- A claude-oauth backend named `claude` uses the v0.1 token (keyring user `oauth-token`, file `token`, `AGENT_SIDEKICK_TOKEN`) and the config dir `~/.claude-sidekick`, as `sidekick` did. Existing installations keep their token and login; nothing needs to be set up again.

### Upgrading

- Without a `[backends]` section the token, login and `default_model` stay, but the backend's name changes: `backend: "sidekick"` in `sidekick_run` and `setup --backend sidekick` / `backends test sidekick` now fail with "unknown backend" (there is no alias). Replace `sidekick` with `claude` in your CLAUDE.md delegation rules and scripts.
- A config.toml with `[backends.sidekick]` keeps working unchanged (same token, same config dir), and the wizard treats it as the existing Claude profile. Rename the section to `[backends.claude]` to use the new name; the stored token stays valid.
- `claude` and `sidekick` may not both be claude-oauth backends (they share one token); such a config is rejected with a message naming both.
- A config.toml that already declared its own claude-oauth backend named `claude` used `oauth-token-claude` and `~/.claude-sidekick-claude`; it now uses the shared v0.1 token and `~/.claude-sidekick`. To keep the old account, set `config_dir = "~/.claude-sidekick-claude"` and run `agent-sidekick setup --backend claude` once (this writes the shared token, replacing one that is already stored). The old secret (keyring user `oauth-token-claude`, file `token-claude`) is no longer used, and `uninstall` does not remove it; delete it by hand.
- The wizard counts an existing `[backends.claude]` as the Claude profile only if it is a claude-oauth backend; another backend type with that name makes the wizard skip the profile with a hint.

## [0.5.0] - 2026-10-22

### Added

- Claude backends: `permission_mode` (default, acceptEdits, auto, dontAsk, plan, bypassPermissions; `doctor` warns about bypassPermissions), `allowed_tools` (used when a `sidekick_run` request names none) and `disallowed_tools` (always applied, also against a request's own list). The wizard's second-account template sets acceptEdits, broad tools and a deny list for git push, sudo, publishing and reading the key folders.
- `model_options` for opencode backends (openai-compatible, ollama): a table merged into every model entry of the generated opencode.json.
- The server reloads config.toml before a tool call when the file changed; a running run keeps its config, a broken config keeps the last valid one and is reported as `config_error`.

### Changed

- Default global `max_concurrent` 16 (was 2); the second-account template has no per-backend limit.
- Groq template: only openai/gpt-oss-120b and openai/gpt-oss-20b, with `model_options = { interleaved = { field = "reasoning" } }`, and an honest description (about one agent step per minute on the free tier).
- Rate-limit detection also matches Groq's "Request too large ... tokens per minute".

### Fixed

- Groq runs failed from the second step on ("reasoning_content is unsupported") and with Qwen ("max_tokens must be less than or equal to 16384").

## [0.4.0] - 2026-10-22

### Added

- Groq as a second free backend: templates/groq.toml (harness opencode, provider openai-compatible, base_url https://api.groq.com/openai/v1, models openai/gpt-oss-120b, qwen/qwen3.8-27b, openai/gpt-oss-20b), a setup wizard profile "Groq (kostenlos, mit Limits)" with hidden key prompt (keyring user `backend-groq`), online key check against `/models` and the hint https://console.groq.com/keys on 401/403; README section "Groq-API-Schlüssel erzeugen".
- Rate limits (HTTP 429): opencode and codex runs are checked in their error events, claude and command runs in the output tail (only unambiguous API markers); the result gets `rate_limited: yes (retry after Ns)`, the wait time comes from "retry-after" or "try again in ..." (22.5s, 1m2.3s, 450ms, 5 minutes).
- A rate-limited run counts as failed, so `fallback` takes over (`fallback reason: rate limited`).
- Per-backend cooldown after a rate limit (the wait time, 60 s when unknown, at most 3600 s, in memory): `sidekick_backends` reports the backend as not reachable ("rate limited, retry in Ns"), `sidekick_status` lists it, `sidekick_run` goes straight to a fallback that is not cooling down or fails with the remaining time, and a cooling fallback is skipped.
- docs/REFERENCE.md section "Rate limits (HTTP 429)"; README FAQ "Groq oder KI:connect meldet 429 (Rate-Limit)".

### Changed

- `CLAUDE.md` now only imports the shared configuration from the `config_md` repository (`@../config_md/projects/agent-sidekick.md`).

## [0.3.1] - 2026-10-22

### Fixed

- A machine with the OpenAI Codex CLI but without Claude Code can now install and set up agent-sidekick and use it from Codex. Before, the setup wizard and the non-interactive `setup` aborted with "Claude Code wurde nicht gefunden" / "Claude Code is required for registration", and `doctor` failed on the missing `claude`.

### Changed

- Setup wizard: Claude Code is no longer required when the Codex CLI is installed. The prerequisites show both; without `claude` the wizard says "Claude Code nicht gefunden – agent-sidekick wird nur in Codex angemeldet", does not offer the profile "Claude-Zweitkonto", skips the Claude Code registration and registers in Codex without asking (an implicit `sidekick` backend of an existing config is not carried over then). Without either CLI it aborts before writing anything and names both install commands (Claude Code installer, `npm install -g @openai/codex`).
- Non-interactive `setup` and `setup --register-only`: when `claude` is not found (and no `--claude` was given) but the Codex CLI is installed, the server is registered in Codex instead (as `setup --codex` does); otherwise the error now also names `setup --codex`. `setup --codex` already worked without Claude Code (now covered by a test).
- `doctor`: a missing `claude` is a failure only when a `claude-*` backend is configured (including the implicit `sidekick`) or the Codex CLI is not installed; otherwise it is an `[OK]` note, the Claude Code registration check is skipped, and a missing Codex registration is a failure (Codex is then the only host).
- The install scripts say the setup registers the server "with Claude Code or Codex".
- README: "Voraussetzungen" asks for the Claude Code CLI or the Codex CLI, new section "Nur mit Codex (ohne Claude Code)"; docs/REFERENCE.md: section "Codex-only machines" and the setup/doctor rows.

## [0.3.0] - 2026-10-21

### Added

- Models per backend ("one backend per provider, the model as a parameter"): `models` (all types) lists the models a backend offers and is an allowlist for the `model` parameter of `sidekick_run`; `model` is the default and must be one of `models` (without it the default is the first entry). Names must be unique, non-blank, without leading/trailing whitespace or control characters, and must not start with `-`. Without `models`, claude-* backends accept any model as before.
- `{model}` placeholder for `command` backends, substituted in the same single pass as `{prompt}`, `{cwd}` and `{max_turns}` (never in `command[0]`). A command using `{model}` requires `models`; `models` or `model` without `{model}` is a config error.
- A `model` not offered by the selected backend is rejected before anything runs (the error lists the allowed models; no fallback). A fallback that does not offer the requested model runs with its own default model, and the result notes it.
- `sidekick_backends` returns `models` and `default_model` per backend; `sidekick_status` shows each backend's default model and allowlist; `backends list` has a MODELS column (default first, marked `*`); `backends test` prints the default model and the models.
- [docs/REFERENCE.md section "Models"](docs/REFERENCE.md#models); `examples/config.toml` and `examples/CLAUDE.md` configure `kiconnect` as one backend offering `openai-gpt-oss-120b` and `qwen-qwen3-8-27b`.
- Harness adapters (step 1): `src/harness/` with a common `Harness` trait (locate, `--version`, credential, launch building, output normalization, reachability check, install hint) and the adapters `claude` and `command`. The launch, credential and reachability logic of `claude-oauth`, `claude-endpoint` and `command` backends moved behind them unchanged (same argv, environment, session isolation, secrets, fallback, results and logs).
- `harness = "claude" | "command"` per backend. `claude` without `base_url` is the claude-oauth mode, with `base_url` the claude-endpoint mode. The legacy `type` keeps working; `type` and `harness` together must agree.
- `agent-sidekick harness list` (installed, path, version, backends using it) and `agent-sidekick harness install <name>` (prints the official install command, never runs it).
- OpenCode harness (step 2): `harness = "opencode"` with `provider = "openai-compatible" | "ollama" | "openrouter"`, `base_url`, `models`/`model` (required), optional `api_key_env`, `harness_path` and `read_only`. Runs `opencode run --format json --model <provider>/<model> --title agent-sidekick` with the prompt on stdin, in a per-backend isolated directory `<config-dir>/opencode/<name>/` (own home, XDG dirs, temp, npm cache), with a generated `opencode.json` that references the key only as `{env:AGENT_SIDEKICK_PROVIDER_KEY}`, a permission set without any "ask" (also passed as `OPENCODE_PERMISSION`), `max_turns` as the build agent's (soft) step limit and the `OPENCODE_DISABLE_*` flags. Inherited `OPENCODE_*`, `ANTHROPIC_*`, `OPENAI_API_KEY`, `OPENROUTER_API_KEY`, `PWD`/`OLDPWD` and the Claude OAuth token never reach it.
- The opencode JSON event stream is translated into the result: final answer, `[error]` lines, `stopped: permission auto-rejected`, and a usage line (steps, tokens in/out, cost). A run that exits 0 without an answer counts as failed (`failed: no answer: ...`) and allows a fallback.
- Provider key per opencode backend: `agent-sidekick setup --backend <name>` (OS keyring user `backend-<name>`, file fallback `backend-<name>.key`; nothing for ollama); `uninstall` removes it; `sidekick_status` shows its source.
- Reachability of opencode backends (`sidekick_backends`, `backends list --check`/`test`, `doctor`, and right before each run): OpenCode installed, key available, and the provider's model list answers (`GET {base_url}/models` with the key as bearer token; ollama `GET /api/tags`; OpenRouter's public list). 401/403 means the key was rejected.
- `doctor` checks the harness executables used by the configured backends (opencode: path and version); `harness list`/`install` know `opencode` (`opencode --version` runs in a temporary isolated home).
- A warning in the run log and the server log when the run's project contains OpenCode plugins (`.opencode/plugin(s)` with `.js`/`.ts` files, or a `plugin`/`plugins` entry in `opencode.json(c)` or `.opencode/opencode.json(c)` up to the repository root), which OpenCode executes regardless of its flags, with the provider key in their environment.
- Codex harness: `harness = "codex"` with `auth = "chatgpt"` (default; the user's ChatGPT plan, logged in once with `codex login` in the backend's own `CODEX_HOME`) or `auth = "api-key"` (OpenAI API key from the keyring user `backend-<name>` or `api_key_env`; with `base_url` a Responses-compatible provider; `wire_api` accepts only `responses`), optional `model`/`models` (without them any model, and Codex's default when none is requested), `read_only` (Codex's read-only sandbox instead of workspace-write), `windows_sandbox` (`mxc` default, `elevated`, `unelevated`) and `harness_path`. Runs `codex exec --json --ephemeral --skip-git-repo-check --color never --sandbox <mode> --cd <cwd> [-m <model>] [-c windows.sandbox=<impl>] -` (native `codex.exe`, never the `.cmd` shim) with the prompt on stdin, with `CODEX_HOME`/`CODEX_SQLITE_HOME` = `<config-dir>/codex/<name>/` and a generated `config.toml` there (file credential store, no update check, no history, no analytics, the shell environment filters below; for `base_url` a custom provider whose key is read from `AGENT_SIDEKICK_PROVIDER_KEY`). The key is only in the child's environment (`CODEX_API_KEY` or `AGENT_SIDEKICK_PROVIDER_KEY`), never in a file or argument. `max_turns` and `allowed_tools` are ignored (noted in the log). Cost defaults: `quota` for chatgpt, `paid` for api-key.
- The agent's own shell commands inside Codex never see a key: the generated config switches on Codex's exclusion of `*KEY*`/`*SECRET*`/`*TOKEN*` variables and excludes `AGENT_SIDEKICK_*`, `CODEX_API_KEY`, `CODEX_ACCESS_TOKEN`, `OPENAI_API_KEY`, `ANTHROPIC_*` and `CLAUDE*` (by default Codex 0.163 passes `CODEX_API_KEY`, `OPENAI_API_KEY` and a provider key to them).
- The codex JSON event stream is translated into the result: a compact activity summary (commands with exit code, MCP tool calls, changed files), warnings (transient `error` events never fail a run), codex's stderr lines, the final `agent_message`, `[error] turn failed: ...` and a usage line (tokens in/cached/out/reasoning). A run that exits 0 without an answer counts as failed and allows a fallback.
- Reachability of codex backends (`sidekick_backends`, `backends list --check`/`test`, `doctor`, before each run): Codex installed; chatgpt: the login file `<CODEX_HOME>/auth.json` exists (never read); api-key: key available and `GET {base_url or https://api.openai.com/v1}/models` accepts it. `setup --backend <name>` stores the API key, or (chatgpt) creates the backend's `CODEX_HOME` with its config and prints the exact `codex login` command for it; `uninstall` removes the key and the login file.
- Codex as MCP host: `agent-sidekick setup --codex` registers the server in the Codex CLI's config (`$CODEX_HOME/config.toml`, default `~/.codex/config.toml`), after a backup, with `tool_timeout_sec = 15000` (Codex cancels a tool call after 60 s by default and progress notifications do not extend it), `startup_timeout_sec = 30`, `AGENT_SIDEKICK_CLAUDE` (and set `AGENT_SIDEKICK_CONFIG_DIR`/`AGENT_SIDEKICK_CLAUDE_CONFIG_DIR`) as `env`, the `api_key_env`/`pass_env` names and proxy/CA variables as `env_vars` (Codex starts MCP servers with an environment allowlist), and `approval_mode = "approve"` for `sidekick_backends`/`sidekick_status` (`sidekick_run` keeps Codex's approval prompt). Written with `toml_edit`: the rest of the file stays byte for byte; re-registering updates the entry in place and keeps user settings. On Unix the file keeps its mode (a new one gets 0600); a symlinked config is written through to its target. `uninstall` removes it, `doctor` checks it when Codex is installed.
- Setup wizard: profile "OpenAI Codex (ChatGPT-Abo oder API-Key)" (`templates/codex.toml`, backend `codex`, `max_concurrent = 1`), offers `npm install -g @openai/codex` when Codex is missing, shows the `codex login` command for the ChatGPT plan, and asks "Auch in Codex anmelden?" when the Codex CLI is installed.
- `harness list`/`install` know codex (`codex --version` with a temporary `CODEX_HOME` that is removed afterwards; install hints: npm and the standalone installers).
- `agent-sidekick update`: checks the latest GitHub release and prints the platform's install one-liner (no download, no self-install). `AGENT_SIDEKICK_UPDATE_URL` overrides the API base.
- `doctor` shows an update hint at most once a day (checked and shown once, result kept in `<config-dir>/update-check.json`); `doctor --offline` skips it.
- Cargo.toml `repository` (https://github.com/AAPPHH/agent-sidekick).
- `request_timeout_s` for opencode backends: OpenCode's `headerTimeout` (seconds to wait for the response headers before the request is retried; unset keeps OpenCode's 300 s). Never OpenCode's `timeout`, which is a total request limit without retry.
- Setup wizard: `agent-sidekick setup` without flags in an interactive terminal (stdin and stdout) starts a guided setup in German: prerequisites (Claude Code path and version, Node.js, OpenCode), profiles "KI:connect.nrw (Hochschul-Kontingent, kostenlos)" (opencode backend `kiconnect`), "Claude-Zweitkonto" (claude-oauth backend `sidekick`, with `claude setup-token` instructions) and "Gemini" (command backend `gemini` running the Gemini CLI's JavaScript entry with node, key in `GEMINI_API_KEY`), `npm install -g opencode-ai` / `@google/gemini-cli` after confirmation (run as `node npm-cli.js ...` without a shell; on Unix also `npm` directly), the default backend, `config.toml` from embedded templates (`templates/`), keys with hidden input (the KI:connect key is optionally checked with `GET {base_url}/models`; 401/403 is reported as rejected), registration and a final `doctor` report. An existing `config.toml` is merged (missing backends appended, existing text unchanged), backed up to `config.toml.bak-<timestamp>` and rewritten, or left alone (cancel). Missing Node.js aborts with install instructions before anything is written.
- `setup --no-wizard`: the non-interactive setup in an interactive terminal. Every other flag, and a bare `setup` without a terminal, behaves as before.

### Changed

- The first line of the `sidekick_run` result includes the model: `backend: <name> (<type>), model <m>` (omitted when no model was requested and the backend has no default, or a command backend takes no model).
- `model` is now allowed for `command` backends (as the default value of `{model}`).
- The note about ignored parameters of command backends names only what was actually ignored (`model` only when the command has no `{model}`).
- The `secret_env` of a command backend is now removed from every backend's inherited environment, like an `api_key_env` (the command backend itself still gets its stored value).
- `sidekick_run` treats a harness-reported failure (exit 0 without an answer) as an error and as a fallback trigger; such results contain a `failed: ...` line after `exit_code`.
- The MCP instructions, the `sidekick_run` description and `--help` no longer say the default is a second Claude Code instance; a run without `backend` goes to the configured default backend (see `sidekick_backends`).
- `agent-sidekick update` and the `doctor` update hint print `cargo install --locked --git https://github.com/AAPPHH/agent-sidekick` on macOS and other non-Linux Unix systems (no prebuilt binary; `install.sh` stops there) instead of the `install.sh` one-liner.
- Setup wizard: "Claude Code wurde nicht gefunden" names the platform's native installer command, says that the VS Code extension alone is not enough and to open a new terminal (or restart VS Code); the Node.js instructions say to restart VS Code when its terminal is used; choosing Gemini shows a warning about `--skip-trust` (project `.env` / `.gemini/settings.json`).
- The header of a wizard-written `config.toml` links to `examples/config.toml` and `docs/REFERENCE.md#backends` on GitHub at this version's tag (the installed binary comes without them).
- A warning in the run log and the server log when a command backend whose `command` contains `gemini` runs in a project with `.env`, `.gemini/.env` or `.gemini/settings.json` up to the repository root (the Gemini CLI loads them because the workspace is trusted).
- Session isolation also covers a Codex host: inherited `CODEX_*` variables (`CODEX_HOME`, `CODEX_SESSION_ID`, `CODEX_THREAD_ID`, `CODEX_SANDBOX*`, `CODEX_API_KEY`, ...; except `CODEX_CA_CERTIFICATE`) are removed from every backend's child, like the Claude Code session variables.
- `auth` accepts `chatgpt`/`api-key` for codex backends besides `x-api-key`/`bearer` for claude-endpoint backends; each harness rejects the other's values.
- The MCP server instructions name the three tools and the possibly long run time within the first 512 characters (Codex uses only those reliably for tool selection).
- Run-log header: the generic `# max_turns:` line is left out when the harness explains it (opencode: step limit; codex: ignored), and `# model:` is left out when the `backend:` line already names the model used (so a backend's default model no longer shows as `(default)`).

### Fixed

- The setup wizard looked up `node` and `npm` with a plain PATH search, which on Unix resolves an empty or relative PATH entry against the current directory; it now uses only the absolute PATH entries, like command backends.
- The hint for removing a registration in another Claude Code scope read "Remove it with remove it where it is configured (unknown scope)" when `claude mcp get` named no scope; it now reads "remove it where it is configured" (`uninstall`, `setup`, `doctor`).

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
