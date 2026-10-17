#!/usr/bin/env bash
# Run cargo commands inside a pinned Rust container.
# Usage: scripts/test-in-docker.sh [cargo args...]   (default: test --workspace)
set -euo pipefail

image="${GPUEUE_TEST_IMAGE:-gpueue-test:1.98.1}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Build the toolchain image (rust + clippy + rustfmt) on first use.
if ! docker image inspect "$image" >/dev/null 2>&1; then
    docker build -q -t "$image" -f "$root/docker/test.Dockerfile" "$root/docker" >/dev/null
fi

if [ "$#" -eq 0 ]; then
    set -- test --workspace
fi

# The cache volume is created root-owned; hand it to the invoking user once.
docker run --rm -v gpueue-cargo-home:/cargo "$image" \
    sh -c "chown $(id -u):$(id -g) /cargo" >/dev/null

# CAP_SYS_PTRACE makes Docker's default seccomp profile allow pidfd_getfd
# (and ptrace, process_vm_readv, ...), which the helper's dump hazard scan
# uses to inspect a job's sockets; without it the scan fails closed and the
# helper's tests fail. The tests run as an unprivileged user, so the
# capability itself is not in their effective set: only the seccomp profile
# changes, not what the processes may do.
exec docker run --rm \
    --cap-add SYS_PTRACE \
    --user "$(id -u):$(id -g)" \
    -e CARGO_HOME=/cargo \
    -e CARGO_TARGET_DIR=/work/target/docker \
    -v gpueue-cargo-home:/cargo \
    -v "$root":/work \
    -w /work \
    "$image" \
    cargo "$@"
