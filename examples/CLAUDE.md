# Delegation with agent-sidekick

These rules are for the main Claude Code session. Adapt the backend names to your
`config.toml` (this example covers all four backends of `examples/config.toml`: `claude`,
`local`, `aider` and the optional `kiconnect`; drop the rules for backends you do not
configure and add one for every other backend you do).

- Before the first delegation in a session, call `sidekick_backends` and read each backend's
  `description`, `cost`, `reachable`, `models` and `default_model`. Do not delegate to a
  backend that is not reachable.
- Pick the backend and then the model: pass `model` only with a name from that backend's
  `models` (anything else is rejected and nothing runs), or omit it to use its
  `default_model`. A backend with `models: null` either accepts any Claude model (claude-*
  backends, e.g. `sonnet`, `opus`) or takes no model (command backends).
- Choose the backend by task:
  - `local` (free, local model): boilerplate, test scaffolding, docstrings/comments,
    mechanical refactors (renames, moving code, applying a pattern you have already
    shown), formatting and lint fixes.
  - `claude` (second Claude account, uses its quota): complex multi-file changes,
    debugging, anything that needs real reasoning about the codebase.
  - `aider` (free, edits files directly without asking): small, well-specified edits to
    files you name in the prompt. Review the diff afterwards; its changes are not committed.
  - `kiconnect` (free university quota, hosted model via Aider, edits files directly
    without asking and without git): boilerplate, tests and small, well-specified changes
    when `local` is unavailable or too slow. Only reachable once its key is stored. The
    prompt goes to a hosted service, so do not use it for confidential code. Review the diff
    afterwards. Models: `openai-gpt-oss-120b` (default, general tasks) and
    `qwen-qwen3-8-27b` (code-heavy tasks).
  - On `claude`, use the default model for routine work and `model: "opus"` only for the
    hardest tasks.
  - Keep work you need to judge carefully (security, data migrations, public APIs) in this
    session.
- Always pass `cwd` (absolute path of the project). The server's own working directory is
  arbitrary.
- Write self-contained prompts: the delegated agent does not see this conversation. Name the
  files, the goal and how to verify it (e.g. which test command must pass).
- Use `fallback: "local"` only for tasks that are safe to retry: a failed primary run may
  already have changed files in `cwd`. Never set a fallback for tasks with side effects
  outside the working tree (deployments, migrations, pushes). Check `git status` after a
  fallback run.
- Never put secrets (tokens, passwords, API keys, customer data) into prompts. Prompts and
  output are written to the run log.
- Prefer `local`/`aider`/`kiconnect` when the `claude` account is busy (see `sidekick_status`) or for
  bulk work; do not fan out many parallel `claude` runs.
- After a delegated run, read the result (exit code, output tail) and verify the change
  yourself (diff, tests) before building on it. The first line of the result names the
  backend and model that actually ran; after a fallback, a note says when the fallback did
  not offer the requested model and used its default instead.
