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
else
    say "SKIP checkpoint/restore: criu is not installed in the test image"
fi

say "ALL OK"
