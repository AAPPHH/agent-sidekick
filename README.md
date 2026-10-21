# agent-sidekick

agent-sidekick is a local MCP server (stdio) that lets a normal Claude Code session hand off prompts to a second, headless agent.
By default that agent is a second Claude Code instance with its own credentials (a separate account's OAuth token) and its own config directory, so its usage and settings stay apart from your main session.
Optionally, further [backends](#backends) send work to a local model (Claude Code against Ollama or LM Studio) or to another agent CLI such as Aider.
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

5. **Optional: add backends** (local model, other agent CLIs) in `config.toml`, see [Backends](#backends).
   Without a `[backends]` section everything works as described above.

## Usage

From a Claude Code session, just ask:

> Use sidekick_run with cwd ~/projects/foo and prompt "Run the test suite and fix failing tests"

> Use sidekick_backends, then delegate writing the missing unit tests in ~/projects/foo to the cheapest suitable backend.

> Use sidekick_status to show running and queued sidekick runs.

## Backends

A backend is a place where `sidekick_run` executes a prompt. Backends are configured in `config.toml` under `[backends.<name>]`; there are three types:

| Type | Runs | Credential |
|------|------|------------|
| `claude-oauth` | The official `claude` CLI with a Claude account's OAuth token (the v0.1 behaviour). | Token from `claude setup-token`, stored with `agent-sidekick setup`. |
| `claude-endpoint` | The official `claude` CLI against an Anthropic-compatible endpoint, e.g. a local model in Ollama or LM Studio. | API key stored with `agent-sidekick setup --backend <name>`, or read from an environment variable (`api_key_env`). |
| `command` | Any other agent CLI, e.g. Aider, started directly (no shell) with the prompt as one argument or on stdin. | None managed by agent-sidekick. |

**Upgrading from v0.1:** without a `[backends]` section nothing changes. There is one implicit backend named `sidekick` of type `claude-oauth` that uses the existing token, config dir and `default_model`.
As soon as `[backends]` exists, only the backends listed there are available.

### Example

Three backends: the second Claude account (default), Claude Code against a local `qwen3-coder` in Ollama, and Aider with the same model. The commented version is [`examples/config.toml`](examples/config.toml).

```toml
max_concurrent = 2          # global limit over all backends

[backends.sidekick]
type = "claude-oauth"
description = "Second Claude subscription account (Claude Code). Best quality for complex, multi-file changes and debugging; uses that account's quota."
default = true
cost = "quota"
max_concurrent = 1

[backends.local]
type = "claude-endpoint"
description = "Claude Code driving a local qwen3-coder via Ollama. Free and private, but slower and weaker than Claude: good for boilerplate, tests, docs and mechanical refactors; not for subtle multi-file logic."
cost = "free"
base_url = "http://localhost:11434"
model = "qwen3-coder"

[backends.aider]
type = "command"
description = "Aider with local qwen3-coder (Ollama). Free; edits files in cwd directly without asking (no tool permissions, no shell commands), changes are left uncommitted. Good for small, well-specified edits to named files."
cost = "free"
command = ["aider", "--model", "ollama_chat/qwen3-coder", "--yes-always", "--no-auto-commits", "--no-check-update", "--message", "{prompt}"]
env = { OLLAMA_API_BASE = "http://127.0.0.1:11434" }
max_concurrent = 1
```

Then store the credentials and check the backends:

```sh
agent-sidekick setup                     # token for every claude-oauth backend (here: sidekick)
agent-sidekick setup --backend local     # API key for the endpoint; for a local Ollama enter any value, e.g. "ollama"
agent-sidekick backends list --check
agent-sidekick backends test local
```

Notes for the `local` backend:

- **Ollama** (0.14 or later serves the Anthropic Messages API): `ollama pull qwen3-coder`, base URL `http://localhost:11434`. A local Ollama ignores the key, so any value works. Keys shorter than 8 characters (such as `ollama`) are not redacted from the agent's output, so a placeholder never mangles it; `setup` warns about this.
- **Ollama's cloud** (`https://ollama.com`) needs a real key and `auth = "bearer"`.
- **LM Studio**: `base_url = "http://localhost:1234"`, `auth = "bearer"` (LM Studio's documentation uses the Bearer form), `model` = the model identifier shown in LM Studio. Optionally `env = { CLAUDE_CODE_ATTRIBUTION_HEADER = "0" }`, as in LM Studio's Claude Code docs (helps its prompt caching).
- The model has to handle Claude Code's long system prompt and tool calls; give the server a large context window (e.g. `OLLAMA_CONTEXT_LENGTH`).

### How the backend is chosen

The **caller** (the main Claude Code session) picks the backend. It calls `sidekick_backends`, reads each backend's `description`, `cost` and `reachable`, and passes `backend` to `sidekick_run`.
agent-sidekick does no routing of its own, so write descriptions for that reader: what the backend is good and bad at, how fast it is, and what it costs. Routing rules in your `CLAUDE.md` help further, see the [CLAUDE.md routing example](#claudemd-routing-example).

- Without `backend`, `sidekick_run` uses the backend with `default = true`. Exactly one backend must be the default.
- An unknown backend name is a tool error that lists the configured names.
- If `config.toml` cannot be parsed or validated, `sidekick_run` fails with a tool error that names the problem; it never silently falls back to the implicit backend. `sidekick_backends` and `sidekick_status` show the error too.

### Fallback

`sidekick_run` accepts `fallback`, the name of a second backend that is used when the primary one fails:

- **Triggers:** the primary backend is not reachable (endpoint probe failed), its credential is missing, its executable is not found or cannot be started, or the primary run **exits with a non-zero exit code**.
- **Never** after a timeout or a cancellation, and not for invalid parameters (`prompt`, `cwd`, `max_turns`, ...).
- **The primary may have changed files.** After a non-zero exit the primary agent did run; the result says so explicitly, with its exit code and log path. Only use a fallback for tasks that are safe to retry, and check `git status` afterwards.
- Single level: the fallback has no fallback of its own. `fallback` equal to `backend` is an error.
- **A fallback from a `claude-oauth` backend to another `claude-oauth` backend is rejected** with an explanatory error: switching from one Claude account to another when the first one fails (for example because its usage limit is reached) would be account switching / rate-limit circumvention, which agent-sidekick does not do (see [Terms of use](#terms-of-use)).
- The primary run's concurrency slots (per backend and global) are released before the fallback waits for its own.

### Concurrency

`max_concurrent` at the top level is a global limit over all backends (default 2). A backend can additionally have its own `max_concurrent`, e.g. 1 for a local GPU or for the Claude account.
A run waits for a slot of its backend first, then for a global slot; queued runs are visible in `sidekick_status`.

### Reachability

`reachable` in `sidekick_backends` means:

- `claude-oauth`: `claude` is found and a token is stored for the backend.
- `claude-endpoint`: `claude` is found, the key is available (stored, or the `api_key_env` variable is set), and an HTTP `GET` of `base_url` answers within 2 seconds. Any HTTP response counts, also 4xx/5xx; connection errors and timeouts do not.
- `command`: the program (`command[0]`) is found.

The HTTP probe is the only network request agent-sidekick makes itself. It goes only to the configured `base_url`, carries no credentials (user info in the URL is stripped), follows no redirects, has a 2 s timeout and runs only for `sidekick_backends`, `agent-sidekick backends list --check` / `backends test`, `doctor`, and right before a run on a `claude-endpoint` backend.
Loopback hosts are probed directly; other hosts go through the proxy from `HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY` (honouring `NO_PROXY`), as `claude` itself would. A proxy that answers for an unreachable upstream makes the endpoint look reachable.

### Secrets per backend

| Backend | Keyring (service `agent-sidekick`), user | File fallback in the config dir | Environment override |
|---------|------------------------------------------|---------------------------------|----------------------|
| `claude-oauth` named `sidekick` (implicit or declared) | `oauth-token` | `token` | `AGENT_SIDEKICK_TOKEN` |
| other `claude-oauth` | `oauth-token-<name>` | `token-<name>` | none |
| `claude-endpoint` | `endpoint-<name>` | `endpoint-<name>.key` | `api_key_env` (a variable of the MCP server's environment) |
| `command` | none | none | none |

A backend named `sidekick` therefore keeps using the token of a v0.1 installation. Files are created with mode 0600 (Unix) or a user-only ACL (Windows).
`api_key_env` is read from the environment of the MCP server, i.e. what Claude Code passes to it; set the variable globally or add it to the registration (`claude mcp add ... -e NAME=value`).

### Child environment and session isolation

Every child inherits the MCP server's environment, minus:

- every variable starting with `CLAUDE_CODE_` or `CLAUDE_AGENT_SDK_`, and `CLAUDECODE`, `CLAUDE_PID`, `CLAUDE_EFFORT`, `CLAUDE_CONFIG_DIR`, `AI_AGENT`, `MCP_CONNECTION_NONBLOCKING`;
- competing credentials: `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `AGENT_SIDEKICK_TOKEN` (and `CLAUDE_CODE_OAUTH_TOKEN` via the prefix rule);
- `ANTHROPIC_BASE_URL` for `claude-endpoint` (which sets its own) and `command` backends. For `claude-oauth` an inherited `ANTHROPIC_BASE_URL` is kept (proxies) and logged as a warning.

Why: Claude Code starts the MCP server and passes its session variables on (session id, entrypoint, a messaging socket and token, ...). A child `claude` that sees them identifies as part of the parent session, receives the parent's messaging socket and token, and in API-key mode refuses to start ("Not logged in"). The prefix rule also removes provider switches such as `CLAUDE_CODE_USE_BEDROCK`.

Then the backend's `env` table is applied (it may set `CLAUDE_CODE_*` variables; only inherited ones are removed), and finally the type-specific variables:

| Type | Set by agent-sidekick |
|------|-----------------------|
| `claude-oauth` | `CLAUDE_CONFIG_DIR`, `CLAUDE_CODE_OAUTH_TOKEN` |
| `claude-endpoint` | `CLAUDE_CONFIG_DIR`, `ANTHROPIC_BASE_URL`, `ANTHROPIC_API_KEY` (`auth = "x-api-key"`) or `ANTHROPIC_AUTH_TOKEN` (`auth = "bearer"`). `CLAUDE_CODE_OAUTH_TOKEN` is never set. |
| `command` | nothing |

Each run's environment is built from the server's environment, so one backend's key never reaches another backend.

`pass_env` (top level for all backends, or per backend) re-allows inherited variables that the rules above would remove, e.g. `pass_env = ["CLAUDE_CODE_MAX_OUTPUT_TOKENS"]`. Variables that are not removed anyway (such as `OLLAMA_API_BASE`) need no `pass_env`. Limits:

- `CLAUDE_CODE_OAUTH_TOKEN` and `AGENT_SIDEKICK_TOKEN` can never be passed (config error).
- `ANTHROPIC_API_KEY` and `ANTHROPIC_AUTH_TOKEN` can only be passed to `command` backends (config error in the top-level `pass_env` and for `claude-*` backends).
- `CLAUDE_CONFIG_DIR` is not passed to `claude-*` backends, and `ANTHROPIC_BASE_URL` not to `claude-endpoint` backends (they set their own).

### Command backends

- `command` is an argument array; the first element is the program. **No shell** is involved: pipes, redirections, `$VAR` or `%VAR%` in the array are passed literally.
- Placeholders `{prompt}`, `{cwd}` and `{max_turns}` are replaced inside each element in a single pass. The replacement is never scanned again, so a prompt that contains `{cwd}` stays literal, and `{prompt}` always ends up in **one** argument, whatever it contains. Unknown `{...}` are left as is.
- `command` must contain `{prompt}`, unless `prompt_via_stdin = true`: then the prompt is written to the program's stdin and `{prompt}` must not appear.
- A bare program name is looked up in the absolute `PATH` entries (on Windows with `PATHEXT`); empty or relative `PATH` entries are ignored, and a relative path with a directory separator is rejected. An absolute path must exist.
- On Windows, `.cmd`/`.bat` programs require `prompt_via_stdin = true` (otherwise the run fails with an error): `cmd.exe` cannot pass arbitrary prompt text safely as an argument. Point `command[0]` at an `.exe` otherwise (pip installs Aider as `aider.exe`).
- The prompt is always an argument unless `prompt_via_stdin` is set; very long prompts can exceed the OS command-line limit (about 32 000 characters on Windows), so prefer `prompt_via_stdin` for tools that support it.
- The `model` and `allowed_tools` parameters of `sidekick_run` are ignored for command backends (the log notes it); `model` in the backend's config is a config error. Put the model into the command instead.
- Command backends are configured only in `config.toml`. `sidekick_run` can select one by name but can never supply or change a command.
- The program runs with the permissions of the MCP server; tools like Aider with `--yes-always` edit files without asking.

### claude-endpoint backends

- `base_url` is required (`http://` or `https://`), `model` is passed as `--model`.
- Each endpoint backend has its own `CLAUDE_CONFIG_DIR` (default `~/.claude-sidekick-<name>`). It must differ from the config dir of every `claude-oauth` backend (config error otherwise): a claude.ai login saved there would stay active and could be used instead of the endpoint.
- `CLAUDE_CODE_OAUTH_TOKEN` is never set for an endpoint backend, also not via `pass_env` or `env`, and `api_key_env` may not name it.

### CLAUDE.md routing example

Put rules like these into the `CLAUDE.md` of your projects (or `~/.claude/CLAUDE.md`), adapted to your backend names. The full version is [`examples/CLAUDE.md`](examples/CLAUDE.md).

```markdown
## Delegation with agent-sidekick

- Before delegating, call `sidekick_backends`; only use backends that are reachable.
- `local` (free): boilerplate, test scaffolding, docs, mechanical refactors.
- `sidekick` (second Claude account, quota): complex multi-file changes and debugging.
- `aider` (free, edits without asking): small edits to files you name; review the diff.
- Always pass `cwd` (absolute project path) and a self-contained prompt.
- Use `fallback: "local"` only for tasks that are safe to retry; the failed primary may
  have changed files. Check `git status` after a fallback.
- Never put secrets into prompts; prompts are written to the run log.
- Verify delegated changes (diff, tests) before building on them.
```

## Tool reference

### `sidekick_run`

| Parameter       | Default              | Notes |
|-----------------|----------------------|-------|
| `prompt`        | (required)           | Task for the delegated agent. |
| `cwd`           | server's cwd         | Working directory. Always pass it; the server's cwd is wherever Claude Code started it. |
| `backend`       | the default backend  | Name of a configured backend. |
| `fallback`      | none                 | Backend used if the primary fails, see [Fallback](#fallback). |
| `max_turns`     | 30                   | Maximum 200. Passed as `--max-turns` (claude-*) or `{max_turns}` (command). |
| `timeout_s`     | 1800                 | Maximum 7200. The whole process tree is killed on timeout. |
| `model`         | backend's `model` / CLI default | Passed as `--model` (claude-* only). |
| `allowed_tools` | none                 | List passed as `--allowedTools` (claude-* only). |

The result starts with the backend that actually ran, `backend: <name> (<type>)`. After a fallback that line also names the failed primary backend and the reason, and, if the primary ran, its exit code and log path.
Then follow the exit code, the path of the full run log and the last 4000 characters of output.

- **Cancellation:** cancelling the tool call in Claude Code (Esc) kills the delegated run's whole process tree; a cancelled run that is still queued never starts.
- **Long runs:** the server sends a progress notification every 30 s while a run is active. This keeps the call alive with respect to Claude Code's MCP tool idle timeout (`CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT`, 30 minutes for stdio servers by default). If you have lowered the overall `MCP_TOOL_TIMEOUT` (default about 28 hours), keep it above `timeout_s`.

### `sidekick_backends`

No parameters. Returns a JSON array with one entry per backend: `name`, `type`, `description`, `cost`, `default`, `reachable`, `detail` (why it is or is not reachable), `max_concurrent` and `running`.
It never contains secrets or environment values.

### `sidekick_status`

Shows running and queued runs, per backend and in total, with the global and per-backend limits.

## CLI reference

| Command | Purpose |
|---------|---------|
| `agent-sidekick setup [--token-stdin] [--no-register] [--claude PATH]` | Ask for the token of every `claude-oauth` backend and register the MCP server. Endpoint keys are not asked here. |
| `agent-sidekick setup --backend <name> [--token-stdin]` | Set up one backend only: `claude-oauth` token, `claude-endpoint` API key (or a note that `api_key_env` is used), nothing to store for `command`. The MCP registration is not changed. |
| `agent-sidekick backends list [--check]` | Table of name, type, cost, default and description; `--check` adds reachability. |
| `agent-sidekick backends test <name>` | Reachability with details (claude version, resolved program, HTTP status). Exit code 0 if reachable, 1 otherwise. |
| `agent-sidekick doctor` | Checks the installation and every backend, and prints next steps. |
| `agent-sidekick uninstall [--purge]` | Removes the stored secrets of all configured backends (and the v0.1 token) and the MCP registration. |

## Configuration

Optional `config.toml` in the config directory:

```toml
max_concurrent = 2        # parallel runs over all backends; more are queued
default_max_turns = 30
default_timeout_s = 1800
default_model = "sonnet"  # optional; only for the implicit backend (no [backends] section)
claude_path = "/abs/path/to/claude"  # optional, used by claude-* backends
pass_env = []             # inherited variables to keep for every backend, see above
```

`[backends.<name>]` keys (names: 1-32 characters `a-z`, `0-9`, `_`, `-`, starting with a letter or digit; keys not valid for the type are a config error):

| Key | Types | Default | Meaning |
|-----|-------|---------|---------|
| `type` | all | (required) | `claude-oauth`, `claude-endpoint` or `command`. |
| `description` | all | (required) | What the backend is good for; read by the calling session. |
| `default` | all | `false` | Exactly one backend must have `default = true`. |
| `cost` | all | `quota` (claude-oauth), `paid` (claude-endpoint), `free` (command) | `free`, `quota` or `paid`; informs the caller. |
| `max_concurrent` | all | no own limit | Per-backend limit (at least 1), in addition to the global one. |
| `env` | all | none | Extra non-secret variables, e.g. `{ CLAUDE_CODE_ATTRIBUTION_HEADER = "0" }`. Not allowed: `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_BASE_URL`, `CLAUDE_CODE_OAUTH_TOKEN`, `CLAUDE_CONFIG_DIR`, `AGENT_SIDEKICK_*`. |
| `pass_env` | all | none | Inherited variables to keep for this backend, see [session isolation](#child-environment-and-session-isolation). |
| `model` | claude-oauth, claude-endpoint | CLI default | Default `--model`; the `model` parameter of `sidekick_run` overrides it. |
| `config_dir` | claude-oauth, claude-endpoint | `sidekick`: `AGENT_SIDEKICK_CLAUDE_CONFIG_DIR` or `~/.claude-sidekick`; others: `~/.claude-sidekick-<name>` | `CLAUDE_CONFIG_DIR` of the agent; absolute or starting with `~`. |
| `base_url` | claude-endpoint | (required) | `http://` or `https://` URL of the Anthropic-compatible API. |
| `auth` | claude-endpoint | `x-api-key` | `x-api-key` (`ANTHROPIC_API_KEY`) or `bearer` (`ANTHROPIC_AUTH_TOKEN`). |
| `api_key_env` | claude-endpoint | none (stored key) | Name of an environment variable with the key. |
| `command` | command | (required) | Program and arguments with `{prompt}`, `{cwd}`, `{max_turns}`. |
| `prompt_via_stdin` | command | `false` | Send the prompt on stdin instead of `{prompt}`. |

Config directory:

- Linux: `~/.config/agent-sidekick`
- Windows: `%APPDATA%\agent-sidekick`
- macOS: `~/Library/Application Support/agent-sidekick`

Environment variables:

| Variable | Purpose |
|----------|---------|
| `AGENT_SIDEKICK_CLAUDE` | Absolute path to the `claude` executable (checked before `claude_path` and PATH). |
| `AGENT_SIDEKICK_CLAUDE_CONFIG_DIR` | `CLAUDE_CONFIG_DIR` for the `sidekick` backend (default `~/.claude-sidekick`). |
| `AGENT_SIDEKICK_CONFIG_DIR` | Overrides the config directory. |
| `AGENT_SIDEKICK_TOKEN` | Token of the `sidekick` backend for CI/testing; takes precedence over the keyring and token file. |
| `AGENT_SIDEKICK_DISABLE_KEYRING` | Set to `1` to never use the OS keyring (file fallback only). |
| `AGENT_SIDEKICK_LOG` | `tracing` filter, e.g. `debug` or `agent_sidekick=trace`. |

## How it works

For each run on a `claude-*` backend the server starts the official CLI:

```
<claude> -p --max-turns N [--model M] [--allowedTools T1 T2 ...] -- <prompt>
```

A `command` backend starts `command[0]` with the remaining elements as arguments, placeholders replaced (see [Command backends](#command-backends)).

The child environment is described in [Child environment and session isolation](#child-environment-and-session-isolation).

For `claude-*` backends the prompt is passed via stdin instead of the command line when it is longer than 8000 characters, or on Windows when `claude` is a `.cmd` shim (avoids command-line length limits and `cmd.exe` quoting issues).

Timeout, cancellation and server shutdown kill the whole process tree (Job Object on Windows, process group on Unix).
On Linux the server also makes itself a child subreaper, so descendants that detach from the process group (e.g. background commands started by the agent) are re-parented to it and swept when a run is killed.
On SIGTERM (sent by Claude Code when it stops the server) running process trees are killed before the server exits.

## Security

- Secrets are stored in the OS keyring (service `agent-sidekick`; users per backend, see [Secrets per backend](#secrets-per-backend)). If no keyring is available they fall back to files in the config directory with mode 0600 (Unix) or a user-only ACL (Windows).
- The server never writes secrets into logs, error messages or tool output itself, and it redacts the exact secrets of the run and anything matching `sk-ant-…` from the agent's output. Secrets shorter than 8 characters (placeholder keys like `ollama`) are not redacted, so they cannot mangle the output.
  The delegated agent can, however, read its own credential from its environment (`CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN`); an agent instructed to print it in altered form (encoded, reversed, split) could expose it in the log and the tool result. Treat the delegated agent as trusted with that credential.
- `sidekick_backends`, `backends list/test` and `doctor` never print secrets or environment values.
- Network: agent-sidekick itself only makes the reachability probe described in [Reachability](#reachability) (configured `base_url` only, no credentials, 2 s timeout). All model traffic is made by `claude` or the configured command.
- Prompts are written to the run log in `<config-dir>/logs`; do not put secrets into prompts.
- To rotate or revoke a Claude token: revoke it in the Claude account settings, then run `claude setup-token` again (Quickstart step 2) and `agent-sidekick setup` (or `setup --backend <name>`).
- Never commit tokens or keys to Git.
- The delegated agent runs with the permissions you grant via `allowed_tools`. In `-p` mode the workspace trust dialog is skipped, so only point `cwd` at directories you trust. Command backends run with the server's permissions and their own permission model.

## VS Code

- The extension starts MCP servers with a reduced PATH. `agent-sidekick setup` therefore stores the absolute path of `claude` as `AGENT_SIDEKICK_CLAUDE` in the MCP registration. For command backends, use an absolute `command[0]` if the program is not on that PATH.
- Remote / WSL / SSH: MCP servers run on the remote side. Install agent-sidekick and run `agent-sidekick setup` there. The OS keyring is often unavailable in WSL/SSH sessions; the file fallback is used automatically.

## Terms of use

agent-sidekick only drives the official `claude` CLI (or the agent CLIs you configure) with credentials you choose. Apart from the reachability probe it makes no network requests of its own, and it does no account switching, load balancing or rate-limit circumvention; that is why a fallback from one Claude account to another is rejected.
You are responsible for complying with Anthropic's terms for each account you use, and with the terms of any other provider you configure.

## Troubleshooting

Run `agent-sidekick doctor` first; it checks the installation and every backend and prints next steps.

- **`claude` not found**: install Claude Code, or set `AGENT_SIDEKICK_CLAUDE` / `claude_path` to its absolute path, then re-run `agent-sidekick setup`.
- **Token missing**: repeat Quickstart steps 2 and 3 (`agent-sidekick setup --backend <name>` for a single backend).
- **Endpoint not reachable**: start the local server (`ollama serve`, LM Studio's server), then `agent-sidekick backends test <name>`.
- **Endpoint run fails with "Not logged in"**: check that the key is stored (`backends test`) and that `config_dir` is the backend's own directory.
- **Config error in `sidekick_run`**: fix `config.toml`; `agent-sidekick backends list` shows the problem.
- **Server not listed in `/mcp`**: reload the VS Code window or restart Claude Code.
- **Logs**: `<config-dir>/logs` (more detail with `AGENT_SIDEKICK_LOG=debug`).

## Uninstall

```sh
agent-sidekick uninstall          # remove stored secrets and the MCP registration
agent-sidekick uninstall --purge  # also delete the config directory (config, token files, logs)
```

The agents' Claude config directories (`~/.claude-sidekick`, `~/.claude-sidekick-<name>`) are left in place. Then delete the binary.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your option.
