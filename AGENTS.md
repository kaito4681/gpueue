# Notes for contributors and coding agents

- Code, comments, docs and commit messages are written in English.
- Run tests inside Docker: `scripts/test-in-docker.sh` (defaults to `cargo test --workspace`;
  pass other cargo arguments, e.g. `clippy --workspace --all-targets` or `fmt --all`;
  the toolchain image is built from `docker/test.Dockerfile`).
- Full-stack e2e (daemon + helper + CLI under systemd in a privileged container, incl. a
  simulated reboot): `scripts/e2e-test.sh` (`GPUEUE_SYSTEMD_BASE=gpueue-poc/systemd259:0.1`
  where apt is offline; `GPUEUE_E2E_GPU=1` with a CUDA base image adds a GPU scenario).
- Real OS reboot/poweroff in a QEMU/KVM VM (Ubuntu 26.04, CPU jobs; needs /dev/kvm, image cached in
  the `gpueue-vm-cache` volume): `scripts/vm-reboot-test.sh [reboot|poweroff|inflight]`.
- The `.deb` (nfpm, `packaging/nfpm.yaml`): `scripts/dist/build-package.sh BIN_DIR OUT_DIR` builds
  it, `scripts/deb-test.sh [DEB]` installs/removes/purges it in a container (`GPUEUE_DEB_BASE`,
  `GPUEUE_DEB_HTTPS=1` where plain-HTTP apt is blocked).
- The `.rpm` (same nfpm definition): `scripts/dist/build-package.sh BIN_DIR OUT_DIR rpm` builds it,
  `scripts/rpm-test.sh [OLD_RPM NEW_RPM]` installs/upgrades/removes it in a container
  (`GPUEUE_RPM_BASE`, default `rockylinux/rockylinux:9`; also `fedora:latest`); `scripts/rpm-sign-test.sh RPM`
  tests the GPG signing of the release workflow (`scripts/dist/sign-rpm.sh`).
- The APT repository (a flat repository in the release `apt`): `scripts/dist/apt-repo.sh`
  (`update DIR DEB...` locally, `github TAG` from `.github/workflows/release-apt.yml` when a
  release is published); `scripts/apt-repo-test.sh [DEB]` upgrades v1 to v2 through a local copy
  in containers (`GPUEUE_APT_BASES`, `GPUEUE_DEB_HTTPS=1`).
- Nix flake: `flake.nix` and `packaging/nix/` (package, CRIU override, the NixOS module
  `packaging/nix/nixos-module.nix`, the home-manager module
  `packaging/nix/home-manager-module.nix`, and their tests in `packaging/nix/tests/`). Without
  Nix on the host, run `nix flake check` / `nix build .#default` in a
  pinned `nixos/nix` Docker image; the NixOS VM tests (`.#checks.x86_64-linux.nixos`, and
  `.#checks.x86_64-linux.helper-tests`, which runs every gpueue-helper test) need `--device /dev/kvm` and
  `--option system-features "kvm nixos-test"`. The package build
  runs no tests (`doCheck = false`: the helper's job-spawning tests cannot pass in the Nix
  sandbox, which refuses setgid); CI runs `nix develop -c cargo test --workspace --locked`
  instead. Published stable releases move the `stable` branch
  (`.github/workflows/release-nix.yml`). The module generates its units from `packaging/`.
