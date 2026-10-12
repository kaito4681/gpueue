#!/bin/bash
# Runs inside the systemd container (as root). See scripts/helper-systemd-test.sh.
set -euo pipefail
cd /e2e

say() { printf '\n=== %s\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
client() { # client <script.json> [user]
    runuser -u "${2:-puzuz}" -- python3 /e2e/client.py /run/puzuz/helper.sock "$1"
}
steps() { local f; f=$(mktemp /tmp/steps.XXXX.json); cat > "$f"; chmod 644 "$f"; echo "$f"; }
# spawns <first job> <count> <command>: a JSON list of spawn requests for
# jobs first.., alternating between alice (even ids) and bob (odd ids).
spawns() {
    python3 - "$@" <<'PY'
import json, sys
first, n, cmd = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
out = []
for j in range(first, first + n):
    uid, home = (1000, "/home/alice") if j % 2 == 0 else (1001, "/home/bob")
    out.append({"op": "spawn", "job": j, "uid": uid, "gid": uid, "cwd": home, "command": cmd,
                "shell": ["sh", "-c"], "envs": {"PATH": "/usr/bin:/bin"}, "gpus": [],
                "log_path": f"/var/lib/puzuz/logs/{uid}/{j}.log"})
print(json.dumps(out))
PY
}
# owner <job>: the uid that `spawns` gives the job.
owner() { if [ $(($1 % 2)) -eq 0 ]; then echo 1000; else echo 1001; fi; }
# requests <op> <python expression for the extra fields of job j> <jobs...>
requests() {
    python3 - "$@" <<'PY'
import json, sys
op, extra, jobs = sys.argv[1], sys.argv[2], [int(j) for j in sys.argv[3:]]
print(json.dumps([{"op": op, "job": j, **eval(extra, {"j": j})} for j in jobs]))
PY
}

if [ "${1:-}" = shutdown-prepare ]; then
    say "start jobs for the OS-shutdown test"
    client "$(steps <<'EOF'
[
 {"send": {"op": "spawn", "job": 10, "uid": 1000, "gid": 1000, "cwd": "/home/alice",
           "command": "i=0; while :; do echo $i; i=$((i+1)); sleep 0.2; done",
           "shell": ["sh", "-c"], "envs": {"PATH": "/usr/bin:/bin"}, "gpus": [],
           "log_path": "/var/lib/puzuz/logs/1000/10.log"},
  "expect": {"result": "spawned"}},
 {"send": {"op": "spawn", "job": 11, "uid": 1000, "gid": 1000, "cwd": "/home/alice",
           "command": "trap 'echo stopped by systemd; exit 0' TERM; while :; do sleep 0.1; done",
           "shell": ["sh", "-c"], "envs": {"PATH": "/usr/bin:/bin"}, "gpus": [],
           "log_path": "/var/lib/puzuz/logs/1000/11.log", "no_checkpoint": true},
  "expect": {"result": "spawned"}}
]
EOF
)"
    exit 0
fi

install_helper() {
    say "install"
    install -m 0755 /e2e/puzuz-helper /usr/bin/puzuz-helper
    mkdir -p /etc/sysusers.d /etc/tmpfiles.d
    install -m 0644 /e2e/packaging/systemd/puzuz-helper.service /etc/systemd/system/
    install -m 0644 /e2e/packaging/sysusers.d/puzuz.conf /etc/sysusers.d/
    install -m 0644 /e2e/packaging/tmpfiles.d/puzuz.conf /etc/tmpfiles.d/
    systemd-sysusers
    systemd-tmpfiles --create /etc/tmpfiles.d/puzuz.conf
    systemctl daemon-reload
    systemctl start puzuz-helper.service
    systemctl is-active puzuz-helper.service
}

