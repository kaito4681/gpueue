#!/usr/bin/env bash
# Maintains the puzuz APT repository: a flat repository (`Suites: ./` in
# packaging/apt/puzuz.sources) whose files are the assets of the GitHub
# release `apt`.
#
#   apt-repo.sh update DIR [DEB...]
#     Works on a local directory: adds the DEBs to DIR, keeps the newest KEEP
#     versions per architecture (Debian version order), writes the index
#     (Packages, Packages.gz, Release, its signatures Release.gpg (detached)
#     and InRelease (clearsigned)), plus the archive keyring as puzuz.gpg and
#     the sources file as puzuz.sources, both taken from the newest kept
#     package (for adding the repository by hand), and only then deletes the
#     .deb files that fell out of the index. Nothing in DIR changes unless
#     the new index was generated, signed and verified.
#
#   apt-repo.sh github TAG
#     Publishes the .deb files of the release TAG, if it is a published,
#     stable (not pre-release) release; otherwise it says so and exits 0.
#     Creates the release `apt` when missing (as a pre-release, so it never
#     becomes the "latest" release that install.sh and `puzuz update`
#     follow), downloads its .deb files and TAG's (checked against their
#     .sha256 and their minisign signatures, with the public key of TAG's
#     commit), runs `update` on them, then uploads the new .deb files first,
#     the index files after them (InRelease last), and only then deletes the
#     .deb files that fell out of the index. A client that runs `apt update`
#     during the upload can see a mismatch between the index files; running
#     it again fixes that.
#
# A .deb must be named puzuz_<version>_<amd64|arm64>.deb, where <version> is
# the release's (tag) version: the package's own version differs for a
# pre-release (0.2.0-rc.1 is 0.2.0~rc.1 inside the package, and GitHub
# cannot store `~` in asset names), so `Filename:` in Packages is the file
# name, never derived from the package version.
#
# The index is signed only with a key that is in the keyring the newest kept
# package installs (/etc/apt/keyrings/puzuz.gpg), and never for packages that
# ship the TEST key (scripts/dist/gen-apt-key.sh --test).
#
# Environment:
#   APT_GPG_PRIVATE_KEY  the ASCII-armored secret key that signs the index
#                        (scripts/dist/gen-apt-key.sh); required
#   APT_GPG_PASSPHRASE   its passphrase (may be empty for `update`)
#   APT_KEYRING          check the signing key against this keyring instead of
#                        the newest package's (tests)
#   PUZUZ_APT_KEEP       versions to keep per architecture (default 5)
#   GITHUB_REPOSITORY    `github`: owner/repo (default kaito4681/puzuz); gh
#                        needs GH_TOKEN with contents: write
# Needs dpkg-deb, apt-ftparchive (apt-utils), gpg and gpgv; `github` also gh
# and curl.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
keep="${PUZUZ_APT_KEEP:-5}"
apt_release=apt
pkg_keyring=etc/apt/keyrings/puzuz.gpg
pkg_sources=etc/apt/sources.list.d/puzuz.sources
index_files=(puzuz.gpg puzuz.sources Packages Packages.gz Release Release.gpg InRelease)
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

# Control fields of the checked .deb files, by path. Every function below
# runs in the main shell (no command substitution), so `die` ends the script.
declare -A deb_version=() deb_arch=()

