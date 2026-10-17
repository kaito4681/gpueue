#!/bin/bash
# Distribution scenarios; run inside the systemd container as root by
# scripts/dist-test.sh, with /dist holding install.sh, the fake GitHub API,
# the releases, a minisign binary, the release public key and the token.
#
# Usage: scenario.sh 17|18
#   17: install.sh: refusals, install v1, a job keeps running through a
#       re-install of v2, uninstall keeps the data
#   18: gpueue update: --check, doctor, a bad signature, v1 -> v2 with a
#       running job, a downgrade below the database schema (v2 adds a
#       migration), a package-managed machine
set -euo pipefail

# shellcheck disable=SC1091
. /dist/versions
TOKEN="$(cat /dist/token)"
# install.sh honours the overrides only with GPUEUE_TEST_HOOKS=1; the test
# versions of gpueue are built with the test-hooks feature. The minisign that
# install.sh downloads when none is installed comes from a local server too
# (the real release tarball, checked against the checksum in install.sh).
export GPUEUE_TEST_HOOKS=1
export GPUEUE_RELEASE_API_URL=http://127.0.0.1:8080
export GPUEUE_UPDATE_PUBKEY_FILE=/dist/release.pub
MINISIGN_TARBALL="$(basename /dist/minisign-download/minisign-*-linux.tar.gz)"
export GPUEUE_MINISIGN_TARBALL_URL="http://127.0.0.1:8081/$MINISIGN_TARBALL"

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

# A dummy .deb named gpueue that ships both units in /usr/lib/systemd/system.
make_deb() {
    local root=/tmp/gpueue-deb
    [ -f /tmp/gpueue.deb ] && return
    rm -rf "$root"
    mkdir -p "$root/DEBIAN" "$root/usr/lib/systemd/system"
    printf 'Package: gpueue\nVersion: 0.0.1\nArchitecture: all\nMaintainer: test <test@example.com>\nDescription: dummy gpueue package for the distribution test\n' >"$root/DEBIAN/control"
    printf '[Service]\nExecStart=/usr/bin/gpueue daemon\n' >"$root/usr/lib/systemd/system/gpueue.service"
    printf '[Service]\nExecStart=/usr/bin/gpueue-helper serve\n' >"$root/usr/lib/systemd/system/gpueue-helper.service"
    dpkg-deb --build --root-owner-group "$root" /tmp/gpueue.deb >/dev/null
}
install_deb() {
    make_deb
    dpkg -i /tmp/gpueue.deb >/dev/null
}

start_server() {
    [ -d /srv/releases ] && return
    mkdir -p /srv
    cp -r /dist/releases /srv/releases
    systemd-run --quiet --unit fake-github python3 /dist/fake_github.py /srv/releases 8080 "$TOKEN"
    wait_for 20 "the fake GitHub API" curl -s -o /dev/null http://127.0.0.1:8080/
    mkdir -p /srv/minisign
    cp /dist/minisign-download/"$MINISIGN_TARBALL" /srv/minisign/
    echo "not a tarball" >/srv/minisign/corrupt.tar.gz
    systemd-run --quiet --unit minisign-download \
        python3 -m http.server --bind 127.0.0.1 --directory /srv/minisign 8081
    wait_for 20 "the minisign download server" curl -s -o /dev/null http://127.0.0.1:8081/
}
set_latest() { echo "v$1" >/srv/releases/latest; }

# The version the daemon runs.
daemon_version() {
    gpueue service status 2>/dev/null | sed -n 's/.*(version \([^,]*\), API.*/\1/p'
}
services_up() {
    systemctl is-active -q gpueue-helper.service gpueue.service &&
        gpueue service status >/dev/null 2>&1
}

# start_job: a counter job of alice; prints its id.
start_job() {
    local out
    out="$(cd /home/alice && runuser -u alice -- gpueue add -p -- \
        'i=0; while :; do echo $i; i=$((i+1)); sleep 0.2; done')"
    grep -o '[0-9]\+' <<<"$out" | head -n1
}
job_unit_active() { systemctl is-active -q "gpueue-job-$1.service"; }
log_lines() { wc -l <"/var/lib/gpueue/logs/1000/$1.log"; }
# job_alive <id>: its unit runs and its log grows.
job_alive() {
    local before
    job_unit_active "$1" || return 1
    before="$(log_lines "$1")"
    sleep 1
    [ "$(log_lines "$1")" -gt "$before" ]
}
job_state() {
    (cd /home/alice && runuser -u alice -- gpueue status --json) |
        python3 -c 'import json,sys; d=json.load(sys.stdin); print([j["state"]["state"] for j in d["jobs"] if j["id"]==int(sys.argv[1])][0])' "$1"
}

