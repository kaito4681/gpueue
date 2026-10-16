#!/usr/bin/env bash
# Real-OS test: gpueue installed as users install it, in QEMU/KVM virtual
# machines booted from distribution cloud images, which are rebooted and
# powered off for real, with CPU-only jobs. Each VM runs inside an
# unprivileged Docker container that only gets --device /dev/kvm (and no
# AppArmor profile, see below); nothing runs privileged on the host.
#
# Scenarios (see scripts/vm/guest.sh), each as [DISTRO/]NAME (DISTRO defaults
# to ubuntu-26.04):
#   basic            the package installs and its services run; a job runs,
#                    is paused and resumed; checkpoint/restore if CRIU allows
#   reboot           counters (alice, bob) and a --no-checkpoint job,
#                    `systemctl reboot`; the stop order in the journal, the
#                    restore after the boot
#   poweroff         the same with `systemctl poweroff` and a fresh boot
#   inflight         `systemctl reboot` while `gpueue checkpoint` of a 1 GiB
#                    job runs
#   desktop          a workstation: systemd-resolved, /tmp as a tmpfs, alice's
#                    FUSE mounts without allow_other (sshfs under /tmp, like an
#                    AppImage, and in her home); checkpoint/restore, a paused
#                    job, a job on the FUSE mount (refused), a reboot with
#                    checkpointed, running and paused jobs, restores after it
#   upgrade          the previous release's package from GitHub (checksum and
#                    minisign signature checked), jobs checkpointed and
#                    running, the upgrade to the tree's package, then restores
#   service-install  `reboot`, with gpueue installed by `gpueue service
#                    install` (what install.sh runs) instead of the package
# The other scenarios install the tree's package with the package manager
# (`apt install ./gpueue_<version>_amd64.deb`, `dnf install ./gpueue-….rpm`).
# Distributions (scripts/vm/distros.sh): ubuntu-26.04, ubuntu-24.04,
# debian-13, debian-12 (.deb), fedora-44 (.rpm). Where the distribution has
# no CRIU 4.2 or newer, every scenario but basic prints SKIP.
#
# Scenarios of one distribution and install method share a VM, in the order
# given; each such group boots a fresh copy of the prepared image.
#
# Sets: quick (the default; CI and the release workflow run it):
#   inflight desktop upgrade service-install
# all: every scenario on ubuntu-26.04, and basic and desktop (plus inflight
# and upgrade on Fedora) on the other distributions.
#
# The prepared VM images (~1 GB each) and the ssh key are cached in the
# Docker volume gpueue-vm-cache (or a directory, see GPUEUE_VM_CACHE); each
# run uses throw-away overlays inside the container. Remove the cache with
# `docker volume rm gpueue-vm-cache`.
#
# Usage: scripts/vm-reboot-test.sh [quick|all|SCENARIO...]
#        scripts/vm-reboot-test.sh prepare-only [DISTRO...]  (only build the cached images)
#        scripts/vm-reboot-test.sh list                      (the scenarios of each set)
# Environment:
#   GPUEUE_VM_SKIP_BUILD=1   reuse target/docker/release binaries
#   GPUEUE_VM_DEB, GPUEUE_VM_RPM
#                            test this .deb / .rpm instead of building one
#                            (the release workflow passes its own)
#   GPUEUE_VM_UPGRADE_FROM   the release the upgrade scenario starts from
#                            (default: the newest release older than the
#                            tree's version)
#   GPUEUE_VM_REPO           the GitHub repository of the releases
#                            (default kaito4681/gpueue); GH_TOKEN or
#                            GITHUB_TOKEN, when set, is used for its API
#                            (else an authenticated `gh`, else none)
#   GPUEUE_VM_CACHE          cache volume, or an absolute directory
#                            (default gpueue-vm-cache)
#   GPUEUE_VM_KEEP=1         on a failure, keep the VM running for debugging
#                            (`docker exec -it gpueue-vm-<pid>-<n> ssh -F /work/ssh_config vm`;
#                            `docker rm -f` the container when done)
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/vm/distros.sh
. "$root/scripts/vm/distros.sh"
# shellcheck source=scripts/dist/lib.sh
. "$root/scripts/dist/lib.sh"
image="${GPUEUE_VM_RUNNER_IMAGE:-gpueue-vm-runner:test}"
cache="${GPUEUE_VM_CACHE:-gpueue-vm-cache}"
repo="${GPUEUE_VM_REPO:-kaito4681/gpueue}"
default_distro=ubuntu-26.04