if [ "${1:-}" = gpu ]; then
    # GPU-only run (PUZUZ_HELPER_GPU=1, see scripts/helper-systemd-test.sh).
    mkdir -p /etc/puzuz
    echo "cuda_checkpoint_mode = \"${PUZUZ_HELPER_CUDA_MODE:-manual}\"" >/etc/puzuz/config.toml
    install_helper
    # shellcheck source=gpu.sh
    . /e2e/gpu.sh
    say "ALL OK"
    exit 0
fi

install_helper

LOG1=/var/lib/puzuz/logs/1000/1.log
say "spawn a job, collect its exit code, check namespace and credentials"
client "$(steps <<'EOF'
[
 {"send": {"op": "spawn", "job": 1, "uid": 0, "gid": 0, "cwd": "/", "command": "true",
           "shell": ["sh", "-c"], "envs": {}, "gpus": [], "log_path": "/tmp/x.log"},
  "expect": {"result": "error", "code": "invalid_request"}},
 {"send": {"op": "spawn", "job": 1, "uid": 1000, "gid": 1000, "cwd": "/home/alice",
           "command": "echo hello; echo pid=$$; grep NSpid /proc/self/status; echo nproc=$(ls /proc | grep -c '^[0-9]'); echo uid=$(id -u) caps=$(grep CapEff /proc/self/status); echo cwd=$(pwd); exit 3",
           "shell": ["sh", "-c"], "envs": {"PATH": "/usr/bin:/bin", "FOO": "bar"}, "gpus": [],
           "log_path": "/var/lib/puzuz/logs/1000/1.log"},
  "expect": {"result": "spawned"}},
 {"event": {"event": "job_exited", "job": 1, "status": {"kind": "exited", "code": 3}}, "ack": true},
 {"send": {"op": "list_running"}, "expect": {"result": "running", "jobs": []}}
]
EOF
)"
cat "$LOG1"
grep -q '^hello$' "$LOG1" || fail "no output in the log"
grep -q '^pid=2$' "$LOG1" || fail "the job is not PID 2 of its namespace"
# /proc only shows the namespace: init, the shell and its pipeline.
[ "$(sed -n 's/^nproc=//p' "$LOG1")" -lt 10 ] || fail "/proc shows processes outside the namespace"
grep -q '^uid=1000 caps=CapEff: 0000000000000000$' "$LOG1" || fail "wrong credentials"
grep -q '^cwd=/home/alice$' "$LOG1" || fail "wrong cwd"
[ "$(stat -c '%U %a' "$LOG1")" = "alice 660" ] || fail "log owner/mode: $(stat -c '%U %a' "$LOG1")"
[ "$(stat -c '%U %G %a' /var/lib/puzuz/logs/1000)" = "alice puzuz 2770" ] || fail "log dir"
systemctl status 'puzuz-job-1.service' >/dev/null 2>&1 && fail "the exited unit was not released"

say "the job cannot write where its owner cannot"
client "$(steps <<'EOF'
[
 {"send": {"op": "spawn", "job": 5, "uid": 1000, "gid": 1000, "cwd": "/root",
           "command": "true", "shell": ["sh", "-c"], "envs": {}, "gpus": [],
           "log_path": "/var/lib/puzuz/logs/1000/5.log"},
  "expect": {"result": "error", "code": "permission_denied"}}
]
EOF
)"

say "other uids are rejected"
if client "$(steps <<<'[]')" root 2>/dev/null; then fail "root could talk to the helper"; fi

