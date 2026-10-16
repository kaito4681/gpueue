#!/bin/bash
# Runs as root inside the VM of scripts/vm-reboot-test.sh (driven by
# scripts/vm/run.sh over ssh). Reuses the helpers of scripts/e2e/scenario.sh
# (users: alice, bob, carol = gpueue-admin).
#
# Usage: guest.sh setup package|service|upgrade
#        guest.sh criu-check                    prints why checkpoints cannot work here
#        guest.sh run basic|upgrade             a scenario without a reboot
#        guest.sh prepare <scenario>            start the jobs
#        guest.sh trigger <scenario> reboot|poweroff
#        guest.sh verify <scenario>             after the boot
# Reboot scenarios: reboot and service-install (the same jobs), poweroff,
# inflight (a checkpoint runs when the shutdown starts), desktop.
set -uo pipefail
# shellcheck source-path=SCRIPTDIR source=../e2e/scenario.sh
source /e2e/scenario.sh lib

# 1 GiB of random memory: its dump takes a few seconds.
BIG='exec python3 -u /e2e/vm/bigcounter.py 1024'
PKG=/e2e/pkg
export DEBIAN_FRONTEND=noninteractive

# pkg_format: deb or rpm, as this distribution installs packages.
pkg_format() {
    if command -v dpkg >/dev/null; then echo deb; else echo rpm; fi
}
# install_pkg <file>: installs a gpueue package as users do (the package
# manager, with a local file); nothing is downloaded: the dependencies are
# installed already.
install_pkg() {
    case "$1" in
    *.deb) apt-get install -y -q "$1" ;;
    *.rpm) dnf install -y -q --disablerepo='*' "$1" ;;
    esac
}
# installed_version: the version of the installed gpueue binary.
installed_version() { gpueue --version | awk '{print $2}'; }
# version_lt <a> <b>: version a sorts before b.
version_lt() { [ "$1" != "$2" ] && [ "$(printf '%s\n' "$1" "$2" | sort -V | head -n1)" = "$1" ]; }

wait_services() {
    wait_for 60 "the gpueue services" systemctl is-active --quiet gpueue-helper.service gpueue.service
    wait_helper
}

setup() {
    local install=$1 dev fmt x
    fmt=$(pkg_format)
    # /var/lib/gpueue on a disk of its own (QEMU serial "gpueue-state").
    dev=$(readlink -f /dev/disk/by-id/virtio-gpueue-state) || fail "no state disk"
    mkfs.ext4 -q -L gpueue-state "$dev" || fail "mkfs"
    echo 'LABEL=gpueue-state /var/lib/gpueue ext4 defaults 0 2' >>/etc/fstab
    mkdir -p /var/lib/gpueue
    systemctl daemon-reload
    systemctl start var-lib-gpueue.mount || fail "mounting /var/lib/gpueue"
    chmod 0755 /var/lib/gpueue
    rmdir /var/lib/gpueue/lost+found
    case "$install" in
    package)
        install_pkg "$PKG/new.$fmt" || fail "installing the $fmt package"
        ;;
    upgrade)
        install_pkg "$PKG/old.$fmt" || fail "installing the previous release's $fmt package"
        ;;
    service)
        # `gpueue service install` (what install.sh runs) of the package's
        # binaries, with the e2e configuration.
        x=$(mktemp -d)
        case "$fmt" in
        deb) dpkg-deb -x "$PKG/new.deb" "$x" ;;
        rpm) (cd "$x" && rpm2cpio "$PKG/new.rpm" | cpio -idm --quiet) ;;
        esac || fail "extracting the package"
        mkdir -p /e2e/bin
        install -m 0755 "$x/usr/bin/gpueue" "$x/usr/bin/gpueue-helper" /e2e/bin/
        rm -rf "$x"
        bash /e2e/install.sh || fail "install"
        ;;
    *) fail "unknown install method $install" ;;
    esac
    wait_services
    echo "installed: $(command -v gpueue), $(gpueue --version)"
    pz carol group add free -p 0 || fail "group add"
    gpueue doctor || true
    findmnt /var/lib/gpueue
    systemctl show -p DefaultTimeoutStopUSec
    systemctl show -p TimeoutStopUSec gpueue-helper.service gpueue.service
}

