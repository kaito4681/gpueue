#!/bin/bash
# Full-stack scenarios; runs inside the systemd container as root (see
# scripts/e2e-test.sh). Users act through the real CLI with runuser:
# alice (1000) and bob (1001) are regular users, carol (1002) is in
# gpueue-admin; scenario 17 adds dave (1003), a user without usage, and
# scenario 18 svcacct (990, a system account) and dockeruser (1004, primary
# group docker).
#
# Usage: scenario.sh main | reboot-prepare | reboot-verify | lib
# Prints "PASS <scenario>" / "FAIL <scenario>: <reason>" per scenario and
# exits non-zero if any failed.
set -uo pipefail

LOGS=/var/lib/gpueue/logs
CKPT=/var/lib/gpueue/checkpoints
STATE=/var/lib/gpueue/e2e   # values handed from reboot-prepare to reboot-verify
COUNTER='i=0; while :; do echo $i; i=$((i+1)); sleep 0.2; done'

say() { printf '\n--- %s\n' "$*"; }
fail() {
    echo "FAIL: $*" >&2
    exit 1
}
# pz <user> <gpueue args...>: the CLI as <user>, from their home directory.
pz() {
    local u=$1
    shift
    (cd "/home/$u" && ${PZ_TIMEOUT:+timeout "$PZ_TIMEOUT"} runuser -u "$u" -- gpueue --color never "$@")
}
# add <user> [add options] -- <command>: prints the new job's id.
add() {
    local u=$1
    shift
    pz "$u" add -p "$@"
}
# q <user> <python expression>: see q.py.
q() { pz "$1" status --json | python3 /e2e/q.py "$2"; }
# st <id> [user]: compact state of a job (as the admin by default).
st() { q "${2:-carol}" "st($1)"; }
# is_state <id> <glob>
is_state() {
    # shellcheck disable=SC2053
    [[ "$(st "$1")" == $2 ]]
}
# wait_for <seconds> <what> <command...>: poll until the command succeeds.
wait_for() {
    local end=$((SECONDS + $1)) what=$2
    shift 2
    until "$@"; do
        [ "$SECONDS" -lt "$end" ] || fail "timed out waiting for $what (state: $(st "${WAIT_ID:-0}" 2>&1))"
        sleep 0.2
    done
}
# wait_state <id> <glob> [seconds]
wait_state() { WAIT_ID=$1 wait_for "${3:-30}" "job $1 to be $2" is_state "$1" "$2"; }
expect_state() {
    local got
    got=$(st "$1")
    # shellcheck disable=SC2053
    [[ "$got" == $2 ]] || fail "job $1 is $got, expected $2"
}
last_line() { tail -n1 "$LOGS/$1/$2.log"; }
# The counter's log must be 0, 1, 2, ... without gaps or repeats.
check_counter_log() {
    python3 - "$LOGS/$1/$2.log" <<'EOF' || fail "the counter log of job $2 has gaps or repeats"
import sys
lines = [l for l in open(sys.argv[1]).read().split("\n") if l.strip().isdigit()]
nums = [int(l) for l in lines]
bad = [(a, b) for a, b in zip(nums, nums[1:]) if b != a + 1]
if not nums or nums[0] != 0 or bad:
    print(f"first={nums[:1]} breaks={bad[:5]}", file=sys.stderr)
    sys.exit(1)
EOF
}
# wait_unit <id> [timeout]: the unit of job <id> is active (the job really
# runs, not just picked by the daemon).
wait_unit() {
    wait_for "${2:-30}" "gpueue-job-$1.service to be active" systemctl is-active --quiet "gpueue-job-$1.service"
}
wait_socket() {
    wait_for 30 "the daemon socket" pz carol status --json >/dev/null 2>&1
}
# helper_connected: the daemon reports the helper connected (`GET
# /v1/version`, `helper.connected`).
helper_connected() {
    python3 -I -c '
import json, socket, sys
s = socket.socket(socket.AF_UNIX)
s.settimeout(5)
s.connect("/run/gpueue/gpueue.sock")
s.sendall(b"GET /v1/version HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
data = b""
while chunk := s.recv(65536):
    data += chunk
head, _, body = data.partition(b"\r\n\r\n")
if b"chunked" in head.lower():
    out, rest = b"", body
    while rest:
        size, _, rest = rest.partition(b"\r\n")
        n = int(size, 16)
        if n == 0:
            break
        out, rest = out + rest[:n], rest[n + 2:]
    body = out
sys.exit(0 if json.loads(body)["helper"]["connected"] else 1)
' 2>/dev/null
}
# wait_helper: after a restart of the helper, wait until the daemon is
# connected to the new one (the daemon socket answers before that).
wait_helper() {
    wait_socket
    wait_for 60 "the daemon to reconnect to the helper" helper_connected
}
started_at() { q carol "job($1)['started_at']"; }
finished_at() { q carol "job($1)['finished_at']"; }
# free_group: the unlimited group "free" (created by s3, or here when s3
# does not run).
free_group() {
    pz carol group | grep -qw free || pz carol group add free -p 0 || fail "group add"
}
log_lines() { grep -c . "$LOGS/$1/$2.log"; }
# unit_states <id>: the state (R, S, T, ...) of every process of job <id>'s
# live unit, one per line; fails without an active unit.
unit_states() {
    local unit cg
    unit=$(systemctl list-units --no-legend --plain --state=active \
        "gpueue-job-$1.service" "gpueue-job-$1-r*.service" | awk '{print $1}' | tail -n1)
    [ -n "$unit" ] || return 1
    cg=$(systemctl show -p ControlGroup --value "$unit")
    find "/sys/fs/cgroup$cg" -name cgroup.procs -exec cat {} + 2>/dev/null \
        | while read -r p; do awk '/^State:/ {print $2}' "/proc/$p/status" 2>/dev/null; done
}
# proc_states <id>: unit_states counted, for information.
proc_states() {
    local states
    states=$(unit_states "$1") || { echo "no active unit"; return; }
    sort <<<"$states" | uniq -c | tr -s ' \n' ' '
}
# expect_paused <uid> <id> [lines]: job <id> is paused, every process of it
# is stopped (T; zombies aside) as soon as it shows as paused, and its log
# does not grow. With <lines>, the length of its log before the restore
# that paused it, the log must not have grown since either: a job paused
# when it was dumped is restored stopped and runs nothing.
expect_paused() {
    local n states
    expect_state "$2" paused
    states=$(unit_states "$2") || fail "job $2 is paused but has no active unit"
    if [ -z "$states" ] || grep -qvE '^[TZ]$' <<<"$states"; then
        fail "job $2 is paused but not stopped: $(proc_states "$2")"
    fi
    n=$(log_lines "$1" "$2")
    [ -z "${3:-}" ] || [ "$n" = "$3" ] \
        || fail "job $2 ran after its restore: its log has $n lines, $3 before"
    sleep 1.5
    [ "$(log_lines "$1" "$2")" = "$n" ] || fail "job $2 runs although it is paused"
    expect_state "$2" paused
    echo "job $2 paused; process states: $(proc_states "$2")"
}
# expect_continues <uid> <id>: the log of job <id> grows.
expect_continues() {
    local n
    n=$(log_lines "$1" "$2")
    wait_for 30 "job $2 to continue" bash -c "[ \$(grep -c . $LOGS/$1/$2.log) -gt $n ]"
}

diagnose() {
    echo "--- jobs with errors"
    q carol "[(j['id'], st(j['id']), j['details'].get('last_error')) for j in d['jobs'] if j['details'].get('last_error')]" || true
    local f
    for f in "$CKPT"/*/*.log /var/lib/gpueue/helper/jobs/*/*.log; do
        [ -f "$f" ] || continue
        echo "--- tail of $f"
        grep -E 'Error|Warn' "$f" | tail -n 20
    done
}

FAILED=()
# GPUEUE_E2E_ONLY: space-separated scenario numbers to run (all if empty).
ONLY="${GPUEUE_E2E_ONLY:-}"
run() { # run <name> <function>
    if [ -n "$ONLY" ] && [[ " $ONLY " != *" ${1%% *} "* ]]; then
        return 0
    fi
    say "$1"
    # Not `if (set -e; "$2")`: bash ignores set -e in a condition, so a
    # failing command without `|| fail` would not end the scenario.
    local rc
    (set -e; "$2")
    rc=$?
    if [ "$rc" -eq 0 ]; then
        echo "PASS $1"
    else
        echo "FAIL $1"
        FAILED+=("$1")
        diagnose
    fi
}

# ---------------------------------------------------------------------------

s1_lifecycle() {
    local id ok out log
    id=$(add alice -- 'echo start; echo pid=$$; echo uid=$(id -u) caps=$(grep CapEff /proc/self/status | cut -f2); echo nproc=$(ls /proc | grep -c "^[0-9]"); for i in 1 2 3 4 5; do echo tick $i; sleep 0.4; done; exit 3')
    # The job runs for about 2 s; on a loaded host the polling may miss
    # that window, and follow also works (prints the log) once it is done.
    wait_state "$id" '@(running|done/*)'
    out=$(PZ_TIMEOUT=30 pz alice follow "$id") || fail "follow failed"
    grep -q '^tick 5$' <<<"$out" || fail "follow did not stream the whole output: $out"
    PZ_TIMEOUT=30 pz alice wait "$id" >/dev/null || true
    expect_state "$id" done/failed/3
    log=$(pz alice log --full "$id") || fail "log failed"
    echo "$log"
    grep -q '^uid=1000 caps=0000000000000000$' <<<"$log" || fail "the job does not run as alice without capabilities"
    # A PID namespace of its own (the shell is PID 2 under the shim), with a private /proc.
    grep -qx 'pid=2' <<<"$log" || fail "the job is not in its own PID namespace"
    [ "$(sed -n 's/^nproc=//p' <<<"$log")" -lt 10 ] || fail "/proc shows processes outside the job"
    [ "$(stat -c %U "$LOGS/1000/$id.log")" = alice ] || fail "the log is not owned by alice"
    # A reader that stops early (`| grep -q`, `| head`) is not an error,
    # even with pipefail (the CLI used to panic on EPIPE).
    pz alice log --full "$id" | true || fail "log fails when its reader goes away"
    pz alice status | true || fail "status fails when its reader goes away"

    ok=$(add alice -- 'echo fine')
    PZ_TIMEOUT=30 pz alice wait "$ok" || fail "wait failed for a successful job"
    expect_state "$ok" done/success

    # The whole output is in the log as soon as wait returns.
    local n
    for n in 1 2 3 4 5 6 7 8; do
        id=$(add alice -- "echo line-$n")
        PZ_TIMEOUT=30 pz alice wait -q "$id" || fail "wait failed"
        out=$(pz alice log "$id")
        grep -qx "line-$n" <<<"$out" || fail "log right after wait misses the output (try $n): $out"
    done

    # wait blocks until the job is done.
    id=$(add alice -- 'sleep 2; exit 4')
    PZ_TIMEOUT=30 pz alice wait "$id" || fail "wait failed"
    expect_state "$id" done/failed/4
    pz alice status | grep -q "Failed (4)" || fail "status does not show the exit code"
}

s2_authz() {
    local id out
    id=$(add alice -- 'sleep 300')
    wait_state "$id" running
    [ "$(q bob "job($id).get('details') is None and job($id)['owner'] == 1000")" = true ] \
        || fail "bob sees alice's job unredacted"
    # The negated checks capture the output first: with `! cmd | grep -q`, a
    # failing cmd (or one killed by SIGPIPE) would pass the check.
    out=$(pz bob status) || fail "bob's status failed"
    ! grep -q 'sleep 300' <<<"$out" || fail "bob's status shows alice's command"
    ! pz bob log "$id" >/dev/null 2>&1 || fail "bob can read alice's log"
    ! pz bob kill "$id" 2>/dev/null || fail "bob can kill alice's job"
    ! pz bob remove "$id" 2>/dev/null || fail "bob can remove alice's job"
    ! pz bob checkpoint "$id" 2>/dev/null || fail "bob can checkpoint alice's job"
    sleep 0.5
    expect_state "$id" running
    [ "$(q carol "job($id).get('details', {}).get('command')")" = 'sleep 300' ] \
        || fail "the admin does not see the details"
    pz carol log "$id" >/dev/null || fail "the admin cannot read the log"
    pz carol kill "$id" || fail "the admin cannot kill the job"
    wait_state "$id" 'done/*'
    pz carol remove "$id" || fail "the admin cannot remove the job"
    [ "$(q carol "[j['id'] for j in d['jobs']].count($id)")" = 0 ] || fail "the job was not removed"
}

s3_limits() {
    local a b c ids=() i running max=0 out
    ! pz alice group add lim -p 2 2>/dev/null || fail "a regular user can add groups"
    pz carol group add lim -p 2 || fail "group add"
    pz carol group add free -p 0 || fail "group add"
    out=$(pz carol gpu --json) || fail "gpu --json"
    grep -q GPU-fake-1 <<<"$out" || fail "the configured GPUs are not listed: $out"
    pz carol gpu parallel 1 || fail "gpu parallel"

    # Group limit: 4 jobs, at most 2 at a time, started in order.
    for i in 1 2 3 4; do ids+=("$(add alice -g lim -- 'sleep 1.5')"); done
    local end=$((SECONDS + 40))
    while [ "$(q carol "len([i for i in $(echo "[${ids[*]}]" | tr ' ' ,) if not st(i).startswith('done')])")" != 0 ]; do
        running=$(q carol "len([i for i in $(echo "[${ids[*]}]" | tr ' ' ,) if st(i) == 'running'])")
        if [ "$running" -gt "$max" ]; then max=$running; fi
        [ "$SECONDS" -lt "$end" ] || fail "group jobs did not finish"
        sleep 0.2
    done
    [ "$max" = 2 ] || fail "group lim ran $max jobs at once (limit 2)"
    for i in 0 1 2; do
        [ "$(started_at "${ids[$i]}")" -le "$(started_at "${ids[$((i + 1))]}")" ] || fail "jobs started out of order"
    done
    for i in "${ids[@]}"; do expect_state "$i" done/success; done
    [ "$(started_at "${ids[2]}")" -ge "$(finished_at "${ids[0]}")" ] \
        || [ "$(started_at "${ids[2]}")" -ge "$(finished_at "${ids[1]}")" ] \
        || fail "the third job started before a slot was free"

    # GPU limit (1 job per GPU) in an unlimited group.
    a=$(add alice -g free --gpu 0 -- 'echo CVD=$CUDA_VISIBLE_DEVICES; sleep 2')
    b=$(add bob -g free --gpu 0 -- 'echo CVD=$CUDA_VISIBLE_DEVICES; sleep 0.5')
    c=$(add alice -g free --gpu 1 -- 'echo CVD=$CUDA_VISIBLE_DEVICES; sleep 1')
    wait_state "$a" running
    # GPU isolation (gpu_device_isolation = true, the default): every job
    # unit is DevicePolicy=closed.
    [ "$(systemctl show -p DevicePolicy --value "gpueue-job-$a.service")" = closed ] \
        || fail "job $a's unit is not DevicePolicy=closed"
    wait_state "$c" running
    expect_state "$b" queued
    wait_state "$b" 'done/*' 30
    # Not just expect_state: launches are serialized, so on a loaded host
    # job c may start late enough to still run when b is done.
    for i in "$a" "$b" "$c"; do wait_state "$i" 'done/*' 30 && expect_state "$i" done/success; done
    [ "$(started_at "$b")" -ge "$(finished_at "$a")" ] || fail "job $b shared GPU 0 with job $a"
    grep -qx 'CVD=0' "$LOGS/1000/$a.log" || fail "CUDA_VISIBLE_DEVICES of job $a"
    grep -qx 'CVD=0' "$LOGS/1001/$b.log" || fail "CUDA_VISIBLE_DEVICES of job $b"
    grep -qx 'CVD=1' "$LOGS/1000/$c.log" || fail "CUDA_VISIBLE_DEVICES of job $c"

    # Immediate launches: a regular user's job is only queued first among
    # their jobs and waits for the GPU slot; an administrator's ignores it.
    a=$(add alice -g free --gpu 0 -- 'sleep 3')
    wait_state "$a" running
    mkdir -p "$STATE"
    b=$(add bob -i -g free --gpu 0 -- 'sleep 0.5' 2>"$STATE/immediate-note")
    expect_state "$b" queued
    grep -q "was queued first among your jobs" "$STATE/immediate-note" \
        || fail "job $b's add -i does not say it was queued: $(cat "$STATE/immediate-note")"
    c=$(add carol -i -g free --gpu 0 -- 'sleep 0.5')
    expect_state "$c" '@(running|done/*)'
    for i in "$a" "$b" "$c"; do wait_state "$i" 'done/*' 30 && expect_state "$i" done/success; done
    [ "$(started_at "$b")" -ge "$(finished_at "$a")" ] || fail "the immediate job $b shared GPU 0 with job $a"
    [ "$(started_at "$c")" -lt "$(finished_at "$a")" ] || fail "the administrator's immediate job $c waited"
}

s4_checkpoint_restore() {
    local id before after
    free_group
    id=$(add alice -g free -- "$COUNTER")
    wait_state "$id" running
    sleep 1.5
    pz alice checkpoint "$id" || fail "checkpoint"
    wait_state "$id" checkpointed 120
    ! systemctl is-active --quiet "gpueue-job-$id.service" || fail "the unit still runs after the dump"
    [ -e "$CKPT/$id" ] || fail "no image in $CKPT/$id"
    before=$(last_line 1000 "$id")
    sleep 1
    [ "$(last_line 1000 "$id")" = "$before" ] || fail "the log grows while checkpointed"
    pz alice restore "$id" || fail "restore"
    wait_state "$id" running 120
    sleep 1.5
    after=$(last_line 1000 "$id")
    echo "counter before dump: $before, after restore: $after"
    [ "$after" -gt "$before" ] || fail "the restored job did not continue"
    check_counter_log 1000 "$id"
    pz alice kill "$id" || fail "kill"
    wait_state "$id" 'done/*'
}

# s5/s6: stop <unit>, let one job exit while it is down, start it again.
restart_while_running() {
    local unit=$1 a b
    # Leave $unit running whatever happens, or every later scenario fails
    # as well. Runs when the scenario's subshell exits (also via `fail`).
    # shellcheck disable=SC2064 # $unit is expanded now, on purpose
    trap "systemctl start $unit || true" EXIT
    free_group
    a=$(add alice -g free -- 'sleep 3; exit 5')
    b=$(add bob -g free -- 'sleep 9; echo finished')
    # The daemon shows a job as running as soon as it asks the helper to
    # start it; wait for the units, so that the restart below finds both
    # jobs started rather than racing their spawns (scenario 10 restarts
    # the helper while spawns are in flight).
    wait_state "$a" running
    wait_state "$b" running
    wait_unit "$a"
    wait_unit "$b"
    systemctl restart "$unit"
    wait_socket
    sleep 1
    expect_state "$a" running
    expect_state "$b" running
    for id in "$a" "$b"; do
        systemctl is-active --quiet "gpueue-job-$id.service" || fail "job $id died with the restart of $unit"
    done
    systemctl stop "$unit"
    sleep 4 # job a exits while $unit is down
    systemctl is-active --quiet "gpueue-job-$b.service" || fail "job $b died with $unit"
    systemctl start "$unit"
    wait_socket
    wait_state "$a" done/failed/5 15
    expect_state "$b" running
    wait_state "$b" done/success 20
    grep -qx finished "$LOGS/1001/$b.log" || fail "job $b's output is missing"
}
s5_daemon_restart() { restart_while_running gpueue.service; }
s6_helper_restart() { restart_while_running gpueue-helper.service; }

s8_shutdown() {
    local a b c d lines
    free_group
    a=$(add alice -g free -- "$COUNTER")
    b=$(add bob -g free -- "$COUNTER")
    c=$(add alice -g free --no-checkpoint -- 'trap "echo got TERM; exit 0" TERM; while :; do sleep 0.1; done')
    d=$(add alice -g free -- "$COUNTER")
    for i in "$a" "$b" "$c" "$d"; do wait_state "$i" running; done
    sleep 1
    # A paused job is dumped too, and comes back paused.
    pz alice pause "$d" || fail "pause $d"
    wait_state "$d" paused
    ! pz alice shutdown 2>/dev/null || fail "a regular user can shut the daemon down"
    pz carol shutdown || fail "shutdown"
    wait_for 120 "the daemon to exit" bash -c '! systemctl is-active --quiet gpueue.service'
    for i in "$a" "$b" "$c" "$d"; do
        ! systemctl is-active --quiet "gpueue-job-$i.service" || fail "job $i still runs after shutdown"
    done
    [ -e "$CKPT/$a" ] && [ -e "$CKPT/$b" ] && [ -e "$CKPT/$d" ] || fail "no images for jobs $a, $b and $d"
    systemctl start gpueue.service
    wait_socket
    expect_state "$a" checkpointed
    expect_state "$b" checkpointed
    expect_state "$d" checkpointed
    expect_state "$c" 'done/*'
    grep -q 'got TERM' "$LOGS/1000/$c.log" || fail "the no-checkpoint job was not terminated gracefully"
    local before_a before_b
    before_a=$(last_line 1000 "$a")
    before_b=$(last_line 1001 "$b")
    pz alice restore "$a" || fail "restore $a"
    pz bob restore "$b" || fail "restore $b"
    wait_state "$a" running 120
    wait_state "$b" running 120
    sleep 1.5
    [ "$(last_line 1000 "$a")" -gt "$before_a" ] || fail "job $a did not continue"
    [ "$(last_line 1001 "$b")" -gt "$before_b" ] || fail "job $b did not continue"
    check_counter_log 1000 "$a"
    check_counter_log 1001 "$b"
    lines=$(log_lines 1000 "$d")
    pz alice restore "$d" || fail "restore $d"
    wait_state "$d" paused 120
    expect_paused 1000 "$d" "$lines"
    pz alice start "$d" || fail "start $d"
    wait_state "$d" running
    expect_continues 1000 "$d"
    check_counter_log 1000 "$d"
    pz carol kill "$a" "$b" "$d"
    wait_state "$a" 'done/*'
    wait_state "$b" 'done/*'
    wait_state "$d" 'done/*'
}

# A job paused before its checkpoint is restored paused (the dump needs it
# running, so it is resumed for the dump; the restore leaves it stopped, so
# it runs nothing before `gpueue start` resumes it). `pause` is refused while a dump
# is in progress; the job is then checkpointed as asked and restored
# running.
s13_paused_checkpoint() {
    local id big state out lines
    free_group
    id=$(add alice -g free -- "$COUNTER")
    wait_state "$id" running
    sleep 1
    pz alice pause "$id" || fail "pause"
    wait_state "$id" paused
    pz alice checkpoint "$id" || fail "checkpoint"
    wait_state "$id" checkpointed 120
    lines=$(log_lines 1000 "$id")
    pz alice restore "$id" || fail "restore"
    wait_state "$id" paused 120
    expect_paused 1000 "$id" "$lines"
    pz alice start "$id" || fail "start"
    wait_state "$id" running
    expect_continues 1000 "$id"
    check_counter_log 1000 "$id"
    # Checkpointed again while running: restored running.
    pz alice checkpoint "$id" || fail "checkpoint 2"
    wait_state "$id" checkpointed 120
    pz alice restore "$id" || fail "restore 2"
    wait_state "$id" running 120
    expect_continues 1000 "$id"
    check_counter_log 1000 "$id"
    pz alice kill "$id" || fail "kill"
    wait_state "$id" 'done/*'

    # 1 GiB of memory, so that the dump takes a moment.
    big=$(add alice -g free -- 'exec python3 -u /e2e/mem_counter.py 1024')
    wait_state "$big" running
    wait_for 60 "the job to print" bash -c "[ \$(grep -c . $LOGS/1000/$big.log) -ge 3 ]"
    pz alice checkpoint "$big" || fail "checkpoint big"
    state=$(st "$big")
    if out=$(pz alice pause "$big" 2>&1); then
        fail "pause accepted while the job is $state"
    fi
    echo "pause while $state: $out"
    if [ "$state" = checkpointing ]; then
        grep -q 'checkpoint' <<<"$out" || fail "the refusal does not mention the checkpoint: $out"
    else
        echo "(the dump was done before the pause; not a test of a dump in progress)"
    fi
    wait_state "$big" checkpointed 120
    pz alice restore "$big" || fail "restore big"
    wait_state "$big" running 120
    expect_continues 1000 "$big"
    check_counter_log 1000 "$big"
    pz alice kill "$big" || fail "kill big"
    wait_state "$big" 'done/*'
}

# check_cuda_mode <id>: the checkpoint of GPU job <id> was made in the
# configured CUDA mode (GPUEUE_E2E_CUDA_MODE, default manual): in plugin
# mode CRIU loaded cuda_plugin.so, in manual mode it did not.
check_cuda_mode() {
    local mode=${GPUEUE_E2E_CUDA_MODE:-manual}
    grep -q "\"mode\": *\"$mode\"" "$CKPT/$1/meta.json" \
        || fail "the checkpoint of job $1 is not in $mode mode: $(cat "$CKPT/$1/meta.json")"
    if [ "$mode" = plugin ]; then
        grep -q 'cuda_plugin' "$CKPT/$1/dump.log" || fail "CRIU did not load cuda_plugin.so"
    elif grep -q 'cuda_plugin' "$CKPT/$1/dump.log"; then
        fail "CRIU loaded cuda_plugin.so in manual mode"
    fi
}

# GPUEUE_E2E_GPU=1 only: GPU isolation, and a CUDA job (GPU 0 is real)
# keeps its GPU memory across checkpoint/restore in a restore unit that is
# isolated to GPU 0 as well.
s9_gpu() {
    local id before after cpu gpu probe
    pz carol gpu parallel 1 || fail "gpu parallel"
    free_group
    # GPU isolation: a CPU job cannot open any NVIDIA device; a job on GPU 0
    # can open that GPU's nodes. What nvidia-smi sees is printed.
    probe='python3 /e2e/open_dev.py /dev/null /dev/nvidiactl /dev/nvidia-uvm /dev/nvidia0'
    probe="$probe; nvidia-smi -L; echo nvidia-smi exit status \$?"
    cpu=$(add alice -g free -- "$probe")
    gpu=$(add alice -g free --gpu 0 -- "$probe")
    wait_state "$cpu" 'done/*' 60
    wait_state "$gpu" 'done/*' 60
    echo "--- CPU job $cpu:"; cat "$LOGS/1000/$cpu.log"
    echo "--- GPU job $gpu:"; cat "$LOGS/1000/$gpu.log"
    for dev in nvidiactl nvidia-uvm nvidia0; do
        grep -qx "open /dev/$dev: EPERM" "$LOGS/1000/$cpu.log" \
            || fail "the CPU job $cpu could open /dev/$dev"
        grep -qx "open /dev/$dev: ok" "$LOGS/1000/$gpu.log" \
            || fail "the GPU job $gpu could not open /dev/$dev"
    done
    grep -qx "open /dev/null: ok" "$LOGS/1000/$cpu.log" || fail "the CPU job could not open /dev/null"
    id=$(add alice -g free --gpu 0 -- 'exec /opt/venv/bin/python3 -u /e2e/gpu_counter.py')
    wait_state "$id" running
    wait_for 60 "the GPU job to print" bash -c "[ \$(wc -l < $LOGS/1000/$id.log) -ge 5 ]"
    pz alice checkpoint "$id" || fail "checkpoint"
    wait_state "$id" checkpointed 300
    check_cuda_mode "$id"
    before=$(last_line 1000 "$id" | cut -d' ' -f1)
    pz alice restore "$id" || fail "restore"
    wait_state "$id" running 300
    # The restore unit is isolated to the GPU of the restore, too.
    [ "$(systemctl show -p DevicePolicy --value "gpueue-job-$id-r1.service")" = closed ] \
        || fail "the restore unit of job $id is not DevicePolicy=closed"
    systemctl show -p DeviceAllow --value "gpueue-job-$id-r1.service" | grep -q '/dev/nvidia0 rw' \
        || fail "the restore unit of job $id may not open /dev/nvidia0: \
$(systemctl show -p DeviceAllow "gpueue-job-$id-r1.service")"
    sleep 2
    after=$(last_line 1000 "$id" | cut -d' ' -f1)
    echo "GPU counter before dump: $before, after restore: $after"
    [ "$after" -gt "$before" ] || fail "the restored GPU job did not continue"
    check_gpu_log "$id"
    pz alice kill "$id" || fail "kill"
    wait_state "$id" 'done/*'
}

# check_gpu_log <id>: the log of gpu_counter.py job <id> (alice's) has
# consecutive steps, each equal to the counter kept in GPU memory.
check_gpu_log() {
    python3 - "$LOGS/1000/$1.log" <<'EOF' || fail "the GPU job's log has gaps or a wrong GPU value"
import sys
rows = [l.split() for l in open(sys.argv[1]) if l.strip()]
rows = [(int(a), int(b)) for a, b in rows]
assert all(a == b for a, b in rows), "GPU memory not preserved"
assert [a for a, _ in rows] == list(range(1, len(rows) + 1)), "steps not consecutive"
EOF
}

# GPUEUE_E2E_GPU=1 only: s13 for CUDA jobs. A paused CUDA counter and a
# paused PyTorch training job (forked DataLoader workers) are restored
# paused with their GPU state, and continue on `start`; `pause` during a
# GPU dump is refused.
s14_gpu_paused() {
    local id state out lines
    pz carol gpu parallel 1 || fail "gpu parallel"
    free_group
    id=$(add alice -g free --gpu 0 -- 'exec /opt/venv/bin/python3 -u /e2e/gpu_counter.py')
    wait_state "$id" running
    wait_for 60 "the GPU job to print" bash -c "[ \$(wc -l < $LOGS/1000/$id.log) -ge 5 ]"
    pz alice pause "$id" || fail "pause"
    wait_state "$id" paused
    pz alice checkpoint "$id" || fail "checkpoint"
    wait_state "$id" checkpointed 300
    check_cuda_mode "$id"
    lines=$(log_lines 1000 "$id")
    pz alice restore "$id" || fail "restore"
    wait_state "$id" paused 300
    expect_paused 1000 "$id" "$lines"
    pz alice start "$id" || fail "start"
    wait_state "$id" running
    expect_continues 1000 "$id"
    sleep 1
    check_gpu_log "$id"

    # pause during the GPU dump.
    pz alice checkpoint "$id" || fail "checkpoint 2"
    state=$(st "$id")
    if out=$(pz alice pause "$id" 2>&1); then
        fail "pause accepted while the job is $state"
    fi
    echo "pause while $state: $out"
    wait_state "$id" checkpointed 300
    pz alice restore "$id" || fail "restore 2"
    wait_state "$id" running 300
    expect_continues 1000 "$id"
    sleep 1
    check_gpu_log "$id"
    pz alice kill "$id" || fail "kill"
    wait_state "$id" 'done/*'

    say "paused training with fork workers"
    PAUSED_CHECKPOINT=1 gpu_train_variant 60 1 2
}

# GPUEUE_E2E_GPU=1 only: PyTorch training whose DataLoader forks its workers
# (the default start method): the workers inherit the CUDA process's NVIDIA
# device mappings, which CRIU cannot dump; the helper's fork guard drops
# them. Each variant is checkpointed and restored <n> times and must log
# the same losses and parameter checksums as an uninterrupted run, with
# the workers still producing batches after each restore.
# gpu_train_variant <steps> <checkpoints> <gpu_train.py args...>
gpu_train_variant() {
    local steps=$1 n=$2 ref id k lines logged last=0 marks=()
    shift 2
    local cmd="exec /opt/venv/bin/python3 -u /e2e/gpu_train.py $steps $*"
    ref=$(add alice -g free --gpu 0 -- "$cmd")
    wait_state "$ref" 'done/*' 300
    expect_state "$ref" done/success
    head -n1 "$LOGS/1000/$ref.log"
    grep -q 'start_method=fork ' "$LOGS/1000/$ref.log" || fail "the workers are not forked"
    grep -q 'preload=.*gpueue-forkguard-' "$LOGS/1000/$ref.log" || fail "the fork guard is not preloaded"
    id=$(add alice -g free --gpu 0 -- "$cmd")
    wait_state "$id" running
    for k in $(seq "$n"); do
        lines=$((last + 12))
        wait_for 120 "the training job to reach $lines steps" \
            bash -c "[ \$(grep -c ^step= $LOGS/1000/$id.log) -ge $lines ]"
        if [ "${PAUSED_CHECKPOINT:-}" = 1 ]; then
            pz alice pause "$id" || fail "pause $k"
            wait_state "$id" paused
        fi
        pz alice checkpoint "$id" || fail "checkpoint $k"
        wait_state "$id" checkpointed 300
        check_cuda_mode "$id"
        last=$(grep -c ^step= "$LOGS/1000/$id.log")
        marks+=("$last")
        logged=$(log_lines 1000 "$id")
        pz alice restore "$id" || fail "restore $k"
        if [ "${PAUSED_CHECKPOINT:-}" = 1 ]; then
            wait_state "$id" paused 300
            expect_paused 1000 "$id" "$logged"
            pz alice start "$id" || fail "start $k"
        fi
        wait_state "$id" running 300
    done
    wait_state "$id" 'done/*' 300
    expect_state "$id" done/success
    echo "steps at the checkpoints: ${marks[*]}"
    python3 - "$LOGS/1000/$ref.log" "$LOGS/1000/$id.log" "$steps" "${marks[@]}" <<'EOF' || fail "the restored training diverged"
import sys
ref, got, steps = sys.argv[1], sys.argv[2], int(sys.argv[3])
marks = [int(m) for m in sys.argv[4:]]
def rows(path):
    out = []
    for line in open(path):
        if line.startswith("step="):
            f = dict(kv.split("=", 1) for kv in line.split())
            out.append((int(f["step"]), f["loss"], f["csum"], f["worker"]))
    return out
a, b = rows(ref), rows(got)
assert [r[:3] for r in b] == [r[:3] for r in a], "losses/checksums differ from the uninterrupted run"
assert [r[0] for r in b] == list(range(1, steps + 1)), "steps missing or repeated"
for i, m in enumerate(marks):
    end = marks[i + 1] if i + 1 < len(marks) else len(b)
    after = {r[3].split(":")[0] for r in b[m:end]}
    assert m < end and "-1" not in after and len(after) >= 2, f"workers after restore {i + 1}: {after}"
print(f"{len(b)} steps identical to the uninterrupted run")
EOF
}
s11_gpu_fork_workers() {
    pz carol gpu parallel 1 || fail "gpu parallel"
    free_group
    say "fork workers"
    gpu_train_variant 60 1 2
    say "fork workers, pin_memory, persistent_workers"
    gpu_train_variant 60 1 3 pin persistent
    say "fork workers re-forked every epoch, checkpointed twice"
    gpu_train_variant 80 2 2 pin epochs
}

# GPUEUE_E2E_GPU=1 only: NVML detection under the packaged, sandboxed
# gpueue.service. The daemon is restarted with gpu_detection = "auto" and
# no [[gpus]] list; it must find the real GPU with NVML (name, UUID,
# memory and driver as nvidia-smi reports them) and `gpueue doctor` must be
# happy with that. The static test configuration is put back afterwards.
s12_nvml_detection() {
    local since out smi
    mkdir -p "$STATE"
    cp /etc/gpueue/config.toml "$STATE/config-static.toml"
    { grep '^cuda_checkpoint_mode' "$STATE/config-static.toml" || true
      printf 'grace_secs = 5\ngpu_detection = "auto"\n'; } >/etc/gpueue/config.toml
    since=$(date '+%Y-%m-%d %H:%M:%S')
    systemctl restart gpueue.service
    wait_socket
    out=$(pz carol gpu --json) || fail "gpu --json"
    echo "$out"
    smi=$(nvidia-smi --query-gpu=index,uuid,name,memory.total,driver_version \
        --format=csv,noheader,nounits) || fail "nvidia-smi"
    echo "nvidia-smi: $smi"
    python3 - "$out" "$smi" <<'EOF' || fail "gpueue gpu does not show the GPU NVML should detect"
import json, sys
gpus = {g["index"]: g for g in json.loads(sys.argv[1])["gpus"]}
index, uuid, name, mib, driver = [f.strip() for f in sys.argv[2].splitlines()[0].split(",")]
g = gpus[int(index)]
assert g.get("available", True), f"GPU {index} is not available: {g}"
assert g["uuid"] == uuid, f"uuid {g['uuid']} != {uuid}"
assert g["name"] == name, f"name {g['name']} != {name}"
assert g.get("driver_version") == driver, f"driver {g.get('driver_version')} != {driver}"
memory = g.get("memory_total_bytes") or 0
assert abs(memory - (int(mib) << 20)) <= 1 << 20, f"memory {memory} != {mib} MiB"
print(f"NVML detected GPU {index}: {name}, {uuid}, {memory} bytes, driver {driver}")
EOF
    # The journal is captured first (see s2_authz): `journalctl | grep -q`
    # fails when grep's early exit kills journalctl with SIGPIPE.
    local journal
    journal="$(journalctl -u gpueue.service --since "$since" -o cat --no-pager)" \
        || fail "journalctl failed"
    grep -q 'detected 1 GPU(s) with NVML' <<<"$journal" || fail "the daemon did not log NVML detection"
    ! grep 'NVML failed' <<<"$journal" || fail "the daemon logged an NVML failure"
    out=$(pz carol doctor 2>&1) || true
    grep -E '^OK +gpus: 1 GPU\(s\), detected with NVML$' <<<"$out" \
        || fail "doctor does not report NVML detection: $(grep gpus <<<"$out")"

    install -m 0644 "$STATE/config-static.toml" /etc/gpueue/config.toml
    systemctl restart gpueue.service
    wait_socket
}

# s7 is split across two containers by scripts/e2e-test.sh.
# The helper dumps the jobs on its own during the OS shutdown: a running
# and a paused counter (the paused one comes back paused), and with
# GPUEUE_E2E_GPU=1 a running and a paused CUDA counter on GPU 0.
s7_prepare() {
    local a b c g h
    free_group
    a=$(add alice -g free -- "$COUNTER")
    b=$(add bob -g free --no-checkpoint -- 'trap "echo got TERM; exit 0" TERM; while :; do sleep 0.1; done')
    c=$(add alice -g free -- "$COUNTER")
    wait_state "$a" running
    wait_state "$b" running
    wait_state "$c" running
    mkdir -p "$STATE"
    if [ "${GPUEUE_E2E_GPU:-}" = 1 ]; then
        pz carol gpu parallel 2 || fail "gpu parallel"
        g=$(add alice -g free --gpu 0 -- 'exec /opt/venv/bin/python3 -u /e2e/gpu_counter.py')
        h=$(add alice -g free --gpu 0 -- 'exec /opt/venv/bin/python3 -u /e2e/gpu_counter.py')
        for i in "$g" "$h"; do
            wait_state "$i" running
            wait_for 60 "GPU job $i to print" bash -c "[ \$(wc -l < $LOGS/1000/$i.log) -ge 5 ]"
        done
        pz alice pause "$h" || fail "pause $h"
        wait_state "$h" paused
        echo "$g" >"$STATE/g"
        echo "$h" >"$STATE/h"
    fi
    sleep 1.5
    pz alice pause "$c" || fail "pause $c"
    wait_state "$c" paused
    echo "$a" >"$STATE/a"
    echo "$b" >"$STATE/b"
    echo "$c" >"$STATE/c"
}
s7_verify() {
    local a b c g h before after lines
    a=$(cat "$STATE/a")
    b=$(cat "$STATE/b")
    c=$(cat "$STATE/c")
    cat "$CKPT/shutdown-manifest.json" || true
    for i in "$a" "$c"; do
        [ -e "$CKPT/$i/COMPLETE" ] || fail "job $i has no complete image"
        wait_state "$i" checkpointed 30
    done
    wait_state "$b" 'done/*' 30
    grep -q 'got TERM' "$LOGS/1001/$b.log" || fail "the no-checkpoint job was not stopped by systemd"
    before=$(last_line 1000 "$a")
    pz alice restore "$a" || fail "restore"
    wait_state "$a" running 120
    sleep 1.5
    after=$(last_line 1000 "$a")
    echo "counter before the reboot: $before, after restore: $after"
    [ "$after" -gt "$before" ] || fail "the restored job did not continue"
    check_counter_log 1000 "$a"
    pz alice kill "$a"
    wait_state "$a" 'done/*'

    lines=$(log_lines 1000 "$c")
    pz alice restore "$c" || fail "restore $c"
    wait_state "$c" paused 120
    expect_paused 1000 "$c" "$lines"
    pz alice start "$c" || fail "start $c"
    wait_state "$c" running
    expect_continues 1000 "$c"
    check_counter_log 1000 "$c"
    pz alice kill "$c"
    wait_state "$c" 'done/*'

    if [ "${GPUEUE_E2E_GPU:-}" = 1 ]; then
        g=$(cat "$STATE/g")
        h=$(cat "$STATE/h")
        for i in "$g" "$h"; do
            [ -e "$CKPT/$i/COMPLETE" ] || fail "GPU job $i has no complete image"
            wait_state "$i" checkpointed 30
            check_cuda_mode "$i"
        done
        pz alice restore "$g" || fail "restore $g"
        wait_state "$g" running 300
        expect_continues 1000 "$g"
        lines=$(log_lines 1000 "$h")
        pz alice restore "$h" || fail "restore $h"
        wait_state "$h" paused 300
        expect_paused 1000 "$h" "$lines"
        pz alice start "$h" || fail "start $h"
        wait_state "$h" running
        expect_continues 1000 "$h"
        sleep 1
        check_gpu_log "$g"
        check_gpu_log "$h"
        pz alice kill "$g" "$h"
        wait_state "$g" 'done/*'
        wait_state "$h" 'done/*'
    fi
}

# s15/s16: the administrator's mounts of scripts/e2e/mounts.sh (a separate
# ext4 filesystem, a bind-mounted /home/alice, an x-systemd.automount
# /data, a tmpfs /scratch, overlayfs, a read-only bind, a private bind and
# sshfs as the network filesystem) across a reboot: they come back as new
# mounts, the loop-mounted ones on other devices.

# mjob <name> <cwd> <file> [prefix]: a counter job of alice's that appends
# to <file> (fd 3) with its cwd in <cwd>; records "<id> <name> <file>" in
# $STATE/mounts and prints the id.
mjob() {
    local name=$1 cwd=$2 file=$3 pre=${4:-} id
    id=$( (cd "$cwd" && runuser -u alice -- gpueue --color never add -p -g free -- \
        "$pre exec 3>>$file; i=0; while :; do echo \$i >&3; i=\$((i+1)); sleep 0.2; done"))
    echo "$id $name $file" >>"$STATE/mounts"
    echo "$id"
}
# The counter file must be 0, 1, 2, ... without gaps or repeats.
check_counter_file() {
    python3 - "$1" <<'EOF' || fail "$1 has gaps or repeats"
import sys
nums = [int(l) for l in open(sys.argv[1]) if l.strip().isdigit()]
bad = [(a, b) for a, b in zip(nums, nums[1:]) if b != a + 1]
if not nums or nums[0] != 0 or bad:
    print(f"first={nums[:1]} breaks={bad[:5]}", file=sys.stderr)
    sys.exit(1)
EOF
}
# restore_continues <id> <file>: restore a checkpointed job and check that
# it goes on appending to <file> consistently.
restore_continues() {
    local id=$1 file=$2 before after
    wait_state "$id" checkpointed 60
    before=$(tail -n1 "$file")
    pz alice restore "$id" >/dev/null || fail "restore $id"
    wait_state "$id" running 120
    sleep 1.5
    after=$(tail -n1 "$file")
    echo "job $id ($file): $before before the dump, $after after the restore"
    [ "$after" -gt "$before" ] || fail "job $id did not continue"
    check_counter_file "$file"
}
# restore_refused <id> <pattern>: the restore of a checkpointed job must be
# refused (before CRIU runs) with an error matching <pattern>.
restore_refused() {
    local id=$1 err
    wait_state "$id" checkpointed 60
    pz alice restore "$id" >/dev/null 2>&1 || true
    sleep 2
    wait_state "$id" checkpointed 60
    err=$(q carol "job($id)['details'].get('last_error')")
    echo "restore of job $id refused: $err"
    grep -q -- "$2" <<<"$err" || fail "the restore of job $id was not refused clearly: $err"
}
need_mounts() {
    mountpoint -q /srv/ext4 || fail "the mounts of mounts.sh are missing"
}
# mount_dev <path>: major:minor of the filesystem mounted on <path>.
mount_dev() { findmnt -fno MAJ:MIN "$1" | tail -n1; }
# job_mountinfo <id>: mountinfo of the job's namespace.
job_mountinfo() {
    local pid
    pid=$(systemctl show -p MainPID --value "gpueue-job-$1.service")
    cat "/proc/$(pgrep -P "$pid" | head -n1)/mountinfo"
}

s15_prepare() {
    local id name f out p
    need_mounts
    # The sshfs server must outlive the helper's dumps at the reboot: a
    # logind session (a PAM login with libpam-systemd) is stopped early in
    # a shutdown, and every job's dump would fail on the dead /nfs.
    for p in $(pgrep -f /usr/lib/openssh/sftp-server || true); do
        echo "sftp-server $p: $(cat "/proc/$p/cgroup")"
        ! grep -q '/session-[^/]*\.scope' "/proc/$p/cgroup" \
            || fail "the sftp-server behind /nfs runs in a logind session"
    done
    pz carol group | grep -qw free || pz carol group add free -p 0 || fail "group add"
    mkdir -p "$STATE"
    : >"$STATE/mounts"
    mount_dev /srv/ext4 >"$STATE/ext4-dev"
    rm -f /home/alice/m-*.txt /data/alice/m-*.txt /srv/ext4/export/m-*.txt /ovl/m-*.txt \
        /priv/m-*.txt
    # Before the automount of /data is triggered: the job's namespace must
    # still get the filesystem (the shim triggers it) and not an empty
    # directory or a dead automount point.
    systemctl stop data.mount
    out=$(findmnt -n /data) || fail "findmnt /data failed"
    grep -q autofs <<<"$out" || fail "/data is not an automount point"
    ! grep -q ext4 <<<"$out" || fail "/data is still mounted"
    mjob data-before /home/alice /data/alice/m-before.txt 'sleep 1;' >/dev/null
    mjob home /home/alice /home/alice/m-home.txt >/dev/null
    mjob data /data/alice /data/alice/m-data.txt >/dev/null
    mjob ext4 /srv/ext4/export /srv/ext4/export/m-ext4.txt >/dev/null
    mjob overlay /ovl /ovl/m-ovl.txt >/dev/null
    mjob ro-bind /ref /home/alice/m-ref.txt 'exec 4</ref/input.txt;' >/dev/null
    mjob private-bind /priv /priv/m-priv.txt >/dev/null
    if mountpoint -q /nfs; then
        # Also a state file of its own, replaced every few steps (a rename
        # on the network filesystem). /nfs exports /srv/ext4/export, hence
        # another prefix than the ext4 job's. A new state file is created
        # and kept open all the time, so the dump finds the job holding one
        # it created after the helper listed the job's files before the
        # dump (on sshfs it shows as root's: the restore must recognise it
        # as held at the dump, see `gpueue-helper dump-hook`).
        rm -f /nfs/n-*
        mjob sshfs /nfs /nfs/n-nfs.txt \
            '(while :; do exec 5>/nfs/n-state.tmp; date +%s%N >&5; mv /nfs/n-state.tmp /nfs/n-state; done) &' \
            >/dev/null
        for _ in $(seq 50); do [ -s /nfs/n-nfs.txt ] && break; sleep 0.2; done
        stat -c %i /nfs/n-nfs.txt >"$STATE/nfs-ino" 2>/dev/null || true
    elif [ "${GPUEUE_E2E_REQUIRE_SSHFS:-}" = 1 ]; then
        fail "/nfs (sshfs) is not mounted, and GPUEUE_E2E_REQUIRE_SSHFS=1"
    else
        echo "SKIP 15 network filesystem part: /nfs (sshfs) is not mounted"
    fi
    mkdir -p /scratch/alice && chown alice: /scratch/alice
    mjob scratch /scratch/alice /scratch/alice/m-scratch.txt >/dev/null
    while read -r id _ _; do wait_state "$id" running; done <"$STATE/mounts"
    sleep 2
    while read -r id _ f; do
        [ "$(tail -n1 "$f")" -gt 0 ] || fail "job $id does not write $f"
    done <"$STATE/mounts"
    ! runuser -u alice -- touch /ref/x 2>/dev/null || fail "/ref is writable"
    # The job started before the trigger sees the filesystem itself on
    # /data, not the automount point.
    id=$(head -n1 "$STATE/mounts" | cut -d' ' -f1)
    out=$(job_mountinfo "$id") || fail "cannot read the mountinfo of job $id"
    out=$(grep ' /data ' <<<"$out") || fail "the job has no /data"
    grep -q ' - ext4 ' <<<"$out" || fail "the job's /data is not the automounted ext4"
    ! grep -q autofs <<<"$out" || fail "the job's namespace still has the automount point"
    # Before the reboot: a checkpoint and restore of the job on /data.
    id=$(awk '$2 == "data" {print $1}' "$STATE/mounts")
    pz alice checkpoint "$id" >/dev/null || fail "checkpoint"
    restore_continues "$id" /data/alice/m-data.txt
    # The tmpfs still has the job's files on the same boot.
    id=$(awk '$2 == "scratch" {print $1}' "$STATE/mounts")
    pz alice checkpoint "$id" >/dev/null || fail "checkpoint"
    restore_continues "$id" /scratch/alice/m-scratch.txt
    # The jobs stay running: the shutdown dumps them.
}
s15_verify() {
    local id name f a b
    need_mounts
    echo "/srv/ext4 on $(cat "$STATE/ext4-dev") before the reboot, on $(mount_dev /srv/ext4) now"
    [ "$(cat "$STATE/ext4-dev")" != "$(mount_dev /srv/ext4)" ] \
        || echo "note: /srv/ext4 is on the same device as before the reboot"
    if mountpoint -q /nfs; then
        # sshfs numbers inodes in the order they are looked up: look up
        # other files first, so that the job's files surely have other
        # numbers than before the reboot (as on CI, where it happened by
        # chance). The restore must recognise them anyway.
        ls -la /srv/ext4/export >/dev/null
        for f in /nfs/*; do [ "$f" = /nfs/n-nfs.txt ] || stat "$f" >/dev/null; done
        echo "/nfs/n-nfs.txt: inode $(cat "$STATE/nfs-ino" 2>/dev/null || echo '?') before the reboot, $(stat -c %i /nfs/n-nfs.txt) now"
    fi
    while read -r id name f; do
        if [ "$name" = scratch ]; then
            # The tmpfs came back empty.
            restore_refused "$id" 'missing on this host.*/scratch/alice'
            pz alice remove "$id" >/dev/null || fail "remove $id"
        else
            restore_continues "$id" "$f"
        fi
    done <"$STATE/mounts"
    if [ -f /nfs/n-nfs.txt ]; then
        a=$(cat /nfs/n-state)
        sleep 1
        b=$(cat /nfs/n-state)
        [ "$b" -gt "$a" ] || fail "the job's state file on sshfs is not updated"
    fi
    while read -r id name _; do
        if [ "$name" != scratch ]; then
            pz alice kill "$id" >/dev/null
            wait_state "$id" 'done/*'
        fi
    done <"$STATE/mounts"
}

s16_prepare() {
    local id
    need_mounts
    pz carol group | grep -qw free || pz carol group add free -p 0 || fail "group add"
    rm -f /data/alice/m-gone.txt
    id=$( (cd /data/alice && runuser -u alice -- gpueue --color never add -p -g free -- \
        'exec 3>>/data/alice/m-gone.txt; i=0; while :; do echo $i >&3; i=$((i+1)); sleep 0.2; done'))
    wait_state "$id" running
    sleep 1.5
    mkdir -p "$STATE"
    echo "$id" >"$STATE/gone"
    # /data does not come back at the next boot (no fstab entry).
    echo /data >>/var/lib/gpueue-e2e-disk/skip-next-boot
}
s16_verify() {
    local id
    id=$(cat "$STATE/gone")
    ! findmnt -n /data >/dev/null || fail "/data is mounted"
    restore_refused "$id" '/data needs ext4 on /data'
    # Mounted again (the automount comes back), the restore works.
    bash /e2e/mounts.sh setup >/dev/null
    restore_continues "$id" /data/alice/m-gone.txt
    pz alice kill "$id" >/dev/null
    wait_state "$id" 'done/*'
}

# s20: a FUSE mount dies while jobs run (its sshfs is killed: the mount
# stays on the host but does not answer; then it is unmounted, as sshfs
# does when its connection drops, but the jobs' namespaces keep their
# copies). Jobs that do not use it are still checkpointed, at once and at
# the shutdown, and restored: the mount is left out of the image and comes
# back as an empty, inaccessible directory while the filesystem is not
# back, and as the host's mount once it is. A job whose working directory
# is on it is refused with a reason naming the mount, and keeps running.
RNFS=/rnfs
mount_rnfs() {
    local disk=/var/lib/gpueue-e2e-disk
    mkdir -p "$RNFS"
    sshfs -o allow_other,default_permissions,IdentityFile="$disk/ssh/id",StrictHostKeyChecking=no,UserKnownHostsFile=/dev/null,LogLevel=ERROR \
        root@127.0.0.1:/srv/ext4/export "$RNFS" || fail "sshfs on $RNFS"
}
# cjob <cwd> <file>: a counter job of alice's with its cwd in <cwd> that
# appends to <file>; prints the id.
cjob() {
    (cd "$1" && runuser -u alice -- gpueue --color never add -p -g free -- \
        "exec 3>>$2; i=0; while :; do echo \$i >&3; i=\$((i+1)); sleep 0.2; done")
}
# live_pid <id>: host PID of the init of the job's namespace, restored or
# not.
active_unit() {
    systemctl list-units --no-legend --plain --state=active "gpueue-job-$1.service" \
        "gpueue-job-$1-r*.service" | awk 'NR == 1 {print $1}'
}
has_active_unit() { [ -n "$(active_unit "$1")" ]; }
live_pid() {
    local unit pid
    wait_for 30 "an active unit of job $1" has_active_unit "$1"
    unit=$(active_unit "$1")
    pid=$(systemctl show -p MainPID --value "$unit")
    case "$unit" in
    *-r*) echo "$pid" ;;
    *) pgrep -P "$pid" | head -n1 ;;
    esac
}
# rnfs_in <id>: the filesystem type the job has on $RNFS.
rnfs_in() {
    awk -v mp="$RNFS" '$5 == mp {for (i = 7; i <= NF; i++) if ($i == "-") {print $(i + 1); exit}}' \
        "/proc/$(live_pid "$1")/mountinfo"
}
rnfs_dead() { ! timeout 5 stat -f "$RNFS" >/dev/null 2>&1; }

