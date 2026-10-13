#!/usr/bin/env bash
# Maintains the puzuz APT repository: a flat repository (`Suites: ./` in
# packaging/apt/puzuz.sources) whose files are the assets of the GitHub
# release `apt`.
#
#   apt-repo.sh update DIR [DEB...]
#     Works on a local directory: adds the DEBs to DIR, keeps the newest KEEP
#     versions per architecture (Debian version order) and deletes the older
#     .deb files, then writes the index: Packages, Packages.gz, Release, its
#     signatures Release.gpg (detached) and InRelease (clearsigned), the
#     public keyring as puzuz.gpg and the newest package's sources file as
#     puzuz.sources (both for adding the repository by hand).
#
#   apt-repo.sh github TAG
#     Publishes the .deb files of the (published, stable) release TAG: creates
#     the release `apt` when missing (as a pre-release, so it never becomes
#     the "latest" release that install.sh and `puzuz update` follow),
#     downloads its .deb files and TAG's (checked against their .sha256 and
#     minisign signatures), runs `update` on them, then uploads the new .deb
#     files first, the index files after them (InRelease last), and only then
#     deletes the .deb files that fell out of the index. A client that runs
#     `apt update` during the upload can see a mismatch between the index
#     files; running it again fixes that.
#
# A .deb must be named puzuz_<version>_<amd64|arm64>.deb, where <version> is
# the release's (tag) version: the package's own version differs for a
# pre-release (0.2.0-rc.1 is 0.2.0~rc.1 inside the package, and GitHub
# cannot store `~` in asset names), so `Filename:` in Packages is the file
# name, never derived from the package version.
#
# Environment:
#   APT_GPG_PRIVATE_KEY  the ASCII-armored secret key that signs the index
#                        (scripts/dist/gen-apt-key.sh); required
#   APT_GPG_PASSPHRASE   its passphrase (may be empty for `update`)
#   APT_KEYRING          the public keyring clients verify the index with; the
#                        signing key must be in it. `update`: default
#                        packaging/apt/puzuz-archive-keyring.gpg. `github`:
#                        the keyring inside TAG's packages (the key they
#                        install), which must not be the TEST key.
#   PUZUZ_APT_KEEP       versions to keep per architecture (default 5)
#   GITHUB_REPOSITORY    `github`: owner/repo (default kaito4681/puzuz); gh
#                        needs GH_TOKEN with contents: write
# Needs dpkg-deb, apt-ftparchive (apt-utils), gpg and gpgv; `github` also gh
# and curl.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
keep="${PUZUZ_APT_KEEP:-5}"
apt_release=apt
temp_dirs=()
cleanup() {
    if [ -n "${GNUPGHOME:-}" ]; then gpgconf --kill all >/dev/null 2>&1 || true; fi
    [ "${#temp_dirs[@]}" -eq 0 ] || rm -rf "${temp_dirs[@]}"
}
trap cleanup EXIT

die() {
    echo "apt-repo: $*" >&2
    exit 1
}
log() { echo "apt-repo: $*" >&2; }

usage() {
    sed -n '/^#   apt-repo.sh/p' "${BASH_SOURCE[0]}" | sed 's/^#  */usage: /' >&2
    exit 2
}

need() {
    for tool in "$@"; do
        command -v "$tool" >/dev/null 2>&1 || die "$tool is needed"
    done
}

# Prints the architecture of the .deb $1 after checking that it is a puzuz
# package whose file name matches its control fields.
check_deb() {
    local deb="$1" name pkg arch version file_version
    name="$(basename "$deb")"
    pkg="$(dpkg-deb --field "$deb" Package)" || die "$name is not a .deb"
    arch="$(dpkg-deb --field "$deb" Architecture)"
    version="$(dpkg-deb --field "$deb" Version)"
    [ "$pkg" = puzuz ] || die "$name: package $pkg, not puzuz"
    case "$arch" in amd64 | arm64) ;; *) die "$name: unsupported architecture $arch" ;; esac
    [[ "$name" =~ ^puzuz_([^_/~]+)_${arch}\.deb$ ]] || die "$name: not named puzuz_<version>_$arch.deb"
    file_version="${BASH_REMATCH[1]}"
    # The tag version, with the pre-release separator as nfpm writes it.
    [ "$version" = "$file_version" ] || [ "$version" = "${file_version/-/\~}" ] ||
        die "$name: the package version $version does not match the file name"
    echo "$arch"
}

