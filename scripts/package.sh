#!/usr/bin/env bash
# Package a release binary exactly as the release workflow publishes it.
#
#   scripts/package.sh <target-triple> <path to agent-sidekick binary> <output dir>
#
# Writes <output dir>/agent-sidekick-<target>.tar.gz (binary, README.md, LICENSE-MIT and
# LICENSE-APACHE at the archive root, no top-level directory) and adds/replaces its line in
# <output dir>/SHA256SUMS ("<sha256>  <file>", what install.sh / install.ps1 verify against).
# Used by .github/workflows/release.yml and by the install dry-run in ci.yml.
set -euo pipefail

if [ "$#" -ne 3 ]; then
    echo "usage: $0 <target-triple> <binary> <output dir>" >&2
    exit 2
fi
target=$1
binary=$2
out=$3
root=$(cd "$(dirname "$0")/.." && pwd)
bin=agent-sidekick
name="$bin-$target.tar.gz"

[ -f "$binary" ] || { echo "error: binary not found: $binary" >&2; exit 1; }
mkdir -p "$out"
out=$(cd "$out" && pwd)

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
cp "$binary" "$stage/$bin"
chmod 755 "$stage/$bin"
cp "$root/README.md" "$root/LICENSE-MIT" "$root/LICENSE-APACHE" "$stage/"
tar -C "$stage" -czf "$out/$name" "$bin" README.md LICENSE-MIT LICENSE-APACHE

if command -v sha256sum >/dev/null 2>&1; then
    hash=$(sha256sum "$out/$name" | cut -d' ' -f1)
else
    hash=$(shasum -a 256 "$out/$name" | cut -d' ' -f1)
fi
sums="$out/SHA256SUMS"
touch "$sums"
grep -v "  $name\$" "$sums" >"$sums.tmp" || true
printf '%s  %s\n' "$hash" "$name" >>"$sums.tmp"
mv "$sums.tmp" "$sums"
echo "$out/$name"
