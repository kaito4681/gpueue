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
#   PUZUZ_E2E_ONLY=<n...>  run only the listed scenarios (numbers as in
#                       scenario.sh, separated by spaces or commas, e.g.
#                       PUZUZ_E2E_ONLY=1 or PUZUZ_E2E_ONLY="3,7"), e.g. to
#                       repeat one many times. The reboot scenarios (7, 15,
#                       16, 20) and the container switch are skipped unless
#                       one of them is listed.
#                       Unknown numbers, and GPU scenarios without
#                       PUZUZ_E2E_GPU=1, are rejected; a run in which no
#                       scenario executed fails.
#   PUZUZ_E2E_ONLY_REBOOT=1 same as PUZUZ_E2E_ONLY=7
#   Scenarios 15, 16 and 20 need the administrator's mounts that
#                       scripts/e2e/mounts.sh sets up at every boot when one
#                       of them runs (loop-mounted ext4, an automount, tmpfs,
#                       overlayfs, bind mounts, sshfs if the image has it);
#                       their contents live on a second volume. Without
#                       sshfs, scenario 15 prints a SKIP line for its FUSE
#                       part, and scenario 20 (a FUSE mount whose sshfs is
#                       killed) is skipped.
#   PUZUZ_E2E_REQUIRE_SSHFS=1  fail scenarios 15 and 20 instead when sshfs
#                       is missing (set in CI)
#   PUZUZ_E2E_NAME      prefix of the container and volume names
#                       (default puzuz-e2e), to tell parallel runs apart
#   PUZUZ_E2E_GPU=1     also run the GPU scenarios (9: GPU
#                       isolation (a CPU job cannot open /dev/nvidia*, a job
#                       on GPU 0 can) and a GPU counter, 11: PyTorch training with forked
#                       DataLoader workers, both checkpointed and restored;
#                       14: both paused before the checkpoint, and pause
#                       during a GPU dump; 12: NVML detection in the
#                       packaged puzuz.service with gpu_detection = "auto";
#                       the reboot scenario 7 also dumps a running and a
#                       paused CUDA counter) on GPU 0
#                       (`docker run --gpus all`; the base image needs CUDA,
#                       torch in /opt/venv and cuda-checkpoint, e.g.
#                       puzuz-poc/criu-cuda-systemd:0.1, with
#                       PUZUZ_SYSTEMD_IMAGE set to a different tag).
#                       The NVIDIA runtime's bind mount of
#                       /run/nvidia-persistenced/socket is hidden under the
#                       container's /run tmpfs there; the shim removes such
#                       hidden mounts from each job's namespace (see
#                       crates/puzuz-helper/src/mounts.rs).
#   PUZUZ_E2E_CUDA_MODE=plugin  run the helper with
#                       cuda_checkpoint_mode = "plugin" (CRIU's cuda_plugin.so
#                       drives cuda-checkpoint) instead of the default
#                       "manual"; the image's criu must have the plugin in
#                       its plugin directory (puzuz-poc/criu-cuda-systemd:0.1
#                       has /usr/lib/criu/cuda_plugin.so from CRIU 4.2.1).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
image="${PUZUZ_SYSTEMD_IMAGE:-puzuz-helper-systemd:test}"
name="${PUZUZ_E2E_NAME:-puzuz-e2e}-$$"
volume="${PUZUZ_E2E_NAME:-puzuz-e2e}-state-$$"
disk="${PUZUZ_E2E_NAME:-puzuz-e2e}-disk-$$"
keep="${1:-}"
gpu_args=()
gpu_env=()
if [ "${PUZUZ_E2E_GPU:-}" = 1 ]; then
    gpu_args=(--gpus all)
    gpu_env=(-e PUZUZ_E2E_GPU=1)
