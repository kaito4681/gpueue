# shellcheck shell=bash
# GPU part of the helper-systemd test, sourced by scenario.sh (`scenario.sh
# gpu`, run by `GPUEUE_HELPER_GPU=1 scripts/helper-systemd-test.sh`) after
# the helper is installed with cuda_checkpoint_mode =
# "${GPUEUE_HELPER_CUDA_MODE:-manual}".
#
# A CUDA training job (gpu_job.py) is checkpointed while the dump is forced
# past its deadline or killed from outside, at the criu step and at the
# cuda-checkpoint step. After each failure the job must be thawed, its CUDA
# state restored and unlocked ("running"), and it must keep computing the
# expected values. Finally a normal checkpoint and restore must work.

MODE=${GPUEUE_HELPER_CUDA_MODE:-manual}
J=20
LOGJ=/var/lib/gpueue/logs/1000/$J.log
# The deadline of the forced-timeout cases: the lock and checkpoint of
# 1 GiB of VRAM take ~1.5 s and its dump ~1 s, so only a stopped tool
# misses it.
DEADLINE=10

say "GPU: $(nvidia-smi --query-gpu=name,driver_version --format=csv,noheader), cuda_checkpoint_mode = $MODE"

client "$(steps <<EOF
[
 {"send": {"op": "spawn", "job": $J, "uid": 1000, "gid": 1000, "cwd": "/home/alice",
           "command": "exec /opt/venv/bin/python3 -u /e2e/gpu_job.py 1",
           "shell": ["sh", "-c"], "envs": {"PATH": "/usr/bin:/bin"}, "gpus": [0],
           "log_path": "$LOGJ"},
  "expect": {"result": "spawned"}}
]
EOF
)"
for _ in $(seq 300); do [ "$(wc -l <"$LOGJ" 2>/dev/null || echo 0)" -ge 5 ] && break; sleep 0.2; done
[ "$(wc -l <"$LOGJ")" -ge 5 ] || { cat "$LOGJ"; fail "the GPU job does not run"; }

# job_cgroup: the cgroup of job J's unit (gpueue-job-J.service, or
# gpueue-job-J-r<n>.service once restored).
job_cgroup() {
    local unit
    unit=$(systemctl list-units --plain --no-legend --state=active \
        "gpueue-job-$J.service" "gpueue-job-$J-r*.service" | awk 'NR == 1 {print $1}')
    [ -n "$unit" ] || fail "job $J has no active unit"
    systemctl show -p ControlGroup --value "$unit"
}

# job_pid: host PID of the job's python process.
job_pid() {
    local cg pid
    cg=$(job_cgroup)
    for pid in $(cat "/sys/fs/cgroup$cg/cgroup.procs"); do
        if tr '\0' ' ' <"/proc/$pid/cmdline" | grep -q gpu_job.py; then
            echo "$pid"
            return
        fi
    done
    fail "no gpu_job.py process in job $J"
}

# signal_tool <pgrep -f pattern> <signal>...: in the background, wait for a
# process matching the pattern and send it the signals, 2 s apart.
signal_tool() {
    local pattern=$1
    shift
    (
        pid=
        for _ in $(seq 1500); do
            pid=$(pgrep -f -- "$pattern" | head -n1 || true)
            [ -n "$pid" ] && break
            sleep 0.02
        done
        [ -n "$pid" ] || { echo "no process matched '$pattern'" >&2; exit 1; }
        cmd=$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null || true)
        for sig in "$@"; do
            sleep 0.1
            kill -s "$sig" "$pid"
            echo "sent SIG$sig to $cmd($pid)"
            sleep 1.7
        done
    ) &
}

# gpu_job_runs_on: no criu or cuda-checkpoint is left, no process of the
# job is stopped, its CUDA state is "running" (restored and unlocked), and
# it goes on computing the expected values.
gpu_job_runs_on() {
    local cg pid st before after state
    pgrep -x criu && fail "criu is still running"
    pgrep -f 'cuda-checkpoint --action' && fail "cuda-checkpoint is still running"
    cg=$(job_cgroup)
    for pid in $(cat "/sys/fs/cgroup$cg/cgroup.procs"); do
        st=$(awk '{print $3}' "/proc/$pid/stat")
        case "$st" in T | t) fail "process $pid of job $J is stopped ($st)" ;; esac
    done
    [ "$(cat "/sys/fs/cgroup$cg/cgroup.freeze" 2>/dev/null || echo 0)" = 0 ] || fail "job $J is frozen"
    state=$(cuda-checkpoint --get-state --pid "$(job_pid)" 2>&1 || true)
    echo "CUDA state of job $J: $state"
    [ "$state" = running ] || fail "the CUDA state of job $J is '$state', not running"
    before=$(wc -l <"$LOGJ")
    sleep 2
    after=$(wc -l <"$LOGJ")
    [ "$after" -gt $((before + 4)) ] || fail "job $J does not run on ($before -> $after lines)"
    runuser -u alice -- /opt/venv/bin/python3 /e2e/gpu_job.py --check "$LOGJ" \
        || fail "job $J computes wrong values"
    tail -n1 "$LOGJ"
}