# criu-check: prints why checkpoints cannot work on this distribution (no
# CRIU or one older than 4.2, see docs/install.md), or nothing.
criu_check() {
    local v
    if ! v=$(criu --version 2>/dev/null | sed -n 's/^Version: *//p') || [ -z "$v" ]; then
        echo "the distribution has no criu package (gpueue needs CRIU 4.2 or newer)"
        return
    fi
    if version_lt "$v" 4.2; then
        echo "the distribution's CRIU $v is older than 4.2"
    fi
}

# load_ids <scenario>: the job ids of a reboot scenario as variables a, b,
# c (and big).
load_ids() {
    local dir=$STATE/$1
    a=$(cat "$dir/a")
    b=$(cat "$dir/b")
    c=$(cat "$dir/c")
    big=$(cat "$dir/big" 2>/dev/null || true)
}

prepare() {
    local s=$1 dir=$STATE/$1 a b c big=
    mkdir -p "$dir"
    a=$(add alice -g free -- "$COUNTER")
    b=$(add bob -g free -- "$COUNTER")
    c=$(add bob -g free --no-checkpoint -- 'trap "echo got TERM; exit 0" TERM; while :; do sleep 0.1; done')
    if [ "$s" = inflight ]; then
        big=$(add alice -g free -- "$BIG")
        echo "$big" >"$dir/big"
    fi
    for i in $a $b $c $big; do wait_state "$i" running; done
    # The ballast is allocated once the counter prints.
    [ -z "$big" ] || wait_for 60 "job $big to print" bash -c "[ -s $LOGS/1000/$big.log ]"
    sleep 1.5
    echo "$a" >"$dir/a"
    echo "$b" >"$dir/b"
    echo "$c" >"$dir/c"
    cat /proc/sys/kernel/random/boot_id >"$dir/boot_id"
    pz carol status
}

trigger() {
    local s=$1 action=$2 big
    big=$(cat "$STATE/$s/big" 2>/dev/null || true)
    if [ -n "$big" ]; then
        pz alice checkpoint "$big" || fail "checkpoint"
        wait_state "$big" checkpointing 10
        echo "checkpoint of job $big in flight: $(find "$CKPT/$big" -mindepth 1 2>/dev/null | wc -l) files, COMPLETE: $([ -e "$CKPT/$big/COMPLETE" ] && echo yes || echo no)"
    fi
    sync
    echo "systemctl $action"
    systemctl "$action"
}