s20_prepare() {
    local u s d n i err
    need_mounts
    if ! command -v sshfs >/dev/null || ! pgrep -x sshd >/dev/null; then
        [ "${GPUEUE_E2E_REQUIRE_SSHFS:-}" != 1 ] || fail "sshfs is missing, and GPUEUE_E2E_REQUIRE_SSHFS=1"
        echo "SKIP 20 a dead FUSE mount: sshfs is not available"
        return 0
    fi
    pz carol group | grep -qw free || pz carol group add free -p 0 || fail "group add"
    mkdir -p "$STATE"
    rm -f "$STATE/s20" /home/alice/m20-*.txt
    mountpoint -q "$RNFS" || mount_rnfs
    u=$(cjob /home/alice /home/alice/m20-u.txt)
    s=$(cjob /home/alice /home/alice/m20-s.txt)
    d=$(cjob "$RNFS" /home/alice/m20-d.txt)
    for i in "$u" "$s" "$d"; do wait_state "$i" running; done
    sleep 1
    # The FUSE daemon dies: the mount stays, but does not answer.
    pkill -9 -f "sshfs .* $RNFS\$" || fail "no sshfs for $RNFS"
    wait_for 10 "$RNFS to stop answering" rnfs_dead
    # A job started now gets the dead mount too.
    n=$(cjob /home/alice /home/alice/m20-n.txt)
    wait_state "$n" running
    [ "$(rnfs_in "$n")" = fuse.sshfs ] || fail "job $n has no $RNFS"
    sleep 1
    # Checkpointed and restored on this boot: the job gets an empty,
    # inaccessible directory on $RNFS (the host's mount does not answer).
    pz alice checkpoint "$u" >/dev/null || fail "checkpoint $u"
    restore_continues "$u" /home/alice/m20-u.txt
    [ "$(rnfs_in "$u")" != fuse.sshfs ] || fail "the restored job $u got the dead mount back"
    ! nsenter -t "$(live_pid "$u")" -m runuser -u alice -- ls "$RNFS" 2>/dev/null \
        || fail "the placeholder on $RNFS of job $u is accessible to alice"
    # The job working in it is refused, with the mount named, and runs on.
    err=$(pz alice checkpoint "$d" 2>&1) && fail "the checkpoint of job $d was not refused"
    echo "checkpoint of job $d: $err"
    grep -q "$RNFS (fuse.sshfs) does not answer" <<<"$err" || fail "the refusal does not name $RNFS: $err"
    grep -q "the working directory $RNFS" <<<"$err" || fail "the refusal does not say why: $err"
    expect_state "$d" running
    # Gone from the host now (sshfs unmounts it when its connection drops);
    # the jobs' namespaces keep their copies, which the shutdown dumps.
    umount -l "$RNFS"
    echo "$u $s $d $n" >"$STATE/s20"
}
s20_verify() {
    local u s d n i err
    [ -f "$STATE/s20" ] || { echo "SKIP 20 a dead FUSE mount: not prepared"; return 0; }
    read -r u s d n <"$STATE/s20"
    ! mountpoint -q "$RNFS" || fail "$RNFS is mounted"
    for i in "$u" "$s" "$n"; do wait_state "$i" checkpointed 30; done
    # The shutdown did not dump the job that uses the mount, and says why.
    wait_state "$d" 'done/*' 30
    err=$(q carol "job($d)['details'].get('last_error')")
    echo "job $d at the shutdown: $(st "$d"): $err"
    grep -q "$RNFS (fuse.sshfs) is no longer mounted on the host" <<<"$err" \
        || fail "the shutdown's refusal of job $d does not name $RNFS: $err"
    [ ! -e "$CKPT/$d/COMPLETE" ] || fail "job $d has an image"
    # The new container has no $RNFS directory (a real reboot keeps it):
    # the restore needs the mount point.
    if [ ! -e "$RNFS" ]; then
        restore_refused "$n" "mount points .*$RNFS.* left out of the checkpoint"
        mkdir "$RNFS"
    fi
    # The filesystem is not back: an empty, inaccessible directory.
    restore_continues "$n" /home/alice/m20-n.txt
    [ "$(rnfs_in "$n")" != fuse.sshfs ] || fail "job $n has a FUSE mount on $RNFS"
    # Back: the host's mount.
    mount_rnfs
    restore_continues "$s" /home/alice/m20-s.txt
    [ "$(rnfs_in "$s")" = fuse.sshfs ] || fail "job $s did not get $RNFS back"
    # Dumped with the placeholder of its first restore.
    restore_continues "$u" /home/alice/m20-u.txt
    pz alice kill "$u" "$s" "$n" >/dev/null
    for i in "$u" "$s" "$n"; do wait_state "$i" 'done/*'; done
    umount "$RNFS" || umount -l "$RNFS"
}