# Reads the .deb $1 once and records its version and architecture, after
# checking that it is a puzuz package whose file name matches its fields.
read_deb() {
    local deb="$1" name fields key value pkg="" arch="" version="" file_version
    name="$(basename "$deb")"
    fields="$(dpkg-deb --field "$deb" Package Architecture Version 2>/dev/null)" || die "$name is not a valid .deb"
    dpkg-deb --fsys-tarfile "$deb" >/dev/null 2>&1 || die "$name is damaged (cannot read its contents)"
    while IFS=': ' read -r key value; do
        case "$key" in
        Package) pkg="$value" ;;
        Architecture) arch="$value" ;;
        Version) version="$value" ;;
        esac
    done <<<"$fields"
    [ "$pkg" = puzuz ] || die "$name: package '$pkg', not puzuz"
    case "$arch" in amd64 | arm64) ;; *) die "$name: unsupported architecture '$arch'" ;; esac
    [ -n "$version" ] || die "$name: no version"
    [[ "$name" =~ ^puzuz_([^_/~]+)_${arch}\.deb$ ]] || die "$name: not named puzuz_<version>_$arch.deb"
    file_version="${BASH_REMATCH[1]}"
    # The tag version, with the pre-release separator as nfpm writes it.
    [ "$version" = "$file_version" ] || [ "$version" = "${file_version/-/\~}" ] ||
        die "$name: the package version $version does not match the file name"
    deb_version[$deb]="$version"
    deb_arch[$deb]="$arch"
}

# Sorts the (read) .deb files given as arguments into `sorted`, newest
# package version first; two files with the same version are an error.
sorted=()
sort_debs() {
    local deb i v
    sorted=()
    for deb in "$@"; do
        v="${deb_version[$deb]}"
        i=0
        while [ "$i" -lt "${#sorted[@]}" ]; do
            if dpkg --compare-versions "$v" gt "${deb_version[${sorted[$i]}]}"; then break; fi
            if dpkg --compare-versions "$v" eq "${deb_version[${sorted[$i]}]}"; then
                die "$(basename "$deb") and $(basename "${sorted[$i]}") have the same version $v"
            fi
            i=$((i + 1))
        done
        sorted=("${sorted[@]:0:$i}" "$deb" "${sorted[@]:$i}")
    done
}

# Imports APT_GPG_PRIVATE_KEY into GNUPGHOME and sets `signing_fpr` to the
# fingerprint of its primary key, after checking that it is in keyring $1.
signing_fpr=""
import_signing_key() {
    local keyring="$1" fprs
    [ -n "${APT_GPG_PRIVATE_KEY:-}" ] || die "APT_GPG_PRIVATE_KEY is not set"
    printf '%s\n' "$APT_GPG_PRIVATE_KEY" | gpg --batch --quiet --import 2>/dev/null ||
        die "cannot import APT_GPG_PRIVATE_KEY"
    signing_fpr="$(gpg --batch --with-colons --list-secret-keys | awk -F: '$1 == "fpr" {print $10; exit}')"
    [ -n "$signing_fpr" ] || die "APT_GPG_PRIVATE_KEY holds no secret key"
    fprs="$(gpg --batch --with-colons --import-options show-only --import "$keyring" 2>/dev/null |
        awk -F: '$1 == "fpr" {print $10}')" || die "cannot read the keyring $keyring"
    grep -qxF "$signing_fpr" <<<"$fprs" ||
        die "the signing key $signing_fpr is not in $keyring, which clients verify the repository with"
}

# Copies $1 to $2 through a temporary file in $2's directory.
put() {
    cp "$1" "$2.tmp"
    mv "$2.tmp" "$2"
}

