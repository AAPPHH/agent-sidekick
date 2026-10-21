#!/bin/sh
# Installer for agent-sidekick (Linux, macOS).
#
#   curl -fsSL https://github.com/OWNER/agent-sidekick/releases/latest/download/install.sh | sh
#
# Environment:
#   REPO                         GitHub repository "owner/name" (default: OWNER/agent-sidekick;
#                                the placeholder must be replaced with the real repository)
#   VERSION                      release to install, e.g. 0.1.0 or v0.1.0 (default: latest)
#   AGENT_SIDEKICK_INSTALL_DIR   target directory (default: $HOME/.local/bin)
set -eu

REPO="${REPO:-OWNER/agent-sidekick}"
VERSION="${VERSION:-latest}"
INSTALL_DIR="${AGENT_SIDEKICK_INSTALL_DIR:-$HOME/.local/bin}"
BIN=agent-sidekick

err() {
    echo "error: $*" >&2
    exit 1
}

# --- detect target triple ---------------------------------------------------
os=$(uname -s)
arch=$(uname -m)
case "$os" in
    Linux) os_part=unknown-linux-gnu ;;
    Darwin) os_part=apple-darwin ;;
    MINGW* | MSYS* | CYGWIN*) err "on Windows use install.ps1 instead" ;;
    *) err "unsupported operating system: $os" ;;
esac
case "$arch" in
    x86_64 | amd64) arch_part=x86_64 ;;
    aarch64 | arm64) arch_part=aarch64 ;;
    *) err "unsupported architecture: $arch" ;;
esac
target="$arch_part-$os_part"
archive="$BIN-$target.tar.gz"

case "$VERSION" in
    latest) base="https://github.com/$REPO/releases/latest/download" ;;
    v*) base="https://github.com/$REPO/releases/download/$VERSION" ;;
    *) base="https://github.com/$REPO/releases/download/v$VERSION" ;;
esac

# --- helpers ------------------------------------------------------------------
download() {
    # download <url> <output file>
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 3 -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$2" "$1"
    else
        err "curl or wget is required"
    fi
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d' ' -f1
    else
        err "sha256sum or shasum is required to verify the download"
    fi
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

# --- download and verify ------------------------------------------------------
echo "Downloading $archive ($VERSION) from $REPO ..."
download "$base/$archive" "$tmp/$archive" || err "download failed: $base/$archive"
download "$base/SHA256SUMS" "$tmp/SHA256SUMS" || err "download failed: $base/SHA256SUMS"

expected=$(awk -v f="$archive" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")
[ -n "$expected" ] || err "$archive is not listed in SHA256SUMS"
actual=$(sha256 "$tmp/$archive")
[ "$expected" = "$actual" ] || err "checksum mismatch for $archive (expected $expected, got $actual)"
echo "Checksum OK."

# --- install --------------------------------------------------------------------
mkdir -p "$tmp/extract"
tar -xzf "$tmp/$archive" -C "$tmp/extract"
[ -f "$tmp/extract/$BIN" ] || err "archive does not contain $BIN"
mkdir -p "$INSTALL_DIR"
# Copy to a temp name first so a running binary is replaced atomically.
cp "$tmp/extract/$BIN" "$INSTALL_DIR/.$BIN.new"
chmod +x "$INSTALL_DIR/.$BIN.new"
mv -f "$INSTALL_DIR/.$BIN.new" "$INSTALL_DIR/$BIN"
echo "Installed $INSTALL_DIR/$BIN"

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) echo "Note: $INSTALL_DIR is not on your PATH; add it to your shell profile." ;;
esac

cat <<EOF

Next steps:
  1. Create an OAuth token for the agent account (log in with that account):
       CLAUDE_CONFIG_DIR=~/.claude-sidekick claude setup-token
  2. Store the token and register the MCP server with Claude Code:
       $BIN setup
  3. Reload VS Code (or restart Claude Code) and check /mcp.
  Problems? Run: $BIN doctor
EOF