# s21: a FUSE mount of the job's owner without allow_other (as an
# AppImage's, or a user's own sshfs): it answers, but refuses root, so CRIU
# could not bind it on restore. A job that does not use it is checkpointed
# with the mount left out and restored with an empty, inaccessible
# directory there; a job working in it is refused, with the mount named,
# and keeps running. One under the host's /tmp is not in the job's
# namespace at all (its private /tmp covers it).
PFUSE=/home/alice/pfuse
TFUSE=/tmp/e2e-tfuse
s21_private_fuse() {
    local disk=/var/lib/gpueue-e2e-disk u d err
    need_mounts
    if ! command -v sshfs >/dev/null || ! pgrep -x sshd >/dev/null; then
        [ "${GPUEUE_E2E_REQUIRE_SSHFS:-}" != 1 ] || fail "sshfs is missing, and GPUEUE_E2E_REQUIRE_SSHFS=1"
        echo "SKIP 21 a FUSE mount that refuses root: sshfs is not available"
        return 0
    fi
    pz carol group | grep -qw free || pz carol group add free -p 0 || fail "group add"
    rm -f /home/alice/m21-*.txt
    install -o alice -m 600 "$disk/ssh/id" /home/alice/.e2e-ssh-id
    runuser -u alice -- mkdir -p "$PFUSE"
    runuser -u alice -- sshfs -o IdentityFile=/home/alice/.e2e-ssh-id,StrictHostKeyChecking=no,UserKnownHostsFile=/dev/null,LogLevel=ERROR \
        root@127.0.0.1:/srv/ext4/export "$PFUSE" || fail "sshfs of alice on $PFUSE"
    ! stat "$PFUSE" >/dev/null 2>&1 || fail "root can look at alice's FUSE mount"
    # The same under the host's /tmp (as an AppImage's mount): the job's
    # private /tmp covers it, so the job's namespace must not keep it.
    runuser -u alice -- mkdir -p "$TFUSE"
    runuser -u alice -- sshfs -o IdentityFile=/home/alice/.e2e-ssh-id,StrictHostKeyChecking=no,UserKnownHostsFile=/dev/null,LogLevel=ERROR \
        root@127.0.0.1:/srv/ext4/export "$TFUSE" || fail "sshfs of alice on $TFUSE"
    u=$(cjob /home/alice /home/alice/m21-u.txt)
    # (Only alice can enter $PFUSE: not `cjob`, which changes into it as root.)
    d=$(runuser -u alice -- gpueue --color never add -p -g free -w "$PFUSE" -- \
        "exec 3>>/home/alice/m21-d.txt; i=0; while :; do echo \$i >&3; i=\$((i+1)); sleep 0.2; done")
    wait_state "$u" running
    wait_state "$d" running
    sleep 1
    pz alice checkpoint "$u" >/dev/null || fail "checkpoint $u"
    restore_continues "$u" /home/alice/m21-u.txt
    [ "$(awk -v mp="$PFUSE" '$5 == mp {for (i = 7; i <= NF; i++) if ($i == "-") {print $(i + 1); exit}}' \
        "/proc/$(live_pid "$u")/mountinfo")" != fuse.sshfs ] \
        || fail "the restored job $u got the FUSE mount that refuses root"
    ! grep -q " $TFUSE " "/proc/$(live_pid "$u")/mountinfo" \
        || fail "the restored job $u has $TFUSE, hidden under its /tmp"
    err=$(pz alice checkpoint "$d" 2>&1) && fail "the checkpoint of job $d was not refused"
    echo "checkpoint of job $d: $err"
    grep -q "$PFUSE (fuse.sshfs) cannot be accessed by root" <<<"$err" \
        || fail "the refusal does not name $PFUSE: $err"
    grep -q "the working directory $PFUSE" <<<"$err" || fail "the refusal does not say why: $err"
    expect_state "$d" running
    pz alice kill "$u" "$d" >/dev/null
    wait_state "$u" 'done/*'
    wait_state "$d" 'done/*'
    runuser -u alice -- fusermount3 -u "$PFUSE" 2>/dev/null || umount -l "$PFUSE"
    runuser -u alice -- fusermount3 -u "$TFUSE" 2>/dev/null || umount -l "$TFUSE"
    rm -f /home/alice/.e2e-ssh-id
}