scenario_17() {
    start_server
    set_latest "$V1"
    local install=/dist/install.sh

    step "17: refusals before anything is installed"
    # The fake API answers 404 without the token (like a private repository),
    # which is also how a repository without releases looks.
    expect_fail "has no published release" bash "$install"
    GH_TOKEN=wrong expect_fail "with the GitHub token: it may be expired or revoked" bash "$install"
    GH_TOKEN="$TOKEN" expect_fail "release v9.9.9 not found" bash "$install" --version 9.9.9
    expect_fail "must run as root" runuser -u alice -- bash "$install"
    mkdir -p /tmp/oldglibc
    printf '#!/bin/sh\necho "glibc 2.31"\n' >/tmp/oldglibc/getconf
    chmod +x /tmp/oldglibc/getconf
    PATH="/tmp/oldglibc:$PATH" GH_TOKEN="$TOKEN" expect_fail "glibc 2.31 is too old" bash "$install"
    GH_TOKEN="$TOKEN" expect_fail "checksum mismatch" bash "$install" --version "$VERSION-test.badsha"
    GH_TOKEN="$TOKEN" expect_fail "is not the checksum of gpueue-v$VERSION-test.nullsha-" \
        bash "$install" --version "$VERSION-test.nullsha"
    # Several times: the listing check once lost a race against SIGPIPE.
    local _try
    for _try in 1 2 3 4 5; do
        GH_TOKEN="$TOKEN" expect_fail "contains links or special files" \
            bash "$install" --version "$VERSION-test.symlink"
    done
    # A unit a package owns is refused before anything is downloaded.
    install_deb
    GH_TOKEN="$TOKEN" expect_fail "distribution package gpueue (dpkg" bash "$install" --version 9.9.9
    # A masked or overridden package unit is still the package, not "mixed".
    ln -s /dev/null /etc/systemd/system/gpueue.service
    GH_TOKEN="$TOKEN" expect_fail "distribution package gpueue (dpkg" bash "$install" --version 9.9.9
    rm /etc/systemd/system/gpueue.service
    printf '[Service]\nExecStart=/usr/bin/gpueue daemon --verbose\n' >/etc/systemd/system/gpueue.service
    GH_TOKEN="$TOKEN" expect_fail "distribution package gpueue (dpkg" bash "$install" --version 9.9.9
    rm /etc/systemd/system/gpueue.service
    dpkg -r gpueue >/dev/null
    # A unit nobody owns: the downloaded binary refuses it.
    printf '[Service]\nExecStart=/usr/bin/gpueue daemon\n' >/usr/lib/systemd/system/gpueue.service
    GH_TOKEN="$TOKEN" expect_fail "no package owns /usr/lib/systemd/system/gpueue.service" bash "$install"
    rm /usr/lib/systemd/system/gpueue.service
    # A mask alone is no installation: the pre-download check lets it through.
    ln -s /dev/null /etc/systemd/system/gpueue.service
    GH_TOKEN="$TOKEN" expect_fail "release v9.9.9 not found" bash "$install" --version 9.9.9
    rm /etc/systemd/system/gpueue.service
    # Units from /nix/store, and NixOS (by os-release only).
    ln -s /nix/store/abc-gpueue.service /etc/systemd/system/gpueue.service
    GH_TOKEN="$TOKEN" expect_fail "come from /nix/store" bash "$install" --version 9.9.9
    rm /etc/systemd/system/gpueue.service
    cp -P /etc/os-release /tmp/os-release.saved
    rm /etc/os-release
    printf 'NAME=NixOS\nID=nixos\n' >/etc/os-release
    GH_TOKEN="$TOKEN" expect_fail "this is NixOS" bash "$install" --version 9.9.9
    rm /etc/os-release
    mv /tmp/os-release.saved /etc/os-release
    [ ! -e /usr/local/bin/gpueue ] || fail "a refused install left /usr/local/bin/gpueue"

    step "17: the pinned, downloaded minisign checks the signature"
    ! command -v minisign >/dev/null || fail "minisign is installed already"
    GH_TOKEN="$TOKEN" expect_fail "bad signature" bash "$install" --version "$VERSION-test.badsig"
    # A minisign download that fails, or does not match the pinned checksum,
    # stops the installation (there is no way to skip the signature).
    GPUEUE_MINISIGN_TARBALL_URL=http://127.0.0.1:8081/missing.tar.gz GH_TOKEN="$TOKEN" \
        expect_fail "downloads a pinned release" bash "$install"
    GPUEUE_MINISIGN_TARBALL_URL=http://127.0.0.1:8081/corrupt.tar.gz GH_TOKEN="$TOKEN" \
        expect_fail "does not match its SHA-256 checksum" bash "$install"
    [ ! -e /usr/local/bin/gpueue ] || fail "a refused install left /usr/local/bin/gpueue"

    step "17: install v1 (token from a file, saved for gpueue update)"
    printf '%s\n' "$TOKEN" >/root/token
    chmod 600 /root/token
    bash "$install" --token-file /root/token --save-token | tee /tmp/install-v1.out
    grep -q "verified the SHA-256 checksum" /tmp/install-v1.out || fail "checksum not verified"
    grep -q "downloading minisign" /tmp/install-v1.out || fail "minisign not downloaded"
    grep -q "verified the minisign signature" /tmp/install-v1.out || fail "signature not verified"
    grep -q "gpueue $V1 is installed in /usr/local/bin" /tmp/install-v1.out || fail "no success message"
    # install.sh no longer passes --bin-dir: `service install` chose /usr/local/bin.
    grep -q "installing into /usr/local/bin" /tmp/install-v1.out || fail "no note about /usr/local/bin"
    grep -q '^copied = \["/usr/local/bin/gpueue", "/usr/local/bin/gpueue-helper"\]' \
        /var/lib/gpueue/install-receipt || fail "the receipt does not list the copies"
    wait_for 20 "the services" services_up
    [ "$(daemon_version)" = "$V1" ] || fail "daemon runs $(daemon_version), expected $V1"
    [ "$(gpueue service install-method)" = service-install ] || fail "install method"
    [ "$(stat -c '%U %a' /etc/gpueue/github-token)" = "root 600" ] || fail "saved token not root 0600"
    [ "$(cat /etc/gpueue/github-token)" = "$TOKEN" ] || fail "saved token differs"
    if grep -rqF "$TOKEN" /tmp/install-v1.out; then fail "the token was printed"; fi

    step "17: a job runs"
    local id
    id="$(start_job)"
    wait_for 20 "job $id to run" job_alive "$id"

    step "17: a bad signature is refused (a minisign in PATH is ignored)"
    install -m 0755 /dist/minisign /usr/local/bin/minisign
    local before
    before="$(sha256sum /usr/local/bin/gpueue)"
    GH_TOKEN="$TOKEN" expect_fail "bad signature" bash "$install" --version "$VERSION-test.badsig"
    [ "$(sha256sum /usr/local/bin/gpueue)" = "$before" ] || fail "the binary changed"

    step "17: an older release's validly signed tarball under a newer name is refused"
    GH_TOKEN="$TOKEN" expect_fail "is for another file (trusted comment \"gpueue v$V1 gpueue-v$V1-" \
        bash "$install" --version "$VERSION-test.replay"
    [ "$(sha256sum /usr/local/bin/gpueue)" = "$before" ] || fail "the binary changed"

    step "17: re-running install.sh installs v2; the job keeps running"
    set_latest "$V2"
    GH_TOKEN="$TOKEN" bash "$install" | tee /tmp/install-v2.out
    grep -q "verified the minisign signature" /tmp/install-v2.out || fail "signature not verified"
    # A minisign in PATH is never trusted: the pinned one is downloaded all the same.
    grep -q "downloading minisign" /tmp/install-v2.out || fail "the pinned minisign was not downloaded"
    wait_for 30 "the v2 daemon" bash -c "gpueue service status 2>/dev/null | grep -q 'version $V2, API'"
    /usr/local/bin/gpueue --version | grep -qF "$V2" || fail "gpueue --version is not $V2"
    job_alive "$id" || fail "job $id stopped during the update"
    [ "$(job_state "$id")" = running ] || fail "job $id is $(job_state "$id") after the update"

    step "17: a latest release older than the installed one is refused without --version"
    set_latest "$V1"
    GH_TOKEN="$TOKEN" expect_fail "not downgrading" bash "$install"
    /usr/local/bin/gpueue --version | grep -qF "$V2" || fail "the binary is not $V2 any more"
    set_latest "$V2"

    step "17: --uninstall keeps the data"
    bash "$install" --uninstall | tee /tmp/uninstall.out
    [ ! -e /etc/systemd/system/gpueue.service ] || fail "unit left behind"
    [ ! -e /usr/local/bin/gpueue ] || fail "binary left behind"
    [ ! -e /usr/local/bin/gpueue-helper ] || fail "helper binary left behind"
    [ ! -e /var/lib/gpueue/install-receipt ] || fail "receipt left behind"
    ! systemctl is-active -q gpueue.service || fail "daemon still running"
    [ -f /var/lib/gpueue/daemon/state.sqlite ] || fail "state removed"
    [ -f /etc/gpueue/config.toml ] || fail "configuration removed"
    systemctl stop 'gpueue-job-*' || true
    expect_fail "nothing to uninstall" bash "$install" --uninstall

    step "17: --uninstall --purge needs a terminal or --yes"
    GH_TOKEN="$TOKEN" bash "$install" >/dev/null
    wait_for 20 "the services" services_up
    expect_fail "pass --yes" setsid bash "$install" --uninstall --purge </dev/null
    bash "$install" --uninstall --purge --yes >/dev/null
    [ ! -e /var/lib/gpueue ] || fail "/var/lib/gpueue left after --purge"

    step "17: re-running without --bin-dir keeps the installed bin dir"
    GH_TOKEN="$TOKEN" bash "$install" --bin-dir /opt/gpueue/bin >/dev/null
    # Even when the installed binary cannot answer `service install-method`
    # (e.g. too old), the marker unit keeps the bin dir.
    rm /opt/gpueue/bin/gpueue
    printf '#!/bin/sh\nexit 1\n' >/opt/gpueue/bin/gpueue
    chmod 0755 /opt/gpueue/bin/gpueue
    GH_TOKEN="$TOKEN" bash "$install" >/dev/null
    grep -q '^ExecStart=/opt/gpueue/bin/gpueue daemon' /etc/systemd/system/gpueue.service ||
        fail "the units no longer run /opt/gpueue/bin/gpueue"
    [ ! -e /usr/local/bin/gpueue ] || fail "a re-run installed /usr/local/bin/gpueue"
    [ ! -e /usr/local/bin/gpueue-helper ] || fail "a re-run installed /usr/local/bin/gpueue-helper"
    bash "$install" --uninstall --purge --yes >/dev/null
    [ ! -e /opt/gpueue/bin/gpueue ] || fail "/opt/gpueue/bin/gpueue left after --uninstall"

    step "17: --uninstall with a gpueue that predates \`gpueue uninstall\`"
    GH_TOKEN="$TOKEN" bash "$install" >/dev/null
    wait_for 20 "the services" services_up
    # An older gpueue: no `uninstall` subcommand and no receipt.
    mv /usr/local/bin/gpueue /root/gpueue.real
    printf '#!/bin/sh\ncase "$1" in uninstall) echo "error: unrecognized subcommand" >&2; exit 2 ;; esac\nexec /root/gpueue.real "$@"\n' \
        >/usr/local/bin/gpueue
    chmod 0755 /usr/local/bin/gpueue
    rm /var/lib/gpueue/install-receipt
    bash "$install" --uninstall --purge --yes | tee /tmp/uninstall-legacy.out
    grep -qF "removed /usr/local/bin/gpueue-helper" /tmp/uninstall-legacy.out || fail "legacy uninstall kept the helper"
    [ ! -e /usr/local/bin/gpueue ] || fail "legacy uninstall kept gpueue"
    [ ! -e /etc/systemd/system/gpueue.service ] || fail "legacy uninstall kept the unit"
    rm -f /root/gpueue.real
    echo
    echo "PASS 17"
}

