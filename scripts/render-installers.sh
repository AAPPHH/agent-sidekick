#!/usr/bin/env bash
# Write release copies of install.sh and install.ps1 with the GitHub repository filled in.
#
#   scripts/render-installers.sh <output dir>
#
# The repository comes from `repository` in Cargo.toml (the single place to change it); the
# placeholder OWNER/agent-sidekick in the source copies is replaced by "<owner>/<name>".
# Prints the repository slug. Used by release.yml (assets) and ci.yml (check).
set -euo pipefail

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <output dir>" >&2
    exit 2
fi
root=$(cd "$(dirname "$0")/.." && pwd)
out=$1
mkdir -p "$out"

# First `repository = "..."` in Cargo.toml (the [package] table comes first). No cargo needed.
url=$(sed -n 's/^repository *= *"\([^"]*\)".*/\1/p' "$root/Cargo.toml" | head -n 1)
slug=${url#https://github.com/}
slug=${slug%/}
slug=${slug%.git}
if [ -z "$url" ] || [ "$slug" = "$url" ] || ! printf '%s' "$slug" | grep -Eq '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$'; then
    echo "error: Cargo.toml needs repository = \"https://github.com/<owner>/<name>\" (found: '${url}')" >&2
    exit 1
fi

for f in install.sh install.ps1; do
    sed "s#OWNER/agent-sidekick#$slug#g" "$root/$f" >"$out/$f"
    if grep -q 'OWNER/agent-sidekick' "$out/$f"; then
        echo "error: placeholder left in $out/$f" >&2
        exit 1
    fi
done
chmod 755 "$out/install.sh"
echo "$slug"