# check_journal <scenario>: the stop order in the previous boot's journal.
check_journal() {
    local s=$1 a b c big
    load_ids "$s"
    journalctl -b -1 -o short-monotonic --no-pager >"$STATE/$s/journal.txt"
    python3 - "$STATE/$s/journal.txt" "$a" "$b" "$c" "${big:-}" <<'EOF'
import re
import sys

path, a, b, c, big = sys.argv[1:6]
rows = []
for line in open(path, errors="replace"):
    m = re.match(r"\[\s*([0-9.]+)\] \S+ ([^:\[]+)(?:\[\d+\])?: (.*)", line)
    if m:
        rows.append((float(m.group(1)), m.group(2), m.group(3).rstrip()))
errors = []


def first(pred, start=0.0):
    for t, ident, msg in rows:
        if t >= start and pred(ident, msg):
            return t
    return None


def sysd(text):
    return lambda ident, msg: ident == "systemd" and msg.startswith(text)


def unit_re(job):
    return re.compile(r"gpueue-job-%s(-r\d+)?\.service" % job)


def unit_stopping(job):
    r = unit_re(job)
    return first(lambda i, m: i == "systemd" and m.startswith("Stopping ") and r.search(m))


def unit_gone(job):
    # The unit's processes are gone: dumped (CRIU kills them), or stopped.
    r = unit_re(job)
    return first(lambda i, m: i == "systemd" and r.search(m)
                 and ("Deactivated successfully" in m or m.startswith("Stopped ")
                      or "Failed with result" in m or "Main process exited" in m))


shutdown = first(sysd("Stopping gpueue.service"))
daemon_stopped = first(sysd("Stopped gpueue.service"))
helper_stopping = first(sysd("Stopping gpueue-helper.service"))
helper_sys = first(lambda i, m: i == "gpueue-helper" and "system shutdown; checkpointing" in m)
manifest = first(lambda i, m: i == "gpueue-helper" and "shutdown manifest written" in m)
helper_stopped = first(sysd("Stopped gpueue-helper.service"))
umount_start = first(sysd("Unmounting var-lib-gpueue.mount"))
umounted = first(sysd("Unmounted var-lib-gpueue.mount"))
localfs = first(sysd("Stopped target local-fs.target"))
journald_last = rows[-1][0] if rows else None
first_stop = first(lambda i, m: i == "systemd" and m.startswith("Stopping "), (shutdown or 0) - 30)

need = dict(shutdown=shutdown, daemon_stopped=daemon_stopped, helper_sys=helper_sys,
            manifest=manifest, helper_stopped=helper_stopped, umount_start=umount_start)
for k, v in need.items():
    if v is None:
        errors.append(f"journal has no {k} event")
if not errors:
    if daemon_stopped > helper_sys:
        errors.append("the helper started dumping before the daemon stopped")
    if not manifest <= helper_stopped:
        errors.append("gpueue-helper.service stopped before its dumps finished")
    for job in (a, b):
        gone, stopping = unit_gone(job), unit_stopping(job)
        if gone is None or not helper_sys <= gone <= manifest:
            errors.append(f"job {job} was not dumped by the helper during shutdown (gone at {gone})")
        if stopping is not None and stopping < manifest:
            errors.append(f"systemd stopped job {job}'s unit at {stopping}, before the dumps ended")
    stopping, gone = unit_stopping(c), unit_gone(c)
    if stopping is None or stopping < helper_stopped:
        errors.append(f"the --no-checkpoint job {c} was stopped at {stopping}, before the helper stopped")
    if big:
        gone = unit_gone(big)
        if gone is None or gone < shutdown or gone > manifest:
            errors.append(f"the in-flight dump of job {big} did not end during the shutdown ({gone})")
        else:
            print(f"in-flight dump of job {big} ended {gone - shutdown:.2f}s after the shutdown began, "
                  f"{'after' if gone >= helper_sys else 'before'} the helper got SIGTERM")
    last_job = max(filter(None, (unit_gone(j) for j in (a, b, c, big) if j)), default=0)
    if umount_start < helper_stopped or umount_start < last_job:
        errors.append("/var/lib/gpueue was unmounted before the helper or a job unit stopped")
    if umounted is None:
        errors.append("/var/lib/gpueue was not unmounted cleanly (no 'Unmounted' line)")
for t, ident, msg in rows:
    if re.search(r"gpueue", msg) and re.search(r"timed out|Killing process|SIGKILL", msg):
        errors.append(f"timeout: [{t}] {msg}")

def rel(t):
    return "-" if t is None or shutdown is None else f"{t - shutdown:+.2f}s"
print("stop order (relative to 'Stopping gpueue.service'):")
for k, v in [("daemon stopped", daemon_stopped), ("helper: system shutdown, dumping", helper_sys),
             ("helper: manifest written", manifest), ("gpueue-helper.service stopped", helper_stopped),
             ("no-checkpoint job unit stopping", unit_stopping(c)),
             ("no-checkpoint job unit gone", unit_gone(c)),
             ("unmounting /var/lib/gpueue", umount_start), ("unmounted /var/lib/gpueue", umounted),
             ("local-fs.target stopped", localfs), ("last journal line", journald_last)]:
    print(f"  {rel(v):>9}  {k}")
if first_stop is not None and journald_last is not None:
    print(f"journal: first 'Stopping' to last line {journald_last - first_stop:.2f}s; "
          f"helper stop {rel(helper_stopped)} (dumps {manifest - helper_sys:.2f}s)"
          if manifest and helper_sys else "")
pat = re.compile(r"gpueue|var-lib-gpueue|local-fs|Unmount|journal|shutdown\.target|final|umount")
print("--- journal excerpt (previous boot, from the shutdown on)")
for t, ident, msg in rows:
    if shutdown is not None and t >= shutdown - 1 and (pat.search(msg) or ident.startswith("gpueue")):
        print(f"[{t:10.3f}] {ident}: {msg}")
if errors:
    print("ordering problems:\n  " + "\n  ".join(errors), file=sys.stderr)
    sys.exit(1)
EOF
}