QUICK="inflight desktop upgrade service-install"
ALL="basic reboot poweroff inflight desktop upgrade service-install
ubuntu-24.04/basic ubuntu-24.04/desktop
debian-13/basic debian-13/desktop
debian-12/basic debian-12/desktop
fedora-44/basic fedora-44/inflight fedora-44/desktop fedora-44/upgrade"

# install_of <scenario>: how gpueue is installed for it.
install_of() {
    case "$1" in
    basic | reboot | poweroff | inflight | desktop) echo package ;;
    upgrade) echo upgrade ;;
    service-install) echo service ;;
    *) return 1 ;;
    esac
}

prepare_only=
specs=()
case "${1:-quick}" in
list)
    echo "quick: $QUICK"
    echo "all:   $(tr '\n' ' ' <<<"$ALL")"
    echo "distributions: $VM_DISTROS"
    exit 0
    ;;
prepare-only)
    prepare_only=1
    shift
    read -r -a specs <<<"${*:-$default_distro}"
    for d in "${specs[@]}"; do distro_info "$d"; done
    ;;
*)
    for arg in "${@:-quick}"; do
        case "$arg" in
        quick) read -r -a more <<<"$QUICK" ;;
        all) read -r -a more <<<"$(tr '\n' ' ' <<<"$ALL")" ;;
        *) more=("$arg") ;;
        esac
        for spec in "${more[@]}"; do
            [[ "$spec" == */* ]] || spec="$default_distro/$spec"
            distro_info "${spec%%/*}"
            install_of "${spec#*/}" >/dev/null || { echo "unknown scenario: ${spec#*/}" >&2; exit 2; }
            specs+=("$spec")
        done
    done
    ;;
esac

if [ ! -r /dev/kvm ] || [ ! -w /dev/kvm ]; then
    echo "/dev/kvm is not accessible (KVM is required)" >&2
    exit 1
fi

stage="$(mktemp -d)"
logs="$stage/logs"
cleanup() { rm -rf "$stage"; }
trap cleanup EXIT
mkdir "$stage/ctx" "$stage/pkg" "$stage/e2e" "$logs"

# --- the packages ----------------------------------------------------------------

need_deb='' need_rpm='' need_upgrade=''
if [ -z "$prepare_only" ]; then
    for spec in "${specs[@]}"; do
        distro_info "${spec%%/*}"
        case "$DISTRO_PKG" in deb) need_deb=1 ;; rpm) need_rpm=1 ;; esac
        [ "${spec#*/}" != upgrade ] || need_upgrade=1
    done
fi
version="$(workspace_version "$root")"
dist_arch
[ "$DIST_DEB_ARCH" = amd64 ] || { echo "the VM test runs x86_64 guests only" >&2; exit 1; }

# The previous release, for the upgrade scenario: GPUEUE_VM_UPGRADE_FROM, or
# the newest stable release older than the tree's version.
old_version=
if [ -n "$need_upgrade" ]; then
    old_version="${GPUEUE_VM_UPGRADE_FROM:-}"
    old_version="${old_version#v}"
    if [ -z "$old_version" ]; then
        auth=()
        token="${GH_TOKEN:-${GITHUB_TOKEN:-}}"
        [ -z "$token" ] || auth=(-H "Authorization: Bearer $token")
        # Anonymous API requests are rate limited: without a token, an
        # authenticated gh is used when there is one.
        if [ -z "$token" ] && command -v gh >/dev/null && gh auth status >/dev/null 2>&1; then
            releases="$(gh api "repos/$repo/releases?per_page=100")"
        else
            releases="$(curl -fsSL --retry 3 "${auth[@]}" -H 'Accept: application/vnd.github+json' \
                "https://api.github.com/repos/$repo/releases?per_page=100")"
        fi || { echo "cannot list the releases of $repo (set GH_TOKEN, or GPUEUE_VM_UPGRADE_FROM)" >&2; exit 1; }
        old_version="$(python3 -c '
