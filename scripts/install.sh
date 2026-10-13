#!/usr/bin/env bash
# Installs, re-installs (updates) or uninstalls puzuz from its GitHub releases.
#
#   curl -fsSL https://github.com/kaito4681/puzuz/releases/latest/download/install.sh | sudo bash
#   sudo bash install.sh [--version X] [--bin-dir DIR] [--save-token] [--token-file F]
#   sudo bash install.sh --uninstall [--purge] [--yes]
#
# Run `bash install.sh --help` for the options. The whole script is functions;
# only the last line runs anything, so a truncated download does nothing.
set -euo pipefail

REPO="kaito4681/puzuz"
# Set by check_arch: x86_64-unknown-linux-gnu or aarch64-unknown-linux-gnu.
TARGET=""
MIN_GLIBC="2.34"
DEFAULT_BIN_DIR="/usr/local/bin"
TOKEN_FILE_SAVED="/etc/puzuz/github-token"
INSTALL_MARKER='# Installed by `puzuz service install`; re-running it overwrites this file.'
# The release signing key; must match packaging/minisign.pub (checked by the
# release workflow and a unit test).
MINISIGN_PUBKEY="RWTK4z+6DPjoIfzjkZTIwZtbHzgq04ifrQnzaIhduY425VWh0CcyUKsv"

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
  --bin-dir DIR      where to put puzuz and puzuz-helper (default: the directory
                     of the current installation, else $DEFAULT_BIN_DIR)
  --token-file FILE  read the GitHub token from FILE
  --save-token       save the token to $TOKEN_FILE_SAVED (root, 0600) for
                     \`puzuz update\`
  --uninstall        stop and remove the services and the binaries; keeps
                     /etc/puzuz and /var/lib/puzuz (state, logs, checkpoints)
  --purge            with --uninstall: also delete /etc/puzuz and /var/lib/puzuz
  -y, --yes          do not ask before purging
  -h, --help         show this help

Releases are downloaded through the GitHub API. A GitHub token is optional (it
raises the API rate limit); one is taken from, in this order, GH_TOKEN or
GITHUB_TOKEN, --token-file, \`gh auth token\` of the user who ran sudo, or
$TOKEN_FILE_SAVED. Any token works; a fine-grained one needs no permissions.
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
    BIN_DIR_GIVEN=0
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
            BIN_DIR_GIVEN=1
            shift 2
            ;;
        --bin-dir=*)
            BIN_DIR="${1#--bin-dir=}"
            BIN_DIR_GIVEN=1
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

# Same mapping as dist_arch in scripts/dist/lib.sh (this script is downloaded
# alone, so it keeps its own copy).
check_arch() {
    local arch
    arch="$(uname -m)"
    case "$arch" in
    x86_64) TARGET="x86_64-unknown-linux-gnu" ;;
    aarch64 | arm64) TARGET="aarch64-unknown-linux-gnu" ;;
    *) die "releases exist for x86_64 and aarch64 only (this machine is $arch); build puzuz from source (see docs/install.md)" ;;
    esac
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

# os_release FIELD: a field of os-release(5) (/etc/os-release, else
# /usr/lib/os-release), or nothing.
os_release() {
    local file
    for file in /etc/os-release /usr/lib/os-release; do
        if [ -r "$file" ]; then
            # shellcheck disable=SC1090
            (. "$file" && eval "printf '%s\n' \"\${$1:-}\"")
            return
        fi
    done
}

# The distribution family (debian, rhel, arch, suse) from ID and ID_LIKE, or
# nothing. Used for wording only, never to decide how puzuz is installed.
os_family() {
    local id
    for id in $(os_release ID) $(os_release ID_LIKE); do
        case "$id" in
        debian | ubuntu) echo debian && return ;;
        fedora | rhel | centos) echo rhel && return ;;
        arch) echo arch && return ;;
        suse | opensuse*) echo suse && return ;;
        esac
    done
}

# unit_puzuz UNIT: the puzuz binary that UNIT's ExecStart= runs (after any
# of systemd's -@:+! prefixes), or nothing.
unit_puzuz() {
    sed -n 's|^ExecStart=[-@:+!]*\(/[^ ]*/puzuz\) daemon.*|\1|p' "$1" 2>/dev/null | head -n1
}

# The directory of the binaries the `service install` unit runs, or nothing.
installed_bin_dir() {
    local exe
    exe="$(unit_puzuz /etc/systemd/system/puzuz.service)"
    [ -z "$exe" ] || dirname "$exe"
}

# The `puzuz` of an existing installation: the one the daemon unit runs, else
# the one in PATH; nothing if there is none.
installed_puzuz() {
    local unit exe
    for unit in /etc/systemd/system/puzuz.service /usr/lib/systemd/system/puzuz.service /lib/systemd/system/puzuz.service; do
        exe="$(unit_puzuz "$unit")"
        if [ -n "$exe" ] && [ -x "$exe" ]; then
            echo "$exe"
            return
        fi
    done
    command -v puzuz 2>/dev/null || true
}

# Whether /etc/systemd/system/puzuz.service was written by `puzuz service install`.
marker_unit_installed() {
    [ "$(head -n1 /etc/systemd/system/puzuz.service 2>/dev/null)" = "$INSTALL_MARKER" ]
}

# installed_method: how an installed puzuz says it is installed (`puzuz
# service install-method`), or nothing if no puzuz can tell (none installed,
# or one too old to know the command, or failing).
installed_method() {
    local puzuz
    puzuz="$(installed_puzuz)"
    [ -n "$puzuz" ] || return 0
    "$puzuz" service install-method 2>/dev/null || true
}