say "a helper restart leaves jobs running and re-discovers them"
client "$(steps <<'EOF'
[
 {"send": {"op": "spawn", "job": 2, "uid": 1000, "gid": 1000, "cwd": "/home/alice",
           "command": "trap 'echo got TERM; exit 7' TERM; while :; do sleep 0.1; done",
           "shell": ["sh", "-c"], "envs": {"PATH": "/usr/bin:/bin"}, "gpus": [],
           "log_path": "/var/lib/puzuz/logs/1000/2.log"},
  "expect": {"result": "spawned"}}
]
EOF
)"
systemctl restart puzuz-helper.service
sleep 1
systemctl is-active puzuz-job-2.service || fail "the job died with the helper"
client "$(steps <<'EOF'
[
 {"send": {"op": "list_running"}, "expect": {"result": "running", "jobs": [{"job": 2, "phase": "running"}]}},
 {"send": {"op": "signal", "job": 2, "signal": 15}, "expect": {"result": "ok"}},
 {"event": {"event": "job_exited", "job": 2, "status": {"kind": "exited", "code": 7}}, "ack": true}
]
EOF
)"
grep -q 'got TERM' /var/lib/puzuz/logs/1000/2.log || fail "SIGTERM did not reach the job"

say "many spawns at once (bob's log directory does not exist yet)"
client "$(steps <<EOF
[
 {"batch": $(spawns 100 24 'echo ok; sleep 1'), "expect": {"result": "spawned"}},
 {"events": {"event": "job_exited", "status": {"kind": "exited", "code": 0}}, "count": 24, "ack": true},
 {"send": {"op": "list_running"}, "expect": {"result": "running", "jobs": []}}
]
EOF
)"
for j in $(seq 100 123); do
    grep -qx ok "/var/lib/puzuz/logs/$(owner "$j")/$j.log" || fail "job $j has no output"
done
[ "$(stat -c '%U %G %a' /var/lib/puzuz/logs/1001)" = "bob puzuz 2770" ] || fail "bob's log dir"
[ -z "$(systemctl list-units --all --plain --no-legend 'puzuz-job-*')" ] || fail "units were left behind"

say "a helper restart while spawns are in flight"
batch=$(steps <<EOF
[{"batch": $(spawns 130 12 'sleep 600'), "save": "/tmp/restart-batch.json"}]
EOF
)
rm -f /tmp/restart-batch.json
client "$batch" >/tmp/restart-client.log 2>&1 &
cpid=$!
for _ in $(seq 500); do
    [ -n "$(systemctl list-units --all --plain --no-legend 'puzuz-job-13*')" ] && break
    sleep 0.01
done
t0=$SECONDS
systemctl restart puzuz-helper.service
# Start jobs queued behind the helper's stop job are cancelled, not
# waited for (that would stall the stop until the spawn timeout).
[ $((SECONDS - t0)) -lt 10 ] || fail "the restart took $((SECONDS - t0)) s"
wait "$cpid" || { cat /tmp/restart-client.log; fail "the client failed"; }
# Every spawn is answered before the old helper exits: either it ran, or it
# was refused with shutting_down (its queued start job cancelled) and
# nothing of it runs.
spawned=$(python3 - <<'PY'
import json
resps = json.load(open("/tmp/restart-batch.json"))
ok = []
for job, r in zip(range(130, 142), resps):
    if r is not None and r.get("result") == "spawned":
        ok.append(job)
    elif r is None or r.get("code") != "shutting_down":
        raise SystemExit(f"job {job}: {r}")
print(" ".join(map(str, ok)))
PY
)
echo "answered 'spawned': ${spawned:-none} (the others: shutting_down)"
units=$(systemctl list-units --plain --no-legend --state=active 'puzuz-job-*' | awk '{print $1}' \
    | sed 's/^puzuz-job-\([0-9]*\)\.service$/\1/' | sort -n | xargs)
[ "$units" = "$spawned" ] || fail "running units ($units) differ from the answered spawns ($spawned)"
# shellcheck disable=SC2086
client "$(steps <<EOF
[
 {"send": {"op": "list_running"}, "expect": {"result": "running", "jobs": $(python3 -c 'import json, sys; print(json.dumps([{"job": int(j), "phase": "running"} for j in sys.argv[1:]]))' $spawned)}},
 {"batch": $(requests signal '{"signal": 9}' $spawned), "expect": {"result": "ok"}},
 {"events": {"event": "job_exited", "status": {"kind": "signaled", "signal": 9}}, "count": $(wc -w <<<"$spawned"), "ack": true},
 {"send": {"op": "list_running"}, "expect": {"result": "running", "jobs": []}}
]
EOF
)"

