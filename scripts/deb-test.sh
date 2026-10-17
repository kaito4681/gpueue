#!/usr/bin/env bash
# Package test: installs, reinstalls, removes and purges the gpueue .deb in a
# plain (unprivileged, no systemd as PID 1) Docker container of a supported
# distribution and checks the result (scripts/dist/deb-check.sh). This covers
# the package's metadata and maintainer scripts; the services themselves are
# tested by scripts/e2e-test.sh and scripts/dist-test.sh.
#
# Usage: scripts/deb-test.sh [DEB]
#   DEB: the package to test (default: build one from the current tree in the
#        toolchain container, like scripts/dist/build-test-versions.sh, and
#        package it with scripts/dist/build-package.sh)
# Environment:
#   GPUEUE_DEB_BASE   base image (default debian:12; also tested: ubuntu:22.04,
#                    ubuntu:24.04, ubuntu:26.04)
#   GPUEUE_DEB_HTTPS=1  fetch packages over HTTPS, with this machine's CA
#                    certificates, where port 80 is blocked
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
base="${GPUEUE_DEB_BASE:-debian:12}"
deb="${1:-}"

if [ -z "$deb" ]; then
    "$root/scripts/test-in-docker.sh" build --locked -p gpueue -p gpueue-helper
    deb="$("$root/scripts/dist/build-package.sh" "$root/target/docker/debug" "$root/target/deb-test")"
fi
deb="$(cd "$(dirname "$deb")" && pwd)/$(basename "$deb")"
[ -f "$deb" ] || { echo "no such file: $deb" >&2; exit 1; }

args=(--rm -v "$deb:/pkg/$(basename "$deb"):ro" -v "$root/scripts/dist/deb-check.sh:/deb-check.sh:ro")
# Retry transient mirror failures (every apt-get of the containers).
setup='printf "Acquire::Retries \"5\";\nAcquire::http::Timeout \"30\";\nAcquire::https::Timeout \"30\";\n" >/etc/apt/apt.conf.d/80-gpueue-retries'
if [ "${GPUEUE_DEB_HTTPS:-}" = 1 ]; then
    ca=/etc/ssl/certs/ca-certificates.crt
    [ -f "$ca" ] || { echo "GPUEUE_DEB_HTTPS=1 needs $ca" >&2; exit 1; }
    # Mounted outside /etc/ssl, so that installing ca-certificates in the
    # container cannot collide with it.
    args+=(-v "$ca:/usr/local/share/host-ca.crt:ro")
    setup+=$'\n''sed -i "s|http://|https://|g" /etc/apt/sources.list /etc/apt/sources.list.d/* 2>/dev/null || true
echo "Acquire::https::CaInfo \"/usr/local/share/host-ca.crt\";" >/etc/apt/apt.conf.d/99host-ca'
fi

echo "testing $(basename "$deb") on $base"
docker run "${args[@]}" "$base" sh -euc "
$setup
apt-get update -q
sh /deb-check.sh /pkg/$(basename "$deb")
"