# booted_again <scenario>: the VM booted since the scenario's prepare.
booted_again() {
    [ "$(cat /proc/sys/kernel/random/boot_id)" != "$(cat "$STATE/$1/boot_id")" ] \
        || fail "the VM did not boot again"
    journalctl --list-boots --no-pager | tail -n 2
    wait_socket
    echo "--- shutdown-manifest.json"
    cat "$CKPT/shutdown-manifest.json" || true
    echo
}

verify() {
    local s=$1 a b c big i before=() after
    load_ids "$s"
    booted_again "$s"
    check_journal "$s" || fail "the previous boot's journal shows a wrong stop order"
    for i in $a $b $big; do
        [ -e "$CKPT/$i/COMPLETE" ] || fail "job $i has no complete image"
        wait_state "$i" checkpointed 60
    done
    wait_state "$c" 'done/*' 30
    grep -q 'got TERM' "$LOGS/1001/$c.log" || fail "the no-checkpoint job did not get SIGTERM"
    pz carol status
    for i in $a $b $big; do before+=("$(last_line "$(q carol "job($i)['owner']")" "$i")"); done
    pz alice restore "$a" ${big:+"$big"} || fail "restore (alice)"
    pz bob restore "$b" || fail "restore (bob)"
    for i in $a $b $big; do wait_state "$i" running 120; done
    sleep 1.5
    local k=0
    for i in $a $b $big; do
        local owner
        owner=$(q carol "job($i)['owner']")
        after=$(last_line "$owner" "$i")
        echo "job $i: counter before the shutdown ${before[$k]}, after restore $after"
        [ "$after" -gt "${before[$k]}" ] || fail "job $i did not continue"
        check_counter_log "$owner" "$i"
        k=$((k + 1))
    done
    pz carol kill "$a" "$b" ${big:+"$big"}
    # grace_secs (30 s by default) and a margin: a job started as
    # `exec python3 ...` may not die on SIGTERM.
    for i in $a $b $big; do wait_state "$i" 'done/*' 60; done
}

# --- basic: what works without CRIU ---------------------------------------------

