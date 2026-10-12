#!/usr/bin/env bash
# Downloads a pinned, checksummed static minisign binary (x86_64 Linux) into
# DIR and prints its path. Used by the release workflow, the distribution
# tests and gen-signing-key.sh, so none of them depends on a distribution
# package of minisign.
#
# Usage: scripts/dist/get-minisign.sh DIR
set -euo pipefail

version=0.12
sha256=9a599b48ba6eb7b1e80f12f36b94ceca7c00b7a5173c95c3efc88d9822957e73
url="https://github.com/jedisct1/minisign/releases/download/$version/minisign-$version-linux.tar.gz"

dir="${1:?usage: $0 DIR}"
mkdir -p "$dir"
bin="$dir/minisign"
if [ ! -x "$bin" ]; then
    archive="$dir/minisign-$version-linux.tar.gz"
    curl -fsSL --retry 3 -o "$archive" "$url"
    echo "$sha256  $archive" | sha256sum -c --quiet - >&2
    tar -C "$dir" -xzf "$archive" minisign-linux/x86_64/minisign 2>/dev/null
    mv "$dir/minisign-linux/x86_64/minisign" "$bin"
    rm -rf "$dir/minisign-linux" "$archive"
    chmod 0755 "$bin"
fi
echo "$bin"
