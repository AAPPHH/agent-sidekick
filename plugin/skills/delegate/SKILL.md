---
name: delegate
description: When and how to hand work to the agent-sidekick MCP tools (sidekick_backends, sidekick_run, sidekick_status). Use when a task could be delegated to a second, headless agent or a local model, e.g. boilerplate, tests, mechanical refactors, or a separate multi-file change.
---

# Delegating with agent-sidekick

- Before the first delegation in a session, call `sidekick_backends` and read each backend's
  `description`, `cost`, `reachable`, `models` and `default_model`. Do not delegate to a
  backend that is not reachable.
- Pick the backend by its description and cost, then the model: pass `model` only with a
  name from that backend's `models`, or omit it to use its `default_model`.
- Typical split: free/local backends for boilerplate, test scaffolding, docstrings,
  mechanical refactors and lint fixes; a second Claude account (quota) for complex
  multi-file changes and debugging. Keep work that needs careful judgment (security, data
  migrations, public APIs) in this session.
- Always pass `cwd` (absolute path of the project).
- Write self-contained prompts: the delegated agent does not see this conversation. Name
  the files, the goal and how to verify it (e.g. which test command must pass).
- Use `fallback` only for tasks that are safe to retry: a failed primary run may already
  have changed files in `cwd`. Never set a fallback for tasks with side effects outside the
  working tree (deployments, migrations, pushes). Check `git status` after a fallback run.
- Never put secrets (tokens, passwords, API keys, customer data) into prompts; prompts and
  output are written to the run log.
- Do not fan out many parallel runs on a quota-limited backend; check `sidekick_status`.
- After a run, read the result (first line: backend and model that ran; exit code; output
  tail) and verify the change yourself (diff, tests) before building on it.
- If a run fails because no token or key is stored, ask the user to run
  `agent-sidekick setup --no-register` (plugin install) or
  `agent-sidekick setup --backend <name>` in a terminal.
