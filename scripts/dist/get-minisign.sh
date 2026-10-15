#!/usr/bin/env bash
# Downloads a pinned, checksummed static minisign binary (Linux, x86_64 or
# aarch64 as this machine; both are in the same release tarball) into
# DIR and prints its path. Used by the release workflow, the distribution
# tests and gen-signing-key.sh, so none of them depends on a distribution
# package of minisign. The checked release tarball stays in DIR (the
# distribution test serves it to install.sh).
#
# scripts/install.sh pins the same version and checksum (it downloads this
# tarball when minisign is not installed); a unit test in crates/gpueue keeps
# the two in sync.
#
# Usage: scripts/dist/get-minisign.sh DIR
set -euo pipefail

# shellcheck source=scripts/dist/lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

version=0.12
sha256=9a599b48ba6eb7b1e80f12f36b94ceca7c00b7a5173c95c3efc88d9822957e73
url="https://github.com/jedisct1/minisign/releases/download/$version/minisign-$version-linux.tar.gz"

dir="${1:?usage: $0 DIR}"
dist_arch
case "$DIST_DEB_ARCH" in
amd64) arch=x86_64 ;;
arm64) arch=aarch64 ;;
esac
mkdir -p "$dir"
bin="$dir/minisign"
archive="$dir/minisign-$version-linux.tar.gz"
if [ ! -f "$archive" ] || ! echo "$sha256  $archive" | sha256sum -c --quiet - >/dev/null 2>&1; then
    curl -fsSL --retry 3 -o "$archive.part" "$url"
    echo "$sha256  $archive.part" | sha256sum -c --quiet - >&2
    mv "$archive.part" "$archive"
fi
if [ ! -x "$bin" ]; then
    tar -C "$dir" -xzf "$archive" "minisign-linux/$arch/minisign" 2>/dev/null
    mv "$dir/minisign-linux/$arch/minisign" "$bin"
    rm -rf "$dir/minisign-linux"
    chmod 0755 "$bin"
fi
echo "$bin"