# package_owner FILE: prints "NAME MANAGER" for the package that owns FILE
# (asking dpkg-query, rpm and pacman, under /usr/lib and /lib), or fails.
package_owner() {
    local file="$1" paths=("$1") path out
    case "$file" in
    /usr/lib/*) paths+=("/lib/${file#/usr/lib/}") ;;
    /lib/*) paths+=("/usr/lib/${file#/lib/}") ;;
    esac
    for path in "${paths[@]}"; do
        if command -v dpkg-query >/dev/null 2>&1 && out="$(dpkg-query -S "$path" 2>/dev/null)"; then
            # Diversion lines (`diversion by ...`, `local diversion ...`) name no owner.
            out="$(grep -v 'diversion' <<<"$out" | grep ': ' | head -n1)" || true
            if [ -n "$out" ]; then
                out="${out%%: *}"
                out="${out%%, *}"
                echo "${out%%:*} dpkg"
                return 0
            fi
        fi
        if command -v rpm >/dev/null 2>&1 && out="$(rpm -qf --queryformat '%{NAME}\n' "$path" 2>/dev/null)"; then
            echo "$(head -n1 <<<"$out") rpm"
            return 0
        fi
        if command -v pacman >/dev/null 2>&1 && out="$(pacman -Qqo "$path" 2>/dev/null)"; then
            echo "$(head -n1 <<<"$out") pacman"
            return 0
        fi
    done
    return 1
}

# package_hint NAME MANAGER: how to update or remove that package.
package_hint() {
    case "$2" in
    dpkg) echo "update it with \`sudo apt update && sudo apt upgrade\`, or remove it with \`sudo apt remove $1\`" ;;
    rpm) echo "update it with \`sudo dnf upgrade\`, or remove it with \`sudo dnf remove $1\`" ;;
    pacman) echo "update it with \`yay -Syu\` (AUR) or \`sudo pacman -Syu\`, or remove it with \`sudo pacman -R $1\`" ;;
    esac
}

nix_message() {
    if [ "$(os_release ID)" = nixos ]; then
        echo "this is NixOS: install puzuz with the \`services.puzuz\` NixOS module instead"
    else
        echo "the puzuz units come from /nix/store: change the Nix configuration that created them instead"
    fi
}

# Without an installed puzuz to ask: refuses only what is clearly not ours to
# change (NixOS, units from /nix/store, units a package owns). The new binary
# decides the rest after the download.
check_obvious_conflicts() {
    local unit file owner
    [ "$(os_release ID)" != nixos ] || die "$(nix_message)"
    case "$(readlink -m /etc/systemd/system)" in
    /nix/store/*) die "/etc/systemd/system resolves into /nix/store (managed by Nix); install puzuz through that Nix configuration instead" ;;
    esac
    for unit in puzuz.service puzuz-helper.service; do
        if [ -L "/etc/systemd/system/$unit" ]; then
            case "$(readlink -m "/etc/systemd/system/$unit")" in
            /nix/store/*) die "$(nix_message)" ;;
            esac
        fi
        for file in "/usr/lib/systemd/system/$unit" "/lib/systemd/system/$unit"; do
            [ -e "$file" ] || continue
            if owner="$(package_owner "$file")"; then
                # shellcheck disable=SC2086
                die "puzuz is installed by the distribution package ${owner% *} (${owner#* }: $file); $(package_hint $owner)"
            fi
        done
    done
}

# Packaged-looking units no package owns.
unowned_units() {
    local file found=()
    for file in /usr/lib/systemd/system/puzuz.service /usr/lib/systemd/system/puzuz-helper.service \
        /lib/systemd/system/puzuz.service /lib/systemd/system/puzuz-helper.service; do
        # On merged-/usr systems /lib is /usr/lib.
        case "$file" in /lib/*) [ /lib -ef /usr/lib ] && continue ;; esac
        [ -e "$file" ] && found+=("$file")
    done
    echo "${found[*]}"
}

# check_install_method METHOD: refuses everything but `service install` or nothing.
check_install_method() {
    local family
    case "$1" in
    none | service-install) ;;
    package)
        family="$(os_family)"
        case "$family" in
        debian) die "puzuz is installed by a distribution package; update it with \`sudo apt update && sudo apt upgrade\`, or remove it with apt, instead (\`puzuz doctor\` names the package)" ;;
        *) die "puzuz is installed by a distribution package; update or remove it with your package manager instead (\`puzuz doctor\` names the package)" ;;
        esac
        ;;
    unowned) die "no package owns $(unowned_units) (left over from a removed package, or copied there by hand); delete it and re-run this script" ;;
    # nixos: what `install-method` printed before it said nix.
    nix | nixos) die "$(nix_message)" ;;
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

# GitHub answers 401 for a bad token, 403 when the API rate limit is used up,
# and 404 for a release or repository that does not exist.
no_access() {
    local what="$1" code="$2"
    if [ "$HAVE_TOKEN" = 1 ]; then
        die "$what: HTTP $code with the GitHub token: it may be expired or revoked, or $REPO has no published release. A token is optional; unset GH_TOKEN/GITHUB_TOKEN (and --token-file, $TOKEN_FILE_SAVED) to download anonymously"
    fi
    if [ "$code" = 404 ]; then
        die "$what: HTTP 404. $REPO has no published release (or does not exist)"
    fi
    die "$what: HTTP $code. GitHub refused the anonymous request, usually because of its API rate limit; try again later, or use a GitHub token: set GH_TOKEN (\`sudo GH_TOKEN=... bash install.sh\`), pass --token-file, or run \`gh auth login\` as the user that runs sudo"
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
        if [ "$code" = 404 ] && [ -n "$VERSION" ] && [[ "$path" == */releases/tags/* ]]; then
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
    local hints=()
    if ! have_program criu; then
        case "$(os_family)" in
        debian) hints+=("criu (checkpointing): \`sudo apt install criu\`; puzuz needs CRIU 4.2 or newer, so on older releases build it from source (see docs/install.md, \"CRIU\")") ;;
        rhel) hints+=("criu (checkpointing): \`sudo dnf install criu\` (4.2 or newer)") ;;
        arch) hints+=("criu (checkpointing): \`sudo pacman -S criu\`") ;;
        suse) hints+=("criu (checkpointing): \`sudo zypper install criu\` (4.2 or newer)") ;;
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
    local method
    # An installed puzuz knows best; without one, only the obvious conflicts
    # are refused here, and the downloaded binary decides below.
    method="$(installed_method)"
    if [ -z "$method" ]; then
        check_obvious_conflicts
        # A `service install` whose binary cannot answer: keep its bin dir.
        if marker_unit_installed; then
            method=service-install
        else
            method=none
        fi
    fi
    check_install_method "$method"
    # An update stays where the current installation is, unless --bin-dir says otherwise.
    if [ "$BIN_DIR_GIVEN" = 0 ] && [ "$method" = service-install ]; then
        local current
        current="$(installed_bin_dir)"
        [ -z "$current" ] || BIN_DIR="$current"
    fi

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
    method="$(installed_method)"
    if [ -z "$method" ] && marker_unit_installed; then
        method=service-install
    fi
    case "$method" in
    service-install) ;;
    "" | none) die "puzuz is not installed by \`puzuz service install\`; nothing to uninstall" ;;
    *) check_install_method "$method" ;;
    esac
    bin_dir="$(installed_bin_dir)"
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
