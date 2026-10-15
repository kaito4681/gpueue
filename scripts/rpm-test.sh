#!/usr/bin/env bash
# Package test: installs, upgrades and removes the gpueue .rpm in a plain
# (unprivileged, no systemd as PID 1) Docker container of a supported
# distribution and checks the result (scripts/dist/rpm-check.sh). This covers
# the package's metadata and scriptlets; the services themselves are tested by
# scripts/e2e-test.sh and scripts/dist-test.sh.
#
# Usage: scripts/rpm-test.sh [OLD_RPM NEW_RPM]
#   OLD_RPM, NEW_RPM: the packages to upgrade between (OLD older than NEW).
#   Default: build the binaries from the current tree in the toolchain
#   container and package them twice with scripts/dist/build-package.sh, as
#   <version>-rc.1 (0.1.0~rc.1 inside the package) and <version>.
# Environment:
#   GPUEUE_RPM_BASE   base image (default rockylinux/rockylinux:9; also
#                    tested: fedora:latest)
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
base="${GPUEUE_RPM_BASE:-rockylinux/rockylinux:9}"

if [ "$#" -eq 0 ]; then
    "$root/scripts/test-in-docker.sh" build --locked -p gpueue -p gpueue-helper
    # shellcheck source=scripts/dist/lib.sh
    . "$root/scripts/dist/lib.sh"
    version="$(workspace_version "$root")"
    out="$root/target/rpm-test"
    rm -rf "$out"
    old="$(GPUEUE_PKG_VERSION="$version-rc.1" "$root/scripts/dist/build-package.sh" "$root/target/docker/debug" "$out" rpm)"
    new="$("$root/scripts/dist/build-package.sh" "$root/target/docker/debug" "$out" rpm)"
elif [ "$#" -eq 2 ]; then
    old="$1"
    new="$2"
else
    echo "usage: $0 [OLD_RPM NEW_RPM]" >&2
    exit 2
fi
abs() { echo "$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"; }
old="$(abs "$old")"
new="$(abs "$new")"
for f in "$old" "$new"; do
    [ -f "$f" ] || { echo "no such file: $f" >&2; exit 1; }
done

echo "testing $(basename "$old") -> $(basename "$new") on $base"
docker run --rm \
    -v "$old:/pkg/old/$(basename "$old"):ro" -v "$new:/pkg/new/$(basename "$new"):ro" \
    -v "$root/scripts/dist/rpm-check.sh:/rpm-check.sh:ro" \
    "$base" sh /rpm-check.sh "/pkg/old/$(basename "$old")" "/pkg/new/$(basename "$new")"