# s22: FUSE mounts of the job's owner in directories the owner controls.
# - One with allow_other (user_allow_other in /etc/fuse.conf) in alice's
#   home: CRIU would bind it on restore by its path, as root, following
#   symlinks on the way, and alice could have renamed a directory on the
#   way and left a symlink to another directory in its place. A job that
#   does not use it is checkpointed with it left out and gets an empty,
#   inaccessible directory there; a job working in it is refused.
# - One without allow_other in a directory of alice's below a /tmp that is
#   a plain directory (made so here if it is a mount): the shim cannot
#   reach it to detach it before the private /tmp covers it, and must
#   still leave it out of the job's namespace.
AFUSE=/home/alice/a22/afuse
UFUSE=/tmp/alice-dir/fuse
s22_owner_fuse() {
    local disk=/var/lib/gpueue-e2e-disk u d err tmp_was_mount=
    need_mounts
    if ! command -v sshfs >/dev/null || ! pgrep -x sshd >/dev/null; then
        [ "${GPUEUE_E2E_REQUIRE_SSHFS:-}" != 1 ] || fail "sshfs is missing, and GPUEUE_E2E_REQUIRE_SSHFS=1"
        echo "SKIP 22 FUSE mounts in the owner's directories: sshfs is not available"
        return 0
    fi
    pz carol group | grep -qw free || pz carol group add free -p 0 || fail "group add"
    rm -f /home/alice/m22-*.txt
    install -o alice -m 600 "$disk/ssh/id" /home/alice/.e2e-ssh-id
    cp -a /etc/fuse.conf /etc/fuse.conf.e2e 2>/dev/null || : >/etc/fuse.conf.e2e
    echo user_allow_other >>/etc/fuse.conf
    local sshfs_opts=IdentityFile=/home/alice/.e2e-ssh-id,StrictHostKeyChecking=no,UserKnownHostsFile=/dev/null,LogLevel=ERROR
    runuser -u alice -- mkdir -p "$AFUSE"
    runuser -u alice -- sshfs -o "allow_other,$sshfs_opts" root@127.0.0.1:/srv/ext4/export "$AFUSE" \
        || fail "sshfs of alice with allow_other on $AFUSE"
    stat "$AFUSE" >/dev/null || fail "root cannot look at alice's allow_other mount"
    if mountpoint -q /tmp; then
        tmp_was_mount=1
        umount -l /tmp
    fi
    runuser -u alice -- mkdir -p "$UFUSE"
    runuser -u alice -- sshfs -o "$sshfs_opts" root@127.0.0.1:/srv/ext4/export "$UFUSE" \
        || fail "sshfs of alice on $UFUSE"
    u=$(cjob /home/alice /home/alice/m22-u.txt)
    d=$(cjob "$AFUSE" /home/alice/m22-d.txt)
    wait_state "$u" running
    wait_state "$d" running
    sleep 1
    ! grep -q " $UFUSE " "/proc/$(live_pid "$u")/mountinfo" \
        || fail "job $u has $UFUSE, hidden under its /tmp"
    pz alice checkpoint "$u" >/dev/null || fail "checkpoint $u"
    restore_continues "$u" /home/alice/m22-u.txt
    [ "$(awk -v mp="$AFUSE" '$5 == mp {for (i = 7; i <= NF; i++) if ($i == "-") {print $(i + 1); exit}}' \
        "/proc/$(live_pid "$u")/mountinfo")" != fuse.sshfs ] \
        || fail "the restored job $u got $AFUSE bound from the host"
    ! grep -q " $UFUSE " "/proc/$(live_pid "$u")/mountinfo" \
        || fail "the restored job $u has $UFUSE"
    err=$(pz alice checkpoint "$d" 2>&1) && fail "the checkpoint of job $d was not refused"
    echo "checkpoint of job $d: $err"
    grep -q "$AFUSE (fuse.sshfs) could only be restored by binding a host path that someone other than root can redirect: $AFUSE (/home/alice is not a directory that only root can change)" <<<"$err" \
        || fail "the refusal does not name $AFUSE: $err"
    grep -q "the working directory $AFUSE" <<<"$err" || fail "the refusal does not say why: $err"
    expect_state "$d" running
    pz alice kill "$u" "$d" >/dev/null
    wait_state "$u" 'done/*'
    wait_state "$d" 'done/*'
    runuser -u alice -- fusermount3 -u "$AFUSE" 2>/dev/null || umount -l "$AFUSE"
    runuser -u alice -- fusermount3 -u "$UFUSE" 2>/dev/null || umount -l "$UFUSE"
    rm -rf /tmp/alice-dir
    if [ -n "$tmp_was_mount" ]; then mount -t tmpfs -o mode=1777 tmpfs /tmp; fi
    mv /etc/fuse.conf.e2e /etc/fuse.conf
    rm -f /home/alice/.e2e-ssh-id
}

