#!/usr/bin/env bash
# Measure how long each step of a manual-mode checkpoint and restore takes
# (cuda-checkpoint lock / checkpoint, criu dump, criu restore,
# cuda-checkpoint restore / unlock) and how large the image is, for jobs of
# a given size. Used to choose the helper's default timeouts
# (crates/gpueue-checkpoint/src/plan.rs).
#
# Runs in a privileged Docker container with the GPU; never runs anything
# privileged on the host. Images go to a Docker volume (on disk, not tmpfs).
#
# Usage: scripts/measure-checkpoint.sh <vram_gb>:<ram_gb> ...
#   e.g. scripts/measure-checkpoint.sh 1:0.5 4:0.5 0:4   (0 VRAM: CPU only)
# Environment:
#   GPUEUE_MEASURE_IMAGE  image with criu, cuda-checkpoint and torch in
#                        /opt/venv (default gpueue-poc/criu-cuda:0.1)
#   GPUEUE_MEASURE_MEMORY container memory limit (default 7g), so that a
#                        large case is OOM-killed in the container rather
#                        than squeezing the host
#   GPUEUE_MEASURE_CYCLES dump/restore cycles per case, alternating warm and
#                        cold page cache for the restore (default 2)
#   GPUEUE_MEASURE_MODE   cuda_checkpoint_mode to mimic: manual (default) or
#                        plugin (CRIU's CUDA plugin does the CUDA part, so
#                        it is counted in the criu dump / restore times)
#   GPUEUE_MEASURE_GPU    1 (default) runs the container with --gpus all;
#                        anything else runs without a GPU (CPU-only cases,
#                        <vram_gb> 0)
#   GPUEUE_MEASURE_NAME   prefix of the container/volume name (default
#                        gpueue-measure); the process id is always appended,
#                        so an existing container or volume is never removed
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
image="${GPUEUE_MEASURE_IMAGE:-gpueue-poc/criu-cuda:0.1}"
name="${GPUEUE_MEASURE_NAME:-gpueue-measure}-$$"
gpu_args=(--gpus all)
[ "${GPUEUE_MEASURE_GPU:-1}" = 1 ] || gpu_args=()

cleanup() {
    docker rm -f "$name" >/dev/null 2>&1 || true
    docker volume rm "$name" >/dev/null 2>&1 || true
}
trap cleanup EXIT
docker volume create "$name" >/dev/null
docker run --rm --name "$name" "${gpu_args[@]}" --privileged \
    --memory "${GPUEUE_MEASURE_MEMORY:-7g}" --memory-swap "${GPUEUE_MEASURE_MEMORY:-7g}" \
    -e CYCLES="${GPUEUE_MEASURE_CYCLES:-2}" -e MODE="${GPUEUE_MEASURE_MODE:-manual}" -v "$root/scripts/measure":/m:ro -v "$name":/data \
    --entrypoint /bin/bash "$image" /m/inside.sh "$@"