cmd_update() {
    local dir="${1:?}" deb arch name
    shift
    need dpkg-deb dpkg apt-ftparchive gpg gpgv gzip cmp
    [[ "$keep" =~ ^[1-9][0-9]*$ ]] || die "PUZUZ_APT_KEEP must be a positive number"

    local work
    work="$(mktemp -d)"
    temp_dirs+=("$work")

    # Every package, existing and new, by file name; each read once.
    local -A by_name=()
    local added=()
    if [ -d "$dir" ]; then
        shopt -s nullglob
        for deb in "$dir"/puzuz_*.deb; do
            read_deb "$deb"
            by_name[$(basename "$deb")]="$deb"
        done
        shopt -u nullglob
    fi
    for deb in "$@"; do
        name="$(basename "$deb")"
        [ -f "$deb" ] || die "no such file: $deb"
        if [ -n "${by_name[$name]:-}" ]; then
            # Already published under that name: it must be the same file.
            cmp -s "$deb" "${by_name[$name]}" || die "$name is already in the repository with other contents"
            continue
        fi
        read_deb "$deb"
        by_name[$name]="$deb"
        added+=("$name")
    done
    [ "${#by_name[@]}" -gt 0 ] || die "no packages for $dir"

    # The newest $keep versions per architecture.
    local kept=() pruned=() list i
    for arch in amd64 arm64; do
        list=()
        for deb in "${by_name[@]}"; do
            if [ "${deb_arch[$deb]}" = "$arch" ]; then list+=("$deb"); fi
        done
        [ "${#list[@]}" -gt 0 ] || continue
        sort_debs "${list[@]}"
        for i in "${!sorted[@]}"; do
            if [ "$i" -lt "$keep" ]; then kept+=("${sorted[$i]}"); else pruned+=("${sorted[$i]}"); fi
        done
    done

    # The keyring and sources file of the newest kept package: what the next
    # `apt upgrade` installs, and what puzuz.gpg / puzuz.sources publish.
    local newest
    # Both architectures of a version ship the same files; the newest version
    # of all is enough (same versions do not collide across architectures).
    newest="${kept[0]}"
    for deb in "${kept[@]}"; do
        if dpkg --compare-versions "${deb_version[$deb]}" gt "${deb_version[$newest]}"; then newest="$deb"; fi
    done
    mkdir "$work/newest"
    dpkg-deb -x "$newest" "$work/newest" || die "cannot unpack $(basename "$newest")"
    [ -f "$work/newest/$pkg_keyring" ] && [ -f "$work/newest/$pkg_sources" ] ||
        die "$(basename "$newest") has no /$pkg_keyring or /$pkg_sources"
    if grep -aq 'TEST KEY' "$work/newest/$pkg_keyring"; then
        die "$(basename "$newest") ships the TEST archive key; create the real key with scripts/dist/gen-apt-key.sh"
    fi
    local keyring="${APT_KEYRING:-$work/newest/$pkg_keyring}"
    [ -f "$keyring" ] || die "no keyring $keyring"
    keyring="$(cd "$(dirname "$keyring")" && pwd)/$(basename "$keyring")"

    GNUPGHOME="$work/gnupg"
    export GNUPGHOME
    mkdir -m 700 "$GNUPGHOME"
    import_signing_key "$keyring"

    # Packages: one stanza per kept file, Filename: the bare asset name.
    local idx="$work/index"
    mkdir "$idx" "$work/pool"
    for deb in "${kept[@]}"; do ln -s "$(cd "$(dirname "$deb")" && pwd)/$(basename "$deb")" "$work/pool/$(basename "$deb")"; done
    (cd "$work/pool" && apt-ftparchive packages .) >"$work/Packages.raw" || die "apt-ftparchive packages failed"
    sed 's|^Filename: \./|Filename: |' "$work/Packages.raw" >"$idx/Packages"
    [ "$(grep -c '^Package: ' "$idx/Packages")" = "${#kept[@]}" ] || die "apt-ftparchive missed a package"
    gzip -9n <"$idx/Packages" >"$idx/Packages.gz"
    # No Suite/Codename: a flat repository is addressed by its URL only, and
    # apt warns when they change. No Valid-Until: the index changes only with
    # a release.
    apt-ftparchive \
        -o APT::FTPArchive::Release::Origin=puzuz \
        -o APT::FTPArchive::Release::Label=puzuz \
        -o APT::FTPArchive::Release::Architectures="amd64 arm64" \
        -o APT::FTPArchive::Release::Description="puzuz (https://github.com/kaito4681/puzuz)" \
        release "$idx" >"$work/Release" || die "apt-ftparchive release failed"
    mv "$work/Release" "$idx/Release"

    local sign=(gpg --batch --quiet --pinentry-mode loopback --passphrase-fd 3
        --local-user "$signing_fpr!" --digest-algo SHA512)
    "${sign[@]}" --clearsign -o "$idx/InRelease" "$idx/Release" 3<<<"${APT_GPG_PASSPHRASE:-}" ||
        die "cannot sign the index (wrong APT_GPG_PASSPHRASE?)"
    "${sign[@]}" --armor --detach-sign -o "$idx/Release.gpg" "$idx/Release" 3<<<"${APT_GPG_PASSPHRASE:-}" ||
        die "cannot sign the index (wrong APT_GPG_PASSPHRASE?)"
    gpgv --quiet --keyring "$keyring" "$idx/InRelease" 2>/dev/null || die "InRelease does not verify with $keyring"
    gpgv --quiet --keyring "$keyring" "$idx/Release.gpg" "$idx/Release" 2>/dev/null ||
        die "Release.gpg does not verify with $keyring"
    cp "$work/newest/$pkg_keyring" "$idx/puzuz.gpg"
    cp "$work/newest/$pkg_sources" "$idx/puzuz.sources"

    # Only now change DIR: new packages, then the index, then deletions.
    mkdir -p "$dir"
    for name in "${added[@]}"; do
        # Not pruned right away (an old tag re-run).
        for deb in "${kept[@]}"; do
            if [ "$(basename "$deb")" = "$name" ]; then
                put "$deb" "$dir/$name"
                log "added $name"
            fi
        done
    done
    for name in "${index_files[@]}"; do put "$idx/$name" "$dir/$name"; done
    for deb in "${pruned[@]}"; do
        name="$(basename "$deb")"
        if [ -e "$dir/$name" ]; then
            rm -f "$dir/$name"
            log "removed $name"
        fi
    done
    log "index of $dir: $(for deb in "${kept[@]}"; do printf '%s ' "$(basename "$deb")"; done)"
}