import json, re, sys
tree = sys.argv[1]
def key(v):
    # 0.2.0-rc.1 sorts before 0.2.0.
    main, _, pre = v.partition("-")
    return [int(x) for x in main.split(".")], pre == "", pre
found = [r["tag_name"][1:] for r in json.load(sys.stdin)
         if not r["draft"] and not r["prerelease"] and re.fullmatch(r"v\d+\.\d+\.\d+", r["tag_name"])]
older = sorted((v for v in found if key(v) < key(tree)), key=key)
print(older[-1] if older else "")
' "$version" <<<"$releases")"
        [ -n "$old_version" ] || { echo "no release of $repo is older than $version (set GPUEUE_VM_UPGRADE_FROM)" >&2; exit 1; }
    fi
    echo "upgrade scenario: from the release v$old_version to the tree ($version)"
fi

# The tree's packages. When the upgrade starts from the tree's own version,
# the tree's package gets a higher version (<version>+vmtest), so that it
# replaces the release's.
pkg_version="$version"
[ "$old_version" != "$version" ] || pkg_version="$version+vmtest"
build=
[ -z "$need_deb" ] || [ -n "${GPUEUE_VM_DEB:-}" ] || build=1
[ -z "$need_rpm" ] || [ -n "${GPUEUE_VM_RPM:-}" ] || build=1
if [ -n "$build" ] && [ "${GPUEUE_VM_SKIP_BUILD:-}" != 1 ]; then
    "$root/scripts/test-in-docker.sh" build --release --locked -p gpueue -p gpueue-helper
fi
if [ -n "$need_deb" ]; then
    if [ -n "${GPUEUE_VM_DEB:-}" ]; then
        [ "$pkg_version" = "$version" ] || { echo "the upgrade from v$old_version needs a package newer than $version" >&2; exit 1; }
        cp "$GPUEUE_VM_DEB" "$stage/pkg/new.deb"
    else
        deb="$(GPUEUE_PKG_VERSION="$pkg_version" "$root/scripts/dist/build-package.sh" "$root/target/docker/release" "$stage/build" deb)"
        mv "$deb" "$stage/pkg/new.deb"
    fi
fi
if [ -n "$need_rpm" ]; then
    if [ -n "${GPUEUE_VM_RPM:-}" ]; then
        [ "$pkg_version" = "$version" ] || { echo "the upgrade from v$old_version needs a package newer than $version" >&2; exit 1; }
        cp "$GPUEUE_VM_RPM" "$stage/pkg/new.rpm"
    else
        rpm="$(GPUEUE_PKG_VERSION="$pkg_version" "$root/scripts/dist/build-package.sh" "$root/target/docker/release" "$stage/build" rpm)"
        mv "$rpm" "$stage/pkg/new.rpm"
    fi
fi

# fetch_release_pkg <deb|rpm>: the previous release's package as
# pkg/old.<format>, checked as install.sh checks a release: its .sha256
# names it, and its minisign signature (packaging/minisign.pub) has the
# trusted comment "gpueue v<version> <file>".
fetch_release_pkg() {
    local fmt=$1 tag="v$old_version" name dir sum
    case "$fmt" in
    deb) name="gpueue_${old_version}_amd64.deb" ;;
    rpm) name="gpueue-${old_version}-1.x86_64.rpm" ;;
    esac
    dir="$stage/release-$fmt"
    mkdir -p "$dir"
    for f in "$name" "$name.sha256" "$name.minisig"; do
        curl -fsSL --retry 3 -o "$dir/$f" "https://github.com/$repo/releases/download/$tag/$f"
    done
    sum="$(cat "$dir/$name.sha256")"
    if ! [[ "$sum" =~ ^([0-9a-f]{64})\ [\ *](.+)$ ]] || [ "${BASH_REMATCH[2]}" != "$name" ]; then
        echo "$name.sha256 is not the checksum of $name" >&2
        return 1
    fi
    [ "$(sha256sum "$dir/$name" | cut -d' ' -f1)" = "${BASH_REMATCH[1]}" ] ||
        { echo "SHA-256 checksum mismatch for $name" >&2; return 1; }
    local minisign comment
    minisign="$("$root/scripts/dist/get-minisign.sh" "$root/target/dist-tools")"
    comment="$("$minisign" -V -Q -p "$root/packaging/minisign.pub" -m "$dir/$name" -x "$dir/$name.minisig")" ||
        { echo "bad signature on $name" >&2; return 1; }
    [ "$comment" = "gpueue $tag $name" ] ||
        { echo "the signature of $name is for another file: \"$comment\"" >&2; return 1; }
    echo "verified $tag $name (SHA-256 and minisign signature)"
    cp "$dir/$name" "$stage/pkg/old.$fmt"
}
if [ -n "$need_upgrade" ]; then
    for spec in "${specs[@]}"; do
        [ "${spec#*/}" = upgrade ] || continue
        distro_info "${spec%%/*}"
        [ -f "$stage/pkg/old.$DISTRO_PKG" ] || fetch_release_pkg "$DISTRO_PKG"
    done
