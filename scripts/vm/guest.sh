#!/bin/bash
# Runs as root inside the VM of scripts/vm-reboot-test.sh (driven by
# scripts/vm/run.sh over ssh). Reuses the helpers of scripts/e2e/scenario.sh
# (users: alice, bob, carol = puzuz-admin).
#
# Usage: guest.sh setup
#        guest.sh prepare <scenario>            start the jobs
#        guest.sh trigger <scenario> reboot|poweroff
#        guest.sh verify <scenario>             after the boot
# Scenarios: reboot, poweroff, inflight (a checkpoint runs when the
# shutdown starts).
set -uo pipefail
# shellcheck source-path=SCRIPTDIR source=../e2e/scenario.sh
source /e2e/scenario.sh lib

# 1 GiB of random memory: its dump takes a few seconds.
BIG='exec python3 -u /e2e/vm/bigcounter.py 1024'

setup() {
    local dev
    # /var/lib/puzuz on a disk of its own (QEMU serial "puzuz-state").
    dev=$(readlink -f /dev/disk/by-id/virtio-puzuz-state) || fail "no state disk"
    mkfs.ext4 -q -L puzuz-state "$dev" || fail "mkfs"
    echo 'LABEL=puzuz-state /var/lib/puzuz ext4 defaults 0 2' >>/etc/fstab
    mkdir -p /var/lib/puzuz
    systemctl daemon-reload
    systemctl start var-lib-puzuz.mount || fail "mounting /var/lib/puzuz"
    chmod 0755 /var/lib/puzuz
    rmdir /var/lib/puzuz/lost+found
    bash /e2e/install.sh || fail "install"
    pz carol group add free -p 0 || fail "group add"
    findmnt /var/lib/puzuz
    systemctl show -p DefaultTimeoutStopUSec
    systemctl show -p TimeoutStopUSec puzuz-helper.service puzuz.service
}

# load_ids <scenario>: the job ids of a scenario as variables a, b, c (and big).
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
    local s=$1 action=$2 a b c big
    load_ids "$s"
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
    return re.compile(r"puzuz-job-%s(-r\d+)?\.service" % job)


def unit_stopping(job):
    r = unit_re(job)
    return first(lambda i, m: i == "systemd" and m.startswith("Stopping ") and r.search(m))


def unit_gone(job):
    # The unit's processes are gone: dumped (CRIU kills them), or stopped.
    r = unit_re(job)
    return first(lambda i, m: i == "systemd" and r.search(m)
                 and ("Deactivated successfully" in m or m.startswith("Stopped ")
                      or "Failed with result" in m or "Main process exited" in m))


shutdown = first(sysd("Stopping puzuz.service"))
daemon_stopped = first(sysd("Stopped puzuz.service"))
helper_stopping = first(sysd("Stopping puzuz-helper.service"))
helper_sys = first(lambda i, m: i == "puzuz-helper" and "system shutdown; checkpointing" in m)
manifest = first(lambda i, m: i == "puzuz-helper" and "shutdown manifest written" in m)
helper_stopped = first(sysd("Stopped puzuz-helper.service"))
umount_start = first(sysd("Unmounting var-lib-puzuz.mount"))
umounted = first(sysd("Unmounted var-lib-puzuz.mount"))
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
        errors.append("puzuz-helper.service stopped before its dumps finished")
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
        errors.append("/var/lib/puzuz was unmounted before the helper or a job unit stopped")
    if umounted is None:
        errors.append("/var/lib/puzuz was not unmounted cleanly (no 'Unmounted' line)")
for t, ident, msg in rows:
    if re.search(r"puzuz", msg) and re.search(r"timed out|Killing process|SIGKILL", msg):
        errors.append(f"timeout: [{t}] {msg}")

def rel(t):
    return "-" if t is None or shutdown is None else f"{t - shutdown:+.2f}s"
print("stop order (relative to 'Stopping puzuz.service'):")
for k, v in [("daemon stopped", daemon_stopped), ("helper: system shutdown, dumping", helper_sys),
             ("helper: manifest written", manifest), ("puzuz-helper.service stopped", helper_stopped),
             ("no-checkpoint job unit stopping", unit_stopping(c)),
             ("no-checkpoint job unit gone", unit_gone(c)),
             ("unmounting /var/lib/puzuz", umount_start), ("unmounted /var/lib/puzuz", umounted),
             ("local-fs.target stopped", localfs), ("last journal line", journald_last)]:
    print(f"  {rel(v):>9}  {k}")
if first_stop is not None and journald_last is not None:
    print(f"journal: first 'Stopping' to last line {journald_last - first_stop:.2f}s; "
          f"helper stop {rel(helper_stopped)} (dumps {manifest - helper_sys:.2f}s)"
          if manifest and helper_sys else "")
pat = re.compile(r"puzuz|var-lib-puzuz|local-fs|Unmount|journal|shutdown\.target|final|umount")
print("--- journal excerpt (previous boot, from the shutdown on)")
for t, ident, msg in rows:
    if shutdown is not None and t >= shutdown - 1 and (pat.search(msg) or ident.startswith("puzuz")):
        print(f"[{t:10.3f}] {ident}: {msg}")
if errors:
    print("ordering problems:\n  " + "\n  ".join(errors), file=sys.stderr)
    sys.exit(1)
EOF
}

verify() {
    local s=$1 a b c big i before=() after
    load_ids "$s"
    [ "$(cat /proc/sys/kernel/random/boot_id)" != "$(cat "$STATE/$s/boot_id")" ] \
        || fail "the VM did not boot again"
    journalctl --list-boots --no-pager | tail -n 2
    wait_socket
    echo "--- shutdown-manifest.json"
    cat "$CKPT/shutdown-manifest.json" || true
    echo
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
    for i in $a $b $big; do wait_state "$i" 'done/*'; done
}

SCENARIO=${2:-}
prepare_scenario() { prepare "$SCENARIO"; }
verify_scenario() { verify "$SCENARIO"; }
case "${1:-}" in
setup) run "setup" setup ;;
prepare) run "$SCENARIO (prepare)" prepare_scenario ;;
trigger) trigger "$SCENARIO" "$3" ;;
verify) run "$SCENARIO" verify_scenario ;;
*) fail "usage: guest.sh setup | prepare|verify <scenario> | trigger <scenario> <action>" ;;
esac
[ "${#FAILED[@]}" -eq 0 ]
