#!/bin/bash
# Distribution scenarios; run inside the systemd container as root by
# scripts/dist-test.sh, with /dist holding install.sh, the fake GitHub API,
# the releases, a minisign binary, the release public key and the token.
#
# Usage: scenario.sh 17
#   17: install.sh: refusals, install v1, a job keeps running through a
#       re-install of v2, uninstall keeps the data
set -euo pipefail

# shellcheck disable=SC1091
. /dist/versions
TOKEN="$(cat /dist/token)"
export PUZUZ_RELEASE_API_URL=http://127.0.0.1:8080
export PUZUZ_UPDATE_PUBKEY_FILE=/dist/release.pub

fail() {
    echo "FAIL: $*" >&2
    exit 1
}
step() { printf '\n=== %s\n' "$*"; }
# expect_fail <pattern> <command...>: the command fails and prints <pattern>.
expect_fail() {
    local pattern="$1" out
    shift
    if out="$("$@" 2>&1)"; then
        printf '%s\n' "$out"
        fail "succeeded unexpectedly: $*"
    fi
    grep -qF -- "$pattern" <<<"$out" || {
        printf '%s\n' "$out"
        fail "expected \"$pattern\" from: $*"
    }
}
wait_for() {
    local end=$((SECONDS + $1)) what="$2"
    shift 2
    until "$@"; do
        [ "$SECONDS" -lt "$end" ] || fail "timed out waiting for $what"
        sleep 0.2
    done
}

start_server() {
    [ -d /srv/releases ] && return
    mkdir -p /srv
    cp -r /dist/releases /srv/releases
    systemd-run --quiet --unit fake-github python3 /dist/fake_github.py /srv/releases 8080 "$TOKEN"
    wait_for 20 "the fake GitHub API" curl -s -o /dev/null http://127.0.0.1:8080/
}
set_latest() { echo "v$1" >/srv/releases/latest; }

# The version the daemon runs.
daemon_version() {
    puzuz service status 2>/dev/null | sed -n 's/.*(version \([^,]*\), API.*/\1/p'
}
services_up() {
    systemctl is-active -q puzuz-helper.service puzuz.service &&
        puzuz service status >/dev/null 2>&1
}

# start_job: a counter job of alice; prints its id.
start_job() {
    local out
    out="$(cd /home/alice && runuser -u alice -- puzuz add -p -- \
        'i=0; while :; do echo $i; i=$((i+1)); sleep 0.2; done')"
    grep -o '[0-9]\+' <<<"$out" | head -n1
}
job_unit_active() { systemctl is-active -q "puzuz-job-$1.service"; }
log_lines() { wc -l <"/var/lib/puzuz/logs/1000/$1.log"; }
# job_alive <id>: its unit runs and its log grows.
job_alive() {
    local before
    job_unit_active "$1" || return 1
    before="$(log_lines "$1")"
    sleep 1
    [ "$(log_lines "$1")" -gt "$before" ]
}
job_state() {
    (cd /home/alice && runuser -u alice -- puzuz status --json) |
        python3 -c 'import json,sys; d=json.load(sys.stdin); print([j["state"]["state"] for j in d["jobs"] if j["id"]==int(sys.argv[1])][0])' "$1"
}