scenario_18() {
    start_server
    set_latest "$V1"
    local install=/dist/install.sh gpueue=/usr/local/bin/gpueue

    step "18: install v1 with a saved token; a job runs"
    GH_TOKEN="$TOKEN" bash "$install" --save-token >/dev/null
    wait_for 20 "the services" services_up
    [ "$(daemon_version)" = "$V1" ] || fail "daemon runs $(daemon_version), expected $V1"
    local id
    id="$(start_job)"
    wait_for 20 "job $id to run" job_alive "$id"

    step "18: update --check reports v2"
    set_latest "$V2"
    "$gpueue" update --check | tee /tmp/check.out
    grep -qF "gpueue $V2 is available (installed: $V1)" /tmp/check.out || fail "no newer release reported"
    # A user cannot read the saved token, so the fake API answers 404.
    expect_fail "has no published release" \
        runuser -u alice -- env GPUEUE_RELEASE_API_URL="$GPUEUE_RELEASE_API_URL" "$gpueue" update
    # With a token, a user's `update` only checks.
    runuser -u alice -- env GPUEUE_RELEASE_API_URL="$GPUEUE_RELEASE_API_URL" GH_TOKEN="$TOKEN" \
        "$gpueue" update | grep -qF "gpueue $V2 is available" || fail "a user's update did not check"
    "$gpueue" doctor >/tmp/doctor.out 2>&1 || true
    grep -qF "INFO  update: gpueue $V2 is available (this is $V1)" /tmp/doctor.out ||
        { cat /tmp/doctor.out; fail "doctor does not report v2"; }

    step "18: a bad signature, an older release's tarball under a newer name, a link in the tarball, a checksum of another file: refused, nothing changes"
    local before
    before="$(sha256sum "$gpueue")"
    expect_fail "bad signature" "$gpueue" update --yes --version "$VERSION-test.badsig"
    [ "$(sha256sum "$gpueue")" = "$before" ] || fail "the binary changed"
    [ "$(daemon_version)" = "$V1" ] || fail "the daemon changed"
    [ ! -e /var/lib/gpueue/update ] || fail "the download directory was left behind"
    expect_fail "is for another file (trusted comment \"gpueue v$V1 gpueue-v$V1-" \
        "$gpueue" update --yes --version "$VERSION-test.replay"
    expect_fail "is a link or special file" "$gpueue" update --yes --version "$VERSION-test.symlink"
    expect_fail "checksum" "$gpueue" update --yes --version "$VERSION-test.nullsha"
    [ "$(sha256sum "$gpueue")" = "$before" ] || fail "the binary changed"
    [ "$(daemon_version)" = "$V1" ] || fail "the daemon changed"

    step "18: sudo gpueue update installs v2; the job keeps running"
    "$gpueue" update --yes | tee /tmp/update.out
    grep -qF "verified the signature" /tmp/update.out || fail "signature not verified"
    grep -qF "gpueue is now $V2." /tmp/update.out || fail "no success message"
    [ "$(daemon_version)" = "$V2" ] || fail "daemon runs $(daemon_version), expected $V2"
    "$gpueue" --version | grep -qF "$V2" || fail "the binary is not $V2"
    job_alive "$id" || fail "job $id stopped during the update"
    [ "$(job_state "$id")" = running ] || fail "job $id is $(job_state "$id") after the update"
    "$gpueue" update --yes | grep -qF "gpueue $V2 is up to date" || fail "a second update did something"

    step "18: a downgrade below the database schema is refused"
    before="$(sha256sum "$gpueue")"
    expect_fail "the database is at schema" "$gpueue" update --yes --version "$V1"
    [ "$(sha256sum "$gpueue")" = "$before" ] || fail "the binary changed"
    [ "$(daemon_version)" = "$V2" ] || fail "the daemon changed"
    job_alive "$id" || fail "job $id stopped"

    step "18: update refuses a package-managed machine"
    install_deb
    set_latest "$VERSION-test.badsig"
    expect_fail "installed in more than one way" "$gpueue" update --yes
    dpkg -r gpueue >/dev/null

    step "18: sudo gpueue uninstall removes the services and the copied binaries"
    systemctl stop 'gpueue-job-*' || true
    "$gpueue" uninstall --purge --yes | tee /tmp/uninstall.out
    grep -qF "removed /usr/local/bin/gpueue-helper" /tmp/uninstall.out || fail "helper not removed"
    [ ! -e /etc/systemd/system/gpueue.service ] || fail "unit left behind"
    [ ! -e "$gpueue" ] || fail "binary left behind"
    [ ! -e /var/lib/gpueue ] || fail "/var/lib/gpueue left after --purge"
    echo
    echo "PASS 18"
}

case "${1:-}" in
17) scenario_17 ;;
18) scenario_18 ;;
*)
    echo "usage: $0 17|18" >&2
    exit 2
    ;;
esac
