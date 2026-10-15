#!/usr/bin/env bash
# Points the AUR package (packaging/aur/gpueue-bin) at release v<VERSION>: sets pkgver
# (and pkgrel=1 when the version changes), downloads the release's tarballs with their
# .minisig files and checks them like install.sh does (the release key, and the trusted
# comment "gpueue v<VERSION> <file>"), computes the PKGBUILD's checksums from the verified
# tarballs and from the tag's source archive, and regenerates .SRCINFO with
# `makepkg --printsrcinfo` (run directly when makepkg is installed, else in an Arch Linux
# container).
#
# The release key is packaging/minisign.pub of the tag (from its source archive), as
# apt-repo.sh takes it: a run for an older tag may come from a newer default branch. No
# signed release asset carries the systemd units, the sysusers.d and tmpfiles.d snippets
# and the default configuration, so the PKGBUILD takes them from the same source archive
# (GitHub generates it from the tag, and keeps it byte-stable).
#
# Usage: scripts/dist/update-aur.sh VERSION      (e.g. 0.2.0; prereleases are not packaged)
# Environment:
#   GPUEUE_RELEASE_BASE_URL  repository URL the PKGBUILD downloads from
#                           (default https://github.com/kaito4681/gpueue; file:// works,
#                           for tests)
#   GPUEUE_AUR_DIR           package directory (default packaging/aur/gpueue-bin)
#   GPUEUE_ARCH_IMAGE        container image with makepkg
#   MINISIGN                the minisign binary (default: scripts/dist/get-minisign.sh)
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
version="${1:-}"
version="${version#v}"
base="${GPUEUE_RELEASE_BASE_URL:-https://github.com/kaito4681/gpueue}"
base="${base%/}"
dir="${GPUEUE_AUR_DIR:-$root/packaging/aur/gpueue-bin}"
image="${GPUEUE_ARCH_IMAGE:-archlinux:base-devel-20260927.0.600689}"
targets=(x86_64:x86_64-unknown-linux-gnu aarch64:aarch64-unknown-linux-gnu)

die() { echo "update-aur: $*" >&2; exit 1; }

# pacman versions cannot contain '-', and prereleases are not published to the AUR.
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "usage: $0 VERSION (X.Y.Z)"
pkgbuild="$dir/PKGBUILD"
[ -f "$pkgbuild" ] || die "$pkgbuild not found"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
fetch() { curl -fsSL --retry 3 -o "$2" "$1" || die "cannot download $1"; }
is_sha256() { [[ "$1" =~ ^[0-9a-f]{64}$ ]]; }

# The source archive has no published checksum; GitHub keeps tag archives byte-stable.
fetch "$base/archive/refs/tags/v$version.tar.gz" "$work/src.tar.gz"
tar -tzf "$work/src.tar.gz" "gpueue-$version/packaging/config.toml" >/dev/null 2>&1 ||
    die "the source archive of v$version has no gpueue-$version/packaging/config.toml"
tar -xzOf "$work/src.tar.gz" "gpueue-$version/packaging/minisign.pub" >"$work/minisign.pub" 2>/dev/null ||
    die "the source archive of v$version has no gpueue-$version/packaging/minisign.pub"
src_sum="$(sha256sum "$work/src.tar.gz" | cut -d' ' -f1)"
is_sha256 "$src_sum" || die "cannot compute the checksum of the source archive"

minisign="${MINISIGN:-}"
[ -n "$minisign" ] || minisign="$("$root/scripts/dist/get-minisign.sh" "$work/minisign")"

# The tarballs, checked against the release key and the release's trusted comment; their
# checksums are computed from the verified files.
declare -A sums
for entry in "${targets[@]}"; do
    arch="${entry%%:*}"
    name="gpueue-v$version-${entry#*:}.tar.gz"
    fetch "$base/releases/download/v$version/$name" "$work/$name"
    fetch "$base/releases/download/v$version/$name.minisig" "$work/$name.minisig"
    comment="$("$minisign" -V -Q -p "$work/minisign.pub" -m "$work/$name" -x "$work/$name.minisig")" ||
        die "bad signature on $name"
    [ "$comment" = "gpueue v$version $name" ] ||
        die "the signature of $name is for another file (trusted comment \"$comment\")"
    sum="$(sha256sum "$work/$name" | cut -d' ' -f1)"
    is_sha256 "$sum" || die "cannot compute the checksum of $name"
    sums[$arch]="$sum"
    echo "update-aur: verified the signature of $name"
done

old_version="$(sed -n 's/^pkgver=//p' "$pkgbuild")"
# set_line KEY VALUE: replaces the single line KEY=... of the PKGBUILD.
set_line() {
    [ "$(grep -c "^$1=" "$pkgbuild")" = 1 ] || die "expected one '$1=' line in $pkgbuild"
    sed -i "s|^$1=.*|$1=$2|" "$pkgbuild"
}
set_line pkgver "$version"
[ "$old_version" = "$version" ] || set_line pkgrel 1
set_line sha256sums "('$src_sum')"
for arch in "${!sums[@]}"; do
    set_line "sha256sums_$arch" "('${sums[$arch]}')"
done

# makepkg refuses to run as root and wants writable build and output directories, although
# --printsrcinfo writes nothing.
dests=(HOME=/tmp BUILDDIR=/tmp PKGDEST=/tmp SRCDEST=/tmp SRCPKGDEST=/tmp LOGDEST=/tmp)
if command -v makepkg >/dev/null 2>&1; then
    as_user=()
    [ "$(id -u)" != 0 ] || as_user=(runuser -u nobody --)
    (cd "$dir" && "${as_user[@]}" env "${dests[@]}" makepkg --printsrcinfo) >"$work/SRCINFO"
elif command -v docker >/dev/null 2>&1; then
    docker run --rm --user 65534:65534 -v "$dir":/pkg:ro -w /pkg "$image" \
        env "${dests[@]}" makepkg --printsrcinfo >"$work/SRCINFO"
else
    die "neither makepkg nor docker is available to generate .SRCINFO"
fi
grep -q "pkgver = $version\$" "$work/SRCINFO" || die "makepkg --printsrcinfo failed"
cp "$work/SRCINFO" "$dir/.SRCINFO"
echo "update-aur: $dir is at $version-$(sed -n 's/^pkgrel=//p' "$pkgbuild")"
