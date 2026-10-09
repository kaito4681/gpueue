#!/usr/bin/env bash
# End-to-end test of puzuz-helper with a real systemd (PID 1) in a
# privileged Docker container: transient job units over D-Bus, the
# namespace shim, exit-code collection, helper restart and re-discovery,
# and (when criu works in the container) a CPU-only checkpoint/restore.
#
# Never runs anything privileged on the host itself; the container is
# privileged because systemd, namespaces and criu need it.
#
# Usage: scripts/helper-systemd-test.sh [--keep]
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
image="${PUZUZ_SYSTEMD_IMAGE:-puzuz-helper-systemd:test}"
name="puzuz-helper-e2e-$$"
keep="${1:-}"

"$root/scripts/test-in-docker.sh" build -p puzuz-helper
binary="$root/target/docker/debug/puzuz-helper"

# PUZUZ_SYSTEMD_BASE: base image (default ubuntu:26.04; needs apt access
# unless it already has systemd, criu and python3).
docker build -q -t "$image" \
    --build-arg "BASE=${PUZUZ_SYSTEMD_BASE:-ubuntu:26.04}" \
    "$root/scripts/helper-systemd" >/dev/null

stage="$(mktemp -d)"
cleanup() {
    if [ "$keep" != "--keep" ]; then
        docker rm -f "$name" >/dev/null 2>&1 || true
    else
        echo "container kept: docker exec -it $name bash"
    fi
    rm -rf "$stage"
}
trap cleanup EXIT
cp "$binary" "$stage/puzuz-helper"
cp "$root/scripts/helper-systemd/client.py" "$root/scripts/helper-systemd/scenario.sh" "$stage/"
cp -r "$root/packaging" "$stage/packaging"
chmod -R a+rX "$stage"

docker run -d --name "$name" --privileged --cgroupns=private \
    --tmpfs /run --tmpfs /run/lock \
    -v "$stage":/e2e:ro \
    "$image" >/dev/null

# Wait for systemd to come up.
for _ in $(seq 50); do
    state="$(docker exec "$name" systemctl is-system-running 2>/dev/null || true)"
    case "$state" in running | degraded) break ;; esac
    sleep 0.2
done
echo "container systemd: ${state:-unknown}"

status=0
docker exec "$name" bash /e2e/scenario.sh || status=$?
if [ "$status" -ne 0 ]; then
    echo "--- journal (helper and jobs)"
    docker exec "$name" journalctl --no-pager -n 200 -u 'puzuz-helper.service' -u 'puzuz-job-*' || true
    exit "$status"
fi

# Phase 2: an OS shutdown. `docker stop` sends SIGRTMIN+3, so the
# container's systemd runs a real halt transaction: the helper (stopped
# first, thanks to the job units' Before=) must see SystemState=stopping,
# dump job 10, skip the no_checkpoint job 11 and write the manifest; job 11
# is then stopped by systemd with SIGTERM.
docker exec "$name" bash /e2e/scenario.sh shutdown-prepare
sleep 1
docker stop -t 120 "$name" >/dev/null
docker cp "$name:/var/lib/puzuz/checkpoints/shutdown-manifest.json" "$stage/manifest.json"
docker cp "$name:/var/lib/puzuz/logs/1000/11.log" "$stage/11.log"
echo "--- shutdown manifest"
cat "$stage/manifest.json"
python3 - "$stage/manifest.json" <<'EOF'
import json, sys
m = {j["job"]: j for j in json.load(open(sys.argv[1]))["jobs"]}
assert m[10]["result"] == "checkpointed", m
assert m[11]["result"] == "skipped", m
EOF
grep -q 'stopped by systemd' "$stage/11.log" || { echo "FAIL: job 11 was not stopped by systemd"; exit 1; }
docker cp "$name:/var/lib/puzuz/checkpoints/10/COMPLETE" "$stage/COMPLETE" \
    || { echo "FAIL: job 10 has no complete checkpoint"; exit 1; }
echo "=== OS shutdown: job 10 dumped, job 11 stopped by systemd"