# s23: a job with processes in a PID and user namespace of their own
# (`unshare -Urpf`, as bwrap or a rootless container runtime): CRIU cannot
# dump nested namespaces, and the helper's scans would not see the
# processes in it, so the checkpoint is refused up front, with the reason,
# and the job keeps running.
s23_nested_namespaces() {
    local j err
    if ! runuser -u alice -- unshare -Urpf --mount-proc true 2>/dev/null; then
        echo "SKIP 23 nested namespaces: unprivileged user namespaces are not available"
        return 0
    fi
    pz carol group | grep -qw free || pz carol group add free -p 0 || fail "group add"
    j=$(add alice -g free -- unshare -Urpf --mount-proc sleep 1000)
    wait_state "$j" running
    sleep 1
    err=$(pz alice checkpoint "$j" 2>&1) && fail "the checkpoint of job $j was not refused"
    echo "checkpoint of job $j: $err"
    grep -q "processes in a PID or user namespace of their own" <<<"$err" \
        || fail "the refusal does not say why: $err"
    expect_state "$j" running
    pz alice kill "$j" >/dev/null
    wait_state "$j" 'done/*'
}

# Many jobs at once: `add --immediate` from several CLIs in parallel races
# the scheduler, so the helper gets concurrent spawns; then again while the
# helper restarts. No job may fail to spawn or get lost.
s10_burst() {
    local ids=() i id u
    burst() { # burst <tag> <count> <command>: add jobs in parallel, ids in $STATE/burst-<tag>
        local tag=$1 n=$2 cmd=$3 k
        mkdir -p "$STATE"
        rm -f "$STATE"/burst-"$tag"-*
        for k in $(seq "$n"); do
            if [ $((k % 2)) -eq 0 ]; then u=alice; else u=bob; fi
            if [ $((k % 3)) -eq 0 ]; then
                add "$u" -g free -- "$cmd" >"$STATE/burst-$tag-$k" &
            else
                add "$u" -g free -i -- "$cmd" >"$STATE/burst-$tag-$k" &
            fi
        done
        wait
        cat "$STATE"/burst-"$tag"-*
    }
    free_group

    mapfile -t ids < <(burst a 24 'echo ok; sleep 1')
    [ "${#ids[@]}" = 24 ] || fail "only ${#ids[@]} of 24 jobs were added"
    for id in "${ids[@]}"; do wait_state "$id" 'done/*' 60; done
    for id in "${ids[@]}"; do expect_state "$id" done/success; done

    # Again, with a helper restart while the spawns are in flight. An
    # `add --immediate` that finds the helper away is refused (no job).
    burst b 16 'echo ok; sleep 3' >/dev/null 2>&1 &
    sleep 0.15
    systemctl restart gpueue-helper.service
    wait
    mapfile -t ids < <(cat "$STATE"/burst-b-*)
    echo "${#ids[@]} of 16 jobs added around the helper restart"
    [ "${#ids[@]}" -ge 8 ] || fail "only ${#ids[@]} of 16 jobs were added"
    wait_socket
    for id in "${ids[@]}"; do wait_state "$id" 'done/*' 90; done
    for id in "${ids[@]}"; do expect_state "$id" done/success; done
    for id in "${ids[@]}"; do
        u=$(q carol "job($id)['owner']")
        grep -qx ok "$LOGS/$u/$id.log" || fail "job $id has no output"
    done
}