if command -v criu >/dev/null; then
    say "checkpoint and restore a CPU job ($(criu --version | head -1))"
    client "$(steps <<'EOF'
[
 {"send": {"op": "spawn", "job": 3, "uid": 1000, "gid": 1000, "cwd": "/home/alice",
           "command": "i=0; while :; do echo $i; i=$((i+1)); sleep 0.2; done",
           "shell": ["sh", "-c"], "envs": {"PATH": "/usr/bin:/bin"}, "gpus": [],
           "log_path": "/var/lib/puzuz/logs/1000/3.log"},
  "expect": {"result": "spawned"}},
 {"sleep": 1.5},
 {"send": {"op": "checkpoint", "job": 3, "image_dir": "/var/lib/puzuz/checkpoints/3"},
  "expect": {"result": "accepted"}},
 {"event": {"event": "checkpoint_finished", "job": 3, "outcome": {"result": "success"}}, "timeout": 120, "ack": true}
]
EOF
)"
    systemctl is-active puzuz-job-3.service >/dev/null 2>&1 && fail "the dumped unit still runs"
    before=$(tail -n1 /var/lib/puzuz/logs/1000/3.log)
    # Occupy the job's old PIDs would need ns_last_pid games; the job's own
    # PID namespace makes that unnecessary.
    client "$(steps <<'EOF'
[
 {"send": {"op": "restore", "job": 3, "uid": 1000, "image_dir": "/var/lib/puzuz/checkpoints/3", "gpus": []},
  "expect": {"result": "accepted"}},
 {"event": {"event": "restore_finished", "job": 3, "outcome": {"result": "success"}}, "timeout": 120, "ack": true},
 {"sleep": 1.5},
 {"send": {"op": "signal", "job": 3, "signal": 15}, "expect": {"result": "ok"}},
 {"event": {"event": "job_exited", "job": 3, "status": {"kind": "signaled", "signal": 15}}, "ack": true}
]
EOF
)"
    after=$(tail -n1 /var/lib/puzuz/logs/1000/3.log)
    echo "counter before dump: $before, after restore: $after"
    [ "$after" -gt "$before" ] || fail "the restored job did not continue"
    [ -e /var/lib/puzuz/checkpoints/3 ] && fail "the image was not removed after restore"

    say "a dump past its deadline, and a criu killed from outside, leave the job running"
    # Job 4 holds 2 GiB of random data (so that criu takes a while) and
    # prints a counter and whether the data is intact.
    big='import os, time
b = bytearray(os.urandom(1 << 20)) * 2048
s = sum(b[::4096])
i = 0
while True:
    print(i, sum(b[::4096]) == s, flush=True)
    i += 1
    time.sleep(0.2)'
    log4=/var/lib/puzuz/logs/1000/4.log
    client "$(steps <<EOF
