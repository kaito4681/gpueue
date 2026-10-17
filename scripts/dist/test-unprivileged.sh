#!/usr/bin/env bash
# Runs the workspace tests as an unprivileged user from a root shell, the way
# release.yml's build job does inside the rust container (the helper's tests
# refuse root). Used by release.yml and by ci.yml's `release-env` job, so that
# a failure specific to this environment shows up in pull requests.
#
# Usage: scripts/dist/test-unprivileged.sh   (as root, in the repository root)
set -euo pipefail

useradd -m builder
chown -R builder: .
exec runuser -u builder -- env PATH="$PATH" RUSTUP_HOME="$RUSTUP_HOME" \
    CARGO_HOME=/home/builder/.cargo CARGO_TARGET_DIR=/home/builder/target \
    cargo test --workspace --locked
