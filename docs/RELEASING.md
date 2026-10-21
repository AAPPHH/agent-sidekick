# Releasing

## Overview

A release consists of static binaries for Linux (x86_64, aarch64) and Windows (x86_64), the install scripts `install.sh` and `install.ps1` with the repository filled in, and a `SHA256SUMS` file. Pushing a tag `vX.Y.Z` builds everything and creates the GitHub release; nothing is uploaded by hand.

## 1. Prepare

1. Bump the version in three places:
   - `Cargo.toml`: `version = "X.Y.Z"`
   - `plugin/.claude-plugin/plugin.json`: `"version": "X.Y.Z"`
   - `CHANGELOG.md`: move the entries from `## [Unreleased]` into a new section `## [X.Y.Z] - YYYY-MM-DD` below it and leave `## [Unreleased]` empty.
2. Update the lock file (CI builds with `--locked`):

   ```sh
   cargo update --offline -p agent-sidekick
   ```

3. Run the checks CI runs:

   ```sh
   cargo fmt --all -- --check
   cargo clippy --locked --all-targets -- -D warnings
   AGENT_SIDEKICK_DISABLE_KEYRING=1 cargo test --locked --all-targets
   ```

   PowerShell: `$env:AGENT_SIDEKICK_DISABLE_KEYRING = "1"; cargo test --locked --all-targets`

4. Commit, push and wait until CI on `main` is green:

   ```sh
   git add Cargo.toml Cargo.lock plugin/.claude-plugin/plugin.json CHANGELOG.md
   git commit -m "Release vX.Y.Z"
   git push origin main
   ```

## 2. Tag and publish

```sh
git tag -a vX.Y.Z -m "vX.Y.Z"
git push origin vX.Y.Z
```

The tag starts `.github/workflows/release.yml`:

| Job | What it does |
|---|---|
| `check` (tag matches Cargo.toml) | Fails unless the tag is `v` + the `Cargo.toml` version; checks that `scripts/render-installers.sh` can read the repository. |
| `build` | Builds `x86_64-unknown-linux-musl` (ubuntu-latest), `aarch64-unknown-linux-musl` (ubuntu-24.04-arm, free only for public repositories) and `x86_64-pc-windows-msvc` (windows-latest); runs each binary with `--version`; packs it with `scripts/package.sh` / `scripts/package.ps1`. |
| `build-macos` | Builds `aarch64-apple-darwin`; `continue-on-error`, not published. |
| `release` (publish release) | Renders `install.sh` / `install.ps1`, computes `SHA256SUMS` over all archives and both scripts, creates the GitHub release (`fail_on_unmatched_files`). |

Release assets:

- `agent-sidekick-x86_64-unknown-linux-musl.tar.gz`
- `agent-sidekick-aarch64-unknown-linux-musl.tar.gz`
- `agent-sidekick-x86_64-pc-windows-msvc.zip`
- `install.sh`, `install.ps1`
- `SHA256SUMS`

## 3. Verify the release

1. Check that the release page lists all six assets.
2. Download all assets into one directory and verify the checksums:

   ```sh
   sha256sum -c SHA256SUMS
   ```

3. Install with the one-liners on a fresh machine (no Rust, Git or Node needed):

   ```powershell
   irm https://github.com/AAPPHH/agent-sidekick/releases/latest/download/install.ps1 | iex
   ```

   ```sh
   curl -fsSL https://github.com/AAPPHH/agent-sidekick/releases/latest/download/install.sh | sh
   ```

   To install a specific version: PowerShell `$env:VERSION = 'vX.Y.Z'` before the one-liner; sh `curl -fsSL .../install.sh | VERSION=vX.Y.Z sh`.

4. Check the installed binary:

   ```sh
   agent-sidekick --version
   agent-sidekick update
   ```

   `update` asks the GitHub releases API for the latest release; right after publishing it must report "up to date". `doctor` shows the same hint at most once a day.

## 4. If something goes wrong

1. Fix the problem on `main` and push.
2. If the release was not published yet (or nobody can have downloaded it), delete the tag and any draft or broken release, then tag again:

   ```sh
   git tag -d vX.Y.Z
   git push origin :refs/tags/vX.Y.Z
   ```

3. Never move a tag that was already published; release a new patch version instead.

## 5. Changing the repository owner

1. Change `repository` in `Cargo.toml` (a URL, e.g. `https://github.com/<owner>/agent-sidekick`). The install scripts, the release workflow and `agent-sidekick update` derive the owner from it.
2. Replace `AAPPHH/agent-sidekick` by hand in the static files: `README.md`, `plugin/README.md`, `plugin/.claude-plugin/plugin.json`, `.claude-plugin/marketplace.json`. `git grep AAPPHH` finds them all.
3. Release as usual; the release job renders the install scripts with the new repository.