fi

# --- the runner ------------------------------------------------------------------

cp "$root/scripts/vm/Dockerfile" "$stage/ctx/"
cp /etc/ssl/certs/ca-certificates.crt "$stage/ctx/ca.crt"
docker build -q -t "$image" "$stage/ctx" >/dev/null
cp "$root"/scripts/e2e/*.sh "$root"/scripts/e2e/*.py "$root"/scripts/e2e/*.toml "$stage/e2e/"
cp -r "$root/scripts/vm" "$stage/vm"
chmod -R a+rX "$stage"

cache_args=(-v "$cache":/cache)
if [[ "$cache" == /* ]]; then
    mkdir -p "$cache"
    cache_args+=(-e "GPUEUE_VM_CACHE_OWNER=$(id -u):$(id -g)")
fi
n=0
# runner <args...>: run.sh in a container of its own; its output goes to
# the terminal and to $logs/<n>.log.
runner() {
    n=$((n + 1))
    # apparmor=unconfined: under docker-default, QEMU may not signal its own
    # vCPU threads where the profile is stacked (rootless/userns Docker:
    # "qemu:cpus_kick_thread: Permission denied"). The container is otherwise
    # unprivileged.
    docker run --rm --name "gpueue-vm-$$-$n" -e GPUEUE_VM_KEEP --device /dev/kvm \
        --security-opt apparmor=unconfined "${cache_args[@]}" -v "$stage":/stage:ro \
        "$image" bash /stage/vm/run.sh "$@" 2>&1 | tee "$logs/$n.log"
}

if [ -n "$prepare_only" ]; then
    for d in "${specs[@]}"; do runner prepare-only "$d"; done
    exit 0
fi

# Groups of scenarios that share a VM: the same distribution and install
# method, in the order they first appear.
groups=()
declare -A members=()
for spec in "${specs[@]}"; do
    g="${spec%%/*} $(install_of "${spec#*/}")"
    if [ -z "${members[$g]+x}" ]; then
        groups+=("$g")
        members[$g]=""
    fi
    [[ " ${members[$g]} " == *" ${spec#*/} "* ]] || members[$g]+=" ${spec#*/}"
done

start=$SECONDS
status=0
for g in "${groups[@]}"; do
    read -r d inst <<<"$g"
    read -r -a scen <<<"${members[$g]}"
    t=$SECONDS
    runner "$d" "$inst" "${scen[@]}" || status=1
    echo "TIMING $d ($inst: ${scen[*]}): $((SECONDS - t))s in all" >>"$logs/$n.log"
done

echo
echo "=== timings"
cat "$logs"/*.log | sed -n 's/^TIMING //p'
echo "total: $((SECONDS - start))s"
echo "=== summary"
results="$(cat "$logs"/*.log | sed -n 's/^RESULT //p')"
echo "$results"
# Every scenario must have reported (a runner that died reports nothing).
for spec in "${specs[@]}"; do
    grep -qE "^(PASS|FAIL|SKIP) $spec(:|\$)" <<<"$results" || { echo "FAIL $spec: no result"; status=1; }
done
! grep -q '^FAIL' <<<"$results" || status=1
exit "$status"
