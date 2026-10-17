#!/usr/bin/env bash
# Downloads a pinned, checksummed static cargo-deny binary (Linux, x86_64 or
# aarch64 as this machine) into DIR and prints its path. Used by the `deny`
# job of .github/workflows/ci.yml, so that the check does not run a floating
# version of the tool. To update: change the version and both checksums
# (the sha256 of the release tarballs).
#
# Usage: scripts/dist/get-cargo-deny.sh DIR
set -euo pipefail

# shellcheck source=scripts/dist/lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

version=0.20.2
dir="${1:?usage: $0 DIR}"
dist_arch
case "$DIST_DEB_ARCH" in
amd64)
    arch=x86_64
    sha256=9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f
    ;;
arm64)
    arch=aarch64
    sha256=995c82be0defc7a025cae49a2aa2644ce8245c9a3318fc4103907c6a285e8c7d
    ;;
esac
name="cargo-deny-$version-$arch-unknown-linux-musl"
url="https://github.com/EmbarkStudios/cargo-deny/releases/download/$version/$name.tar.gz"
mkdir -p "$dir"
bin="$dir/cargo-deny"
archive="$dir/$name.tar.gz"
if [ ! -f "$archive" ] || ! echo "$sha256  $archive" | sha256sum -c --quiet - >/dev/null 2>&1; then
    curl -fsSL --retry 3 -o "$archive.part" "$url"
    echo "$sha256  $archive.part" | sha256sum -c --quiet - >&2
    mv "$archive.part" "$archive"
fi
if [ ! -x "$bin" ]; then
    tar -C "$dir" -xzf "$archive" "$name/cargo-deny"
    mv "$dir/$name/cargo-deny" "$bin"
    rm -rf "${dir:?}/$name"
    chmod 0755 "$bin"
fi
echo "$bin"