# prio_ids <user>: the pending jobs as `gpueue prio --json` orders them.
prio_ids() {
    pz "$1" prio --json | python3 -c 'import json, sys; print(" ".join(str(j["id"]) for j in json.load(sys.stdin)["jobs"]))'
}
# raw_usage <user> <uid>: the decayed usage of <uid> as `gpueue share --json` shows it.
raw_usage() {
    pz "$1" share --json | python3 -c "import json, sys; print(next((u['raw_usage'] for u in json.load(sys.stdin)['users'] if u['uid'] == $2), 0))"
}
bob_charged() { python3 -c "import sys; sys.exit(0 if float('$(raw_usage carol 1001)') > 0 else 1)"; }

s17_priority() {
    local run bobs daves blocker x1 x2 x3 out
    free_group
    # dave has never used the machine; bob uses GPU 0 for a while.
    getent passwd 1003 >/dev/null || useradd -m -u 1003 -s /bin/sh dave
    run=$(add bob -g free --gpu 0 -- 'sleep 4')
    wait_state "$run" running
    bobs=$(add bob -g free --gpu 0 -- 'echo bob')
    sleep 0.3
    daves=$(add dave -g free --gpu 0 -- 'echo dave')
    # Usage is recalculated every second (priority_calc_period_secs = 1).
    wait_for 30 "bob's usage to be recorded" bob_charged
    out=$(pz dave share) || fail "share"
    echo "$out"
    grep -q bob <<<"$out" || fail "share does not list bob"
    out=$(pz dave prio) || fail "prio"
    echo "$out"
    ! grep -q 'echo bob' <<<"$out" || fail "prio shows another user's command"
    [ "$(prio_ids dave)" = "$daves $bobs" ] || fail "dave's job is not first: $(prio_ids dave)"
    wait_state "$bobs" 'done/*' 60
    expect_state "$daves" done/success
    [ "$(started_at "$daves")" -le "$(started_at "$bobs")" ] \
        || fail "bob's older job started before the job of dave, who used less"

    # top: alice's newest job goes first among her own jobs.
    blocker=$(add alice -g free --gpu 1 -- 'sleep 3')
    wait_state "$blocker" running
    x1=$(add alice -g free --gpu 1 -- 'true')
    x2=$(add alice -g free --gpu 1 -- 'true')
    x3=$(add alice -g free --gpu 1 -- 'true')
    ! pz alice nice "$x1" -5 2>/dev/null || fail "a user set a negative nice value"
    ! pz bob top "$x1" 2>/dev/null || fail "bob moved alice's job"
    ! pz alice share set alice 100 2>/dev/null || fail "a user set shares"
    pz alice nice "$x1" 10 || fail "nice"
    pz alice top "$x3" || fail "top"
    [ "$(prio_ids alice)" = "$x3 $x2 $x1" ] || fail "order after top and nice: $(prio_ids alice)"
    pz carol share set bob 2 || fail "share set"
    pz carol group priority free 1 || fail "group priority"
    for id in "$x1" "$x2" "$x3"; do wait_state "$id" 'done/*' 60; done
    [ "$(started_at "$x3")" -le "$(started_at "$x2")" ] || fail "job $x3 did not start first"
    [ "$(started_at "$x2")" -le "$(started_at "$x1")" ] || fail "the niced job $x1 did not start last"
}

