# Toolchain image used by scripts/test-in-docker.sh.
FROM rust:1.98.1-bookworm
RUN rustup component add clippy rustfmt