# The package is installed and its services run; a job runs to its end, is
# paused and resumed; a checkpoint and restore when CRIU is new enough.
basic() {
    local id reason
    systemctl is-active gpueue-helper.service gpueue.service || fail "the services are not active"
    id=$(add alice -g free -- 'echo hello from $(id -un); sleep 1')
    PZ_TIMEOUT=60 pz alice wait "$id" >/dev/null || true
    expect_state "$id" done/success
    pz alice log "$id" | grep -qx 'hello from alice' || fail "the job's output is missing"
    rm -f /home/alice/b-c.txt
    id=$(cjob /home/alice /home/alice/b-c.txt)
    wait_state "$id" running
    sleep 1
    pz alice pause "$id" || fail "pause"
    wait_state "$id" paused
    expect_paused_file "$id" /home/alice/b-c.txt "$(log_lines_file /home/alice/b-c.txt)"
    pz alice start "$id" || fail "start"
    wait_state "$id" running
    file_grows /home/alice/b-c.txt
    reason=$(criu_check)
    if [ -z "$reason" ]; then
        pz alice checkpoint "$id" >/dev/null || fail "checkpoint"
        restore_continues "$id" /home/alice/b-c.txt
    else
        # Not tried: gpueue documents CRIU 4.2 as required and does not
        # promise anything about older versions.
        echo "SKIP basic: checkpoint and restore ($reason)"
    fi
    check_counter_file /home/alice/b-c.txt
    pz alice kill "$id" >/dev/null
    wait_state "$id" 'done/*'
}

# --- upgrade: the previous release's package, then this tree's ------------------

# Jobs of the previous release (one checkpointed, one running) across the
# upgrade to this tree's package. From 0.1.2 on, both must be restorable
# afterwards (docs/install.md, Upgrading: "Checkpoints survive upgrades");
# from 0.1.1 or older the documented limits apply instead: the restore of
# the checkpointed job fails, and the running job cannot be checkpointed
# (it keeps running).
upgrade() {
    local old new k r n fmt err
    fmt=$(pkg_format)
    old=$(installed_version)
    rm -f /home/alice/u-*.txt
    k=$(cjob /home/alice /home/alice/u-k.txt)
    r=$(cjob /home/alice /home/alice/u-r.txt)
    wait_state "$k" running
    wait_state "$r" running
    sleep 1
    pz alice checkpoint "$k" >/dev/null || fail "checkpoint $k with $old"
    wait_state "$k" checkpointed 60
    echo "--- upgrading $old to the tree's package"
    install_pkg "$PKG/new.$fmt" || fail "upgrading to the tree's $fmt package"
    wait_services
    new=$(installed_version)
    echo "upgraded: $old -> $new ($(command -v gpueue))"
    expect_state "$r" running
    expect_state "$k" checkpointed
    file_grows /home/alice/u-r.txt
    if version_lt "$old" 0.1.2; then
        echo "upgrading from $old (0.1.1 or older): expecting the documented limits"
        pz alice restore "$k" >/dev/null 2>&1 || true
        wait_for 120 "the restore of job $k to end" not_restoring "$k"
        sleep 2
        ! is_state "$k" running || fail "job $k of $old was restored after the upgrade (docs/install.md says it fails)"
        err=$(cat "$CKPT/$k"/restore*.log 2>/dev/null | grep -m1 'bad size' || true)
        echo "restore of job $k: $(st "$k"), $(q carol "job($k)['details'].get('last_error')"); restore.log: ${err:-no 'bad size' line}"
        [ -n "$err" ] || fail "the restore of job $k did not fail as documented (bad size of gpueue-helper)"
        # The running job keeps running; a checkpoint of it fails, which
        # stops it (docs/install.md, Checkpoint failures).
        file_grows /home/alice/u-r.txt
        pz alice checkpoint "$r" >/dev/null 2>&1 || true
        wait_state "$r" 'done/*' 120
        err=$(q carol "job($r)['state'].get('reason') or job($r)['details'].get('last_error')")
        echo "checkpoint of job $r: $(st "$r"): $err"
        expect_state "$r" done/checkpoint_failed
        grep -q "Can't dump ghost file /usr/bin/gpueue-helper" <<<"$err" \
            || fail "the checkpoint of job $r did not fail as documented (ghost file gpueue-helper)"
        pz alice remove "$k" >/dev/null 2>&1 || true
    else
        restore_continues "$k" /home/alice/u-k.txt
        pz alice checkpoint "$r" >/dev/null || fail "checkpoint $r after the upgrade"
        restore_continues "$r" /home/alice/u-r.txt
        pz alice kill "$k" "$r" >/dev/null
        wait_state "$k" 'done/*'
        wait_state "$r" 'done/*'
    fi
    # A job of the new version.
    n=$(cjob /home/alice /home/alice/u-n.txt)
    wait_state "$n" running
    sleep 1
    pz alice checkpoint "$n" >/dev/null || fail "checkpoint $n"
    restore_continues "$n" /home/alice/u-n.txt
    pz alice kill "$n" >/dev/null
    wait_state "$n" 'done/*'
    ls -l /var/lib/gpueue/lib/
}