s18_job_policy() {
    local id log groups journal
    # A system account (uid below UID_MIN of /etc/login.defs, or 1000).
    getent passwd 990 >/dev/null || useradd -m -u 990 -s /bin/sh svcacct
    id=$(add svcacct -- 'echo should not run')
    wait_state "$id" done/failed_to_spawn
    # The log header (with the reason) goes to stderr.
    log=$(pz svcacct log "$id" 2>&1) || fail "log failed"
    echo "$log"
    grep -q 'gpueue refuses to run jobs as svcacct: uid 990 is below job_min_uid' <<<"$log" \
        || fail "the log does not show the refusal"

    # A denied supplementary group is left out (the helper's journal says so).
    groupadd -f docker
    usermod -aG docker alice
    id=$(add alice -- 'echo groups=$(id -Gn)')
    PZ_TIMEOUT=30 pz alice wait "$id" >/dev/null || fail "wait failed"
    expect_state "$id" done/success
    log=$(pz alice log --full "$id") || fail "log failed"
    echo "$log"
    journal=$(journalctl -u gpueue-helper.service -o cat)
    grep -q "job $id of alice: leaving out the supplementary groups \\[.*\"docker\"" <<<"$journal" \
        || fail "the helper's journal does not say that docker was left out"
    groups=$(sed -n 's/^groups=//p' <<<"$log")
    [ -n "$groups" ] || fail "the job printed no groups"
    ! grep -qw docker <<<"$groups" || fail "the job runs with the docker group: $groups"
    gpasswd -d alice docker >/dev/null

    # A denied primary group refuses the job.
    getent passwd 1004 >/dev/null || useradd -m -u 1004 -g docker -s /bin/sh dockeruser
    id=$(add dockeruser -- 'echo should not run')
    wait_state "$id" done/failed_to_spawn
    log=$(pz dockeruser log "$id" 2>&1) || fail "log failed"
    grep -q 'gpueue refuses to run jobs as dockeruser: primary group docker is in job_denied_groups' \
        <<<"$log" || fail "the log does not show the refusal: $log"
}

# s19: a job of a user who can become root in every usual way outside gpueue
# (member of sudo with a NOPASSWD rule, a polkit rule trusting the sudo
# group, member of docker with a root daemon socket, member of disk with a
# block device, member of libvirt with world-connectable sockets in
# /run/libvirt, which libvirt authorizes through polkit) cannot do any of
# it, before and after a checkpoint/restore.
# Each way is first shown to work for the same user outside gpueue.
E2E_DOCKER_SOCK=/run/e2e-docker.sock
E2E_LISTENERS=
# listen_world <socket>: as root, listen on a world-connectable Unix
# socket (like libvirt's polkit-authorized ones) in the background, and
# check that eve can connect to it outside gpueue.
listen_world() {
    local sock=$1
    rm -f "$sock"
    python3 -c '
import os, socket, sys
s = socket.socket(socket.AF_UNIX)
s.bind(sys.argv[1])
os.chmod(sys.argv[1], 0o666)
s.listen(16)
while True:
    s.accept()[0].close()' "$sock" &
    E2E_LISTENERS="$E2E_LISTENERS $!"
    wait_for 10 "the socket $sock" test -S "$sock"
    runuser -u eve -- python3 -c 'import socket, sys; socket.socket(socket.AF_UNIX).connect(sys.argv[1])' \
        "$sock" || fail "eve cannot connect to $sock outside gpueue"
}
# The default job_hidden_socket_dirs.
E2E_SOCKET_DIRS="/run/libvirt /run/cups"
# late_sockets <job> <n>: as root, listen on new sockets in /run/dbus,
# /run/systemd (also journal/ and resolve/) and the directories of
# job_hidden_socket_dirs after job <job> started, and add them to the list
# scenario 19's job checks.
late_sockets() {
    local dir sock
    for dir in /run/dbus /run/systemd /run/systemd/journal /run/systemd/resolve $E2E_SOCKET_DIRS; do
        mkdir -p "$dir"
        sock=$dir/e2e-late-$2
        listen_world "$sock"
        echo "$sock" >>/home/eve/e2e-late-sockets
    done
    echo "control: eve can connect to the late sockets (round $2) outside gpueue"
}
s19_no_escalation() {
    local id log dev uid gid out r
    getent group sudo >/dev/null || groupadd sudo
    groupadd -f docker
    getent passwd 1005 >/dev/null || useradd -m -u 1005 -s /bin/bash eve
    usermod -aG sudo,docker,disk eve
    uid=$(id -u eve)
    gid=$(id -g eve)
    rm -f /home/eve/round1 /home/eve/round2 /home/eve/e2e-late-sockets

    # A unit only root (or polkit) may start; it leaves a marker.
    cat >/etc/systemd/system/e2e-escalate.service <<'EOF'
[Service]
Type=oneshot
ExecStart=/bin/touch /run/e2e-escalated
EOF
    systemctl daemon-reload
    rm -f /run/e2e-escalated
    if command -v sudo >/dev/null; then
        echo 'eve ALL=(ALL) NOPASSWD: ALL' >/etc/sudoers.d/e2e-eve
        chmod 0440 /etc/sudoers.d/e2e-eve
        runuser -u eve -- sudo -n true || fail "the sudoers rule does not work outside gpueue"
        echo "control: eve can sudo outside gpueue"
    else
        echo "SKIP: no sudo in the image (the sudo checks are skipped)"
    fi
    if [ -d /usr/share/polkit-1 ] && command -v pkexec >/dev/null; then
        mkdir -p /etc/polkit-1/rules.d
        cat >/etc/polkit-1/rules.d/10-e2e-sudo.rules <<'EOF'
polkit.addRule(function(action, subject) {
    if (subject.isInGroup("sudo")) {
        return polkit.Result.YES;
    }
});
EOF
        systemctl restart polkit.service 2>/dev/null || true
        runuser -u eve -- systemctl --no-ask-password start e2e-escalate.service \
            || fail "the polkit rule does not let eve start units outside gpueue"
        [ -e /run/e2e-escalated ] || fail "the control start left no marker"
        rm -f /run/e2e-escalated
        echo "control: eve can start system units through polkit outside gpueue"
    else
        echo "SKIP: no polkit in the image (no control for the D-Bus checks)"
    fi

    # systemd's Varlink services: every socket under /run/systemd except
    # the kept log and resolution sockets and userdb/ must be out of a
    # job's reach; two polkit-authorized calls are shown to work for eve
    # outside gpueue.
    find /run/systemd -type s \
        -not -path /run/systemd/journal/socket -not -path /run/systemd/journal/stdout \
        -not -path /run/systemd/journal/dev-log -not -path /run/systemd/resolve/io.systemd.Resolve \
        -not -path '/run/systemd/userdb/*' >/home/eve/e2e-varlink-sockets
    echo "Varlink and other sockets under /run/systemd: $(tr '\n' ' ' </home/eve/e2e-varlink-sockets)"
    rm -f /home/eve/e2e-cred-decrypt.json
    if command -v varlinkctl >/dev/null && [ -S /run/systemd/io.systemd.Credentials ] \
        && [ -d /etc/polkit-1/rules.d ]; then
        out=$(varlinkctl call /run/systemd/io.systemd.Credentials io.systemd.Credentials.Encrypt \
            '{"name":"e2e","text":"secret","scope":"system"}') || fail "encrypting a credential"
        python3 -c 'import json, sys; b = json.loads(sys.argv[1])["blob"]; print(json.dumps({"name": "e2e", "blob": b, "scope": "system"}))' \
            "$out" >/home/eve/e2e-cred-decrypt.json
        runuser -u eve -- varlinkctl call /run/systemd/io.systemd.Credentials \
            io.systemd.Credentials.Decrypt "$(cat /home/eve/e2e-cred-decrypt.json)" \
            || fail "the polkit rule does not let eve decrypt system credentials outside gpueue"
        echo "control: eve can decrypt system credentials through Varlink and polkit outside gpueue"
    else
        echo "SKIP: no io.systemd.Credentials or polkit (no Varlink control)"
    fi
    chown eve: /home/eve/e2e-varlink-sockets /home/eve/e2e-cred-decrypt.json 2>/dev/null || true

    # A root daemon socket of group docker.
    rm -f "$E2E_DOCKER_SOCK"
    python3 -c '
import os, socket, sys, time
s = socket.socket(socket.AF_UNIX)
s.bind(sys.argv[1])
os.chown(sys.argv[1], 0, int(sys.argv[2]))
os.chmod(sys.argv[1], 0o660)
s.listen(16)
while True:
    c, _ = s.accept()
    c.close()' "$E2E_DOCKER_SOCK" "$(getent group docker | cut -d: -f3)" &
    wait_for 10 "the fake docker socket" test -S "$E2E_DOCKER_SOCK"
    runuser -u eve -- python3 -c 'import socket, sys; socket.socket(socket.AF_UNIX).connect(sys.argv[1])' \
        "$E2E_DOCKER_SOCK" || fail "eve cannot connect to the docker socket outside gpueue"
    echo "control: eve can connect to the docker socket outside gpueue"

    # A block device of group disk.
    truncate -s 1M "$STATE/e2e-blk.img" 2>/dev/null || { mkdir -p "$STATE"; truncate -s 1M "$STATE/e2e-blk.img"; }
    dev=$(losetup -f --show "$STATE/e2e-blk.img") || fail "losetup"
    chgrp disk "$dev"
    chmod 0660 "$dev"
    runuser -u eve -- python3 -c 'import os, sys; os.open(sys.argv[1], os.O_RDONLY)' "$dev" \
        || fail "eve cannot open $dev outside gpueue"
    echo "control: eve can open $dev outside gpueue"

    # Daemon sockets that authorize by the caller's groups in the user
    # database (libvirt: polkit; CUPS: SystemGroup), as world-connectable
    # stand-ins: one in /run/libvirt, which exists before the job starts,
    # and none in /run/cups, which the shim creates when the image has no
    # CUPS (its late sockets then appear in a directory the job never saw).
    groupadd -f libvirt
    usermod -aG libvirt eve
    command -v cupsd >/dev/null || rm -rf /run/cups
    mkdir -p /run/libvirt
    listen_world /run/libvirt/e2e-virtqemud-sock
    echo /run/libvirt/e2e-virtqemud-sock >/home/eve/e2e-daemon-sockets
    if [ -d /run/cups ]; then
        listen_world /run/cups/e2e-cups.sock
        echo /run/cups/e2e-cups.sock >>/home/eve/e2e-daemon-sockets
    fi
    echo "control: eve can connect to the daemon sockets outside gpueue: $(tr '\n' ' ' </home/eve/e2e-daemon-sockets)"
    : >/home/eve/e2e-late-sockets
    chown eve: /home/eve/e2e-late-sockets /home/eve/e2e-daemon-sockets
    id=$(add eve -- "bash /e2e/escalation_checks.sh $uid $gid $E2E_DOCKER_SOCK $dev /home/eve")
    wait_state "$id" running
    # Sockets that appear after the job started: in the spawned job ...
    late_sockets "$id" 1
    runuser -u eve -- touch /home/eve/round1
    wait_for 120 "the first round of checks" grep -q '^round 1 done$' "$LOGS/$uid/$id.log"
    pz eve checkpoint "$id" || fail "checkpoint"
    wait_state "$id" checkpointed 120
    pz eve restore "$id" || fail "restore"
    wait_state "$id" running 120
    # The restored job's namespace still hides /run/dbus, /run/systemd and
    # the directories of job_hidden_socket_dirs.
    out=$(grep -E ' /run/(dbus|systemd|libvirt|cups) ' \
        "/proc/$(systemctl show -p MainPID --value "gpueue-job-$id-r1.service")/mountinfo" || true)
    echo "restored job's mounts on /run/dbus, /run/systemd, /run/libvirt and /run/cups: $out"
    for r in /run/dbus /run/systemd $E2E_SOCKET_DIRS; do
        grep -q " $r .* - tmpfs " <<<"$out" || fail "the restored job's namespace does not hide $r"
    done
    # ... and in the restored one.
    late_sockets "$id" 2
    runuser -u eve -- touch /home/eve/round2
    wait_state "$id" 'done/*' 120
    expect_state "$id" done/success

    log=$(cat "$LOGS/$uid/$id.log")
    echo "$log"
    ! grep -q '^BAD ' <<<"$log" || fail "a job escalated: $(grep '^BAD ' <<<"$log" | head -n 3)"
    for r in 1 2; do
        grep -qx "round $r done" <<<"$log" || fail "round $r did not finish"
    done
    for r in "systemctl --system start" "busctl --system call" "connect to the system bus socket" \
        "connect to the docker socket" "open the block device" uid gid groups \
        NoNewPrivs CapPrm CapEff CapBnd; do
        [ "$(grep -cx "ok $r" <<<"$log")" = 2 ] || fail "check '$r' did not pass in both rounds"
    done
    if command -v sudo >/dev/null; then
        [ "$(grep -cx 'ok sudo -n true' <<<"$log")" = 2 ] || fail "the sudo check did not run twice"
    fi
    if command -v pkexec >/dev/null; then
        [ "$(grep -cx 'ok pkexec true' <<<"$log")" = 2 ] || fail "the pkexec check did not run twice"
    fi
    if command -v run0 >/dev/null; then
        [ "$(grep -cx 'ok run0 true' <<<"$log")" = 2 ] || fail "the run0 check did not run twice"
    fi
    if [ -s /home/eve/e2e-cred-decrypt.json ]; then
        for r in "varlinkctl Credentials.Decrypt" "varlinkctl Manager.Reload"; do
            [ "$(grep -cx "ok $r" <<<"$log")" = 2 ] || fail "check '$r' did not pass in both rounds"
        done
    fi
    [ "$(grep -cx 'ok journal socket reachable' <<<"$log")" = 2 ] || fail "the journal socket is gone in the job"
    [ "$(grep -c '^ok connect to /run/systemd/' <<<"$log")" = $((2 * $(grep -c . /home/eve/e2e-varlink-sockets))) ] \
        || fail "not every Varlink socket was tried and refused in both rounds"
    for r in 'daemon socket .* hidden$' 'connect to the daemon socket '; do
        [ "$(grep -c "^ok $r" <<<"$log")" = $((2 * $(grep -c . /home/eve/e2e-daemon-sockets))) ] \
            || fail "not every daemon socket passed '$r' in both rounds"
    done
    [ ! -e /run/e2e-escalated ] || fail "a job started the root-only unit"
    # Round 1 checks the sockets created before it, round 2 all of them.
    [ "$(grep -c '^ok late socket .* hidden$' <<<"$log")" = $((3 * $(grep -c . /home/eve/e2e-late-sockets) / 2)) ] \
        || fail "not every late socket was hidden"

    for r in $E2E_LISTENERS; do kill "$r" 2>/dev/null || true; done
    rm -f /run/dbus/e2e-late-* /run/systemd/e2e-late-* /run/systemd/journal/e2e-late-* \
        /run/systemd/resolve/e2e-late-* /run/libvirt/e2e-* /run/cups/e2e-*
    gpasswd -d eve libvirt >/dev/null 2>&1 || true
    kill %1 2>/dev/null || true
    losetup -d "$dev" || true
    rm -f "$E2E_DOCKER_SOCK" /etc/sudoers.d/e2e-eve /etc/polkit-1/rules.d/10-e2e-sudo.rules
}