[
 {"send": {"op": "spawn", "job": 4, "uid": 1000, "gid": 1000, "cwd": "/home/alice",
           "command": $(python3 -c 'import json, sys; print(json.dumps(sys.argv[1]))' "python3 -c '$big'"),
           "shell": ["sh", "-c"], "envs": {"PATH": "/usr/bin:/bin"}, "gpus": [],
           "log_path": "$log4"},
  "expect": {"result": "spawned"}}
]
EOF
)"
    for _ in $(seq 100); do [ "$(wc -l <"$log4")" -ge 3 ] && break; sleep 0.2; done
    # signal_criu <signal>...: in the background, wait for criu to start
    # and send it the signals, 2 s apart.
    signal_criu() {
        (
            pid=
            for _ in $(seq 400); do pid=$(pgrep -x criu | head -n1 || true); [ -n "$pid" ] && break; sleep 0.02; done
            [ -n "$pid" ] || { echo "criu never ran" >&2; exit 1; }
            for sig in "$@"; do
                sleep 0.1
                kill -s "$sig" "$pid"
                echo "sent SIG$sig to criu ($pid)"
                sleep 1.7
            done
        ) &
    }
    # job_runs_on: no criu is left, no process of job 4 is stopped, and the
    # counter goes on with intact data.
    job_runs_on() {
        pgrep -x criu && fail "criu is still running"
        local cg pid st before after
        cg=$(systemctl show -p ControlGroup --value puzuz-job-4.service)
        for pid in $(cat "/sys/fs/cgroup$cg/cgroup.procs"); do
            st=$(awk '{print $3}' "/proc/$pid/stat")
            case "$st" in T | t) fail "process $pid of job 4 is stopped ($st)" ;; esac
        done
        before=$(wc -l <"$log4")
        sleep 2
        after=$(wc -l <"$log4")
        [ "$after" -gt $((before + 4)) ] || fail "job 4 does not run on ($before -> $after lines)"
        grep -q ' False$' "$log4" && fail "job 4's data changed"
        tail -n1 "$log4"
    }

    # The deadline (here 3 s) passes while criu is stopped: the helper
    # kills criu's process group, and the job runs on.
    signal_criu STOP
    t0=$SECONDS
    client "$(steps <<'EOF'
[
 {"send": {"op": "checkpoint", "job": 4, "image_dir": "/var/lib/puzuz/checkpoints/4",
           "options": {"timeout_secs": 3}},
  "expect": {"result": "accepted"}},
 {"event": {"event": "checkpoint_finished", "job": 4, "outcome": {"result": "failed"}}, "timeout": 60, "ack": true}
]
EOF
)"
    wait
    echo "checkpoint failed after $((SECONDS - t0)) s"
    journalctl -u puzuz-helper.service -o cat | grep 'checkpoint of job 4 failed' | tail -n1
    journalctl -u puzuz-helper.service -o cat | grep 'checkpoint of job 4 failed' | tail -n1 \
        | grep -q 'did not finish within its deadline of 3s.*the job was resumed' \
        || fail "no deadline error for job 4"
    job_runs_on
    [ -e /var/lib/puzuz/checkpoints/4 ] && fail "the failed image of job 4 was kept"

    # criu is SIGKILLed by someone else in the middle of the dump.
    signal_criu STOP KILL
    client "$(steps <<'EOF'
[
 {"send": {"op": "checkpoint", "job": 4, "image_dir": "/var/lib/puzuz/checkpoints/4"},
  "expect": {"result": "accepted"}},
 {"event": {"event": "checkpoint_finished", "job": 4, "outcome": {"result": "failed"}}, "timeout": 60, "ack": true}
]
EOF
)"
    wait
    journalctl -u puzuz-helper.service -o cat | grep 'checkpoint of job 4 failed' | tail -n1 \
        | grep -q 'was killed by signal 9.*the job was resumed' \
        || fail "no killed-criu error for job 4"
    job_runs_on

    # And it can still be checkpointed and restored.
    client "$(steps <<'EOF'
[
 {"send": {"op": "checkpoint", "job": 4, "image_dir": "/var/lib/puzuz/checkpoints/4"},
  "expect": {"result": "accepted"}},
 {"event": {"event": "checkpoint_finished", "job": 4, "outcome": {"result": "success"}}, "timeout": 120, "ack": true},
 {"send": {"op": "restore", "job": 4, "uid": 1000, "image_dir": "/var/lib/puzuz/checkpoints/4", "gpus": []},
  "expect": {"result": "accepted"}},
 {"event": {"event": "restore_finished", "job": 4, "outcome": {"result": "success"}}, "timeout": 120, "ack": true},
 {"sleep": 1.5},
 {"send": {"op": "signal", "job": 4, "signal": 15}, "expect": {"result": "ok"}},
 {"event": {"event": "job_exited", "job": 4, "status": {"kind": "signaled", "signal": 15}}, "ack": true}
]
EOF
)"
    grep -q ' False$' "$log4" && fail "job 4's data changed across checkpoint/restore"
    python3 - "$log4" <<'PY' || fail "the counter of job 4 has gaps or repeats"
