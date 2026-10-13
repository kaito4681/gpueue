#!/usr/bin/env bash
# Tests the AUR package (packaging/aur/puzuz-bin) in an Arch Linux container with systemd
# as PID 1: builds a release tarball of the working tree (x86_64, like
# .github/workflows/release.yml), lays it out like a GitHub release next to a source
# archive of the tree, runs scripts/dist/update-aur.sh against that (file://), then
# builds the package with makepkg as an unprivileged user, installs it with pacman, starts
# the services, runs a job, upgrades to pkgrel 2 while a job runs, and removes the package.
# namcap is run on the PKGBUILD and the package when it can be installed.
#
# Usage: scripts/dist/aur-test.sh [TARBALL]
#   TARBALL: an existing puzuz-v<version>-x86_64-unknown-linux-gnu.tar.gz of the current
#            workspace version (default: build one with scripts/test-in-docker.sh)
# PUZUZ_ARCH_IMAGE: the Arch Linux image (default: the one update-aur.sh pins).
# PUZUZ_AUR_NAMCAP=0 skips namcap (it needs the package mirrors).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
image="${PUZUZ_ARCH_IMAGE:-$(sed -n 's/^image="\${PUZUZ_ARCH_IMAGE:-\(.*\)}"$/\1/p' "$root/scripts/dist/update-aur.sh")}"
[ -n "$image" ] || { echo "cannot read the image from update-aur.sh" >&2; exit 1; }
version="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' "$root/Cargo.toml")"
target=x86_64-unknown-linux-gnu
name="puzuz-v$version-$target"

stage="$(mktemp -d)"
container="puzuz-aur-test-$$"
cleanup() {
    docker rm -f "$container" >/dev/null 2>&1 || true
    rm -rf "$stage"
}
trap cleanup EXIT

# The release tarball.
release="$stage/repo/releases/download/v$version"
mkdir -p "$release" "$stage/repo/archive/refs/tags"
if [ "$#" -gt 0 ]; then
    cp "$1" "$release/$name.tar.gz"
else
    "$root/scripts/test-in-docker.sh" build --release --locked -p puzuz -p puzuz-helper
    mkdir -p "$stage/build/$name"
    install -m 0755 "$root/target/docker/release/puzuz" "$root/target/docker/release/puzuz-helper" \
        "$stage/build/$name/"
    cp -r "$root/README.md" "$root/LICENSE" "$root/docs" "$stage/build/$name/"
    tar -C "$stage/build" -czf "$release/$name.tar.gz" "$name"
fi
(cd "$release" && sha256sum "$name.tar.gz" >"$name.tar.gz.sha256")
# Never used on x86_64; update-aur.sh only needs its checksum file.
aarch64="puzuz-v$version-aarch64-unknown-linux-gnu.tar.gz"
cp "$release/$name.tar.gz" "$release/$aarch64"
(cd "$release" && sha256sum "$aarch64" >"$aarch64.sha256")

# The source archive: tracked and new (not ignored) files of the working tree, as GitHub
# would archive the tag.
(cd "$root" && git ls-files -z --cached --others --exclude-standard |
    tar --null --ignore-failed-read -T - --transform "s,^,puzuz-$version/," -czf - 2>/dev/null) \
    >"$stage/repo/archive/refs/tags/v$version.tar.gz"

cp -r "$root/packaging/aur/puzuz-bin" "$stage/pkg"
PUZUZ_RELEASE_BASE_URL="file://$stage/repo" PUZUZ_AUR_DIR="$stage/pkg" \
    "$root/scripts/dist/update-aur.sh" "$version"
cp "$root/scripts/dist/aur-scenario.sh" "$stage/"
chmod -R a+rX "$stage"

docker run -d --name "$container" --privileged --cgroupns=private \
    --tmpfs /run --tmpfs /run/lock \
    -e PUZUZ_AUR_NAMCAP="${PUZUZ_AUR_NAMCAP:-1}" \
    -v "$stage":/stage \
    "$image" /usr/lib/systemd/systemd >/dev/null
state=
for _ in $(seq 100); do
    state="$(docker exec "$container" systemctl is-system-running 2>/dev/null || true)"
    case "$state" in running | degraded) break ;; esac
    sleep 0.2
done
echo "aur-test: systemd ${state:-unknown}"
docker exec "$container" bash /stage/aur-scenario.sh