scenario_17() {
    start_server
    set_latest "$V1"
    local install=/dist/install.sh

    step "17: refusals before anything is installed"
    expect_fail "A GitHub token that can read" bash "$install"
    GH_TOKEN=wrong expect_fail "The token cannot read" bash "$install"
    GH_TOKEN="$TOKEN" expect_fail "release v9.9.9 not found" bash "$install" --version 9.9.9
    expect_fail "must run as root" runuser -u alice -- bash "$install"
    mkdir -p /tmp/oldglibc
    printf '#!/bin/sh\necho "glibc 2.31"\n' >/tmp/oldglibc/getconf
    chmod +x /tmp/oldglibc/getconf
    PATH="/tmp/oldglibc:$PATH" GH_TOKEN="$TOKEN" expect_fail "glibc 2.31 is too old" bash "$install"
    GH_TOKEN="$TOKEN" expect_fail "checksum mismatch" bash "$install" --version "$VERSION-test.badsha"
    printf '[Service]\nExecStart=/usr/bin/puzuz daemon\n' >/usr/lib/systemd/system/puzuz.service
    GH_TOKEN="$TOKEN" expect_fail "distribution package" bash "$install"
    # A masked or overridden package unit is still the package, not "mixed".
    ln -s /dev/null /etc/systemd/system/puzuz.service
    GH_TOKEN="$TOKEN" expect_fail "distribution package" bash "$install"
    rm /etc/systemd/system/puzuz.service
    printf '[Service]\nExecStart=/usr/bin/puzuz daemon --verbose\n' >/etc/systemd/system/puzuz.service
    GH_TOKEN="$TOKEN" expect_fail "distribution package" bash "$install"
    rm /etc/systemd/system/puzuz.service /usr/lib/systemd/system/puzuz.service
    # A mask alone is no installation: the pre-download check lets it through.
    ln -s /dev/null /etc/systemd/system/puzuz.service
    GH_TOKEN="$TOKEN" expect_fail "release v9.9.9 not found" bash "$install" --version 9.9.9
    rm /etc/systemd/system/puzuz.service
    touch /etc/NIXOS
    GH_TOKEN="$TOKEN" expect_fail "NixOS" bash "$install"
    rm /etc/NIXOS
    [ ! -e /usr/local/bin/puzuz ] || fail "a refused install left /usr/local/bin/puzuz"

    step "17: install v1 (token from a file, saved for puzuz update)"
    printf '%s\n' "$TOKEN" >/root/token
    chmod 600 /root/token
    bash "$install" --token-file /root/token --save-token | tee /tmp/install-v1.out
    grep -q "verified the SHA-256 checksum" /tmp/install-v1.out || fail "checksum not verified"
    grep -q "signature was not checked" /tmp/install-v1.out || fail "no note about the missing minisign"
    grep -q "puzuz $V1 is installed in /usr/local/bin" /tmp/install-v1.out || fail "no success message"
    wait_for 20 "the services" services_up
    [ "$(daemon_version)" = "$V1" ] || fail "daemon runs $(daemon_version), expected $V1"
    [ "$(puzuz service install-method)" = service-install ] || fail "install method"
    [ "$(stat -c '%U %a' /etc/puzuz/github-token)" = "root 600" ] || fail "saved token not root 0600"
    [ "$(cat /etc/puzuz/github-token)" = "$TOKEN" ] || fail "saved token differs"
    if grep -rqF "$TOKEN" /tmp/install-v1.out; then fail "the token was printed"; fi

    step "17: a job runs"
    local id
    id="$(start_job)"
    wait_for 20 "job $id to run" job_alive "$id"

    step "17: a bad signature is refused (minisign installed now)"
    install -m 0755 /dist/minisign /usr/local/bin/minisign
    local before
    before="$(sha256sum /usr/local/bin/puzuz)"
    GH_TOKEN="$TOKEN" expect_fail "bad signature" bash "$install" --version "$VERSION-test.badsig"
    [ "$(sha256sum /usr/local/bin/puzuz)" = "$before" ] || fail "the binary changed"

    step "17: re-running install.sh installs v2; the job keeps running"
    set_latest "$V2"
    GH_TOKEN="$TOKEN" bash "$install" | tee /tmp/install-v2.out
    grep -q "verified the minisign signature" /tmp/install-v2.out || fail "signature not verified"
    wait_for 30 "the v2 daemon" bash -c "puzuz service status 2>/dev/null | grep -q 'version $V2, API'"
    /usr/local/bin/puzuz --version | grep -qF "$V2" || fail "puzuz --version is not $V2"
    job_alive "$id" || fail "job $id stopped during the update"
    [ "$(job_state "$id")" = running ] || fail "job $id is $(job_state "$id") after the update"

    step "17: --uninstall keeps the data"
    bash "$install" --uninstall | tee /tmp/uninstall.out
    [ ! -e /etc/systemd/system/puzuz.service ] || fail "unit left behind"
    [ ! -e /usr/local/bin/puzuz ] || fail "binary left behind"
    [ ! -e /usr/local/bin/puzuz-helper ] || fail "helper binary left behind"
    ! systemctl is-active -q puzuz.service || fail "daemon still running"
    [ -f /var/lib/puzuz/daemon/state.sqlite ] || fail "state removed"
    [ -f /etc/puzuz/config.toml ] || fail "configuration removed"
    systemctl stop 'puzuz-job-*' || true
    expect_fail "nothing to uninstall" bash "$install" --uninstall

    step "17: --uninstall --purge needs a terminal or --yes"
    GH_TOKEN="$TOKEN" bash "$install" >/dev/null
    wait_for 20 "the services" services_up
    expect_fail "pass --yes" setsid bash "$install" --uninstall --purge </dev/null
    bash "$install" --uninstall --purge --yes >/dev/null
    [ ! -e /var/lib/puzuz ] || fail "/var/lib/puzuz left after --purge"

    step "17: re-running without --bin-dir keeps the installed bin dir"
    GH_TOKEN="$TOKEN" bash "$install" --bin-dir /opt/puzuz/bin >/dev/null
    GH_TOKEN="$TOKEN" bash "$install" >/dev/null
    grep -q '^ExecStart=/opt/puzuz/bin/puzuz daemon' /etc/systemd/system/puzuz.service ||
        fail "the units no longer run /opt/puzuz/bin/puzuz"
    [ ! -e /usr/local/bin/puzuz ] || fail "a re-run installed /usr/local/bin/puzuz"
    [ ! -e /usr/local/bin/puzuz-helper ] || fail "a re-run installed /usr/local/bin/puzuz-helper"
    bash "$install" --uninstall --purge --yes >/dev/null
    [ ! -e /opt/puzuz/bin/puzuz ] || fail "/opt/puzuz/bin/puzuz left after --uninstall"
    echo
    echo "PASS 17"
}

case "${1:-}" in
17) scenario_17 ;;
*)
    echo "usage: $0 17" >&2
    exit 2
    ;;
esac