# failed_dump <expected error regex> [<timeout_secs>]: request a checkpoint
# of job J that must fail with the error, and check that the job runs on.
failed_dump() {
    local want=$1 opts=
    [ -n "${2:-}" ] && opts=", \"options\": {\"timeout_secs\": $2}"
    local t0=$SECONDS
    client "$(steps <<EOF
[
 {"send": {"op": "checkpoint", "job": $J, "image_dir": "/var/lib/gpueue/checkpoints/$J"$opts},
  "expect": {"result": "accepted"}},
 {"event": {"event": "checkpoint_finished", "job": $J, "outcome": {"result": "failed"}}, "timeout": 900, "ack": true}
]
EOF
)"
    wait
    echo "checkpoint failed after $((SECONDS - t0)) s"
    local err
    err=$(journalctl -u gpueue-helper.service -o cat | grep "checkpoint of job $J failed" | tail -n1)
    echo "$err"
    grep -qE "$want" <<<"$err" || fail "unexpected error for job $J (want /$want/)"
    journalctl -u gpueue-helper.service -o cat --grep 'rollback incomplete' && fail "a rollback was incomplete"
    gpu_job_runs_on
    [ -e "/var/lib/gpueue/checkpoints/$J" ] && fail "the failed image of job $J was kept"
    return 0
}

resumed='the job was resumed'

say "GPU ($MODE): criu stopped past the dump deadline of ${DEADLINE}s"
signal_tool '(^|/)criu dump ' STOP
failed_dump "did not finish within its deadline of ${DEADLINE}s.*$resumed" "$DEADLINE"

say "GPU ($MODE): cuda-checkpoint stopped during the checkpoint, past the deadline"
signal_tool 'cuda-checkpoint --action checkpoint' STOP
failed_dump "did not finish within its deadline of ${DEADLINE}s.*$resumed" "$DEADLINE"

say "GPU ($MODE): cuda-checkpoint killed from outside during the checkpoint"
signal_tool 'cuda-checkpoint --action checkpoint' KILL
failed_dump "$resumed"

say "GPU ($MODE): criu killed from outside during the dump"
signal_tool '(^|/)criu dump ' STOP KILL
failed_dump "was killed by signal 9.*$resumed"

say "GPU ($MODE): a normal checkpoint and restore still work"
client "$(steps <<EOF
[
 {"send": {"op": "checkpoint", "job": $J, "image_dir": "/var/lib/gpueue/checkpoints/$J"},
  "expect": {"result": "accepted"}},
 {"event": {"event": "checkpoint_finished", "job": $J, "outcome": {"result": "success"}}, "timeout": 600, "ack": true}
]
EOF
)"
grep -q "\"mode\": *\"$MODE\"" "/var/lib/gpueue/checkpoints/$J/meta.json" \
    || fail "the checkpoint is not in $MODE mode: $(cat "/var/lib/gpueue/checkpoints/$J/meta.json")"
before=$(tail -n1 "$LOGJ" | cut -d' ' -f1)
client "$(steps <<EOF
[
 {"send": {"op": "restore", "job": $J, "uid": 1000, "image_dir": "/var/lib/gpueue/checkpoints/$J", "gpus": [0]},
  "expect": {"result": "accepted"}},
 {"event": {"event": "restore_finished", "job": $J, "outcome": {"result": "success"}}, "timeout": 600, "ack": true}
]
EOF
)"
gpu_job_runs_on
after=$(tail -n1 "$LOGJ" | cut -d' ' -f1)
echo "step before the dump: $before, after the restore: $after"
[ "$after" -gt "$before" ] || fail "the restored GPU job did not continue"
client "$(steps <<EOF
[
 {"send": {"op": "signal", "job": $J, "signal": 9}, "expect": {"result": "ok"}},
 {"event": {"event": "job_exited", "job": $J, "status": {"kind": "signaled", "signal": 9}}, "ack": true}
]
EOF
)"
