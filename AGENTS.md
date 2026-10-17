# Notes for contributors and coding agents

## Rules
- Code, comments, docs and commit messages are in English.
- `gpueue-helper` runs as root and must treat everything it gets from the daemon, from jobs and
  from users' files as untrusted. Do not weaken its checks to make a test or a workload pass;
  fix the cause or ask.
- Keep `gpueue-core` free of I/O other than its storage module, and keep `gpueue-helper`'s
  dependency tree minimal (no HTTP server, no SQLite). Shared dependencies are declared once in
  the root `[workspace.dependencies]`.
- Never run `gpueue clean`, `gpueue reset` or remove jobs you did not create on a host with a
  real installation; clean up only your own jobs by id.

## Testing
- Run tests only through Docker: `scripts/test-in-docker.sh` (default `cargo test --workspace`;
  pass other cargo arguments, e.g. `clippy --workspace --all-targets -- -D warnings`). The
  container needs `--cap-add SYS_PTRACE` (the script adds it) so that `pidfd_getfd`, used by the
  helper's dump hazard scan, is allowed; without it those tests fail on purpose.
- Tests that need unprivileged user namespaces print `skipped: ` in Docker and on GitHub's
  Ubuntu runners; they run in the NixOS VM check (`nix build .#checks.x86_64-linux.helper-tests`,
  needs `/dev/kvm`), which fails on that marker. Without Nix on the host, use a pinned
  `nixos/nix` Docker image.
- Fix flaky tests at the root (synchronization, injected clocks/delays, child-process isolation,
  explicit file modes); do not add retries or longer sleeps, and do not loosen assertions.
- Helper, shim or CRIU changes: run the full-stack e2e, `scripts/e2e-test.sh`
  (`GPUEUE_SYSTEMD_BASE=gpueue-poc/systemd259:0.1` where apt is offline).
- Install, upgrade, reboot and shutdown behaviour: `scripts/vm-reboot-test.sh [quick|all|...]`
  (QEMU/KVM in Docker; `list` shows the distributions and scenarios).
- GPU checkpointing cannot be tested in CI or a VM. `scripts/gpu-host-test.sh` runs against the
  gpueue installed on a GPU host, as a normal user.

## Releases
- Before tagging `v<version>`: install the release candidate's `.deb` on a GPU host and run
  `scripts/gpu-host-test.sh`; every scenario must pass. Run `scripts/vm-reboot-test.sh all` too.
- Pushing the tag builds, signs and VM-tests the release and creates a draft (no draft unless
  `vm-test` passes). Publishing the draft as a stable release triggers `release-apt.yml`,
  `release-aur.yml` and `release-nix.yml` (moves the `stable` branch); each can be run by hand.
- The release signing key is `packaging/minisign.pub`; rotation is described in `docs/install.md`.

## Documentation
- Update `README.md`, `docs/install.md` and `docs/architecture.md` when commands, flags,
  configuration keys, packaging or behaviour change. The English docs are canonical; the same PR
  must update `README.ja.md`, `README.zh-CN.md`, `docs/ja/` and `docs/zh-CN/` (commands, code,
  keys and paths untranslated, English anchor ids kept).
- Update the Agent Skill in `skills/gpueue/` when user-facing commands, flags, JSON output, error
  messages or scheduling/checkpoint behaviour change; every command it shows must exist, and the
  frontmatter `name` must equal the directory name.

## CI details that are easy to miss
- The `changes` job in `ci.yml` decides when the `nix` and `vm` jobs run on pull requests; keep
  its file list in sync with the source fileset in `packaging/nix/package.nix`.
- The Nix package build runs no tests (the Nix sandbox refuses setgid); CI runs
  `nix develop -c cargo test --workspace --locked` instead.