import sys
nums = [int(l.split()[0]) for l in open(sys.argv[1]) if l.split() and l.split()[0].isdigit()]
assert nums[0] == 0 and all(b == a + 1 for a, b in zip(nums, nums[1:])), nums[-5:]
print(f"job 4: {len(nums)} steps without gaps")
PY

    say "checkpoints, then restores and spawns at once"
    counter='i=0; while :; do echo $i; i=$((i+1)); sleep 0.2; done'
    # Restores of 150..153, each followed by two spawns and a list_running.
    mixed=$(python3 - "$(requests restore '{"uid": 1000 if j % 2 == 0 else 1001, "image_dir": f"/var/lib/puzuz/checkpoints/{j}", "gpus": []}' 150 151 152 153)" "$(spawns 160 8 'echo ok; sleep 0.5')" <<'PY'
import json, sys
restores, spawns = json.loads(sys.argv[1]), json.loads(sys.argv[2])
out = []
for i, r in enumerate(restores):
    out += [r] + spawns[2 * i:2 * i + 2] + [{"op": "list_running"}]
print(json.dumps(out))
PY
)
    client "$(steps <<EOF
[
 {"batch": $(spawns 150 4 "$counter"), "expect": {"result": "spawned"}},
 {"sleep": 1.5},
 {"batch": $(requests checkpoint '{"image_dir": f"/var/lib/puzuz/checkpoints/{j}"}' 150 151 152 153), "expect": {"result": "accepted"}},
 {"events": {"event": "checkpoint_finished", "outcome": {"result": "success"}}, "count": 4, "timeout": 300, "ack": true},
 {"batch": $mixed, "expect": {"type": "response"}},
 {"events": {"event": "restore_finished", "outcome": {"result": "success"}}, "count": 4, "timeout": 300, "ack": true},
 {"events": {"event": "job_exited", "status": {"kind": "exited", "code": 0}}, "count": 8, "ack": true},
 {"sleep": 1},
 {"batch": $(requests signal '{"signal": 15}' 150 151 152 153), "expect": {"result": "ok"}},
 {"events": {"event": "job_exited", "status": {"kind": "signaled", "signal": 15}}, "count": 4, "ack": true},
 {"send": {"op": "list_running"}, "expect": {"result": "running", "jobs": []}}
]
EOF
)"
    for j in 150 151 152 153; do
        python3 - "/var/lib/puzuz/logs/$(owner "$j")/$j.log" <<'PY' || fail "the counter of job $j has gaps or repeats"
import sys
nums = [int(l) for l in open(sys.argv[1]).read().split() if l.isdigit()]
assert len(nums) > 10 and nums[0] == 0, nums[-5:]
assert all(b == a + 1 for a, b in zip(nums, nums[1:])), nums
PY
    done
    for j in $(seq 160 167); do
        grep -qx ok "/var/lib/puzuz/logs/$(owner "$j")/$j.log" || fail "job $j has no output"
    done

    say "a restore refuses paths the owner redirected after the dump"
    # A root-only stand-in for a system file, and alice's file of the same
    # name and size that her job holds open read-write.
    mkdir -p /srv/rootonly
    printf 'root-owned content\n' >/srv/rootonly/target
    chmod 0644 /srv/rootonly/target
    runuser -u alice -- sh -c 'mkdir -p /home/alice/x && printf "alice-own content\n" >/home/alice/x/target'
    client "$(steps <<'EOF'
