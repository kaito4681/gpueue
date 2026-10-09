# Notes for contributors and coding agents

- Code, comments, docs and commit messages are written in English.
- Run tests inside Docker: `scripts/test-in-docker.sh` (defaults to `cargo test --workspace`;
  pass other cargo arguments, e.g. `clippy --workspace --all-targets` or `fmt --all`;
  the toolchain image is built from `docker/test.Dockerfile`).
- Workspace layout:
  - `crates/puzuz` — the `puzuz` binary (pueue-compatible CLI, `puzuz daemon` entry point)
  - `crates/puzuz-core` — domain model, scheduling decisions, authorization, SQLite storage
  - `crates/puzuz-protocol` — wire types: CLI ⇄ daemon (HTTP over a Unix socket),
    daemon ⇄ helper (newline-delimited JSON)
  - `crates/puzuz-daemon` — the daemon (runs as the unprivileged `puzuz` system user)
  - `crates/puzuz-checkpoint` — CRIU and cuda-checkpoint integration
  - `crates/puzuz-helper` — the root helper binary (spawns jobs as their owner, checkpoints)
- Shared dependencies are declared once in the root `[workspace.dependencies]`.
- Keep `puzuz-core` free of I/O other than its storage module, and keep `puzuz-helper`'s
  dependency tree minimal (no HTTP server, no SQLite).
