# agent-sidekick Claude Code plugin

This plugin registers the [agent-sidekick](https://github.com/AAPPHH/agent-sidekick) MCP
server (`agent-sidekick serve`) with Claude Code and adds a small skill, `delegate`, that
tells Claude when and how to hand work to it (`/agent-sidekick:delegate`).

It does **not** contain the binary. Install agent-sidekick first.

## 1. Install the binary

```sh
# Linux / macOS (installs to ~/.local/bin)
curl -fsSL https://github.com/AAPPHH/agent-sidekick/releases/latest/download/install.sh | sh
```

```powershell
# Windows (installs to %LOCALAPPDATA%\Programs\agent-sidekick and adds it to the user PATH)
irm https://github.com/AAPPHH/agent-sidekick/releases/latest/download/install.ps1 | iex
```

Or from source: `cargo install --path .` in a clone. See the main
[README](https://github.com/AAPPHH/agent-sidekick#quickstart) for details.

## 2. Store the tokens, without registering

Create the token(s) as described in the main README (Quickstart step 2), then store them
**without** the MCP registration, because the plugin does the registering:

```sh
agent-sidekick setup --no-register
agent-sidekick setup --backend <name>   # endpoint keys / command secrets, if configured
```

## 3. Install the plugin

In Claude Code:

```
/plugin marketplace add AAPPHH/agent-sidekick
/plugin install agent-sidekick@agent-sidekick
```

(`/plugin install agent-sidekick` also works as long as no other added marketplace has a
plugin of that name.) From a shell, the same is `claude plugin marketplace add
AAPPHH/agent-sidekick` and `claude plugin install agent-sidekick@agent-sidekick`.

Restart Claude Code (or reload the VS Code window) and check `/mcp`: the server is listed
as `plugin:agent-sidekick:agent-sidekick` and should be connected.

## Plugin or `agent-sidekick setup`: use one, not both

`agent-sidekick setup` registers the same server itself (`claude mcp add agent-sidekick -s
user ...`). If both are active, Claude Code may start the server twice and show every tool
twice (`mcp__agent-sidekick__sidekick_run` and
`mcp__plugin_agent-sidekick_agent-sidekick__sidekick_run`). Pick one:

- **Plugin** (updates through `/plugin`, includes the `delegate` skill): run `setup
  --no-register`, and if you registered earlier, remove that registration with
  `claude mcp remove agent-sidekick -s user`.
- **`agent-sidekick setup`** (stores the absolute paths of `agent-sidekick` and `claude` in
  the registration, so it works with a reduced PATH): do not install the plugin, or
  uninstall it with `/plugin uninstall agent-sidekick@agent-sidekick`.

`agent-sidekick doctor` (0.3.0) only looks at the `setup` registration (`claude mcp get
agent-sidekick`). With the plugin it reports `[FAIL] MCP registration: agent-sidekick is
not registered` and suggests `agent-sidekick setup`; ignore that line and hint (following it
would create the duplicate). Use `/mcp` to check the plugin's server.

Permission rules and hooks must use the plugin's tool names, e.g.
`mcp__plugin_agent-sidekick_agent-sidekick__sidekick_run`.

## When `agent-sidekick` is not found (VS Code, GUI apps)

The plugin starts the command `agent-sidekick serve` by default, found on the PATH that
Claude Code itself was started with. A GUI-launched VS Code or Claude Desktop keeps the
PATH from when it was started, so a fresh install (which changes the user PATH) is not seen
until the app is restarted from a new shell, or until you sign out and in again. On Linux
and macOS, `~/.local/bin` is often missing from the PATH of GUI sessions.

Symptom: `/mcp` shows `plugin:agent-sidekick:agent-sidekick` as failed. Fixes, simplest
first:

1. Quit the app completely and start it again (on Windows from the Start menu or a new
   terminal, after the install finished).
2. Set the plugin option **agent-sidekick executable** to the absolute path of the binary:
   `/plugin configure agent-sidekick@agent-sidekick` in Claude Code, or from a shell
   (example for the Windows default location, adjust the user name):

   ```sh
   echo '{"binary":"C:\\Users\\<you>\\AppData\\Local\\Programs\\agent-sidekick\\agent-sidekick.exe"}' \
     | claude plugin configure agent-sidekick@agent-sidekick --values-stdin
   ```

   On Linux/macOS the default location is `/home/<you>/.local/bin/agent-sidekick` or
   `/Users/<you>/.local/bin/agent-sidekick`. Restart Claude Code afterwards.

The server also has to find `claude` for the `claude-*` backends. `setup` stores its path
in the registration; with the plugin, the server looks at `claude_path` in agent-sidekick's
`config.toml`, then PATH, then the usual install locations. If runs fail with "claude not
found", set `claude_path = "/abs/path/to/claude"` in `config.toml`.

## Remote / WSL / SSH

MCP servers run where Claude Code runs. Install the binary and store the tokens on that
side.
