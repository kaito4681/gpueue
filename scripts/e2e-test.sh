#!/usr/bin/env bash
# Full-stack end-to-end test: the real daemon (puzuz.service, user puzuz),
# the root helper (puzuz-helper.service) and the CLI, installed with
# `puzuz service install` in a privileged Docker container with systemd as PID 1.
# See scripts/e2e/scenario.sh for the scenarios. The "reboot" scenario
# stops the container (a real systemd shutdown, so the helper dumps the
# jobs) and restores in a new container that reuses the state volume.
#
# Never runs anything privileged on the host itself; the containers are
# privileged because systemd, namespaces and criu need it.
#
# Usage: scripts/e2e-test.sh [--keep]
# Environment:
#   PUZUZ_SYSTEMD_BASE  base image (default ubuntu:26.04; needs apt unless it
#                       already has systemd, criu and python3, e.g.
#                       puzuz-poc/systemd259:0.1)
#   PUZUZ_E2E_SKIP_BUILD=1  reuse target/docker/debug binaries
#   PUZUZ_E2E_ONLY_REBOOT=1 skip the main scenarios and run only the
#                       reboot scenario (7), e.g. to repeat it many times
#   PUZUZ_E2E_NAME      prefix of the container and volume names
#                       (default puzuz-e2e), to tell parallel runs apart
#   PUZUZ_E2E_GPU=1     also run a CUDA checkpoint/restore scenario on GPU 0
#                       (`docker run --gpus all`; the base image needs CUDA,
#                       torch in /opt/venv and cuda-checkpoint, e.g.
#                       puzuz-poc/criu-cuda-systemd:0.1, with
#                       PUZUZ_SYSTEMD_IMAGE set to a different tag).
#                       Known failure: every restore in that mode fails
#                       because the NVIDIA runtime's bind mount of
#                       /run/nvidia-persistenced/socket is hidden under the
#                       container's /run tmpfs, so CRIU cannot recreate it.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
image="${PUZUZ_SYSTEMD_IMAGE:-puzuz-helper-systemd:test}"
name="${PUZUZ_E2E_NAME:-puzuz-e2e}-$$"
volume="${PUZUZ_E2E_NAME:-puzuz-e2e}-state-$$"
keep="${1:-}"
gpu_args=()
gpu_env=()
if [ "${PUZUZ_E2E_GPU:-}" = 1 ]; then
    gpu_args=(--gpus all)
    gpu_env=(-e PUZUZ_E2E_GPU=1)
fi

if [ "${PUZUZ_E2E_SKIP_BUILD:-}" != 1 ]; then
    "$root/scripts/test-in-docker.sh" build -p puzuz -p puzuz-helper
fi
docker build -q -t "$image" \
    --build-arg "BASE=${PUZUZ_SYSTEMD_BASE:-ubuntu:26.04}" \
    "$root/scripts/helper-systemd" >/dev/null

stage="$(mktemp -d)"
cleanup() {
    if [ "$keep" != "--keep" ]; then
        docker rm -f "$name-a" "$name-b" >/dev/null 2>&1 || true
        docker volume rm "$volume" >/dev/null 2>&1 || true
    else
        echo "kept: containers $name-a/$name-b, volume $volume"
    fi
    rm -rf "$stage"
}
trap cleanup EXIT
mkdir "$stage/bin"
cp "$root/target/docker/debug/puzuz" "$root/target/docker/debug/puzuz-helper" "$stage/bin/"
cp "$root"/scripts/e2e/*.sh "$root"/scripts/e2e/*.py "$root"/scripts/e2e/*.toml "$stage/"
chmod -R a+rX "$stage"
docker volume create "$volume" >/dev/null

# boot <container>: start a container on the shared state volume, wait for
# systemd and install puzuz.
boot() {
    docker run -d "${gpu_args[@]}" --name "$1" --hostname puzuz-e2e --privileged --cgroupns=private \
        --tmpfs /run --tmpfs /run/lock \
        -v "$stage":/e2e:ro -v "$volume":/var/lib/puzuz \
        "$image" >/dev/null
    local state=
    for _ in $(seq 100); do
        state="$(docker exec "$1" systemctl is-system-running 2>/dev/null || true)"
        case "$state" in running | degraded) break ;; esac
        sleep 0.2
    done
    echo "$1: systemd ${state:-unknown}"
    docker exec "${gpu_env[@]}" "$1" bash /e2e/install.sh >/dev/null
}
journal() {
    echo "--- journal of $1"
    docker exec "$1" journalctl --no-pager -n 300 -u puzuz.service -u puzuz-helper.service \
        -u 'puzuz-job-*' || true
}
# Warnings of the daemon and the helper, for information.
warnings() {
    echo "--- daemon/helper warnings in $1"
    docker exec "$1" journalctl --no-pager -o cat -u puzuz.service -u puzuz-helper.service \
        --grep 'WARN|ERROR|puzuz-helper:' || true
}

status=0
boot "$name-a"
touch "$stage/results"
if [ "${PUZUZ_E2E_ONLY_REBOOT:-}" != 1 ]; then
    docker exec "${gpu_env[@]}" "$name-a" bash /e2e/scenario.sh main | tee "$stage/results" || status=1
fi
docker exec "${gpu_env[@]}" "$name-a" bash /e2e/scenario.sh reboot-prepare | tee -a "$stage/results" || status=1
if [ "$status" -eq 0 ]; then warnings "$name-a"; else journal "$name-a"; fi

# A real systemd shutdown: `docker stop` sends SIGRTMIN+3.
docker stop -t 120 "$name-a" >/dev/null
boot "$name-b"
if docker exec "${gpu_env[@]}" "$name-b" bash /e2e/scenario.sh reboot-verify | tee -a "$stage/results"; then
    warnings "$name-b"
else
    status=1
    journal "$name-b"
fi

echo
echo "=== summary"
grep -E '^(PASS|FAIL) ' "$stage/results" || true
exit "$status"
