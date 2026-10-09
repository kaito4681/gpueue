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
else
    say "SKIP checkpoint/restore: criu is not installed in the test image"
fi

say "ALL OK"
