#!/usr/bin/env bash
# Maintains the gpueue APT repository: a flat repository (`Suites: ./` in
# packaging/apt/gpueue.sources) whose files are the assets of the GitHub
# release `apt`.
#
#   apt-repo.sh update DIR [DEB...]
#     Works on a local directory: first verifies the packages already in DIR
#     against DIR's signed index (InRelease must verify with the packaged
#     archive keyring, and each .deb must have the SHA256 that the signed
#     Packages lists; a mismatch is refused, and a .deb the index does not
#     list is dropped), then adds the DEBs to DIR, keeps the newest KEEP
#     versions per architecture (Debian version order), writes the index
#     (Packages, Packages.gz, Release, its signatures Release.gpg (detached)
#     and InRelease (clearsigned)), plus the archive keyring as gpueue.gpg and
#     the sources file as gpueue.sources, both taken from the newest kept
#     package (for adding the repository by hand), and only then deletes the
#     .deb files that fell out of the index. Nothing in DIR changes unless
#     the new index was generated, signed and verified.
#
#   apt-repo.sh github TAG
#     Publishes the .deb files of the release TAG, if it is a published,
#     stable (not pre-release) release; otherwise it says so and exits 0.
#     Creates the release `apt` when missing (as a pre-release, so it never
#     becomes the "latest" release that install.sh and `gpueue update`
#     follow), downloads its .deb files and index files and TAG's .deb files
#     (checked against their .sha256 and their minisign signatures, with the
#     public key of the default branch, which TAG's commit must carry too), runs `update` on them (which verifies the
#     existing packages), then uploads the new .deb files first, the index
#     files after them (InRelease last), and only then deletes the .deb files
#     that fell out of the index. Each upload is tried 3 times; when the index
#     still cannot be uploaded, the previous index files are put back and the
#     job fails, saying to re-run it. A client that runs `apt update` during
#     the upload can see a mismatch between the index files; running it again
#     fixes that.
#
# A .deb must be named gpueue_<version>_<amd64|arm64>.deb, where <version> is
# the release's (tag) version: the package's own version differs for a
# pre-release (0.2.0-rc.1 is 0.2.0~rc.1 inside the package, and GitHub
# cannot store `~` in asset names), so `Filename:` in Packages is the file
# name, never derived from the package version.
#
# The index is signed only with a key that is in the keyring the newest kept
# package installs (/etc/apt/keyrings/gpueue.gpg), and never for packages that
# ship the TEST key (scripts/dist/gen-apt-key.sh --test).
#
# Environment:
#   APT_GPG_PRIVATE_KEY  the ASCII-armored secret key that signs the index
#                        (scripts/dist/gen-apt-key.sh); required
#   APT_GPG_PASSPHRASE   its passphrase (may be empty for `update`)
#   APT_KEYRING          check the signing key against this keyring instead of
#                        the newest package's (tests)
#   APT_VERIFY_KEYRING   verify the existing index with this keyring (default:
#                        packaging/apt/gpueue-archive-keyring.gpg of this tree;
#                        when the archive key changes, it must also hold the
#                        key that signed the previous index)
#   MINISIGN             `github`: the minisign binary (default: a pinned one,
#                        downloaded by scripts/dist/get-minisign.sh)
#   GPUEUE_APT_RETRY_DELAY  `github`: seconds before the 2nd upload attempt,
#                        twice that before the 3rd (default 5)
#   GPUEUE_APT_KEEP       versions to keep per architecture (default 5)
#   GITHUB_REPOSITORY    `github`: owner/repo (default kaito4681/gpueue); gh
#                        needs GH_TOKEN with contents: write
# Needs dpkg-deb, apt-ftparchive (apt-utils), gpg and gpgv; `github` also gh
# and curl.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
keep="${GPUEUE_APT_KEEP:-5}"
apt_release=apt
pkg_keyring=etc/apt/keyrings/gpueue.gpg
pkg_sources=etc/apt/sources.list.d/gpueue.sources
index_files=(gpueue.gpg gpueue.sources Packages Packages.gz Release Release.gpg InRelease)
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
# checking that it is a gpueue package whose file name matches its fields.
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
    [ "$pkg" = gpueue ] || die "$name: package '$pkg', not gpueue"
    case "$arch" in amd64 | arm64) ;; *) die "$name: unsupported architecture '$arch'" ;; esac
    [ -n "$version" ] || die "$name: no version"
    [[ "$name" =~ ^gpueue_([^_/~]+)_${arch}\.deb$ ]] || die "$name: not named gpueue_<version>_$arch.deb"
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
    signing_fpr="$(gpg --batch --with-colons --list-secret-keys | awk -F: '$1 == "fpr" && fpr == "" {fpr = $10} END {print fpr}')"
    [ -n "$signing_fpr" ] || die "APT_GPG_PRIVATE_KEY holds no secret key"
    fprs="$(gpg --batch --with-colons --import-options show-only --import "$keyring" 2>/dev/null |
        awk -F: '$1 == "fpr" {print $10}')" || die "cannot read the keyring $keyring"
    grep -qxF "$signing_fpr" <<<"$fprs" ||
        die "the signing key $signing_fpr is not in $keyring, which clients verify the repository with"
}