cmd_github() {
    local tag="${1:?}" repo="${GITHUB_REPOSITORY:-kaito4681/puzuz}" state
    need gh curl dpkg-deb sha256sum

    # The publishing policy: published, stable releases of version tags.
    if [[ ! "$tag" =~ ^v[0-9] ]]; then
        echo "::notice::$tag is not a version tag; not publishing it to the APT repository"
        return 0
    fi
    state="$(gh release view "$tag" -R "$repo" --json isDraft,isPrerelease -q '"\(.isDraft) \(.isPrerelease)"')" ||
        die "no release $tag"
    if [ "$state" != "false false" ]; then
        echo "::notice::release $tag is a draft or a pre-release (draft, pre-release: $state); only published stable releases go to the APT repository"
        return 0
    fi

    local work
    work="$(mktemp -d)"
    temp_dirs+=("$work")
    mkdir -p "$work/new" "$work/repo"

    # The release's packages, checked like a user would check them, with the
    # minisign key of the tag's commit (a re-run for an older tag may come
    # from a newer default branch).
    gh api -H "Accept: application/vnd.github.raw" \
        "repos/$repo/contents/packaging/minisign.pub?ref=$tag" >"$work/minisign.pub" ||
        die "cannot read packaging/minisign.pub at $tag"
    gh release download "$tag" -R "$repo" -D "$work/new" \
        -p 'puzuz_*.deb' -p 'puzuz_*.deb.sha256' -p 'puzuz_*.deb.minisig'
    local minisign deb new=()
    minisign="$("$root/scripts/dist/get-minisign.sh" "$work/minisign")"
    shopt -s nullglob
    for deb in "$work/new"/puzuz_*.deb; do
        (cd "$work/new" && sha256sum --quiet -c "$(basename "$deb").sha256") || die "$(basename "$deb"): checksum mismatch"
        "$minisign" -Vq -p "$work/minisign.pub" -m "$deb" || die "$(basename "$deb"): bad signature"
        new+=("$deb")
    done
    shopt -u nullglob
    [ "${#new[@]}" -gt 0 ] || die "release $tag has no .deb files"

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
    shopt -s nullglob
    for deb in "$work/repo"/puzuz_*.deb; do after+=("$(basename "$deb")"); done
    shopt -u nullglob
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
    for name in "${index_files[@]}"; do
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