# Sorts the .deb files given as arguments, newest package version first.
sort_debs() {
    local sorted=() deb i j version
    for deb in "$@"; do
        version="$(dpkg-deb --field "$deb" Version)"
        i=0
        while [ "$i" -lt "${#sorted[@]}" ]; do
            if dpkg --compare-versions "$version" gt "$(dpkg-deb --field "${sorted[$i]}" Version)"; then
                break
            fi
            if dpkg --compare-versions "$version" eq "$(dpkg-deb --field "${sorted[$i]}" Version)"; then
                die "$(basename "$deb") and $(basename "${sorted[$i]}") have the same version $version"
            fi
            i=$((i + 1))
        done
        sorted=("${sorted[@]:0:$i}" "$deb" "${sorted[@]:$i}")
    done
    for j in "${sorted[@]}"; do echo "$j"; done
}

# Imports APT_GPG_PRIVATE_KEY into a new GNUPGHOME and prints the
# fingerprint of its primary key, after checking that it is in APT_KEYRING.
import_signing_key() {
    local keyring="$1" fpr
    [ -n "${APT_GPG_PRIVATE_KEY:-}" ] || die "APT_GPG_PRIVATE_KEY is not set"
    printf '%s\n' "$APT_GPG_PRIVATE_KEY" | gpg --batch --quiet --import 2>/dev/null ||
        die "cannot import APT_GPG_PRIVATE_KEY"
    fpr="$(gpg --batch --with-colons --list-secret-keys | awk -F: '$1 == "fpr" {print $10; exit}')"
    [ -n "$fpr" ] || die "APT_GPG_PRIVATE_KEY holds no secret key"
    gpg --batch --with-colons --import-options show-only --import "$keyring" 2>/dev/null |
        awk -F: '$1 == "fpr" {print $10}' | grep -qx "$fpr" ||
        die "the signing key $fpr is not in $keyring, which clients verify the repository with"
    echo "$fpr"
}

