#!/usr/bin/env bash
# Run cargo commands inside a pinned Rust container.
# Usage: scripts/test-in-docker.sh [cargo args...]   (default: test --workspace)
set -euo pipefail

image="${PUZUZ_TEST_IMAGE:-rust:1.98.1-bookworm}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [ "$#" -eq 0 ]; then
    set -- test --workspace
fi

# The cache volume is created root-owned; hand it to the invoking user once.
docker run --rm -v puzuz-cargo-home:/cargo "$image" \
    sh -c "chown $(id -u):$(id -g) /cargo" >/dev/null

exec docker run --rm \
    --user "$(id -u):$(id -g)" \
    -e CARGO_HOME=/cargo \
    -e CARGO_TARGET_DIR=/work/target/docker \
    -v puzuz-cargo-home:/cargo \
    -v "$root":/work \
    -w /work \
    "$image" \
    cargo "$@"
