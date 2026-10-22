#!/bin/sh
# Installer for agent-sidekick (Linux x86_64 / aarch64).
#
#   curl -fsSL https://github.com/OWNER/agent-sidekick/releases/latest/download/install.sh | sh
#
# The copy attached to a GitHub release has the repository filled in by the release workflow
# (from `repository` in Cargo.toml). The copy in the source tree carries a placeholder and is
# meant for testing with AGENT_SIDEKICK_INSTALL_BASE_URL.
#
# Needs only a POSIX shell, curl or wget, tar, gzip and sha256sum (or shasum). No Rust, Git or Node.
#
# Environment:
#   VERSION                          release tag to install, e.g. v0.3.0 (default: latest)
#   AGENT_SIDEKICK_INSTALL_DIR       target directory (default: $HOME/.local/bin)
#   AGENT_SIDEKICK_INSTALL_BASE_URL  download from this URL or local directory instead of the
#                                    GitHub release (it must contain the archive and SHA256SUMS)
#   AGENT_SIDEKICK_REPO              GitHub repository "owner/name" (default: set at release time)
#   AGENT_SIDEKICK_TARGET            override the detected target triple (testing)
#   AGENT_SIDEKICK_NO_SETUP=1        do not run `agent-sidekick setup` at the end
#   AGENT_SIDEKICK_NO_MODIFY_PATH=1  never offer to edit a shell profile (only print the line)
#
# Everything is wrapped in main() so a truncated download cannot run half a script.
set -eu

BIN=agent-sidekick

say() {
    printf '%s\n' "$*"
}

err() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

# True when a terminal is available for questions. With `curl ... | sh` stdin is the script,
# so answers are read from /dev/tty; opening it fails when there is no controlling terminal.
has_tty() {
    [ -r /dev/tty ] && [ -w /dev/tty ] && (: </dev/tty >/dev/tty) 2>/dev/null
}

detect_target() {
    if [ -n "${AGENT_SIDEKICK_TARGET:-}" ]; then
        printf '%s\n' "$AGENT_SIDEKICK_TARGET"
        return
    fi
    os=$(uname -s)
    arch=$(uname -m)
    case "$os" in
        Linux) ;;
        Darwin) err "no prebuilt macOS binary is published yet; build from source with: cargo install --locked --git https://github.com/$REPO" ;;
        MINGW* | MSYS* | CYGWIN*) err "on Windows use install.ps1 in PowerShell: irm https://github.com/$REPO/releases/latest/download/install.ps1 | iex" ;;
        *) err "unsupported operating system: $os (build from source with: cargo install --locked --git https://github.com/$REPO)" ;;
    esac
    case "$arch" in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) err "unsupported CPU architecture: $arch (prebuilt binaries exist for x86_64 and aarch64; build from source with: cargo install --locked --git https://github.com/$REPO)" ;;
    esac
    # Static musl builds: no dependency on the glibc version of the machine.
    printf '%s\n' "$arch-unknown-linux-musl"
}

# fetch <name> <output file>: copy from the local base directory or download from the base URL.
fetch() {
    if [ -d "$BASE" ]; then
        [ -f "$BASE/$1" ] || err "$1 not found in $BASE"
        cp "$BASE/$1" "$2"
    elif command -v curl >/dev/null 2>&1; then
        # With an https base (the default) every hop, redirects included, must be https with TLS >= 1.2.
        # Plain http only happens through an explicit AGENT_SIDEKICK_INSTALL_BASE_URL override.
        case "$BASE" in
            https://*) curl_tls="--proto =https --tlsv1.2" ;;
            *) curl_tls="" ;;
        esac
        # shellcheck disable=SC2086 # curl_tls is a fixed list of flags, split on purpose
        curl -fsSL --retry 3 $curl_tls -o "$2" "$BASE/$1" ||
            err "download failed: $BASE/$1 (check VERSION and your network; releases: https://github.com/$REPO/releases)"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$2" "$BASE/$1" ||
            err "download failed: $BASE/$1 (check VERSION and your network; releases: https://github.com/$REPO/releases)"
    else
        err "curl or wget is required; install one of them (e.g. sudo apt install curl) and run this again"
    fi
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d' ' -f1
    else
        err "sha256sum or shasum is required to verify the download (install coreutils)"
    fi
}