cmd_update() {
    local dir="${1:?}" deb arch name
    shift
    need dpkg-deb dpkg apt-ftparchive gpg gpgv gzip
    [[ "$keep" =~ ^[1-9][0-9]*$ ]] || die "PUZUZ_APT_KEEP must be a positive number"
    local keyring="${APT_KEYRING:-$root/packaging/apt/puzuz-archive-keyring.gpg}"
    [ -f "$keyring" ] || die "no keyring $keyring"
    keyring="$(cd "$(dirname "$keyring")" && pwd)/$(basename "$keyring")"
    mkdir -p "$dir"
    dir="$(cd "$dir" && pwd)"

    local work
    work="$(mktemp -d)"
    temp_dirs+=("$work")
    GNUPGHOME="$work/gnupg"
    export GNUPGHOME
    mkdir -m 700 "$GNUPGHOME"
    local fpr
    fpr="$(import_signing_key "$keyring")"

    # New files first: an existing file of the same name must be the same file
    # (it is already published under that name).
    for deb in "$@"; do
        name="$(basename "$deb")"
        check_deb "$deb" >/dev/null
        if [ -e "$dir/$name" ]; then
            cmp -s "$deb" "$dir/$name" || die "$name is already in the repository with other contents"
        else
            cp "$deb" "$dir/$name.tmp"
            mv "$dir/$name.tmp" "$dir/$name"
            log "added $name"
        fi
    done

    # The newest $keep versions per architecture.
    local -A by_arch=()
    shopt -s nullglob
    for deb in "$dir"/puzuz_*.deb; do
        arch="$(check_deb "$deb")"
        by_arch[$arch]+="$deb"$'\n'
    done
    shopt -u nullglob
    local kept=() list i
    for arch in amd64 arm64; do
        [ -n "${by_arch[$arch]:-}" ] || continue
        mapfile -t list < <(printf '%s' "${by_arch[$arch]}")
        mapfile -t list < <(sort_debs "${list[@]}")
        for i in "${!list[@]}"; do
            if [ "$i" -lt "$keep" ]; then
                kept+=("${list[$i]}")
            else
                rm -f "${list[$i]}"
                log "removed $(basename "${list[$i]}")"
            fi
        done
    done
    [ "${#kept[@]}" -gt 0 ] || die "no packages in $dir"

    # Packages: one stanza per kept file, Filename: the bare asset name.
    local idx="$work/index"
    mkdir "$idx"
    for deb in "${kept[@]}"; do ln -s "$deb" "$idx/$(basename "$deb")"; done
    (cd "$idx" && apt-ftparchive packages .) | sed 's|^Filename: \./|Filename: |' >"$work/Packages"
    [ "$(grep -c '^Package: ' "$work/Packages")" = "${#kept[@]}" ] || die "apt-ftparchive missed a package"
    rm -f "$idx"/*.deb
    cp "$work/Packages" "$idx/Packages"
    gzip -9n <"$idx/Packages" >"$idx/Packages.gz"
    # No Suite/Codename: a flat repository is addressed by its URL only, and
    # apt warns when they change. No Valid-Until: the index changes only with
    # a release.
    apt-ftparchive \
        -o APT::FTPArchive::Release::Origin=puzuz \
        -o APT::FTPArchive::Release::Label=puzuz \
        -o APT::FTPArchive::Release::Architectures="amd64 arm64" \
        -o APT::FTPArchive::Release::Description="puzuz (https://github.com/kaito4681/puzuz)" \
        release "$idx" >"$work/Release"
    mv "$work/Release" "$idx/Release"

    local sign=(gpg --batch --quiet --pinentry-mode loopback --passphrase-fd 3
        --local-user "$fpr!" --digest-algo SHA512)
    "${sign[@]}" --clearsign -o "$idx/InRelease" "$idx/Release" 3<<<"${APT_GPG_PASSPHRASE:-}" ||
        die "cannot sign the index (wrong APT_GPG_PASSPHRASE?)"
    "${sign[@]}" --armor --detach-sign -o "$idx/Release.gpg" "$idx/Release" 3<<<"${APT_GPG_PASSPHRASE:-}" ||
        die "cannot sign the index (wrong APT_GPG_PASSPHRASE?)"
    gpgv --quiet --keyring "$keyring" "$idx/InRelease" 2>/dev/null || die "InRelease does not verify with $keyring"
    gpgv --quiet --keyring "$keyring" "$idx/Release.gpg" "$idx/Release" 2>/dev/null ||
        die "Release.gpg does not verify with $keyring"
    cp "$keyring" "$idx/puzuz.gpg"
    # The sources file of the newest package, for adding the repository by
    # hand: the same bytes as the package's conffile, so that installing the
    # package afterwards does not ask about it.
    rm -rf "$work/newest"
    dpkg-deb -x "${kept[0]}" "$work/newest"
    cp "$work/newest/etc/apt/sources.list.d/puzuz.sources" "$idx/puzuz.sources"

    for name in puzuz.gpg puzuz.sources Packages Packages.gz Release Release.gpg InRelease; do
        cp "$idx/$name" "$dir/$name.tmp"
        mv "$dir/$name.tmp" "$dir/$name"
    done
    log "index of $dir: $(for deb in "${kept[@]}"; do printf '%s ' "$(basename "$deb")"; done)"
}

cmd_github() {
    local tag="${1:?}" repo="${GITHUB_REPOSITORY:-kaito4681/puzuz}"
    need gh curl dpkg-deb sha256sum
    [[ "$tag" =~ ^v[0-9] ]] || die "$tag is not a version tag"
    local state
    state="$(gh release view "$tag" -R "$repo" --json isDraft,isPrerelease -q '"\(.isDraft) \(.isPrerelease)"')" ||
        die "no release $tag"
    [ "$state" = "false false" ] || die "release $tag is a draft or a pre-release (draft, pre-release: $state); only published stable releases go to the APT repository"

    local work
    work="$(mktemp -d)"
    temp_dirs+=("$work")
    mkdir -p "$work/new" "$work/repo"

    # The release's packages, checked like a user would check them.
    gh release download "$tag" -R "$repo" -D "$work/new" \
        -p 'puzuz_*.deb' -p 'puzuz_*.deb.sha256' -p 'puzuz_*.deb.minisig'
    local minisign deb new=()
    minisign="$("$root/scripts/dist/get-minisign.sh" "$work/minisign")"
    shopt -s nullglob
    for deb in "$work/new"/puzuz_*.deb; do
        (cd "$work/new" && sha256sum --quiet -c "$(basename "$deb").sha256") || die "$(basename "$deb"): checksum mismatch"
        "$minisign" -Vq -p "$root/packaging/minisign.pub" -m "$deb" || die "$(basename "$deb"): bad signature"
        new+=("$deb")
    done
    shopt -u nullglob
    [ "${#new[@]}" -gt 0 ] || die "release $tag has no .deb files"

    # The key the packages install is the key the index must be signed with.
    if [ -z "${APT_KEYRING:-}" ]; then
        local keyring="$work/keyring.gpg" k="$work/k"
        for deb in "${new[@]}"; do
            rm -rf "$k"
            dpkg-deb -x "$deb" "$k"
            if [ ! -f "$keyring" ]; then
                cp "$k/usr/share/keyrings/puzuz-archive-keyring.gpg" "$keyring"
            else
                cmp -s "$keyring" "$k/usr/share/keyrings/puzuz-archive-keyring.gpg" ||
                    die "the packages of $tag ship different archive keys"
            fi
        done
        export APT_KEYRING="$keyring"
    fi
    if grep -aq 'TEST KEY' "$APT_KEYRING"; then
        die "the packages of $tag ship the TEST archive key; create the real key with scripts/dist/gen-apt-key.sh"
    fi

    if ! gh release view "$apt_release" -R "$repo" >/dev/null 2>&1; then
        log "creating the release $apt_release"
        gh release create "$apt_release" -R "$repo" --prerelease --latest=false \
            --title "APT repository" \
            --notes "The puzuz APT repository (a flat repository; its files are the assets of this release). It is updated by .github/workflows/release-apt.yml; see docs/install.md." >/dev/null
    fi
    local before=() name
    mapfile -t before < <(gh release view "$apt_release" -R "$repo" --json assets -q '.assets[].name' | grep -E '^puzuz_.*\.deb$' || true)
    if [ "${#before[@]}" -gt 0 ]; then
        gh release download "$apt_release" -R "$repo" -D "$work/repo" -p 'puzuz_*.deb'
    fi

    cmd_update "$work/repo" "${new[@]}"

    local after=() uploads=() removed=()
    mapfile -t after < <(cd "$work/repo" && ls puzuz_*.deb)
    for name in "${after[@]}"; do
        printf '%s\n' "${before[@]}" | grep -qxF "$name" || uploads+=("$work/repo/$name")
    done
    for name in "${before[@]}"; do
        printf '%s\n' "${after[@]}" | grep -qxF "$name" || removed+=("$name")
    done

    # gh uploads the files of one call in parallel: one call per step keeps
    # the order (packages, then the index, InRelease last).
    if [ "${#uploads[@]}" -gt 0 ]; then
        log "uploading ${uploads[*]##*/}"
        gh release upload "$apt_release" -R "$repo" --clobber "${uploads[@]}"
    fi
    for name in puzuz.gpg puzuz.sources Packages Packages.gz Release Release.gpg InRelease; do
        gh release upload "$apt_release" -R "$repo" --clobber "$work/repo/$name"
    done
    for name in "${removed[@]}"; do
        log "deleting $name"
        gh release delete-asset "$apt_release" "$name" -R "$repo" --yes
    done
    log "published $tag to https://github.com/$repo/releases/download/$apt_release/"
}

case "${1:-}" in
update)
    [ "$#" -ge 2 ] || usage
    shift
    cmd_update "$@"
    ;;
github)
    [ "$#" -eq 2 ] || usage
    cmd_github "$2"
    ;;
*) usage ;;
esac