[
 {"send": {"op": "spawn", "job": 6, "uid": 1000, "gid": 1000, "cwd": "/home/alice",
           "command": "exec 3<>/home/alice/x/target; while :; do sleep 0.2; done",
           "shell": ["sh", "-c"], "envs": {"PATH": "/usr/bin:/bin"}, "gpus": [],
           "log_path": "/var/lib/puzuz/logs/1000/6.log"},
  "expect": {"result": "spawned"}},
 {"sleep": 1},
 {"send": {"op": "checkpoint", "job": 6, "image_dir": "/var/lib/puzuz/checkpoints/6"},
  "expect": {"result": "accepted"}},
 {"event": {"event": "checkpoint_finished", "job": 6, "outcome": {"result": "success"}}, "timeout": 120, "ack": true}
]
EOF
)"
    # After the dump, alice swaps her directory for a symlink to the root
    # directory.
    runuser -u alice -- sh -c 'mv /home/alice/x /home/alice/x.old && ln -s /srv/rootonly /home/alice/x'
    client "$(steps <<'EOF'
[
 {"send": {"op": "restore", "job": 6, "uid": 1000, "image_dir": "/var/lib/puzuz/checkpoints/6", "gpus": []},
  "expect": {"result": "error", "code": "permission_denied"}}
]
EOF
)"
    systemctl list-units --all --plain --no-legend 'puzuz-job-6*' | grep -q . && fail "a unit of job 6 was started"
    [ -e /var/lib/puzuz/checkpoints/6/COMPLETE ] || fail "the image of job 6 was not kept"
    grep -qx 'root-owned content' /srv/rootonly/target || fail "the root file changed"
    # Put back, it restores.
    runuser -u alice -- sh -c 'rm /home/alice/x && mv /home/alice/x.old /home/alice/x'
    client "$(steps <<'EOF'
[
 {"send": {"op": "restore", "job": 6, "uid": 1000, "image_dir": "/var/lib/puzuz/checkpoints/6", "gpus": []},
  "expect": {"result": "accepted"}},
 {"event": {"event": "restore_finished", "job": 6, "outcome": {"result": "success"}}, "timeout": 120, "ack": true}
]
EOF
)"
    # criu ran the check of the restored tasks before resuming them.
    result=/var/lib/puzuz/helper/jobs/6/restore-check.json.result
    [ "$(cat "$result")" = '{"Ok":[]}' ] || fail "the restore check did not pass: $(cat "$result" 2>&1)"
    client "$(steps <<'EOF'
[
 {"send": {"op": "signal", "job": 6, "signal": 9}, "expect": {"result": "ok"}},
 {"event": {"event": "job_exited", "job": 6}, "ack": true}
]
EOF
)"

    say "a job with a zombie restores (CRIU restores zombies as root tasks without namespaces)"
    client "$(steps <<'EOF'
[
 {"send": {"op": "spawn", "job": 8, "uid": 1000, "gid": 1000, "cwd": "/home/alice",
           "command": "true & exec sleep 1000",
           "shell": ["sh", "-c"], "envs": {"PATH": "/usr/bin:/bin"}, "gpus": [],
           "log_path": "/var/lib/puzuz/logs/1000/8.log"},
  "expect": {"result": "spawned"}},
 {"sleep": 1},
 {"send": {"op": "checkpoint", "job": 8, "image_dir": "/var/lib/puzuz/checkpoints/8"},
  "expect": {"result": "accepted"}},
 {"event": {"event": "checkpoint_finished", "job": 8, "outcome": {"result": "success"}}, "timeout": 120, "ack": true},
 {"send": {"op": "restore", "job": 8, "uid": 1000, "image_dir": "/var/lib/puzuz/checkpoints/8", "gpus": []},
  "expect": {"result": "accepted"}},
 {"event": {"event": "restore_finished", "job": 8, "outcome": {"result": "success"}}, "timeout": 120, "ack": true},
 {"send": {"op": "signal", "job": 8, "signal": 9}, "expect": {"result": "ok"}},
 {"event": {"event": "job_exited", "job": 8}, "ack": true}
]
EOF
)"

    say "a restore refuses groups the owner left after the dump"
    groupadd -f e2egrp
    usermod -aG e2egrp alice
    client "$(steps <<'EOF'