# Offer to add the install directory to PATH in the shell's rc file. Never writes unasked.
offer_path() {
    case ":${PATH:-}:" in
        *":$INSTALL_DIR:"*) return ;;
    esac
    dir_expr=$INSTALL_DIR
    case "$INSTALL_DIR" in
        "$HOME"/*) dir_expr="\$HOME/${INSTALL_DIR#"$HOME"/}" ;;
    esac
    shell_name=$(basename "${SHELL:-sh}")
    case "$shell_name" in
        bash) rc="$HOME/.bashrc"; line="export PATH=\"$dir_expr:\$PATH\"" ;;
        zsh) rc="${ZDOTDIR:-$HOME}/.zshrc"; line="export PATH=\"$dir_expr:\$PATH\"" ;;
        fish) rc="${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish"; line="fish_add_path \"$dir_expr\"" ;;
        *) rc="$HOME/.profile"; line="export PATH=\"$dir_expr:\$PATH\"" ;;
    esac
    if [ -f "$rc" ] && grep -qF "$line" "$rc"; then
        say "$rc already adds $INSTALL_DIR to PATH; open a new terminal to pick it up."
        return
    fi
    say ""
    say "$INSTALL_DIR is not on your PATH. Add this line to $rc:"
    say "  $line"
    if [ "${AGENT_SIDEKICK_NO_MODIFY_PATH:-}" = 1 ] || ! has_tty; then
        return
    fi
    printf 'Append it to %s now? [y/N] ' "$rc" >/dev/tty
    answer=
    read -r answer </dev/tty || answer=
    case "$answer" in
        y | Y | yes | Yes | YES)
            mkdir -p "$(dirname "$rc")"
            printf '\n# agent-sidekick\n%s\n' "$line" >>"$rc"
            say "Updated $rc. Open a new terminal (or source $rc) to pick it up."
            ;;
        *) say "Left $rc unchanged." ;;
    esac
}

main() {
    REPO="${AGENT_SIDEKICK_REPO:-OWNER/agent-sidekick}"
    VERSION="${VERSION:-latest}"
    INSTALL_DIR="${AGENT_SIDEKICK_INSTALL_DIR:-${HOME:?HOME is not set}/.local/bin}"

    if [ -n "${AGENT_SIDEKICK_INSTALL_BASE_URL:-}" ]; then
        BASE=${AGENT_SIDEKICK_INSTALL_BASE_URL%/}
    else
        case "$REPO" in
            OWNER/*) err "this copy of install.sh has no repository filled in (only release assets have); set AGENT_SIDEKICK_REPO=owner/name or AGENT_SIDEKICK_INSTALL_BASE_URL=<url or directory>" ;;
        esac
        case "$VERSION" in
            latest) BASE="https://github.com/$REPO/releases/latest/download" ;;
            v*) BASE="https://github.com/$REPO/releases/download/$VERSION" ;;
            *) BASE="https://github.com/$REPO/releases/download/v$VERSION" ;;
        esac
    fi

    target=$(detect_target)
    archive="$BIN-$target.tar.gz"

    command -v tar >/dev/null 2>&1 || err "tar is required; install it and run this again"
    tmp=$(mktemp -d 2>/dev/null || mktemp -d -t agent-sidekick) || err "cannot create a temporary directory"
    trap 'rm -rf "$tmp"' EXIT
    trap 'exit 130' INT TERM

    say "Fetching $archive from $BASE ..."
    fetch "$archive" "$tmp/$archive"
    fetch SHA256SUMS "$tmp/SHA256SUMS"

    expected=$(awk -v f="$archive" '$2 == f || $2 == "*" f { print tolower($1); exit }' "$tmp/SHA256SUMS")
    [ -n "$expected" ] || err "$archive is not listed in SHA256SUMS; this release has no build for $target"
    actual=$(sha256_of "$tmp/$archive")
    [ "$expected" = "$actual" ] ||
        err "checksum mismatch for $archive (expected $expected, got $actual); the download is corrupt or was tampered with, nothing was installed"
    say "Checksum OK."

    mkdir -p "$tmp/extract"
    tar -xzf "$tmp/$archive" -C "$tmp/extract" || err "could not unpack $archive"
    [ -f "$tmp/extract/$BIN" ] || err "$archive does not contain $BIN"

    mkdir -p "$INSTALL_DIR" || err "cannot create $INSTALL_DIR; set AGENT_SIDEKICK_INSTALL_DIR to a writable directory"
    # Copy under a temporary name, then rename: replaces a running binary atomically (upgrade in place).
    cp "$tmp/extract/$BIN" "$INSTALL_DIR/.$BIN.new"
    chmod 755 "$INSTALL_DIR/.$BIN.new"
    mv -f "$INSTALL_DIR/.$BIN.new" "$INSTALL_DIR/$BIN"

    installed=$("$INSTALL_DIR/$BIN" --version 2>/dev/null) ||
        err "installed $INSTALL_DIR/$BIN but it does not run on this machine ($target)"
    say "Installed $installed to $INSTALL_DIR/$BIN"

    offer_path

    if [ "${AGENT_SIDEKICK_NO_SETUP:-}" != 1 ] && has_tty; then
        say ""
        say "Running '$BIN setup' (stores the token and registers the MCP server with Claude Code or Codex) ..."
        "$INSTALL_DIR/$BIN" setup </dev/tty ||
            say "Setup did not finish. Run it again later with: $INSTALL_DIR/$BIN setup"
    else
        say ""
        say "Next step: run '$INSTALL_DIR/$BIN setup' in a terminal (stores the token and registers"
        say "the MCP server with Claude Code or Codex), then reload VS Code. Problems? Run: $BIN doctor"
    fi
}

main "$@"
