# agent-sidekick

agent-sidekick is a local MCP server (stdio) that lets a normal Claude Code session hand off prompts to a second, headless Claude Code instance.
The second instance runs with its own credentials (a separate account's OAuth token) and its own config directory, so its usage and settings stay apart from your main session.
It is a single binary with no runtime dependencies besides the official `claude` CLI, for Linux, Windows 11 and macOS.

## Quickstart

1. **Install** the binary (release archives are verified against `SHA256SUMS`):

   ```sh
   # Linux / macOS (installs to ~/.local/bin)
   curl -fsSL https://github.com/OWNER/agent-sidekick/releases/latest/download/install.sh | sh
   ```

   ```powershell
   # Windows (installs to %LOCALAPPDATA%\Programs\agent-sidekick and adds it to the user PATH)
   irm https://github.com/OWNER/agent-sidekick/releases/latest/download/install.ps1 | iex
   ```

   Or build from source: `cargo install --path .`

   > `OWNER/agent-sidekick` is a placeholder until the repository is published; set `REPO=owner/name` for the install scripts.

2. **Create a token for the agent account.** Log in with the account the sidekick should use:

   ```sh
   CLAUDE_CONFIG_DIR=~/.claude-sidekick claude setup-token
   ```

   ```powershell
   $env:CLAUDE_CONFIG_DIR = "$HOME\.claude-sidekick"; claude setup-token; Remove-Item Env:CLAUDE_CONFIG_DIR
   ```

3. **Store the token and register the MCP server** with Claude Code:

   ```sh
   agent-sidekick setup
   ```

4. **Reload VS Code** (Command Palette: "Developer: Reload Window") or restart Claude Code, then check that the agent-sidekick server shows up as connected in `/mcp`.

## Usage

From a Claude Code session, just ask:

> Use sidekick_run with cwd ~/projects/foo and prompt "Run the test suite and fix failing tests"

> Use sidekick_status to show running and queued sidekick runs.

## Tool reference

### `sidekick_run`

| Parameter       | Default              | Notes |
|-----------------|----------------------|-------|
| `prompt`        | (required)           | Task for the delegated agent. |
| `cwd`           | server's cwd         | Working directory. Always pass it; the server's cwd is wherever Claude Code started it. |
| `max_turns`     | 30                   | Maximum 200. |
| `timeout_s`     | 1800                 | Maximum 7200. The whole process tree is killed on timeout. |
| `model`         | `default_model` / CLI default | Passed as `--model`. |
| `allowed_tools` | none                 | List passed as `--allowedTools`. |

Returns the exit code, the path of the full run log and the last 4000 characters of output.
At most 2 runs execute concurrently (configurable); further runs are queued.

### `sidekick_status`

Shows running and queued runs.

## Configuration

Optional `config.toml` in the config directory:

```toml
max_concurrent = 2        # parallel runs; more are queued
default_max_turns = 30
default_model = "sonnet"  # optional
default_timeout_s = 1800
claude_path = "/abs/path/to/claude"  # optional
```

Config directory:

- Linux: `~/.config/agent-sidekick`
- Windows: `%APPDATA%\agent-sidekick`
- macOS: `~/Library/Application Support/agent-sidekick`

Environment variables:

| Variable | Purpose |
|----------|---------|
| `AGENT_SIDEKICK_CLAUDE` | Absolute path to the `claude` executable (checked before `claude_path` and PATH). |
| `AGENT_SIDEKICK_CLAUDE_CONFIG_DIR` | `CLAUDE_CONFIG_DIR` for the agent (default `~/.claude-sidekick`). |
| `AGENT_SIDEKICK_CONFIG_DIR` | Overrides the config directory. |
| `AGENT_SIDEKICK_TOKEN` | Token for CI/testing; takes precedence over the keyring and token file. |
| `AGENT_SIDEKICK_DISABLE_KEYRING` | Set to `1` to never use the OS keyring (file fallback only). |
| `AGENT_SIDEKICK_LOG` | `tracing` filter, e.g. `debug` or `agent_sidekick=trace`. |

## How it works

For each run the server starts the official CLI:

```
<claude> -p --max-turns N [--model M] [--allowedTools T1 T2 ...] -- <prompt>
```

The child environment has `ANTHROPIC_API_KEY` removed and `CLAUDE_CONFIG_DIR` plus `CLAUDE_CODE_OAUTH_TOKEN` set to the agent's config dir and token.
On Windows, if `claude` is a `.cmd` shim, the prompt is passed via stdin instead of the command line (avoids `cmd.exe` quoting issues).
A timeout kills the whole process tree (Job Object on Windows, process group on Unix).

## Security

- The token is stored in the OS keyring (service `agent-sidekick`, user `oauth-token`). If no keyring is available it falls back to `<config-dir>/token` with mode 0600 (Unix) or a user-only ACL (Windows).
- The token never appears in logs, error messages or tool output.
- To rotate or revoke: revoke the token in the Claude account settings, then run `claude setup-token` again (step 2) and `agent-sidekick setup`.
- Never commit tokens to Git.
- The delegated agent runs with the permissions you grant via `allowed_tools`. In `-p` mode the workspace trust dialog is skipped, so only point `cwd` at directories you trust.

## VS Code

- The extension starts MCP servers with a reduced PATH. `agent-sidekick setup` therefore stores the absolute path of `claude` as `AGENT_SIDEKICK_CLAUDE` in the MCP registration.
- Remote / WSL / SSH: MCP servers run on the remote side. Install agent-sidekick and run `agent-sidekick setup` there. The OS keyring is often unavailable in WSL/SSH sessions; the file fallback is used automatically.

## Terms of use

agent-sidekick only drives the official `claude` CLI with credentials you choose. It makes no direct API calls and does no account switching, load balancing or rate-limit circumvention.
You are responsible for complying with Anthropic's terms for each account you use.

## Troubleshooting

Run `agent-sidekick doctor` first; it checks the installation and prints next steps.

- **`claude` not found**: install Claude Code, or set `AGENT_SIDEKICK_CLAUDE` / `claude_path` to its absolute path, then re-run `agent-sidekick setup`.
- **Token missing**: repeat Quickstart steps 2 and 3.
- **Server not listed in `/mcp`**: reload the VS Code window or restart Claude Code.
- **Logs**: `<config-dir>/logs` (more detail with `AGENT_SIDEKICK_LOG=debug`).

## Uninstall

```sh
agent-sidekick uninstall          # remove stored token and MCP registration
agent-sidekick uninstall --purge  # also delete the config directory (config, token file, logs)
```

Then delete the binary.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your option.