[
 {"send": {"op": "spawn", "job": 7, "uid": 1000, "gid": 1000, "cwd": "/home/alice",
           "command": "while :; do sleep 0.2; done",
           "shell": ["sh", "-c"], "envs": {"PATH": "/usr/bin:/bin"}, "gpus": [],
           "log_path": "/var/lib/puzuz/logs/1000/7.log"},
  "expect": {"result": "spawned"}},
 {"sleep": 1},
 {"send": {"op": "checkpoint", "job": 7, "image_dir": "/var/lib/puzuz/checkpoints/7"},
  "expect": {"result": "accepted"}},
 {"event": {"event": "checkpoint_finished", "job": 7, "outcome": {"result": "success"}}, "timeout": 120, "ack": true}
]
EOF
)"
    grp=$(getent group e2egrp | cut -d: -f3)
    grep -q "$grp" /var/lib/puzuz/checkpoints/7/helper-job.json || fail "the dump did not record group $grp"
    gpasswd -d alice e2egrp >/dev/null
    client "$(steps <<'EOF'
[
 {"send": {"op": "restore", "job": 7, "uid": 1000, "image_dir": "/var/lib/puzuz/checkpoints/7", "gpus": []},
  "expect": {"result": "error", "code": "permission_denied"}}
]
EOF
)"
    # An image without the recorded groups (as an older helper wrote it):
    # the check of the restored tasks, run by criu before it resumes them,
    # still refuses it.
    python3 - /var/lib/puzuz/checkpoints/7/helper-job.json <<'PY'
import json, sys
p = sys.argv[1]
d = json.load(open(p))
d.pop("dump_groups", None)
json.dump(d, open(p, "w"))
PY
    client "$(steps <<'EOF'
[
 {"send": {"op": "restore", "job": 7, "uid": 1000, "image_dir": "/var/lib/puzuz/checkpoints/7", "gpus": []},
  "expect": {"result": "accepted"}},
 {"event": {"event": "restore_finished", "job": 7, "outcome": {"result": "failed"}}, "timeout": 120, "ack": true},
 {"send": {"op": "list_running"}, "expect": {"result": "running", "jobs": []}}
]
EOF
)"
    journalctl -u 'puzuz-job-7-r*' -o cat | grep -i 'restore refused' | tail -n1 || true
    grep -q 'no longer in' /var/lib/puzuz/helper/jobs/7/restore-check.json.result \
        || fail "the restore check did not refuse the group: $(cat /var/lib/puzuz/helper/jobs/7/restore-check.json.result 2>&1)"
    [ -e /var/lib/puzuz/checkpoints/7/COMPLETE ] || fail "the image of job 7 was not kept"
    pgrep -u alice -f 'while :; do sleep 0.2' >/dev/null && fail "a process of job 7 runs"
    usermod -aG e2egrp alice
    client "$(steps <<'EOF'
[
 {"send": {"op": "restore", "job": 7, "uid": 1000, "image_dir": "/var/lib/puzuz/checkpoints/7", "gpus": []},
  "expect": {"result": "accepted"}},
 {"event": {"event": "restore_finished", "job": 7, "outcome": {"result": "success"}}, "timeout": 120, "ack": true},
 {"send": {"op": "signal", "job": 7, "signal": 9}, "expect": {"result": "ok"}},
 {"event": {"event": "job_exited", "job": 7}, "ack": true}
]
EOF
)"
elif [ "${PUZUZ_HELPER_ALLOW_NO_CRIU:-}" = 1 ]; then
    say "SKIP checkpoint/restore: criu is not installed in the test image"
else
    fail "criu is not installed in the test image (PUZUZ_HELPER_ALLOW_NO_CRIU=1 skips the checkpoint part)"
fi

say "ALL OK"
