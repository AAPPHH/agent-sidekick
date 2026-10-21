# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-10-21

### Added

- Configurable backends in `config.toml` under `[backends.<name>]` with `type`, `description`, `default`, `cost` and optional `max_concurrent`, `env`, `pass_env`:
  - `claude-oauth`: the `claude` CLI with a Claude account's OAuth token (v0.1 behaviour), optional own `config_dir` and `model`.
  - `claude-endpoint`: the `claude` CLI against an Anthropic-compatible endpoint (e.g. Ollama, LM Studio) with `base_url`, `model`, `auth` (`x-api-key` or `bearer`), the key from `api_key_env` or the keyring/file store, and its own `config_dir` (default `~/.claude-sidekick-<name>`).
  - `command`: any agent CLI as an argument array with `{prompt}`, `{cwd}`, `{max_turns}` placeholders, started without a shell; optional `prompt_via_stdin`.
  - Without `[backends]`: an implicit `claude-oauth` backend `sidekick`, so existing installations keep working unchanged.
- Per-backend secrets: keyring users `oauth-token-<name>` / `endpoint-<name>`, files `token-<name>` / `endpoint-<name>.key`; a `claude-oauth` backend named `sidekick` keeps the v0.1 `oauth-token` / `token` / `AGENT_SIDEKICK_TOKEN`.
- Tool `sidekick_backends`: name, type, description, cost, default and reachability of every backend (no secrets).
- `sidekick_run` parameters `backend` and `fallback`; the result names the backend that actually ran. A fallback from one `claude-oauth` backend to another is rejected.
- `sidekick_status` shows running and queued runs per backend; optional per-backend `max_concurrent` in addition to the global limit.
- Reachability probe for `claude-endpoint` backends: HTTP GET of `base_url` without credentials, 2 s timeout.
- CLI: `backends list [--check]`, `backends test <name>`, `setup --backend <name>`; `setup` asks for the token of every `claude-oauth` backend; `doctor` checks all backends.
- Top-level and per-backend `pass_env` to keep specific inherited variables that session isolation would remove.
- `examples/config.toml` and `examples/CLAUDE.md` (routing rules for the calling session).

### Changed

- An invalid `config.toml` now makes `sidekick_run` fail with an error naming the problem instead of silently running with default settings; `sidekick_backends` and `sidekick_status` show the error.
- Secrets shorter than 8 characters (e.g. placeholder keys like `ollama`) are no longer redacted from output; `sk-ant-…` patterns are always redacted.
- The list of variables removed from the child environment is extended for session isolation: every `CLAUDE_CODE_*` and `CLAUDE_AGENT_SDK_*` variable, `CLAUDECODE`, `CLAUDE_PID`, `CLAUDE_EFFORT`, `AI_AGENT`, `MCP_CONNECTION_NONBLOCKING` (in addition to the credentials removed before). An inherited `ANTHROPIC_BASE_URL` is only kept for `claude-oauth` backends.
- `default_model` applies only to the implicit backend; explicit backends use their own `model`.

### Fixed

- Claude Code session variables inherited by the MCP server (session id, entrypoint, messaging socket and token, ...) were passed to the delegated `claude`, which then identified as part of the parent session and, with an API key, refused to start ("Not logged in").
- Cancelling a `sidekick_run` call was ignored; it now kills the run's whole process tree, and a run cancelled while queued never starts.
- Long runs could hit Claude Code's MCP tool idle timeout; the server now sends progress notifications every 30 s.
- On SIGTERM/SIGINT or client disconnect, running process trees are killed before the server exits.
- Unix: process trees survived when a run ended other than by timeout; timeout and cancellation send SIGTERM, then SIGKILL after 3 s. Linux: the server is a child subreaper and sweeps descendants that left the process group (e.g. `setsid` background commands).
- Prompts over 8000 characters failed on Windows (command-line limit); they are passed via stdin. "Command line too long" errors are reported clearly.
- `ANTHROPIC_AUTH_TOKEN` and `CLAUDE_CODE_USE_BEDROCK`/`VERTEX`/`FOUNDRY` inherited from the caller could outrank the agent's token; they are removed.
- Very long output lines bypassed the memory cap; secrets are still redacted across the split points.
- Logging no longer panics when the log file cannot be created (stderr only, with a warning).
- `doctor` checks the `claude` path stored in the MCP registration and no longer creates the logs directory; `setup` without `--token-stdin` stops with a hint when stdin is not a terminal; re-registration respects the scope of an existing entry.
- `install.ps1` keeps `%VAR%` references in the user PATH; CI runs clippy and tests with `--locked`.

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
