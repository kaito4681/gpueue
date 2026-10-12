#!/usr/bin/env bash
# Installs, re-installs (updates) or uninstalls puzuz from its GitHub releases.
#
#   gh release download -R kaito4681/puzuz -p install.sh -O - | sudo bash
#   sudo bash install.sh [--version X] [--bin-dir DIR] [--save-token] [--token-file F]
#   sudo bash install.sh --uninstall [--purge] [--yes]
#
# Run `bash install.sh --help` for the options. The whole script is functions;
# only the last line runs anything, so a truncated download does nothing.
set -euo pipefail

REPO="kaito4681/puzuz"
TARGET="x86_64-unknown-linux-gnu"
MIN_GLIBC="2.34"
DEFAULT_BIN_DIR="/usr/local/bin"
TOKEN_FILE_SAVED="/etc/puzuz/github-token"
INSTALL_MARKER='# Installed by `puzuz service install`; re-running it overwrites this file.'
# The release signing key; must match packaging/minisign.pub (checked by the
# release workflow and a unit test).
MINISIGN_PUBKEY="RWQI3XV1shOR22n9kPlB9C81e0sx6XdiE3jTLTgBXVhx+M8c5Vq1r2Sc"

usage() {
    cat <<EOF
Install puzuz (the daemon, the root helper and the CLI) from its GitHub releases,
or uninstall it. Must run as root.

Usage:
  install.sh [--version X] [--bin-dir DIR] [--save-token] [--token-file FILE]
  install.sh --uninstall [--purge] [--yes]

Options:
  --version X        install version X (default: the latest release); re-running
                     with a newer version updates puzuz, running jobs keep running
  --bin-dir DIR      where to put puzuz and puzuz-helper (default: $DEFAULT_BIN_DIR)
  --token-file FILE  read the GitHub token from FILE
  --save-token       save the token to $TOKEN_FILE_SAVED (root, 0600) for
                     \`puzuz update\`
  --uninstall        stop and remove the services and the binaries; keeps
                     /etc/puzuz and /var/lib/puzuz (state, logs, checkpoints)
  --purge            with --uninstall: also delete /etc/puzuz and /var/lib/puzuz
  -y, --yes          do not ask before purging
  -h, --help         show this help

The repository is private: downloads need a GitHub token that can read it,
taken from (in this order) GH_TOKEN or GITHUB_TOKEN, --token-file, \`gh auth token\`
of the user who ran sudo, or $TOKEN_FILE_SAVED. A fine-grained personal access
token with read-only "Contents" access to $REPO is enough.
EOF
}

info() { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

# --- options ---------------------------------------------------------------

parse_args() {
    VERSION=""
    BIN_DIR="$DEFAULT_BIN_DIR"
    UNINSTALL=0
    PURGE=0
    YES=0
    SAVE_TOKEN=0
    TOKEN_FILE=""
    while [ "$#" -gt 0 ]; do
        case "$1" in
        --version)
            [ "$#" -ge 2 ] || die "--version needs a value"
            VERSION="${2#v}"
            shift 2
            ;;
        --version=*)
            VERSION="${1#--version=}"
            VERSION="${VERSION#v}"
            shift
            ;;
        --bin-dir)
            [ "$#" -ge 2 ] || die "--bin-dir needs a value"
            BIN_DIR="$2"
            shift 2
            ;;
        --bin-dir=*)
            BIN_DIR="${1#--bin-dir=}"
            shift
            ;;
        --token-file)
            [ "$#" -ge 2 ] || die "--token-file needs a value"
            TOKEN_FILE="$2"
            shift 2
            ;;
        --token-file=*)
            TOKEN_FILE="${1#--token-file=}"
            shift
            ;;
        --save-token)
            SAVE_TOKEN=1
            shift
            ;;
        --uninstall)
            UNINSTALL=1
            shift
            ;;
        --purge)
            PURGE=1
            shift
            ;;
        -y | --yes)
            YES=1
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *) die "unknown option: $1 (see --help)" ;;
        esac
    done
    case "$BIN_DIR" in
    /*) ;;
    *) die "--bin-dir must be an absolute path" ;;
    esac
    if [ "$PURGE" = 1 ] && [ "$UNINSTALL" = 0 ]; then
        die "--purge only goes with --uninstall"
    fi
    if [ -n "$VERSION" ] && ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.+-][0-9A-Za-z.+-]+)?$ ]]; then
        die "--version $VERSION is not a version like 0.2.0"
    fi
}

# --- checks ----------------------------------------------------------------

check_root() {
    [ "$(id -u)" = 0 ] || die "this must run as root (pipe it to \`sudo bash\`, or run \`sudo bash install.sh\`)"
}

check_systemd() {
    [ -d /run/systemd/system ] || die "systemd is not the init system (PID 1); puzuz runs as systemd services"
}

check_arch() {
    local arch
    arch="$(uname -m)"
    [ "$arch" = x86_64 ] || die "only x86_64 is supported for now (this machine is $arch)"
}

# version_ge A B: whether version A >= version B (dotted numbers).
version_ge() {
    [ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -n1)" = "$2" ]
}

check_glibc() {
    local version
    version="$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}')" || true
    [ -n "$version" ] || die "cannot determine the glibc version (\`getconf GNU_LIBC_VERSION\`); puzuz needs glibc $MIN_GLIBC or newer"
    version_ge "$version" "$MIN_GLIBC" || die "glibc $version is too old; puzuz needs $MIN_GLIBC or newer (e.g. Ubuntu 22.04, Debian 12, RHEL 9)"
}

# The install method as `puzuz service install-method` reports it, decided
# from the same files, for before anything is downloaded.
install_method() {
    local methods=() etc=/etc/systemd/system/puzuz.service
    [ -e /etc/NIXOS ] && methods+=(nixos)
    if [ -f /usr/lib/systemd/system/puzuz.service ] || [ -f /lib/systemd/system/puzuz.service ]; then
        if grep -q '^ExecStart=/nix/store/' /usr/lib/systemd/system/puzuz.service /lib/systemd/system/puzuz.service 2>/dev/null; then
            methods+=(nixos)
        else
            methods+=(package)
        fi
    fi
    if [ -f "$etc" ]; then
        if [ "$(head -n1 "$etc")" = "$INSTALL_MARKER" ]; then
            methods+=(service-install)
        elif grep -q '^ExecStart=/nix/store/' "$etc"; then
            methods+=(nixos)
        else
            methods+=(manual)
        fi
    fi
    mapfile -t methods < <(printf '%s\n' "${methods[@]}" | awk 'NF && !seen[$0]++')
    case "${#methods[@]}" in
    0) echo none ;;
    1) echo "${methods[0]}" ;;
    *) echo mixed ;;
    esac
}

# check_install_method METHOD: refuses everything but `service install` or nothing.
check_install_method() {
    case "$1" in
    none | service-install) ;;
    package) die "puzuz is installed by a distribution package (/usr/lib/systemd/system/puzuz.service); update or remove it with your package manager instead" ;;
    nixos) die "this is NixOS (or puzuz comes from /nix/store); use the NixOS module instead" ;;
    manual) die "/etc/systemd/system/puzuz.service was not written by \`puzuz service install\`; remove it (or run \`sudo puzuz service install --force\` once) and re-run this script" ;;
    mixed) die "puzuz is installed in more than one way; run \`puzuz doctor\` and remove all but one first" ;;
    *) die "unknown install method: $1" ;;
    esac
}

require_tools() {
    local tool missing=()
    for tool in curl tar sha256sum awk sed sort mktemp getconf; do
        command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
    done
    [ "${#missing[@]}" -eq 0 ] || die "missing tools: ${missing[*]} (e.g. \`sudo apt install curl\`)"
}

# --- GitHub access ---------------------------------------------------------

# Prints the token to use, or nothing for unauthenticated access. Never logs it.
resolve_token() {
    if [ -n "${GH_TOKEN:-}" ]; then
        printf '%s' "$GH_TOKEN"
        return
    fi
    if [ -n "${GITHUB_TOKEN:-}" ]; then
        printf '%s' "$GITHUB_TOKEN"
        return
    fi
    if [ -n "$TOKEN_FILE" ]; then
        [ -r "$TOKEN_FILE" ] || die "cannot read --token-file $TOKEN_FILE"
        head -n1 "$TOKEN_FILE" | tr -d '[:space:]'
        return
    fi
    if [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != root ] && command -v gh >/dev/null 2>&1; then
        local token
        if token="$(sudo -H -u "$SUDO_USER" gh auth token 2>/dev/null)" && [ -n "$token" ]; then
            printf '%s' "$token"
            return
        fi
    fi
    if [ -f "$TOKEN_FILE_SAVED" ]; then
        head -n1 "$TOKEN_FILE_SAVED" | tr -d '[:space:]'
    fi
}

# Writes the request headers to $WORK/headers (mode 0600). The token goes to
# curl through this file (`-H @file`), never on its command line.
write_headers() {
    local token="$1"
    (
        umask 077
        {
            printf 'X-GitHub-Api-Version: 2022-11-28\n'
            printf 'User-Agent: puzuz-install\n'
            if [ -n "$token" ]; then
                printf 'Authorization: Bearer %s\n' "$token"
            fi
        } >"$WORK/headers"
    )
    HAVE_TOKEN=0
    [ -n "$token" ] && HAVE_TOKEN=1
    return 0
}

no_access() {
    local what="$1" code="$2"
    if [ "$HAVE_TOKEN" = 1 ]; then
        die "$what: HTTP $code. The token cannot read $REPO (expired, revoked, or without \"Contents: Read-only\" access to it)"
    fi
    die "$what: HTTP $code. A GitHub token that can read $REPO is needed: set GH_TOKEN (\`sudo GH_TOKEN=... bash install.sh\`), pass --token-file, or run \`gh auth login\` as the user that runs sudo"
}

# api_get PATH OUT ACCEPT: GET $API_URL$PATH into OUT.
api_get() {
    local path="$1" out="$2" accept="$3" code
    code="$(curl -sS -L --retry 3 -o "$out" -w '%{http_code}' \
        -H @"$WORK/headers" -H "Accept: $accept" "$API_URL$path")" ||
        die "cannot reach $API_URL (curl failed)"
    case "$code" in
    200) ;;
    401 | 403 | 404)
        if [ "$code" = 404 ] && [ -n "$VERSION" ] && [ "$HAVE_TOKEN" = 1 ] && [[ "$path" == */releases/tags/* ]]; then
            die "release v$VERSION not found in $REPO"
        fi
        no_access "GET $path" "$code"
        ;;
    *) die "GET $path: HTTP $code" ;;
    esac
}

# json_tag FILE: the release's tag_name.
json_tag() {
    tr ',{}' '\n\n\n' <"$1" | sed -n 's/^[[:space:]]*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1
}

# json_asset_id FILE NAME: the id of the release asset called NAME. Each asset
# object has its API "url" (…/releases/assets/<id>) before its
# "browser_download_url" (…/<NAME>).
json_asset_id() {
    tr ',{}' '\n\n\n' <"$1" | awk -v want="$2" '
        /"url"[[:space:]]*:[[:space:]]*"[^"]*\/releases\/assets\/[0-9]+"/ {
            id = $0
            sub(/.*\/releases\/assets\//, "", id)
            sub(/".*/, "", id)
        }
        /"browser_download_url"[[:space:]]*:/ {
            url = $0
            sub(/"[[:space:]]*$/, "", url)
            n = length(want)
            if (substr(url, length(url) - n) == "/" want) { print id; exit }
        }'
}

# download_asset RELEASE_JSON NAME: downloads the asset NAME into $WORK.
download_asset() {
    local release="$1" name="$2" id
    id="$(json_asset_id "$release" "$name")"
    [ -n "$id" ] || die "the release has no $name"
    api_get "/repos/$REPO/releases/assets/$id" "$WORK/$name" application/octet-stream
}

# --- install ---------------------------------------------------------------

fetch_release() {
    local path
    if [ -n "$VERSION" ]; then
        path="/repos/$REPO/releases/tags/v$VERSION"
    else
        path="/repos/$REPO/releases/latest"
    fi
    api_get "$path" "$WORK/release.json" application/vnd.github+json
    TAG="$(json_tag "$WORK/release.json")"
    [ -n "$TAG" ] || die "cannot read the release's tag from $API_URL$path"
}

verify() {
    local tarball="$1"
    (cd "$WORK" && sha256sum -c --quiet "$tarball.sha256") ||
        die "SHA-256 checksum mismatch for $tarball; not installing it"
    info "verified the SHA-256 checksum"
    if command -v minisign >/dev/null 2>&1; then
        local key=(-P "$MINISIGN_PUBKEY")
        # Test-only: another public key file.
        [ -n "${PUZUZ_UPDATE_PUBKEY_FILE:-}" ] && key=(-p "$PUZUZ_UPDATE_PUBKEY_FILE")
        minisign -V -q "${key[@]}" -m "$WORK/$tarball" -x "$WORK/$tarball.minisig" >/dev/null ||
            die "bad signature on $tarball; not installing it"
        info "verified the minisign signature"
    else
        info "minisign is not installed: the signature was not checked (\`puzuz update\` always checks it)"
    fi
}

# Safe extraction: regular files and directories only, below one top directory.
extract() {
    local tarball="$1" top="$2" entry
    while IFS= read -r entry; do
        case "$entry" in
        "$top" | "$top/" | "$top"/*) ;;
        *) die "unexpected path in $tarball: $entry" ;;
        esac
        case "/$entry/" in
        */../*) die "unexpected path in $tarball: $entry" ;;
        esac
    done < <(tar -tzf "$WORK/$tarball")
    if tar -tvzf "$WORK/$tarball" | grep -qv '^[-d]'; then
        die "$tarball contains links or special files"
    fi
    tar -C "$WORK" --no-same-owner -xzf "$WORK/$tarball"
}

save_token() {
    local token="$1"
    if [ -z "$token" ]; then
        warn "--save-token: no token to save"
        return
    fi
    install -d -m 0755 /etc/puzuz
    (
        umask 077
        printf '%s\n' "$token" >"$TOKEN_FILE_SAVED.tmp"
    )
    chown root:root "$TOKEN_FILE_SAVED.tmp"
    chmod 0600 "$TOKEN_FILE_SAVED.tmp"
    mv -f "$TOKEN_FILE_SAVED.tmp" "$TOKEN_FILE_SAVED"
    info "saved the token to $TOKEN_FILE_SAVED (root only) for \`puzuz update\`"
}

# have_program NAME: whether NAME is in systemd's default PATH for services
# (what the helper sees).
have_program() {
    local dir
    for dir in /usr/local/sbin /usr/local/bin /usr/sbin /usr/bin /sbin /bin; do
        [ -x "$dir/$1" ] && return 0
    done
    return 1
}

# Prints how to install what puzuz needs but this script does not install.
print_missing_deps() {
    local id="" hints=()
    # shellcheck disable=SC1091
    [ -r /etc/os-release ] && id="$(. /etc/os-release && echo "${ID:-}")"
    if ! have_program criu; then
        case "$id" in
        ubuntu | debian) hints+=("criu (checkpointing): \`sudo apt install criu\`; puzuz needs CRIU 4.2 or newer, so on older releases build it from source (see docs/install.md, \"CRIU\")") ;;
        fedora | rhel | rocky | almalinux | centos) hints+=("criu (checkpointing): \`sudo dnf install criu\` (4.2 or newer)") ;;
        arch) hints+=("criu (checkpointing): \`sudo pacman -S criu\`") ;;
        *) hints+=("criu 4.2 or newer (checkpointing), from your distribution or https://criu.org") ;;
        esac
    fi
    if [ -e /proc/driver/nvidia/version ] && ! have_program cuda-checkpoint; then
        hints+=("cuda-checkpoint (GPU jobs): put the binary matching your driver from https://github.com/NVIDIA/cuda-checkpoint into /usr/local/bin")
    fi
    if [ "${#hints[@]}" -gt 0 ]; then
        info ""
        info "Not installed by this script; install them yourself:"
        printf '  - %s\n' "${hints[@]}"
    fi
}

do_install() {
    require_tools
    check_systemd
    check_arch
    check_glibc
    check_install_method "$(install_method)"

    local token
    token="$(resolve_token)"
    write_headers "$token"
    fetch_release
    local version="${TAG#v}"
    local name="puzuz-$TAG-$TARGET"
    local tarball="$name.tar.gz"
    info "installing puzuz $version from $REPO"
    download_asset "$WORK/release.json" "$tarball"
    download_asset "$WORK/release.json" "$tarball.sha256"
    if command -v minisign >/dev/null 2>&1; then
        download_asset "$WORK/release.json" "$tarball.minisig"
    fi
    verify "$tarball"
    extract "$tarball" "$name"

    local new="$WORK/$name/puzuz"
    [ -x "$new" ] && [ -x "$WORK/$name/puzuz-helper" ] || die "$tarball lacks puzuz or puzuz-helper"
    # The new binary decides with its own detection, too.
    check_install_method "$("$new" service install-method)"

    [ "$SAVE_TOKEN" = 1 ] && save_token "$token"

    local status=0
    "$new" service install --bin-dir "$BIN_DIR" || status=$?
    if [ "$status" -ne 0 ]; then
        if "$BIN_DIR/puzuz" service status >/dev/null 2>&1; then
            warn "puzuz $version is installed and running, but some checks above failed; fix them and re-run \`puzuz doctor\`"
        else
            print_missing_deps
            die "\`puzuz service install\` failed (see above)"
        fi
    fi
    print_missing_deps
    info ""
    info "puzuz $version is installed in $BIN_DIR."
}

# --- uninstall -------------------------------------------------------------

do_uninstall() {
    local method puzuz bin_dir=""
    method="$(install_method)"
    case "$method" in
    service-install) ;;
    none) die "puzuz is not installed by \`puzuz service install\`; nothing to uninstall" ;;
    *) check_install_method "$method" ;;
    esac
    bin_dir="$(sed -n 's|^ExecStart=\(/.*\)/puzuz daemon.*|\1|p' /etc/systemd/system/puzuz.service | head -n1)"
    [ -n "$bin_dir" ] || bin_dir="$BIN_DIR"
    puzuz="$bin_dir/puzuz"
    [ -x "$puzuz" ] || die "$puzuz not found"

    local args=(service uninstall)
    if [ "$PURGE" = 1 ]; then
        args+=(--purge)
        if [ "$YES" = 1 ]; then
            args+=(--yes)
        elif ! (exec </dev/tty) 2>/dev/null; then
            die "--purge asks for confirmation on the terminal; there is none, so pass --yes"
        fi
    fi
    # stdin may be this script (curl | sudo bash): ask on the terminal.
    if [ "$PURGE" = 1 ] && [ "$YES" = 0 ]; then
        "$puzuz" "${args[@]}" </dev/tty
    else
        "$puzuz" "${args[@]}" </dev/null
    fi
    local file
    for file in "$bin_dir/puzuz" "$bin_dir/puzuz-helper"; do
        if [ -e "$file" ]; then
            rm -f "$file"
            info "removed $file"
        fi
    done
}

main() {
    parse_args "$@"
    # Test-only override of the GitHub API (a fake release server).
    API_URL="${PUZUZ_RELEASE_API_URL:-https://api.github.com}"
    check_root
    WORK="$(mktemp -d)"
    trap 'rm -rf "$WORK"' EXIT
    if [ "$UNINSTALL" = 1 ]; then
        do_uninstall
    else
        do_install
    fi
}

main "$@"