# --- desktop: a workstation's setup -------------------------------------------

# systemd-resolved (whose directory /run/systemd/resolve belongs to the
# systemd-resolve user, and whose files the shim binds into every job), /tmp
# as a tmpfs, an AppImage-like FUSE mount of alice's under /tmp and her
# sshfs in her home directory (both without allow_other, so they refuse
# root). Jobs: A (counter in the home directory) and T (keeps its state in
# its private /tmp) are checkpointed and restored and run into the reboot;
# P is paused, checkpointed and restored paused, and stays paused into the
# reboot; C is checkpointed before the reboot; F works in the sshfs mount
# and cannot be checkpointed. After the reboot alice mounts her FUSE
# filesystems again and every job is restored and must go on counting.
DESKTOP=$STATE/desktop
APPIMG=/tmp/.mount_gpueueAppXXXX
REMOTE=/home/alice/remote
SSH_OPTS=StrictHostKeyChecking=no,UserKnownHostsFile=/dev/null,LogLevel=ERROR

# The desktop's FUSE mounts, by alice and without allow_other.
mount_user_fuse() {
    runuser -u alice -- mkdir -p "$APPIMG" "$REMOTE" /home/alice/remote-src /home/alice/app-src
    runuser -u alice -- sshfs -o "$SSH_OPTS" alice@127.0.0.1:/home/alice/app-src "$APPIMG" \
        || fail "alice's sshfs on $APPIMG"
    runuser -u alice -- sshfs -o "$SSH_OPTS" alice@127.0.0.1:/home/alice/remote-src "$REMOTE" \
        || fail "alice's sshfs on $REMOTE"
    findmnt -n "$APPIMG" "$REMOTE" || true
    ! stat "$REMOTE/." >/dev/null 2>&1 || fail "root can look into alice's FUSE mount"
}
# tjob <file>: a counter of alice's that keeps its next value in its private
# /tmp (/tmp/n), and appends it to <file>; if its /tmp were lost, it would
# count from 0 again.
tjob() {
    (cd /home/alice && runuser -u alice -- gpueue --color never add -p -g free -- \
        "exec 3>>$1; while :; do i=\$(cat /tmp/n 2>/dev/null || echo 0); echo \$i >&3; echo \$((i + 1)) >/tmp/n.new; mv /tmp/n.new /tmp/n; sleep 0.2; done")
}

