#!/usr/bin/env bash
# Distribution test: installing, updating and uninstalling puzuz the way an
# administrator does with scripts/install.sh, against a fake GitHub releases
# API (scripts/dist/fake_github.py, which like a private repository answers
# 404 without the token), in a privileged Docker container with systemd as
# PID 1. Two versions of the current tree (<version>-test.1 and -test.2) are
# built and signed with a throwaway minisign key. See scripts/dist/scenario.sh
# for the scenarios. Nothing privileged runs on the host.
#
# Usage: scripts/dist-test.sh [--keep]
# Environment:
#   PUZUZ_DIST_BASE        base image (default ubuntu:26.04; also tested:
#                          ubuntu:24.04, ubuntu:22.04, debian:12); see
#                          scripts/dist/Dockerfile
#   PUZUZ_DIST_SKIP_BUILD=1  reuse the release tarballs in target/dist-releases
#   PUZUZ_DIST_SCENARIOS   scenarios to run (default: all, "17 18")
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
base="${PUZUZ_DIST_BASE:-ubuntu:26.04}"
image="${PUZUZ_DIST_IMAGE:-puzuz-dist:$(tr -c 'a-zA-Z0-9.\n' '-' <<<"$base")}"
name="puzuz-dist-$$"
keep="${1:-}"
releases="$root/target/dist-releases"
scenarios="${PUZUZ_DIST_SCENARIOS:-17 18}"

version="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' "$root/Cargo.toml")"
v1="$version-test.1"
v2="$version-test.2"
# The release target of this machine (x86_64 or aarch64), as install.sh picks it.
target="$(uname -m)-unknown-linux-gnu"

if [ "${PUZUZ_DIST_SKIP_BUILD:-}" != 1 ] || [ ! -d "$releases/v$v2" ]; then
    rm -rf "$releases"
    "$root/scripts/dist/build-test-versions.sh" "$releases" test.1 test.2:migration
fi
minisign="$("$root/scripts/dist/get-minisign.sh" "$root/target/dist-tools")"
docker build -q -t "$image" --build-arg "BASE=$base" "$root/scripts/dist" >/dev/null

stage="$(mktemp -d)"
cleanup() {
    if [ "$keep" != "--keep" ]; then
        docker rm -f "$name" >/dev/null 2>&1 || true
    else
        echo "container kept: docker exec -it $name bash"
    fi
    rm -rf "$stage"
}
trap cleanup EXIT

# Throwaway keys: the release key, and another one for a bad signature.
mkdir -p "$stage/keys" "$stage/releases"
"$minisign" -G -W -p "$stage/keys/release.pub" -s "$stage/keys/release.key" >/dev/null
"$minisign" -G -W -p "$stage/keys/other.pub" -s "$stage/keys/other.key" >/dev/null

# release TAG SOURCE_TAG KEY: a release TAG holding SOURCE_TAG's binaries,
# repackaged under TAG's name and signed with KEY.
release() {
    local tag="$1" src="$2" key="$3" work
    local from="puzuz-$src-$target" to="puzuz-$tag-$target"
    work="$(mktemp -d)"
    tar -C "$work" -xzf "$releases/$src/$from.tar.gz"
    [ "$from" = "$to" ] || mv "$work/$from" "$work/$to"
    mkdir -p "$stage/releases/$tag"
    tar -C "$work" -czf "$stage/releases/$tag/$to.tar.gz" "$to"
    rm -rf "$work"
    (cd "$stage/releases/$tag" && sha256sum "$to.tar.gz" >"$to.tar.gz.sha256")
    "$minisign" -S -s "$stage/keys/$key.key" -m "$stage/releases/$tag/$to.tar.gz" \
        -t "puzuz $tag"
    cp "$root/scripts/install.sh" "$stage/releases/$tag/install.sh"
}
release "v$v1" "v$v1" release
release "v$v2" "v$v2" release
# A tarball that does not match its checksum.
release "v$version-test.badsha" "v$v2" release
echo "corrupted" >>"$stage/releases/v$version-test.badsha/puzuz-v$version-test.badsha-$target.tar.gz"
# A tarball signed with another key.
release "v$version-test.badsig" "v$v2" other
echo "v$v1" >"$stage/releases/latest"

cp "$root/scripts/install.sh" "$root/scripts/dist/fake_github.py" "$root/scripts/dist/scenario.sh" \
    "$minisign" "$stage/"
cp "$stage/keys/release.pub" "$stage/release.pub"
rm -f "$stage/keys/release.key" "$stage/keys/other.key"
token="test-$(od -An -N12 -tx1 /dev/urandom | tr -d ' \n')"
printf '%s\n' "$token" >"$stage/token"
printf 'V1=%s\nV2=%s\nVERSION=%s\n' "$v1" "$v2" "$version" >"$stage/versions"
chmod -R a+rX "$stage"

docker run -d --name "$name" --privileged --cgroupns=private \
    --tmpfs /run --tmpfs /run/lock \
    -v "$stage":/dist:ro \
    "$image" >/dev/null
state=
for _ in $(seq 100); do
    state="$(docker exec "$name" systemctl is-system-running 2>/dev/null || true)"
    case "$state" in running | degraded) break ;; esac
    sleep 0.2
done
echo "$base: systemd ${state:-unknown}"

status=0
for scenario in $scenarios; do
    if ! docker exec "$name" bash /dist/scenario.sh "$scenario"; then
        status=1
        echo "--- journal"
        docker exec "$name" journalctl --no-pager -n 200 -u puzuz.service -u puzuz-helper.service \
            -u fake-github.service || true
        echo "--- fake GitHub requests"
        docker exec "$name" cat /srv/requests.log || true
        break
    fi
done
exit "$status"
