#!/usr/bin/env bash
# Downloads a pinned, checksummed nfpm binary (for this machine's
# architecture: x86_64 or aarch64) into DIR and prints its path. Used by
# scripts/dist/build-package.sh, so neither the release workflow nor CI
# depends on a distribution package or a container image of nfpm.
#
# Usage: scripts/dist/get-nfpm.sh DIR
set -euo pipefail

version=2.47.0
case "$(uname -m)" in
x86_64)
    asset="nfpm_${version}_Linux_x86_64.tar.gz"
    sha256=0660ca602b2d2d2ae4781a06c692b3eeb9d437ffea05b831d76e41f4a3188783
    ;;
aarch64 | arm64)
    asset="nfpm_${version}_Linux_arm64.tar.gz"
    sha256=1c0f5f2999b9a974bfb04fdb0cc3306096de530ac5dbb25d739cc5f5219c919c
    ;;
*)
    echo "no pinned nfpm for $(uname -m)" >&2
    exit 1
    ;;
esac
url="https://github.com/goreleaser/nfpm/releases/download/v$version/$asset"

dir="${1:?usage: $0 DIR}"
mkdir -p "$dir"
bin="$dir/nfpm-$version"
if [ ! -x "$bin" ]; then
    archive="$dir/$asset"
    curl -fsSL --retry 3 -o "$archive" "$url"
    echo "$sha256  $archive" | sha256sum -c --quiet - >&2
    tar -C "$dir" -xzf "$archive" nfpm
    mv "$dir/nfpm" "$bin"
    rm -f "$archive"
    chmod 0755 "$bin"
fi
echo "$bin"
