#!/usr/bin/env bash
# Full-stack end-to-end test: the real daemon (gpueue.service, user gpueue),
# the root helper (gpueue-helper.service) and the CLI, installed with
# `gpueue service install` in a privileged Docker container with systemd as PID 1.
# See scripts/e2e/scenario.sh for the scenarios. The "reboot" scenario
# stops the container (a real systemd shutdown, so the helper dumps the
# jobs) and restores in a new container that reuses the state volume.
#
# Never runs anything privileged on the host itself; the containers are
# privileged because systemd, namespaces and criu need it.
#
# Usage: scripts/e2e-test.sh [--keep]
# Environment:
#   GPUEUE_SYSTEMD_BASE  base image (default ubuntu:26.04; needs apt unless it
#                       already has systemd, criu and python3, e.g.
#                       gpueue-poc/systemd259:0.1)
#   GPUEUE_E2E_SKIP_BUILD=1  reuse target/docker/debug binaries
#   GPUEUE_E2E_ONLY=<n...>  run only the listed scenarios (numbers as in
#                       scenario.sh, separated by spaces or commas, e.g.
#                       GPUEUE_E2E_ONLY=1 or GPUEUE_E2E_ONLY="3,7"), e.g. to
#                       repeat one many times. The reboot scenarios (7, 15,
#                       16, 20, 21) and the container switch are skipped unless
#                       one of them is listed.
#                       Unknown numbers, and GPU scenarios without
#                       GPUEUE_E2E_GPU=1, are rejected; a run in which no
#                       scenario executed fails.
#   GPUEUE_E2E_ONLY_REBOOT=1 same as GPUEUE_E2E_ONLY=7
#   Scenarios 15, 16, 20 and 21 need the administrator's mounts that
#                       scripts/e2e/mounts.sh sets up at every boot when one
#                       of them runs (loop-mounted ext4, an automount, tmpfs,
#                       overlayfs, bind mounts, sshfs if the image has it);
#                       their contents live on a second volume. Without
#                       sshfs, scenario 15 prints a SKIP line for its FUSE
#                       part, and scenarios 20 (a FUSE mount whose sshfs is
#                       killed) and 21 (a user's FUSE mount that refuses
#                       root) are skipped.
#   GPUEUE_E2E_REQUIRE_SSHFS=1  fail scenarios 15, 20 and 21 instead when sshfs
#                       is missing (set in CI)
#   GPUEUE_E2E_NAME      prefix of the container and volume names
#                       (default gpueue-e2e), to tell parallel runs apart
#   GPUEUE_E2E_GPU=1     also run the GPU scenarios (9: GPU
#                       isolation (a CPU job cannot open /dev/nvidia*, a job
#                       on GPU 0 can) and a GPU counter, 11: PyTorch training with forked
#                       DataLoader workers, both checkpointed and restored;
#                       14: both paused before the checkpoint, and pause
#                       during a GPU dump; 12: NVML detection in the
#                       packaged gpueue.service with gpu_detection = "auto";
#                       the reboot scenario 7 also dumps a running and a
#                       paused CUDA counter) on GPU 0
#                       (`docker run --gpus all`; the base image needs CUDA,
#                       torch in /opt/venv and cuda-checkpoint, e.g.
#                       gpueue-poc/criu-cuda-systemd:0.1, with
#                       GPUEUE_SYSTEMD_IMAGE set to a different tag).
#                       The NVIDIA runtime's bind mount of
#                       /run/nvidia-persistenced/socket is hidden under the
#                       container's /run tmpfs there; the shim removes such
#                       hidden mounts from each job's namespace (see
#                       crates/gpueue-helper/src/mounts.rs).
#   GPUEUE_E2E_CUDA_MODE=plugin  run the helper with
#                       cuda_checkpoint_mode = "plugin" (CRIU's cuda_plugin.so
#                       drives cuda-checkpoint) instead of the default
#                       "manual"; the image's criu must have the plugin in
#                       its plugin directory (gpueue-poc/criu-cuda-systemd:0.1
#                       has /usr/lib/criu/cuda_plugin.so from CRIU 4.2.1).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
image="${GPUEUE_SYSTEMD_IMAGE:-gpueue-helper-systemd:test}"
name="${GPUEUE_E2E_NAME:-gpueue-e2e}-$$"
volume="${GPUEUE_E2E_NAME:-gpueue-e2e}-state-$$"
disk="${GPUEUE_E2E_NAME:-gpueue-e2e}-disk-$$"
keep="${1:-}"
gpu_args=()
gpu_env=()
if [ "${GPUEUE_E2E_GPU:-}" = 1 ]; then
    gpu_args=(--gpus all)
    gpu_env=(-e GPUEUE_E2E_GPU=1)
