# Notes for contributors and coding agents

- Code, comments, docs and commit messages are written in English.
- Run tests inside Docker: `scripts/test-in-docker.sh` (defaults to `cargo test --workspace`;
  pass other cargo arguments, e.g. `clippy --workspace --all-targets` or `fmt --all`;
  the toolchain image is built from `docker/test.Dockerfile`).
- Full-stack e2e (daemon + helper + CLI under systemd in a privileged container, incl. a
  simulated reboot): `scripts/e2e-test.sh` (`PUZUZ_SYSTEMD_BASE=puzuz-poc/systemd259:0.1`
  where apt is offline; `PUZUZ_E2E_GPU=1` with a CUDA base image adds a GPU scenario).
- Real OS reboot/poweroff in a QEMU/KVM VM (Ubuntu 26.04, CPU jobs; needs /dev/kvm, image cached in
  the `puzuz-vm-cache` volume): `scripts/vm-reboot-test.sh [reboot|poweroff|inflight]`.
- The `.deb` (nfpm, `packaging/nfpm.yaml`): `scripts/dist/build-package.sh BIN_DIR OUT_DIR` builds
  it, `scripts/deb-test.sh [DEB]` installs/removes/purges it in a container (`PUZUZ_DEB_BASE`,
  `PUZUZ_DEB_HTTPS=1` where plain-HTTP apt is blocked).
- The `.rpm` (same nfpm definition): `scripts/dist/build-package.sh BIN_DIR OUT_DIR rpm` builds it,
  `scripts/rpm-test.sh [OLD_RPM NEW_RPM]` installs/upgrades/removes it in a container
  (`PUZUZ_RPM_BASE`, default `rockylinux/rockylinux:9`; also `fedora:latest`).
- CI (`.github/workflows/ci.yml`) runs fmt, clippy, tests, an MSRV check (`rust-version`),
  shellcheck and the CPU e2e on pull requests; pushing a `v<version>` tag builds the release
  tarball and a draft GitHub release (`.github/workflows/release.yml`).
- Distribution test (`scripts/install.sh` against a fake GitHub API, two test versions of the
  tree): `scripts/dist-test.sh` (`PUZUZ_DIST_BASE=ubuntu:22.04|ubuntu:24.04|debian:12`, default
  ubuntu:26.04). The release signing key is created once with `scripts/dist/gen-signing-key.sh`;
  `packaging/minisign.pub` is a test key until then.
- Workspace layout:
  - `crates/puzuz` — the `puzuz` binary (pueue-compatible CLI, `puzuz daemon` entry point)
  - `crates/puzuz-core` — domain model, scheduling decisions, authorization, SQLite storage
  - `crates/puzuz-protocol` — wire types: CLI ⇄ daemon (HTTP over a Unix socket),
    daemon ⇄ helper (newline-delimited JSON)
  - `crates/puzuz-config` — schema of `/etc/puzuz/config.toml`, shared by the daemon, the
    helper (which loads it with root-ownership checks) and `puzuz doctor`
  - `crates/puzuz-daemon` — the daemon (runs as the unprivileged `puzuz` system user)
  - `crates/puzuz-checkpoint` — CRIU and cuda-checkpoint integration
  - `crates/puzuz-helper` — the root helper binary (spawns jobs as their owner, checkpoints)
- User documentation: `README.md`, `docs/install.md`, `docs/architecture.md`. Update them when
  commands, flags, configuration keys, packaging files or behavior change.
- Shared dependencies are declared once in the root `[workspace.dependencies]`.
- Keep `puzuz-core` free of I/O other than its storage module, and keep `puzuz-helper`'s
  dependency tree minimal (no HTTP server, no SQLite).
