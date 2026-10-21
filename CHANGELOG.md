# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