fi
only="${GPUEUE_E2E_ONLY:-}"
if [ "${GPUEUE_E2E_ONLY_REBOOT:-}" = 1 ]; then only="${only:+$only }7"; fi
only="${only//,/ }"
# Reject unknown scenario numbers (a typo would otherwise run nothing) and
# GPU scenarios without GPUEUE_E2E_GPU=1 (they would be skipped silently).
for n in $only; do
    case "$n" in
    1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 10 | 13 | 15 | 16 | 17 | 18 | 19 | 20 | 21 | 22 | 23 | 24 | 25 | 26 | 27) ;;
    9 | 11 | 12 | 14)
        if [ "${GPUEUE_E2E_GPU:-}" != 1 ]; then
            echo "scenario $n needs GPUEUE_E2E_GPU=1" >&2
            exit 2
        fi
        ;;
    *)
        echo "unknown scenario in GPUEUE_E2E_ONLY: $n" >&2
        exit 2
        ;;
    esac
done
# selected <n>: whether scenario <n> runs.
selected() { [ -z "$only" ] || [[ " $only " == *" $1 "* ]]; }
reboot_selected() { selected 7 || selected 15 || selected 16 || selected 20 || selected 21 || selected 22; }
mounts_selected() { selected 15 || selected 16 || selected 20 || selected 21 || selected 22; }
# Whether a scenario of the main phase (not 7, 15, 16, 20, 21 or 22) is selected.
main_selected() {
    local n
    [ -z "$only" ] && return 0
    for n in $only; do
        case "$n" in 7 | 15 | 16 | 20 | 21 | 22) ;; *) return 0 ;; esac
    done
    return 1
}
gpu_env+=(-e "GPUEUE_E2E_ONLY=$only" -e "GPUEUE_E2E_CUDA_MODE=${GPUEUE_E2E_CUDA_MODE:-}"
    -e "GPUEUE_E2E_REQUIRE_SSHFS=${GPUEUE_E2E_REQUIRE_SSHFS:-}")

if [ "${GPUEUE_E2E_SKIP_BUILD:-}" != 1 ]; then
    "$root/scripts/test-in-docker.sh" build -p gpueue -p gpueue-helper
fi
docker build -q -t "$image" \
    --build-arg "BASE=${GPUEUE_SYSTEMD_BASE:-ubuntu:26.04}" \
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
cp "$root/target/docker/debug/gpueue" "$root/target/docker/debug/gpueue-helper" "$stage/bin/"
cp "$root"/scripts/e2e/*.sh "$root"/scripts/e2e/*.py "$root"/scripts/e2e/*.toml "$stage/"
chmod -R a+rX "$stage"
docker volume create "$volume" >/dev/null
docker volume create "$disk" >/dev/null

# boot <container>: start a container on the shared state volume, wait for
# systemd and install gpueue.
boot() {
    docker run -d "${gpu_args[@]}" --name "$1" --hostname gpueue-e2e --privileged --cgroupns=private \
        --tmpfs /run --tmpfs /run/lock \
        -v "$stage":/e2e:ro -v "$volume":/var/lib/gpueue -v "$disk":/var/lib/gpueue-e2e-disk \
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
    docker exec "$1" journalctl --no-pager -n 300 -u gpueue.service -u gpueue-helper.service \
        -u 'gpueue-job-*' || true
}
# Warnings of the daemon and the helper, for information.
warnings() {
    echo "--- daemon/helper warnings in $1"
    docker exec "$1" journalctl --no-pager -o cat -u gpueue.service -u gpueue-helper.service \
        --grep 'WARN|ERROR|gpueue-helper:' || true
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