desktop_prepare() {
    local a t p c f err
    mkdir -p "$DESKTOP"
    systemctl is-active --quiet systemd-resolved || fail "systemd-resolved is not active"
    [ -S /run/systemd/resolve/io.systemd.Resolve ] || fail "resolved has no io.systemd.Resolve socket"
    echo "/run/systemd/resolve: $(stat -c '%U:%G %a' /run/systemd/resolve); /etc/resolv.conf -> $(readlink /etc/resolv.conf)"
    # /tmp as a tmpfs, for this and every later boot.
    if [ "$(findmnt -n -o FSTYPE /tmp)" != tmpfs ]; then
        echo "/tmp is not a tmpfs here; mounting one (and adding it to /etc/fstab)"
        echo 'tmpfs /tmp tmpfs mode=1777,strictatime,nosuid,nodev,size=50% 0 0' >>/etc/fstab
        systemctl daemon-reload
        mount /tmp || fail "mounting a tmpfs on /tmp"
    else
        echo "/tmp is a tmpfs already"
    fi
    findmnt /tmp
    touch /tmp/gpueue-vm-marker
    # ssh for alice to herself, for sshfs.
    systemctl start ssh.service 2>/dev/null || systemctl start sshd.service 2>/dev/null || true
    if [ ! -f /home/alice/.ssh/id_ed25519 ]; then
        runuser -u alice -- bash -c 'mkdir -p -m 700 ~/.ssh && ssh-keygen -q -t ed25519 -N "" -f ~/.ssh/id_ed25519 && cat ~/.ssh/id_ed25519.pub >>~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys'
        ! command -v restorecon >/dev/null || restorecon -R /home/alice/.ssh
    fi
    mount_user_fuse
    rm -f /home/alice/d-*.txt
    a=$(cjob /home/alice /home/alice/d-a.txt)
    t=$(tjob /home/alice/d-t.txt)
    p=$(cjob /home/alice /home/alice/d-p.txt)
    c=$(cjob /home/alice /home/alice/d-c.txt)
    f=$(runuser -u alice -- gpueue --color never add -p -g free -w "$REMOTE" -- \
        "exec 3>>/home/alice/d-f.txt; i=0; while :; do echo \$i >&3; i=\$((i+1)); sleep 0.2; done")
    for i in $a $t $p $c $f; do wait_state "$i" running; done
    sleep 1.5
    grep -q ' /run/systemd/resolve/io.systemd.Resolve ' "/proc/$(live_pid "$a")/mountinfo" \
        || fail "job $a has no bind of resolved's socket"
    ! grep -q " $APPIMG " "/proc/$(live_pid "$a")/mountinfo" || fail "job $a has $APPIMG"

    # Checkpoint and restore on this boot.
    pz alice checkpoint "$a" >/dev/null || fail "checkpoint $a"
    restore_continues "$a" /home/alice/d-a.txt
    pz alice checkpoint "$t" >/dev/null || fail "checkpoint $t"
    restore_continues "$t" /home/alice/d-t.txt
    # Paused, checkpointed, restored paused, and left paused.
    pz alice pause "$p" || fail "pause $p"
    wait_state "$p" paused
    pz alice checkpoint "$p" >/dev/null || fail "checkpoint $p"
    wait_state "$p" checkpointed 120
    local lines
    lines=$(log_lines_file /home/alice/d-p.txt)
    pz alice restore "$p" >/dev/null || fail "restore $p"
    wait_state "$p" paused 120
    expect_paused_file "$p" /home/alice/d-p.txt "$lines"
    # A job working in alice's sshfs cannot be checkpointed (root cannot
    # reach the mount), and keeps running.
    err=$(pz alice checkpoint "$f" 2>&1) && fail "the checkpoint of job $f was not refused"
    echo "checkpoint of job $f: $err"
    grep -q "$REMOTE" <<<"$err" || fail "the refusal does not name $REMOTE: $err"
    expect_state "$f" running
    pz alice kill "$f" >/dev/null
    wait_state "$f" 'done/*'
    # C stays checkpointed through the reboot; A, T and P (paused) are dumped
    # by the shutdown.
    pz alice checkpoint "$c" >/dev/null || fail "checkpoint $c"
    wait_state "$c" checkpointed 60
    for i in a t p c; do echo "${!i}" >"$DESKTOP/$i"; done
    for i in a t p c; do tail -n1 "/home/alice/d-$i.txt" >"$DESKTOP/$i.last"; done
    cat /proc/sys/kernel/random/boot_id >"$DESKTOP/boot_id"
    pz carol status
}
log_lines_file() { grep -c . "$1"; }
# file_grows <file>: a counter keeps appending to <file>.
file_grows() {
    local n
    n=$(log_lines_file "$1")
    wait_for 30 "$1 to grow" bash -c "[ \$(grep -c . $1) -gt $n ]"
}
not_restoring() { ! is_state "$1" restoring; }
# expect_paused_file <id> <file> <lines>: the paused job <id> has not
# written to <file> since it had <lines> lines, and does not.
expect_paused_file() {
    expect_state "$1" paused
    [ "$(log_lines_file "$2")" = "$3" ] || fail "job $1 ran while paused ($(log_lines_file "$2") lines, $3 before)"
    sleep 1.5
    [ "$(log_lines_file "$2")" = "$3" ] || fail "job $1 runs although it is paused"
    expect_state "$1" paused
    echo "job $1 paused; process states: $(proc_states "$1")"
}