fi
only="${PUZUZ_E2E_ONLY:-}"
if [ "${PUZUZ_E2E_ONLY_REBOOT:-}" = 1 ]; then only="${only:+$only }7"; fi
only="${only//,/ }"
# Reject unknown scenario numbers (a typo would otherwise run nothing) and
# GPU scenarios without PUZUZ_E2E_GPU=1 (they would be skipped silently).
for n in $only; do
    case "$n" in
    1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 10 | 13 | 15 | 16 | 17 | 18 | 19 | 20) ;;
    9 | 11 | 12 | 14)
        if [ "${PUZUZ_E2E_GPU:-}" != 1 ]; then
            echo "scenario $n needs PUZUZ_E2E_GPU=1" >&2
            exit 2
        fi
        ;;
    *)
        echo "unknown scenario in PUZUZ_E2E_ONLY: $n" >&2
        exit 2
        ;;
    esac
done
# selected <n>: whether scenario <n> runs.
selected() { [ -z "$only" ] || [[ " $only " == *" $1 "* ]]; }
reboot_selected() { selected 7 || selected 15 || selected 16 || selected 20; }
mounts_selected() { selected 15 || selected 16 || selected 20; }
# Whether a scenario of the main phase (not 7, 15, 16 or 20) is selected.
main_selected() {
    local n
    [ -z "$only" ] && return 0
    for n in $only; do
        case "$n" in 7 | 15 | 16 | 20) ;; *) return 0 ;; esac
    done
    return 1
}
gpu_env+=(-e "PUZUZ_E2E_ONLY=$only" -e "PUZUZ_E2E_CUDA_MODE=${PUZUZ_E2E_CUDA_MODE:-}"
    -e "PUZUZ_E2E_REQUIRE_SSHFS=${PUZUZ_E2E_REQUIRE_SSHFS:-}")

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
        docker volume rm "$volume" "$disk" >/dev/null 2>&1 || true
    else
        echo "kept: containers $name-a/$name-b, volumes $volume $disk"
    fi
    rm -rf "$stage"
}
trap cleanup EXIT
mkdir "$stage/bin"
cp "$root/target/docker/debug/puzuz" "$root/target/docker/debug/puzuz-helper" "$stage/bin/"
cp "$root"/scripts/e2e/*.sh "$root"/scripts/e2e/*.py "$root"/scripts/e2e/*.toml "$stage/"
chmod -R a+rX "$stage"
docker volume create "$volume" >/dev/null
docker volume create "$disk" >/dev/null

# boot <container>: start a container on the shared state volume, wait for
# systemd and install puzuz.
boot() {
    docker run -d "${gpu_args[@]}" --name "$1" --hostname puzuz-e2e --privileged --cgroupns=private \
        --tmpfs /run --tmpfs /run/lock \
        -v "$stage":/e2e:ro -v "$volume":/var/lib/puzuz -v "$disk":/var/lib/puzuz-e2e-disk \
        "$image" >/dev/null
    local state=
    for _ in $(seq 100); do
        state="$(docker exec "$1" systemctl is-system-running 2>/dev/null || true)"
        case "$state" in running | degraded) break ;; esac
        sleep 0.2
    done
    echo "$1: systemd ${state:-unknown}"
    if mounts_selected; then
        docker exec "$1" bash /e2e/mounts.sh setup
    fi
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
# The main phase runs unless only reboot scenarios are selected.
if main_selected; then
    docker exec "${gpu_env[@]}" "$name-a" bash /e2e/scenario.sh main | tee "$stage/results" || status=1
fi
if reboot_selected; then
    docker exec "${gpu_env[@]}" "$name-a" bash /e2e/scenario.sh reboot-prepare | tee -a "$stage/results" || status=1
fi
if [ "$status" -eq 0 ]; then warnings "$name-a"; else journal "$name-a"; fi

if reboot_selected; then
    # A real systemd shutdown: `docker stop` sends SIGRTMIN+3.
    docker stop -t 120 "$name-a" >/dev/null
    boot "$name-b"
    if docker exec "${gpu_env[@]}" "$name-b" bash /e2e/scenario.sh reboot-verify | tee -a "$stage/results"; then
        warnings "$name-b"
    else
        status=1
        journal "$name-b"
    fi
fi

echo
echo "=== summary"
grep -E '^(PASS|FAIL|SKIP) ' "$stage/results" || true
# A run in which no scenario executed is not a success.
if ! grep -qE '^(PASS|FAIL) ' "$stage/results"; then
    echo "no scenario ran"
    status=1
fi
exit "$status"