# Checks the packages already in DIR ($1) against DIR's signed index before
# they are indexed and signed again: InRelease must verify with the keyring
# $2, Packages must have the SHA256 that the signed Release gives it, and
# every .deb in DIR that Packages lists must have the SHA256 listed there.
# A .deb that Packages does not list (left behind by an interrupted run,
# which adds packages before the index) is not trusted either: its name goes
# to `stale`, and it is neither indexed nor kept.
stale=()
verify_existing() {
    local dir="$1" keyring="$2" deb name want got release debs
    local -A listed=()
    stale=()
    shopt -s nullglob
    debs=("$dir"/gpueue_*.deb)
    shopt -u nullglob
    [ "${#debs[@]}" -gt 0 ] || return 0
    [ -f "$dir/InRelease" ] && [ -f "$dir/Packages" ] ||
        die "$dir has packages but no InRelease or Packages to verify them with"
    [ -f "$keyring" ] || die "no keyring $keyring to verify the existing index with"
    release="$(gpgv --quiet --keyring "$keyring" --output - "$dir/InRelease" 2>/dev/null)" ||
        die "the existing InRelease does not verify with $keyring; refusing to sign its packages again"
    want="$(awk '/^SHA256:/ { s = 1; next } /^[^ ]/ { s = 0 } s && $3 == "Packages" { print $1 }' <<<"$release")"
    [ -n "$want" ] || die "the existing Release has no SHA256 of Packages"
    got="$(sha256sum <"$dir/Packages")"
    [ "${got%% *}" = "$want" ] || die "the existing Packages does not match the signed InRelease"
    while read -r name want; do
        listed[$name]="$want"
    done < <(awk '/^Filename: / { f = $2 } /^SHA256: / { h = $2 }
        /^$/ { if (f != "") print f, h; f = h = "" } END { if (f != "") print f, h }' "$dir/Packages")
    for deb in "${debs[@]}"; do
        name="$(basename "$deb")"
        if [ -z "${listed[$name]:-}" ]; then
            log "warning: $name is not in the signed index (left by an interrupted run?); dropping it"
            stale+=("$name")
            continue
        fi
        got="$(sha256sum <"$deb")"
        [ "${got%% *}" = "${listed[$name]}" ] ||
            die "$name differs from the signed index (SHA256 ${got%% *}, signed ${listed[$name]}); refusing to sign it again"
    done
    log "verified the existing index and its $((${#debs[@]} - ${#stale[@]})) packages"
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
    [[ "$keep" =~ ^[1-9][0-9]*$ ]] || die "GPUEUE_APT_KEEP must be a positive number"

    local work
    work="$(mktemp -d)"
    temp_dirs+=("$work")

    # Every package, existing and new, by file name; each read once.
    local -A by_name=() is_stale=()
    local added=()
    if [ -d "$dir" ]; then
        local verify_keyring="${APT_VERIFY_KEYRING:-$root/packaging/apt/gpueue-archive-keyring.gpg}"
        verify_keyring="$(cd "$(dirname "$verify_keyring")" && pwd)/$(basename "$verify_keyring")"
        verify_existing "$dir" "$verify_keyring"
        for name in "${stale[@]}"; do is_stale[$name]=1; done
        shopt -s nullglob
        for deb in "$dir"/gpueue_*.deb; do
            [ -z "${is_stale[$(basename "$deb")]:-}" ] || continue
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
    # `apt upgrade` installs, and what gpueue.gpg / gpueue.sources publish.
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
        -o APT::FTPArchive::Release::Origin=gpueue \
        -o APT::FTPArchive::Release::Label=gpueue \
        -o APT::FTPArchive::Release::Architectures="amd64 arm64" \
        -o APT::FTPArchive::Release::Description="gpueue (https://github.com/kaito4681/gpueue)" \
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
    cp "$work/newest/$pkg_keyring" "$idx/gpueue.gpg"
    cp "$work/newest/$pkg_sources" "$idx/gpueue.sources"

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
    # Unverified leftovers, unless a package of that name was just added.
    for name in "${!is_stale[@]}"; do
        if [ -z "${by_name[$name]:-}" ] && [ -e "$dir/$name" ]; then
            rm -f "$dir/$name"
            log "removed $name (not in the signed index)"
        fi
    done
    log "index of $dir: $(for deb in "${kept[@]}"; do printf '%s ' "$(basename "$deb")"; done)"
}

# upload REPO FILE...: uploads the files to the release `apt` (replacing
# assets of the same name), in up to 3 attempts.
upload() {
    local repo="$1" attempt
    shift
    for attempt in 1 2 3; do
        gh release upload "$apt_release" -R "$repo" --clobber "$@" && return 0
        log "uploading ${*##*/} failed (attempt $attempt of 3)"
        [ "$attempt" = 3 ] || sleep "$((attempt * ${GPUEUE_APT_RETRY_DELAY:-5}))"
    done
    return 1
}

# restore_index REPO PREV: after a failed upload of the index, puts back the
# index files kept in PREV (the ones published before) and deletes those
# that were not published before, so that the index stays consistent.
restore_index() {
    local repo="$1" prev="$2" name ok=1
    log "restoring the previous index"
    for name in "${index_files[@]}"; do
        if [ -f "$prev/$name" ]; then
            upload "$repo" "$prev/$name" || ok=0
        else
            gh release delete-asset "$apt_release" "$name" -R "$repo" --yes >/dev/null 2>&1 || true
        fi
    done
    if [ "$ok" = 1 ]; then
        log "restored the previous index"
    else
        log "error: cannot restore the previous index; apt clients may fail to verify the repository until a run succeeds"
    fi
}

cmd_github() {
    local tag="${1:?}" repo="${GITHUB_REPOSITORY:-kaito4681/gpueue}" state
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

    # The release's packages, checked like a user would check them. The
    # minisign key is the default branch's, never one read from the tag alone
    # (whoever can push a tag could otherwise ship their own key); the tag's
    # packaging/minisign.pub must be the same key, which also refuses a tag
    # made before a key rotation (re-run it after reviewing that). Only the
    # versioned gpueue_<version>_<arch>.deb files: the version-less copies
    # gpueue_<arch>.deb (for releases/latest/download) are the same packages.
    gh api -H "Accept: application/vnd.github.raw" \
        "repos/$repo/contents/packaging/minisign.pub" >"$work/minisign.pub" ||
        die "cannot read packaging/minisign.pub of the default branch"
    gh api -H "Accept: application/vnd.github.raw" \
        "repos/$repo/contents/packaging/minisign.pub?ref=$tag" >"$work/tag-minisign.pub" ||
        die "cannot read packaging/minisign.pub at $tag"
    [ -n "$(sed -n 2p "$work/minisign.pub")" ] || die "the default branch's packaging/minisign.pub is empty"
    [ "$(sed -n 2p "$work/minisign.pub")" = "$(sed -n 2p "$work/tag-minisign.pub")" ] ||
        die "packaging/minisign.pub at $tag differs from the default branch's; refusing to publish"
    gh release download "$tag" -R "$repo" -D "$work/new" \
        -p 'gpueue_*_*.deb' -p 'gpueue_*_*.deb.sha256' -p 'gpueue_*_*.deb.minisig'
    local minisign="${MINISIGN:-}" deb new=()
    [ -n "$minisign" ] || minisign="$("$root/scripts/dist/get-minisign.sh" "$work/minisign")"
    shopt -s nullglob
    for deb in "$work/new"/gpueue_*_*.deb; do
        (cd "$work/new" && sha256sum --quiet -c "$(basename "$deb").sha256") || die "$(basename "$deb"): checksum mismatch"
        "$minisign" -Vq -p "$work/minisign.pub" -m "$deb" || die "$(basename "$deb"): bad signature"
        new+=("$deb")
    done
    shopt -u nullglob
    [ "${#new[@]}" -gt 0 ] || die "release $tag has no .deb files"

    if ! gh release view "$apt_release" -R "$repo" >/dev/null 2>&1; then
        log "creating the release $apt_release"
        gh release create "$apt_release" -R "$repo" --latest=false \
            --title "APT repository" \
            --notes "The gpueue APT repository (a flat repository; its files are the assets of this release). It is updated by .github/workflows/release-apt.yml; see docs/install.md." >/dev/null
    fi
    # The current assets: the packages and the index files, all downloaded,
    # so that `update` verifies the packages against the signed index, and
    # the index files are kept to restore after a failed upload.
    local names before=() prev_index=() name file
    names="$(gh release view "$apt_release" -R "$repo" --json assets -q '.assets[].name')" ||
        die "cannot list the assets of the release $apt_release"
    while IFS= read -r name; do
        case "$name" in gpueue_*.deb) before+=("$name") ;; esac
        for file in "${index_files[@]}"; do
            if [ "$name" = "$file" ]; then prev_index+=("$name"); fi
        done
    done <<<"$names"
    local patterns=()
    [ "${#before[@]}" -eq 0 ] || patterns+=(-p 'gpueue_*.deb')
    for name in "${prev_index[@]}"; do patterns+=(-p "$name"); done
    if [ "${#patterns[@]}" -gt 0 ]; then
        gh release download "$apt_release" -R "$repo" -D "$work/repo" "${patterns[@]}" ||
            die "cannot download the release $apt_release"
    fi
    mkdir "$work/prev"
    for name in "${prev_index[@]}"; do cp "$work/repo/$name" "$work/prev/$name"; done

    cmd_update "$work/repo" "${new[@]}"

    # Uploaded: every package that is not a verified one of `before` (a
    # dropped unverified leftover of the same name is replaced). Deleted:
    # every package of `before` that is not in the new index.
    local -A in_after=() trusted=()
    local uploads=() removed=()
    for name in "${before[@]}"; do trusted[$name]=1; done
    for name in "${stale[@]}"; do unset "trusted[$name]"; done
    shopt -s nullglob
    for file in "$work/repo"/gpueue_*.deb; do
        name="$(basename "$file")"
        in_after[$name]=1
        [ -n "${trusted[$name]:-}" ] || uploads+=("$file")
    done
    shopt -u nullglob
    for name in "${before[@]}"; do
        [ -n "${in_after[$name]:-}" ] || removed+=("$name")
    done

    local recovery="rerun the workflow for $tag (Actions > APT repository > Run workflow, or \`gh workflow run release-apt.yml -f tag=$tag\`)"
    # gh uploads the files of one call in parallel: one call per step keeps
    # the order (packages, then the index, InRelease last).
    if [ "${#uploads[@]}" -gt 0 ]; then
        log "uploading ${uploads[*]##*/}"
        upload "$repo" "${uploads[@]}" ||
            die "uploading the packages failed; the index was not changed. To publish $tag, $recovery"
    fi
    for name in "${index_files[@]}"; do
        if ! upload "$repo" "$work/repo/$name"; then
            restore_index "$repo" "$work/prev"
            die "uploading $name failed. To publish $tag, $recovery"
        fi
    done
    for name in "${removed[@]}"; do
        log "deleting $name"
        gh release delete-asset "$apt_release" "$name" -R "$repo" --yes ||
            log "warning: cannot delete $name (no longer in the index); the next run deletes it"
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