desktop_verify() {
    local a t p c lines i
    booted_again desktop
    for i in a t p c; do eval "$i=\$(cat \"\$DESKTOP/\$i\")"; done
    systemctl is-active --quiet systemd-resolved || fail "systemd-resolved is not active"
    [ "$(findmnt -n -o FSTYPE /tmp)" = tmpfs ] || fail "/tmp is not a tmpfs after the reboot"
    [ ! -e /tmp/gpueue-vm-marker ] || fail "/tmp kept its contents across the reboot"
    for i in "$a" "$t" "$p" "$c"; do
        [ -e "$CKPT/$i/COMPLETE" ] || fail "job $i has no complete image"
        wait_state "$i" checkpointed 60
    done
    # alice logs in again and mounts her FUSE filesystems.
    mount_user_fuse
    pz carol status
    restore_continues "$c" /home/alice/d-c.txt
    restore_continues "$a" /home/alice/d-a.txt
    restore_continues "$t" /home/alice/d-t.txt
    lines=$(log_lines_file /home/alice/d-p.txt)
    pz alice restore "$p" >/dev/null || fail "restore $p"
    wait_state "$p" paused 120
    expect_paused_file "$p" /home/alice/d-p.txt "$lines"
    pz alice start "$p" || fail "start $p"
    wait_state "$p" running
    sleep 1.5
    [ "$(log_lines_file /home/alice/d-p.txt)" -gt "$lines" ] || fail "job $p did not continue after start"
    check_counter_file /home/alice/d-p.txt
    pz alice kill "$a" "$t" "$p" "$c" >/dev/null
    for i in "$a" "$t" "$p" "$c"; do wait_state "$i" 'done/*'; done
    runuser -u alice -- fusermount3 -u "$APPIMG" 2>/dev/null || umount -l "$APPIMG"
    runuser -u alice -- fusermount3 -u "$REMOTE" 2>/dev/null || umount -l "$REMOTE"
}

SCENARIO=${2:-}
prepare_scenario() {
    case "$SCENARIO" in
    desktop) desktop_prepare ;;
    *) prepare "$SCENARIO" ;;
    esac
}
verify_scenario() {
    case "$SCENARIO" in
    desktop) desktop_verify ;;
    *) verify "$SCENARIO" ;;
    esac
}
run_scenario() {
    case "$SCENARIO" in
    basic) basic ;;
    upgrade) upgrade ;;
    *) fail "unknown scenario $SCENARIO" ;;
    esac
}
setup_install() { setup "$SCENARIO"; }
case "${1:-}" in
setup) run "setup ($SCENARIO)" setup_install ;;
criu-check) criu_check; exit 0 ;;
run) run "$SCENARIO" run_scenario ;;
prepare) run "$SCENARIO (prepare)" prepare_scenario ;;
trigger) trigger "$SCENARIO" "$3" ;;
verify) run "$SCENARIO" verify_scenario ;;
*) fail "usage: guest.sh setup <install> | criu-check | run <scenario> | prepare|verify <scenario> | trigger <scenario> <action>" ;;
esac
[ "${#FAILED[@]}" -eq 0 ]
