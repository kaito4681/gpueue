#!/usr/bin/env bash
# End-to-end test of puzuz-helper with a real systemd (PID 1) in a
# privileged Docker container: transient job units over D-Bus, the
# namespace shim, exit-code collection, helper restart and re-discovery,
# and a CPU-only checkpoint/restore with criu.
#
# Never runs anything privileged on the host itself; the container is
# privileged because systemd, namespaces and criu need it.
#
# Usage: scripts/helper-systemd-test.sh [--keep]
# Environment:
#   PUZUZ_SYSTEMD_BASE   base image (see below)
#   PUZUZ_SYSTEMD_IMAGE  tag of the test image (default puzuz-helper-systemd:test)
#   PUZUZ_HELPER_NAME    container name (default puzuz-helper-e2e-<pid>)
#   PUZUZ_HELPER_GPU=1   instead of the CPU scenarios, run the GPU
#                        forced-timeout scenario (scripts/helper-systemd/gpu.sh)
#                        with `docker run --gpus all`: a CUDA training job whose
#                        dump is forced past its deadline or killed (criu and
#                        cuda-checkpoint) must be resumed with its CUDA state
#                        restored and unlocked. The base image needs CUDA, torch
#                        in /opt/venv and cuda-checkpoint, e.g.
#                        PUZUZ_SYSTEMD_BASE=puzuz-poc/criu-cuda-systemd:0.1 with
#                        a different PUZUZ_SYSTEMD_IMAGE tag.
#   PUZUZ_HELPER_CUDA_MODE=plugin  run the GPU scenario with
#                        cuda_checkpoint_mode = "plugin" (default manual)
#   PUZUZ_HELPER_ALLOW_NO_CRIU=1  skip the checkpoint part of the CPU
#                        scenarios when the image has no criu (by default
#                        that is a failure)
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
image="${PUZUZ_SYSTEMD_IMAGE:-puzuz-helper-systemd:test}"
name="${PUZUZ_HELPER_NAME:-puzuz-helper-e2e}-$$"
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
cp "$root/scripts/helper-systemd/client.py" "$root/scripts/helper-systemd/scenario.sh" \
    "$root/scripts/helper-systemd/gpu.sh" "$root/scripts/helper-systemd/gpu_job.py" "$stage/"
cp -r "$root/packaging" "$stage/packaging"
chmod -R a+rX "$stage"

gpu_args=()
[ "${PUZUZ_HELPER_GPU:-}" = 1 ] && gpu_args=(--gpus all)
docker run -d "${gpu_args[@]}" --name "$name" --privileged --cgroupns=private \
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
if [ "${PUZUZ_HELPER_GPU:-}" = 1 ]; then
    docker exec -e "PUZUZ_HELPER_CUDA_MODE=${PUZUZ_HELPER_CUDA_MODE:-manual}" "$name" \
        bash /e2e/scenario.sh gpu || status=$?
    if [ "$status" -ne 0 ]; then
        echo "--- journal (helper and jobs)"
        docker exec "$name" journalctl --no-pager -n 300 -u 'puzuz-helper.service' -u 'puzuz-job-*' || true
    fi
    exit "$status"
fi
docker exec -e "PUZUZ_HELPER_ALLOW_NO_CRIU=${PUZUZ_HELPER_ALLOW_NO_CRIU:-}" "$name" bash /e2e/scenario.sh \
    || status=$?
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
