#!/bin/bash
# Full-stack scenarios; runs inside the systemd container as root (see
# scripts/e2e-test.sh). Users act through the real CLI with runuser:
# alice (1000) and bob (1001) are regular users, carol (1002) is in
# puzuz-admin.
#
# Usage: scenario.sh main | reboot-prepare | reboot-verify
# Prints "PASS <scenario>" / "FAIL <scenario>: <reason>" per scenario and
# exits non-zero if any failed.
set -uo pipefail

LOGS=/var/lib/puzuz/logs
CKPT=/var/lib/puzuz/checkpoints
STATE=/var/lib/puzuz/e2e   # values handed from reboot-prepare to reboot-verify
COUNTER='i=0; while :; do echo $i; i=$((i+1)); sleep 0.2; done'

say() { printf '\n--- %s\n' "$*"; }
fail() {
    echo "FAIL: $*" >&2
    exit 1
}
# pz <user> <puzuz args...>: the CLI as <user>, from their home directory.
pz() {
    local u=$1
    shift
    (cd "/home/$u" && ${PZ_TIMEOUT:+timeout "$PZ_TIMEOUT"} runuser -u "$u" -- puzuz --color never "$@")
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
wait_socket() {
    wait_for 30 "the daemon socket" pz carol status --json >/dev/null 2>&1
}
started_at() { q carol "job($1)['started_at']"; }
finished_at() { q carol "job($1)['finished_at']"; }
log_lines() { grep -c . "$LOGS/$1/$2.log"; }
# proc_states <id>: the states (R, S, T, ...) of the processes of job <id>'s
# live unit, counted, for information.
proc_states() {
    local unit cg
    unit=$(systemctl list-units --no-legend --plain --state=active \
        "puzuz-job-$1.service" "puzuz-job-$1-r*.service" | awk '{print $1}' | tail -n1)
    [ -n "$unit" ] || { echo "no active unit"; return; }
    cg=$(systemctl show -p ControlGroup --value "$unit")
    find "/sys/fs/cgroup$cg" -name cgroup.procs -exec cat {} + 2>/dev/null \
        | while read -r p; do awk '/^State:/ {print $2}' "/proc/$p/status" 2>/dev/null; done \
        | sort | uniq -c | tr -s ' \n' ' '
}
# expect_paused <uid> <id>: job <id> is paused and its log does not grow.
expect_paused() {
    local n
    expect_state "$2" paused
    n=$(log_lines "$1" "$2")
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
    for f in "$CKPT"/*/*.log /var/lib/puzuz/helper/jobs/*/*.log; do
        [ -f "$f" ] || continue
        echo "--- tail of $f"
        grep -E 'Error|Warn' "$f" | tail -n 20
    done
}

FAILED=()
# PUZUZ_E2E_ONLY: space-separated scenario numbers to run (all if empty).
ONLY="${PUZUZ_E2E_ONLY:-}"
run() { # run <name> <function>
    if [ -n "$ONLY" ] && [[ " $ONLY " != *" ${1%% *} "* ]]; then
        return 0
    fi
    say "$1"
    if (set -e; "$2"); then
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
    local id
    id=$(add alice -- 'sleep 300')
    wait_state "$id" running
    [ "$(q bob "job($id).get('details') is None and job($id)['owner'] == 1000")" = true ] \
        || fail "bob sees alice's job unredacted"
    ! pz bob status | grep -q 'sleep 300' || fail "bob's status shows alice's command"
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
}

s4_checkpoint_restore() {
    local id before after
    id=$(add alice -g free -- "$COUNTER")
    wait_state "$id" running
    sleep 1.5
    pz alice checkpoint "$id" || fail "checkpoint"
    wait_state "$id" checkpointed 120
    ! systemctl is-active --quiet "puzuz-job-$id.service" || fail "the unit still runs after the dump"
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
    a=$(add alice -g free -- 'sleep 3; exit 5')
    b=$(add bob -g free -- 'sleep 9; echo finished')
    wait_state "$a" running
    wait_state "$b" running
    systemctl restart "$unit"
    wait_socket
    sleep 1
    expect_state "$a" running
    expect_state "$b" running
    systemctl stop "$unit"
    sleep 4 # job a exits while $unit is down
    systemctl is-active --quiet "puzuz-job-$b.service" || fail "job $b died with $unit"
    systemctl start "$unit"
    wait_socket
    wait_state "$a" done/failed/5 15
    expect_state "$b" running
    wait_state "$b" done/success 20
    grep -qx finished "$LOGS/1001/$b.log" || fail "job $b's output is missing"
}
s5_daemon_restart() { restart_while_running puzuz.service; }
s6_helper_restart() { restart_while_running puzuz-helper.service; }

s8_shutdown() {
    local a b c d
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
    wait_for 120 "the daemon to exit" bash -c '! systemctl is-active --quiet puzuz.service'
    for i in "$a" "$b" "$c" "$d"; do
        ! systemctl is-active --quiet "puzuz-job-$i.service" || fail "job $i still runs after shutdown"
    done
    [ -e "$CKPT/$a" ] && [ -e "$CKPT/$b" ] && [ -e "$CKPT/$d" ] || fail "no images for jobs $a, $b and $d"
    systemctl start puzuz.service
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
    pz alice restore "$d" || fail "restore $d"
    wait_state "$d" paused 120
    expect_paused 1000 "$d"
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
# running, so it is resumed for the dump and stopped again after the
# restore), and `puzuz start` resumes it. `pause` is refused while a dump
# is in progress; the job is then checkpointed as asked and restored
# running.
s13_paused_checkpoint() {
    local id big state out
    pz carol group | grep -qw free || pz carol group add free -p 0 || fail "group add"
    id=$(add alice -g free -- "$COUNTER")
    wait_state "$id" running
    sleep 1
    pz alice pause "$id" || fail "pause"
    wait_state "$id" paused
    pz alice checkpoint "$id" || fail "checkpoint"
    wait_state "$id" checkpointed 120
    pz alice restore "$id" || fail "restore"
    wait_state "$id" paused 120
    expect_paused 1000 "$id"
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
# configured CUDA mode (PUZUZ_E2E_CUDA_MODE, default manual): in plugin
# mode CRIU loaded cuda_plugin.so, in manual mode it did not.
check_cuda_mode() {
    local mode=${PUZUZ_E2E_CUDA_MODE:-manual}
    grep -q "\"mode\": *\"$mode\"" "$CKPT/$1/meta.json" \
        || fail "the checkpoint of job $1 is not in $mode mode: $(cat "$CKPT/$1/meta.json")"
    if [ "$mode" = plugin ]; then
        grep -q 'cuda_plugin' "$CKPT/$1/dump.log" || fail "CRIU did not load cuda_plugin.so"
    elif grep -q 'cuda_plugin' "$CKPT/$1/dump.log"; then
        fail "CRIU loaded cuda_plugin.so in manual mode"
    fi
}

# PUZUZ_E2E_GPU=1 only: a CUDA job (GPU 0 is real) keeps its GPU memory
# across checkpoint/restore.
s9_gpu() {
    local id before after
    pz carol gpu parallel 1 || fail "gpu parallel"
    # Created by s3 unless only GPU scenarios run.
    pz carol group | grep -qw free || pz carol group add free -p 0 || fail "group add"
    id=$(add alice -g free --gpu 0 -- 'exec /opt/venv/bin/python3 -u /e2e/gpu_counter.py')
    wait_state "$id" running
    wait_for 60 "the GPU job to print" bash -c "[ \$(wc -l < $LOGS/1000/$id.log) -ge 5 ]"
    pz alice checkpoint "$id" || fail "checkpoint"
    wait_state "$id" checkpointed 300
    check_cuda_mode "$id"
    before=$(last_line 1000 "$id" | cut -d' ' -f1)
    pz alice restore "$id" || fail "restore"
    wait_state "$id" running 300
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

# PUZUZ_E2E_GPU=1 only: s13 for CUDA jobs. A paused CUDA counter and a
# paused PyTorch training job (forked DataLoader workers) are restored
# paused with their GPU state, and continue on `start`; `pause` during a
# GPU dump is refused.
s14_gpu_paused() {
    local id state out
    pz carol gpu parallel 1 || fail "gpu parallel"
    pz carol group | grep -qw free || pz carol group add free -p 0 || fail "group add"
    id=$(add alice -g free --gpu 0 -- 'exec /opt/venv/bin/python3 -u /e2e/gpu_counter.py')
    wait_state "$id" running
    wait_for 60 "the GPU job to print" bash -c "[ \$(wc -l < $LOGS/1000/$id.log) -ge 5 ]"
    pz alice pause "$id" || fail "pause"
    wait_state "$id" paused
    pz alice checkpoint "$id" || fail "checkpoint"
    wait_state "$id" checkpointed 300
    check_cuda_mode "$id"
    pz alice restore "$id" || fail "restore"
    wait_state "$id" paused 300
    expect_paused 1000 "$id"
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

# PUZUZ_E2E_GPU=1 only: PyTorch training whose DataLoader forks its workers
# (the default start method): the workers inherit the CUDA process's NVIDIA
# device mappings, which CRIU cannot dump; the helper's fork guard drops
# them. Each variant is checkpointed and restored <n> times and must log
# the same losses and parameter checksums as an uninterrupted run, with
# the workers still producing batches after each restore.
# gpu_train_variant <steps> <checkpoints> <gpu_train.py args...>
gpu_train_variant() {
    local steps=$1 n=$2 ref id k lines last=0 marks=()
    shift 2
    local cmd="exec /opt/venv/bin/python3 -u /e2e/gpu_train.py $steps $*"
    ref=$(add alice -g free --gpu 0 -- "$cmd")
    wait_state "$ref" 'done/*' 300
    expect_state "$ref" done/success
    head -n1 "$LOGS/1000/$ref.log"
    grep -q 'start_method=fork ' "$LOGS/1000/$ref.log" || fail "the workers are not forked"
    grep -q 'preload=.*puzuz-forkguard-' "$LOGS/1000/$ref.log" || fail "the fork guard is not preloaded"
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
        pz alice restore "$id" || fail "restore $k"
        if [ "${PAUSED_CHECKPOINT:-}" = 1 ]; then
            wait_state "$id" paused 300
            expect_paused 1000 "$id"
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
    pz carol group | grep -qw free || pz carol group add free -p 0 || fail "group add"
    say "fork workers"
    gpu_train_variant 60 1 2
    say "fork workers, pin_memory, persistent_workers"
    gpu_train_variant 60 1 3 pin persistent
    say "fork workers re-forked every epoch, checkpointed twice"
    gpu_train_variant 80 2 2 pin epochs
}

# PUZUZ_E2E_GPU=1 only: NVML detection under the packaged, sandboxed
# puzuz.service. The daemon is restarted with gpu_detection = "auto" and
# no [[gpus]] list; it must find the real GPU with NVML (name, UUID,
# memory and driver as nvidia-smi reports them) and `puzuz doctor` must be
# happy with that. The static test configuration is put back afterwards.
s12_nvml_detection() {
    local since out smi
    mkdir -p "$STATE"
    cp /etc/puzuz/config.toml "$STATE/config-static.toml"
    { grep '^cuda_checkpoint_mode' "$STATE/config-static.toml" || true
      printf 'grace_secs = 5\ngpu_detection = "auto"\n'; } >/etc/puzuz/config.toml
    since=$(date '+%Y-%m-%d %H:%M:%S')
    systemctl restart puzuz.service
    wait_socket
    out=$(pz carol gpu --json) || fail "gpu --json"
    echo "$out"
    smi=$(nvidia-smi --query-gpu=index,uuid,name,memory.total,driver_version \
        --format=csv,noheader,nounits) || fail "nvidia-smi"
    echo "nvidia-smi: $smi"
    python3 - "$out" "$smi" <<'EOF' || fail "puzuz gpu does not show the GPU NVML should detect"
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
    journalctl -u puzuz.service --since "$since" -o cat --no-pager \
        | grep -q 'detected 1 GPU(s) with NVML' || fail "the daemon did not log NVML detection"
    ! journalctl -u puzuz.service --since "$since" -o cat --no-pager \
        | grep 'NVML failed' || fail "the daemon logged an NVML failure"
    out=$(pz carol doctor 2>&1) || true
    grep -E '^OK +gpus: 1 GPU\(s\), detected with NVML$' <<<"$out" \
        || fail "doctor does not report NVML detection: $(grep gpus <<<"$out")"

    install -m 0644 "$STATE/config-static.toml" /etc/puzuz/config.toml
    systemctl restart puzuz.service
    wait_socket
}

# s7 is split across two containers by scripts/e2e-test.sh.
# The helper dumps the jobs on its own during the OS shutdown: a running
# and a paused counter (the paused one comes back paused), and with
# PUZUZ_E2E_GPU=1 a running and a paused CUDA counter on GPU 0.
s7_prepare() {
    local a b c g h
    # Created by s3 unless only the reboot scenario runs.
    pz carol group | grep -qw free || pz carol group add free -p 0 || fail "group add"
    a=$(add alice -g free -- "$COUNTER")
    b=$(add bob -g free --no-checkpoint -- 'trap "echo got TERM; exit 0" TERM; while :; do sleep 0.1; done')
    c=$(add alice -g free -- "$COUNTER")
    wait_state "$a" running
    wait_state "$b" running
    wait_state "$c" running
    mkdir -p "$STATE"
    if [ "${PUZUZ_E2E_GPU:-}" = 1 ]; then
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
    local a b c g h before after
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

    pz alice restore "$c" || fail "restore $c"
    wait_state "$c" paused 120
    expect_paused 1000 "$c"
    pz alice start "$c" || fail "start $c"
    wait_state "$c" running
    expect_continues 1000 "$c"
    check_counter_log 1000 "$c"
    pz alice kill "$c"
    wait_state "$c" 'done/*'

    if [ "${PUZUZ_E2E_GPU:-}" = 1 ]; then
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
        pz alice restore "$h" || fail "restore $h"
        wait_state "$h" paused 300
        expect_paused 1000 "$h"
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

# Many jobs at once: `add --immediate` from several CLIs in parallel races
# the scheduler, so the helper gets concurrent spawns; then again while the
# helper restarts. No job may fail to spawn or get lost.
s10_burst() {
    local ids=() i id u pids=()
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
    pz carol group | grep -qw free || pz carol group add free -p 0 || fail "group add"

    mapfile -t ids < <(burst a 24 'echo ok; sleep 1')
    [ "${#ids[@]}" = 24 ] || fail "only ${#ids[@]} of 24 jobs were added"
    for id in "${ids[@]}"; do wait_state "$id" 'done/*' 60; done
    for id in "${ids[@]}"; do expect_state "$id" done/success; done

    # Again, with a helper restart while the spawns are in flight. An
    # `add --immediate` that finds the helper away is refused (no job).
    burst b 16 'echo ok; sleep 3' >/dev/null 2>&1 &
    sleep 0.15
    systemctl restart puzuz-helper.service
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

case "${1:-main}" in
main)
    run "1 add/status/log/follow/wait" s1_lifecycle
    run "2 authorization" s2_authz
    run "3 group and GPU limits" s3_limits
    run "4 checkpoint and restore" s4_checkpoint_restore
    run "5 daemon restart" s5_daemon_restart
    run "6 helper restart" s6_helper_restart
    run "8 puzuz shutdown" s8_shutdown
    run "10 many jobs at once" s10_burst
    run "13 checkpoint of a paused job" s13_paused_checkpoint
    if [ "${PUZUZ_E2E_GPU:-}" = 1 ]; then
        run "9 GPU checkpoint and restore" s9_gpu
        run "11 GPU training with forked DataLoader workers" s11_gpu_fork_workers
        run "14 GPU checkpoint of paused jobs" s14_gpu_paused
        run "12 NVML GPU detection in the packaged unit" s12_nvml_detection
    fi
    ;;
reboot-prepare) run "7 reboot (prepare)" s7_prepare ;;
reboot-verify) run "7 reboot" s7_verify ;;
*) fail "unknown phase $1" ;;
esac

if [ "${#FAILED[@]}" -ne 0 ]; then
    printf '\nfailed: %s\n' "${FAILED[*]}"
    exit 1
fi