- CI (`.github/workflows/ci.yml`, on pull requests, pushes to `main` and `workflow_dispatch`) has
  these jobs: `lint` (fmt, clippy), `test`, `msrv` (`rust-version`), `shellcheck`, `actionlint`
  (the workflow files), the CPU e2e (`e2e`), `service-install`, `dist-build` and `dist` (tarball
  and `install.sh`), `deb`, `rpm`, `apt-repo`, `aur` (builds and installs the AUR package from a
  locally built tarball with `scripts/dist/aur-test.sh`) and `nix`. `nix` is a matrix of four
  runners: on x86_64 the NixOS module VM test (with the home-manager check and the package/CRIU
  smoke checks), the helper-tests VM, and `nix flake check --no-build --all-systems` plus
  `nix develop -c cargo test`; on aarch64 `nix flake check` plus the same tests and smoke checks.
  A `changes` job skips `nix` on pull requests whose files no flake output or test reads (keep
  its list in sync with the source fileset in `packaging/nix/package.nix`). Pushing a `v<version>` tag runs
  `.github/workflows/release.yml`, which builds and signs the tarballs, `.deb` and `.rpm`
  packages and attaches them, with `install.sh` and `RPM-GPG-KEY-gpueue`, to a draft GitHub
  release. Publishing the release as a stable one (`release: released`, not the tag push) triggers
  `release-apt.yml` (APT repository), `release-aur.yml` (AUR) and `release-nix.yml` (moves the
  `stable` branch); each can also be run by hand (`workflow_dispatch`), which is needed when the
  release was published with a workflow's `GITHUB_TOKEN`. Release workflows are read-only by
  default and grant `contents: write` only to the job that writes (creates the release, uploads
  the APT files, pushes `stable`); checkouts use `persist-credentials: false` except where the job
  pushes with git.
- Other test scripts: `scripts/service-install-test.sh` (`gpueue service install` / `status` /
  `uninstall` and `doctor`), `scripts/helper-systemd-test.sh` (the helper alone under systemd),
  `scripts/measure-checkpoint.sh` (times each checkpoint step; not a pass/fail test).
- Distribution test (`scripts/install.sh` against a fake GitHub API, two test versions of the
  tree): `scripts/dist-test.sh` (`GPUEUE_DIST_BASE=ubuntu:22.04|ubuntu:24.04|debian:12`, default
  ubuntu:26.04). The release signing key (`packaging/minisign.pub`, id EA3C2ED855877E21) is the
  real one; to rotate it, create a new pair with `scripts/dist/gen-signing-key.sh` (see
  `docs/install.md`).
- AUR package (`packaging/aur/gpueue-bin`): `scripts/dist/aur-test.sh` builds and installs it in
  an Arch Linux container with systemd; `scripts/dist/update-aur.sh <version>` points it at a
  release (run by `.github/workflows/release-aur.yml`).
- Workspace layout:
  - `crates/gpueue` — the `gpueue` binary (pueue-compatible CLI, `gpueue daemon` entry point)
  - `crates/gpueue-core` — domain model, scheduling decisions, authorization, SQLite storage
  - `crates/gpueue-protocol` — wire types: CLI ⇄ daemon (HTTP over a Unix socket),
    daemon ⇄ helper (newline-delimited JSON)
  - `crates/gpueue-config` — schema of `/etc/gpueue/config.toml`, shared by the daemon, the
    helper (which loads it with root-ownership checks) and `gpueue doctor`
  - `crates/gpueue-daemon` — the daemon (runs as the unprivileged `gpueue` system user)
  - `crates/gpueue-checkpoint` — CRIU and cuda-checkpoint integration
  - `crates/gpueue-helper` — the root helper binary (spawns jobs as their owner, checkpoints)
- User documentation: `README.md`, `docs/install.md`, `docs/architecture.md`. Update them when
  commands, flags, configuration keys, packaging files or behavior change.
- Agent Skill: `skills/gpueue/` (`SKILL.md` plus `references/*.md`, installed by users with
  `npx skills add kaito4681/gpueue`) teaches coding agents to use the CLI. Update it when
  user-facing commands, flags, JSON output, error messages or scheduling/checkpoint behavior
  change; every command it shows must exist. Keep the frontmatter `name` equal to the directory
  name.
- The English documentation is canonical. It is translated into Japanese (`README.ja.md`,
  `docs/ja/`) and Simplified Chinese (`README.zh-CN.md`, `docs/zh-CN/`). A PR that changes
  `README.md`, `docs/install.md` or `docs/architecture.md` must update both translations in the
  same PR. Translations keep commands, code blocks, configuration keys and paths untranslated,
  link to pages of the same language, and keep the English anchor ids (see the HTML comment at
  the top of `docs/ja/install.md`).
- Shared dependencies are declared once in the root `[workspace.dependencies]`.
- Keep `gpueue-core` free of I/O other than its storage module, and keep `gpueue-helper`'s
  dependency tree minimal (no HTTP server, no SQLite).
