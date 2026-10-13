#!/usr/bin/env bash
# Builds puzuz release tarballs of test versions of the current tree: for each
# SUFFIX, a copy of the tree with the workspace version set to
# <version>-<SUFFIX> is built in the toolchain container (as
# scripts/test-in-docker.sh does) and packaged like .github/workflows/release.yml
# does, into OUT/v<version>-<SUFFIX>/:
#   puzuz-v<v>-<this machine's target>.tar.gz and .tar.gz.sha256
# Signing is left to the caller. A SUFFIX written as SUFFIX:migration also
# gets one more (no-op) database migration, i.e. a newer schema version, so
# that a later downgrade to an older test version must be refused.
#
# Usage: scripts/dist/build-test-versions.sh OUT SUFFIX[:migration]...
#        (e.g. OUT test.1 test.2:migration)
# PUZUZ_DIST_PROFILE=release builds optimized binaries (default: dev, faster).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
out="${1:?usage: $0 OUT SUFFIX...}"
shift
[ "$#" -gt 0 ] || { echo "usage: $0 OUT SUFFIX..." >&2; exit 2; }
image="${PUZUZ_TEST_IMAGE:-puzuz-test:1.98.1}"
profile="${PUZUZ_DIST_PROFILE:-dev}"
# shellcheck source=scripts/dist/lib.sh
. "$root/scripts/dist/lib.sh"
# The release target of this machine (x86_64 or aarch64), as install.sh picks it.
dist_arch
target="$DIST_TARGET"
base="$(workspace_version "$root")"

# The toolchain image (built on first use) and the shared cargo cache.
"$root/scripts/test-in-docker.sh" --version >/dev/null

mkdir -p "$out" "$root/target/dist-build"
out="$(cd "$out" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

for arg in "$@"; do
    suffix="${arg%%:*}"
    version="$base-${suffix#-}"
    tag="v$version"
    src="$work/src"
    rm -rf "$src"
    mkdir -p "$src"
    # Tracked and new (not ignored) files, as they are in the working tree.
    (cd "$root" && git ls-files -z --cached --others --exclude-standard |
        tar --null --ignore-failed-read -T - -cf -) 2>/dev/null | tar -C "$src" -xf -
    sed -i "/^\[workspace.package\]/,/^\[/s/^version = \".*\"/version = \"$version\"/" "$src/Cargo.toml"
    if [ "$arg" != "$suffix" ]; then
        [ "${arg#*:}" = migration ] || { echo "unknown option in $arg" >&2; exit 2; }
        migrations="$src/crates/puzuz-core/src/storage/migrations.rs"
        sed -i 's/^\(pub(crate) const MIGRATIONS: &\[&str\] = &\[.*\)\];$/\1, "CREATE TABLE dist_test_marker (x INTEGER) STRICT;"];/' "$migrations"
        grep -q dist_test_marker "$migrations" || { echo "cannot add a migration" >&2; exit 1; }
    fi

    # Every copy is mounted at the same path, so the shared target directory
    # only rebuilds the workspace crates.
    docker run --rm \
        --user "$(id -u):$(id -g)" \
        -e CARGO_HOME=/cargo \
        -e CARGO_TARGET_DIR=/target \
        -v puzuz-cargo-home:/cargo \
        -v "$root/target/dist-build":/target \
        -v "$src":/work \
        -w /work \
        "$image" \
        cargo build --profile "$profile" -p puzuz -p puzuz-helper

    dir="$( [ "$profile" = dev ] && echo debug || echo "$profile")"
    name="puzuz-$tag-$target"
    stage="$work/$name"
    rm -rf "$stage"
    mkdir -p "$stage"
    install -m 0755 "$root/target/dist-build/$dir/puzuz" "$root/target/dist-build/$dir/puzuz-helper" "$stage/"
    cp -r "$src/README.md" "$src/LICENSE" "$src/docs" "$stage/"
    mkdir -p "$out/$tag"
    tar -C "$work" -czf "$out/$tag/$name.tar.gz" "$name"
    (cd "$out/$tag" && sha256sum "$name.tar.gz" >"$name.tar.gz.sha256")
    rm -rf "$stage"
    echo "built $out/$tag/$name.tar.gz"
done