# s24: an upgrade replaces the helper binary in place while one job runs
# and another is checkpointed (as dpkg, rpm, install.sh and pacman do: a
# new file renamed over the old one), then restarts the helper. Every job's
# process tree contains the helper (`shim-init`), and CRIU restores a
# mapped executable only if it is the same file (and cannot dump a deleted
# one this large): jobs run a root-owned, content-addressed copy of the
# helper under /var/lib/gpueue/lib, which is kept while a job or an image
# needs it and removed at the next start of the helper afterwards.
# job_exe <id>: the executable of the init of job <id>'s live unit.
job_exe() { readlink "/proc/$(live_pid "$1")/exe"; }
s24_helper_upgrade() {
    local a b c exe=/usr/local/bin/gpueue-helper lib=/var/lib/gpueue/lib before old new
    mkdir -p "$STATE"
    cp -p "$exe" "$STATE/helper.orig"
    # Put the original binary back whatever happens (runs when the
    # scenario's subshell exits, also via `fail`).
    # shellcheck disable=SC2064 # expanded now, on purpose
    trap "cp -p '$STATE/helper.orig' '$exe.e2e' && mv -f '$exe.e2e' '$exe'; systemctl restart gpueue-helper.service; wait_helper || true" EXIT
    free_group
    a=$(add alice -g free -- "$COUNTER")
    b=$(add alice -g free -- "$COUNTER")
    wait_state "$a" running
    wait_state "$b" running
    wait_unit "$a"
    wait_unit "$b"
    old=$(job_exe "$a")
    echo "jobs run $old"
    case "$old" in "$lib"/gpueue-helper-*) ;; *) fail "job $a runs $old, not a copy in $lib" ;; esac
    cmp -s "$old" "$exe" || fail "$old is not a copy of $exe"
    [ "$(stat -c '%U %a' "$old")" = "root 755" ] || fail "$old is not root's, mode 0755"
    sleep 1
    pz alice checkpoint "$a" || fail "checkpoint $a"
    wait_state "$a" checkpointed 120
    # Another build: same code, another size and contents.
    cp -p "$exe" "$exe.e2e"
    printf 'gpueue e2e upgrade %s\n' "$(date +%s%N)" >>"$exe.e2e"
    mv -f "$exe.e2e" "$exe"
    systemctl restart gpueue-helper.service
    wait_helper
    sleep 1
    expect_state "$b" running
    [ "$(job_exe "$b")" = "$old" ] || fail "job $b no longer runs $old"
    # A new job runs a copy of the new binary.
    c=$(add alice -g free -- "$COUNTER")
    wait_state "$c" running
    wait_unit "$c"
    new=$(job_exe "$c")
    echo "new jobs run $new"
    [ "$new" != "$old" ] && cmp -s "$new" "$exe" || fail "job $c runs $new, not a copy of the new $exe"
    [ -e "$old" ] || fail "$old was removed while jobs need it"
    # A job dumped before the upgrade is restored by the new helper ...
    before=$(last_line 1000 "$a")
    pz alice restore "$a" || fail "restore $a"
    wait_state "$a" running 120
    sleep 1.5
    [ "$(last_line 1000 "$a")" -gt "$before" ] || fail "job $a (dumped before the upgrade) did not continue"
    [ "$(job_exe "$a")" = "$old" ] || fail "the restored job $a does not run $old"
    # ... and a job that ran across the upgrade is dumped by it and restored.
    pz alice checkpoint "$b" || fail "checkpoint $b"
    wait_state "$b" checkpointed 120
    before=$(last_line 1000 "$b")
    pz alice restore "$b" || fail "restore $b"
    wait_state "$b" running 120
    sleep 1.5
    [ "$(last_line 1000 "$b")" -gt "$before" ] || fail "job $b (started before the upgrade) did not continue"
    check_counter_log 1000 "$a"
    check_counter_log 1000 "$b"
    pz carol kill "$a" "$b" || fail "kill"
    wait_state "$a" 'done/*'
    wait_state "$b" 'done/*'
    # Back to the original binary (a "downgrade"): job c still needs the
    # new copy; once it is gone, the next start removes that copy. (The
    # original's copy stays: earlier scenarios' checkpoints may need it.)
    cp -p "$STATE/helper.orig" "$exe.e2e"
    mv -f "$exe.e2e" "$exe"
    systemctl restart gpueue-helper.service
    wait_helper
    [ -e "$new" ] || fail "$new was removed while job $c runs it"
    [ -e "$old" ] || fail "$old, the current copy, is missing"
    pz carol kill "$c" || fail "kill $c"
    wait_state "$c" 'done/*'
    sleep 1
    systemctl restart gpueue-helper.service
    wait_helper
    [ ! -e "$new" ] || fail "$new is still there, but no job or checkpoint needs it"
    [ -e "$old" ] || fail "$old, the current copy, was removed"
    journalctl -u gpueue-helper.service -o cat | grep -F "removed $new" \
        || fail "the helper did not log the removal of $new"
}

case "${1:-main}" in
main)
    run "1 add/status/log/follow/wait" s1_lifecycle
    run "2 authorization" s2_authz
    run "3 group and GPU limits" s3_limits
    run "4 checkpoint and restore" s4_checkpoint_restore
    run "5 daemon restart" s5_daemon_restart
    run "6 helper restart" s6_helper_restart
    run "8 gpueue shutdown" s8_shutdown
    run "10 many jobs at once" s10_burst
    run "13 checkpoint of a paused job" s13_paused_checkpoint
    run "17 multifactor priority (fair share, nice, top)" s17_priority
    run "18 job credential policy" s18_job_policy
    run "19 no privilege escalation from jobs" s19_no_escalation
    run "23 nested namespaces" s23_nested_namespaces
    run "24 helper binary replaced by an upgrade" s24_helper_upgrade
    if [ "${GPUEUE_E2E_GPU:-}" = 1 ]; then
        run "9 GPU checkpoint and restore" s9_gpu
        run "11 GPU training with forked DataLoader workers" s11_gpu_fork_workers
        run "14 GPU checkpoint of paused jobs" s14_gpu_paused
        run "12 NVML GPU detection in the packaged unit" s12_nvml_detection
    fi
    ;;
reboot-prepare)
    run "7 reboot (prepare)" s7_prepare
    run "15 mounts across a reboot (prepare)" s15_prepare
    run "16 a mount missing after a reboot (prepare)" s16_prepare
    run "20 a dead FUSE mount (prepare)" s20_prepare
    run "21 a FUSE mount that refuses root" s21_private_fuse
    run "22 FUSE mounts in the owner's directories" s22_owner_fuse
    ;;
reboot-verify)
    # 16 first: until it mounts /data again, every restore is refused.
    run "16 a mount missing after a reboot" s16_verify
    run "7 reboot" s7_verify
    run "15 mounts across a reboot" s15_verify
    run "20 a dead FUSE mount" s20_verify
    ;;
# Only defines the helpers above (sourced by scripts/vm/guest.sh).
lib) ;;
*) fail "unknown phase $1" ;;
esac

if [ "${#FAILED[@]}" -ne 0 ]; then
    printf '\nfailed: %s\n' "${FAILED[*]}"
    exit 1
fi
