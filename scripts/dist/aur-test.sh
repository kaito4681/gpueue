#!/usr/bin/env bash
# Tests the AUR package (packaging/aur/gpueue-bin) in an Arch Linux container with systemd
# as PID 1: builds a release tarball of the working tree (x86_64, like
# .github/workflows/release.yml), lays it out like a GitHub release (signed with a
# throwaway minisign key, which the source archive carries as packaging/minisign.pub) next to
# a source archive of the tree, runs scripts/dist/update-aur.sh against that (file://), then
# builds the package with makepkg as an unprivileged user, installs it with pacman, starts
# the services, runs a job, upgrades to pkgrel 2 while a job runs, and removes the package.
# namcap is run on the PKGBUILD and the package when it can be installed.
#
# Usage: scripts/dist/aur-test.sh [TARBALL]
#   TARBALL: an existing gpueue-v<version>-x86_64-unknown-linux-gnu.tar.gz of the current
#            workspace version (default: build one with scripts/test-in-docker.sh)
# GPUEUE_ARCH_IMAGE: the Arch Linux image (default: the one update-aur.sh pins).
# GPUEUE_AUR_NAMCAP=0 skips namcap (it needs the package mirrors).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
image="${GPUEUE_ARCH_IMAGE:-$(sed -n 's/^image="\${GPUEUE_ARCH_IMAGE:-\(.*\)}"$/\1/p' "$root/scripts/dist/update-aur.sh")}"
[ -n "$image" ] || { echo "cannot read the image from update-aur.sh" >&2; exit 1; }
# shellcheck source=scripts/dist/lib.sh
. "$root/scripts/dist/lib.sh"
version="$(workspace_version "$root")"
target=x86_64-unknown-linux-gnu
name="gpueue-v$version-$target"

stage="$(mktemp -d)"
container="gpueue-aur-test-$$"
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
    "$root/scripts/test-in-docker.sh" build --release --locked -p gpueue -p gpueue-helper
    mkdir -p "$stage/build/$name"
    install -m 0755 "$root/target/docker/release/gpueue" "$root/target/docker/release/gpueue-helper" \
        "$stage/build/$name/"
    cp -r "$root/README.md" "$root/LICENSE" "$root/docs" "$stage/build/$name/"
    tar -C "$stage/build" -czf "$release/$name.tar.gz" "$name"
fi
(cd "$release" && sha256sum "$name.tar.gz" >"$name.tar.gz.sha256")
# Never used on x86_64; update-aur.sh only checks its signature.
aarch64="gpueue-v$version-aarch64-unknown-linux-gnu.tar.gz"
cp "$release/$name.tar.gz" "$release/$aarch64"
(cd "$release" && sha256sum "$aarch64" >"$aarch64.sha256")

# A throwaway release key: the tarballs are signed as release.yml signs them.
minisign="$("$root/scripts/dist/get-minisign.sh" "$root/target/dist-tools")"
mkdir -p "$stage/keys"
"$minisign" -G -W -p "$stage/keys/release.pub" -s "$stage/keys/release.key" >/dev/null
"$minisign" -G -W -p "$stage/keys/other.pub" -s "$stage/keys/other.key" >/dev/null
# sign FILE KEY COMMENT
sign() { "$minisign" -S -s "$stage/keys/$2.key" -m "$release/$1" -t "$3" >/dev/null; }
sign "$name.tar.gz" release "gpueue v$version $name.tar.gz"
sign "$aarch64" release "gpueue v$version $aarch64"

# The source archive: tracked and new (not ignored) files of the working tree, as GitHub
# would archive the tag, with the throwaway key as packaging/minisign.pub.
mkdir -p "$stage/src/gpueue-$version"
(cd "$root" && git ls-files -z --cached --others --exclude-standard |
    tar --null --ignore-failed-read -T - -cf - 2>/dev/null) | tar -C "$stage/src/gpueue-$version" -xf -
cp "$stage/keys/release.pub" "$stage/src/gpueue-$version/packaging/minisign.pub"
tar -C "$stage/src" -czf "$stage/repo/archive/refs/tags/v$version.tar.gz" "gpueue-$version"
rm -rf "$stage/src"

cp -r "$root/packaging/aur/gpueue-bin" "$stage/pkg"
update_aur() {
    GPUEUE_RELEASE_BASE_URL="file://$stage/repo" GPUEUE_AUR_DIR="$stage/pkg" MINISIGN="$minisign" \
        "$root/scripts/dist/update-aur.sh" "$version"
}
# expect_refused PATTERN: update-aur.sh fails, says PATTERN and leaves the PKGBUILD alone.
expect_refused() {
    local out
    if out="$(update_aur 2>&1)"; then
        echo "$out"
        echo "FAIL: update-aur.sh accepted a bad release" >&2
        exit 1
    fi
    grep -qF -- "$1" <<<"$out" || { echo "$out"; echo "FAIL: expected \"$1\"" >&2; exit 1; }
    cmp -s "$root/packaging/aur/gpueue-bin/PKGBUILD" "$stage/pkg/PKGBUILD" ||
        { echo "FAIL: a refused release changed the PKGBUILD" >&2; exit 1; }
}
cp "$release/$aarch64.minisig" "$stage/aarch64.minisig"
# Signed with another key.
sign "$aarch64" other "gpueue v$version $aarch64"
expect_refused "bad signature on $aarch64"
# Signed with the release key, but as another release's file.
sign "$aarch64" release "gpueue v0.0.1 gpueue-v0.0.1-aarch64-unknown-linux-gnu.tar.gz"
expect_refused "the signature of $aarch64 is for another file"
# A tarball that does not match its signature.
cp "$stage/aarch64.minisig" "$release/$aarch64.minisig"
echo corrupted >>"$release/$aarch64"
expect_refused "bad signature on $aarch64"
cp "$release/$name.tar.gz" "$release/$aarch64"
rm -f "$stage/keys/release.key" "$stage/keys/other.key"

update_aur
grep -qx "sha256sums_x86_64=('$(sha256sum "$release/$name.tar.gz" | cut -d' ' -f1)')" "$stage/pkg/PKGBUILD" ||
    { echo "FAIL: the PKGBUILD does not have the tarball's checksum" >&2; exit 1; }
cp "$root/scripts/dist/aur-scenario.sh" "$stage/"
chmod -R a+rX "$stage"

docker run -d --name "$container" --privileged --cgroupns=private \
    --tmpfs /run --tmpfs /run/lock \
    -e GPUEUE_AUR_NAMCAP="${GPUEUE_AUR_NAMCAP:-1}" \
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
