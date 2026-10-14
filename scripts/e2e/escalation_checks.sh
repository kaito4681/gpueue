#!/bin/bash
# Run inside a puzuz job by scenario 19 (scripts/e2e/scenario.sh): tries
# every way the test container offers a member of `sudo`, `docker` and
# `disk` to become root or act as root, and checks the job's identity.
# Prints "ok <check>" for each attempt that failed as it must, "BAD <check>:
# <detail>" otherwise, and "round <n> done" at the end of round <n>.
#
# Usage: escalation_checks.sh <uid> <gid> <docker socket> <block device> <home>
# Round 1 runs at once; round n (n > 1) when <home>/round<n> appears (after
# the job has been checkpointed and restored). The job ends after round 2.
set -u

uid=$1 gid=$2 docker_sock=$3 blockdev=$4 home=$5

ok() { echo "ok $1"; }
bad() { echo "BAD $1: $2"; }
# must_fail <name> <command...>: the command must exit non-zero.
must_fail() {
    local name=$1 out
    shift
    if out=$(timeout 20 "$@" </dev/null 2>&1); then
        bad "$name" "succeeded: $out"
    else
        ok "$name"
        echo "   ($name: $(head -c 200 <<<"$out" | tr '\n' ' '))"
    fi
}
# must_fail_if <program> <name> <command...>: skipped without <program>.
must_fail_if() {
    local prog=$1
    shift
    if command -v "$prog" >/dev/null; then
        must_fail "$@"
    else
        echo "skip $1 (no $prog)"
    fi
}

checks() {
    must_fail_if sudo "sudo -n true" sudo -n true
    must_fail_if sudo "sudo -n id" sudo -n id -u
    must_fail_if su "su -c true root" su -c true root
    must_fail_if pkexec "pkexec true" pkexec true
    must_fail_if run0 "run0 true" run0 true
    must_fail "systemctl --system start" systemctl --system --no-ask-password start e2e-escalate.service
    must_fail "busctl --system call" busctl --system call org.freedesktop.systemd1 \
        /org/freedesktop/systemd1 org.freedesktop.systemd1.Manager StartUnit ss \
        e2e-escalate.service replace
    must_fail "connect to the system bus socket" python3 -c '
import socket, sys
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])' /run/dbus/system_bus_socket
    must_fail "connect to the docker socket" python3 -c '
import socket, sys
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])' "$docker_sock"
    must_fail "open the block device" python3 -c '
import os, sys
os.open(sys.argv[1], os.O_RDONLY)' "$blockdev"

    # systemd's Varlink services (polkit-authorized without D-Bus): every
    # socket the host has under /run/systemd outside journal/, resolve/
    # and userdb/ (listed by the scenario), and two calls that the polkit
    # rule lets the owner make outside puzuz.
    local sock n=0
    while read -r sock; do
        [ -n "$sock" ] || continue
        n=$((n + 1))
        must_fail "connect to $sock" python3 -c '
import socket, sys
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])' "$sock"
    done <"$home/e2e-varlink-sockets"
    echo "   (tried $n Varlink sockets)"
    if [ -s "$home/e2e-cred-decrypt.json" ]; then
        must_fail "varlinkctl Credentials.Decrypt" varlinkctl call \
            /run/systemd/io.systemd.Credentials io.systemd.Credentials.Decrypt \
            "$(cat "$home/e2e-cred-decrypt.json")"
        must_fail "varlinkctl Manager.Reload" varlinkctl call \
            /run/systemd/io.systemd.Manager io.systemd.Manager.Reload '{}'
    else
        echo "skip varlinkctl calls (no io.systemd.Credentials on this host)"
    fi
    must_fail_if systemd-dissect "systemd-dissect --mount (mountfsd)" \
        systemd-dissect --mount --mkdir /usr/lib/os-release "$home/e2e-dissect"
    # What stays: logging and name resolution.
    [ -S /run/systemd/journal/socket ] && ok "journal socket reachable" \
        || bad "journal socket reachable" "missing"

    # Identity: the submitter's uid and primary gid, no denied groups, no
    # privileges.
    [ "$(id -u)" = "$uid" ] && ok "uid" || bad "uid" "$(id -u) != $uid"
    [ "$(id -g)" = "$gid" ] && ok "gid" || bad "gid" "$(id -g) != $gid"
    local groups
    groups=$(id -Gn)
    if grep -qwE 'sudo|docker|disk' <<<"$groups"; then
        bad "groups" "$groups"
    else
        ok "groups"
    fi
    echo "   (groups: $groups)"
    grep -qx 'NoNewPrivs:[[:space:]]*1' /proc/self/status && ok "NoNewPrivs" \
        || bad "NoNewPrivs" "$(grep NoNewPrivs /proc/self/status)"
    local cap value
    for cap in CapInh CapPrm CapEff CapBnd CapAmb; do
        value=$(awk -v c="$cap:" '$1 == c {print $2}' /proc/self/status)
        [ "$value" = 0000000000000000 ] && ok "$cap" || bad "$cap" "$value"
    done
}

checks
echo "round 1 done"
until [ -e "$home/round2" ]; do sleep 0.2; done
checks
echo "round 2 done"
