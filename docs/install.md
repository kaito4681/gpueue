# Installing and operating puzuz

This guide is for the administrator of the machine. Regular users need nothing beyond the `puzuz`
command on their `PATH`.

- [Requirements](#requirements)
- [Installing with install.sh](#installing-with-installsh)
- [Building](#building)
- [Installing](#installing)
- [Verifying](#verifying)
- [Configuration](#configuration)
- [Administrators](#administrators)
- [GPU detection](#gpu-detection)
- [Checkpoint storage](#checkpoint-storage)
- [Upgrading](#upgrading)
- [Uninstalling](#uninstalling)
- [Troubleshooting](#troubleshooting)

## Requirements

| Component | Requirement | Tested with |
| --- | --- | --- |
| OS | Linux with systemd as init | Ubuntu (Docker image with systemd as PID 1), x86_64 |
| systemd | 240 or newer (`puzuz doctor` checks this) | 259 |
| Kernel | `CONFIG_CHECKPOINT_RESTORE=y` (`/proc/sys/kernel/ns_last_pid` must exist), PID and mount namespaces, 5.5 or newer recommended (`clone3` with `set_tid`, used by CRIU to restore PIDs), 5.2 or newer for `open_tree`/`move_mount` (used to clean up hidden mounts), 5.8 or newer to restore (`openat2`, `faccessat2` and `statx` mount ids, used to check what a restore opens) | 7.0 |
| CRIU | 4.2 or newer | 4.2 (distribution package), 4.2.1 |
| NVIDIA driver (GPU jobs) | a driver whose `cuda-checkpoint` supports `--action lock/checkpoint/restore/unlock` and `--timeout`: r570 or newer (cuda-checkpoint itself supports r550, but only with `--toggle`, which puzuz does not use) | 595 |
| cuda-checkpoint (GPU jobs) | the binary from NVIDIA, with the same major version as the driver | 595 |
| nvidia-smi (GPU jobs) | the helper uses it to find CUDA processes and GPU memory | from the driver |
| Rust (building only) | 1.88 or newer, plus a C compiler (`cc`) for the helper's fork guard library | 1.98.1 |

Without an NVIDIA driver puzuz still runs CPU-only jobs, and checkpoints them as well.

### CRIU

Install the distribution's package (for example `sudo apt install criu`) or build it from
<https://criu.org/Installation>. In the default **manual** mode
(`cuda_checkpoint_mode = "manual"`) the helper calls cuda-checkpoint itself and runs CRIU without
plugins. The distribution package works for this, even though it ships without CRIU's CUDA plugin.

The **plugin** mode (`cuda_checkpoint_mode = "plugin"`) lets CRIU's CUDA plugin (`cuda_plugin.so`,
CRIU 4.0 or newer) drive cuda-checkpoint instead. It is optional; see
[CUDA plugin mode](#cuda-plugin-mode).

### cuda-checkpoint

NVIDIA distributes cuda-checkpoint as a prebuilt binary in
<https://github.com/NVIDIA/cuda-checkpoint>, under `bin/x86_64_Linux/cuda-checkpoint`. Put it
where root's `PATH` finds it, owned by root and not writable by others:

```sh
sudo install -o root -g root -m 0755 cuda-checkpoint /usr/local/bin/cuda-checkpoint
```

You can put it somewhere else and set `cuda_checkpoint_path` in `/etc/puzuz/config.toml`. Absolute
paths of `criu`, `cuda-checkpoint` and `nvidia-smi` must be owned by root, because the helper runs
them as root. Symlinks owned by root are followed and their targets checked the same way, and
`/nix/store` (owned by root, sticky, group `nixbld`) is accepted although it is group-writable, so
paths like `/run/current-system/sw/bin/criu` on NixOS work.

## Installing with install.sh

`scripts/install.sh` is the usual way to install puzuz on x86_64 Linux. It is attached to every
release. It downloads a release tarball, checks it, and runs that release's
`puzuz service install --bin-dir /usr/local/bin` (see [Installing](#installing) for what that
does).

### A token for the private repository

The repository is private, so every download goes through the GitHub API with a token that can
read it. Use a fine-grained personal access token limited to this repository with the
"Contents: Read-only" permission and an expiry date. `install.sh` takes the token from, in this
order:

1. the `GH_TOKEN` or `GITHUB_TOKEN` environment variable,
2. `--token-file FILE` (the first line of FILE),
3. `gh auth token`, run as the user who ran `sudo` (if the GitHub CLI is installed and logged in),
4. `/etc/puzuz/github-token`, which `--save-token` writes (owned by root, mode 0600).

The token is passed to `curl` in a header file, so it does not show up in `ps` or in the output.
Without any token, `install.sh` tries without authentication, which works only once the
repository is public. A missing or insufficient token fails with a message that says so (GitHub
answers 404, not 403, when a token cannot read a private repository).

### Installing

With the GitHub CLI logged in:

```sh
gh release download -R kaito4681/puzuz -p install.sh -O - | sudo bash
```

Without the GitHub CLI, fetch the script through the API with your token:

```sh
curl -fsSL -H "Authorization: Bearer $GH_TOKEN" \
  -H "Accept: application/vnd.github.raw" \
  https://api.github.com/repos/kaito4681/puzuz/contents/scripts/install.sh \
  | sudo GH_TOKEN="$GH_TOKEN" bash
```

Options go after `bash -s --` when the script comes from a pipe (for example
`... | sudo bash -s -- --version 0.2.0`), or after the file name with a downloaded copy
(`sudo bash install.sh --version 0.2.0`):

| Option | Meaning |
| --- | --- |
| `--version X` | install version X instead of the latest release |
| `--bin-dir DIR` | where to put `puzuz` and `puzuz-helper` (default: the directory of the current `service install` installation, else `/usr/local/bin`) |
| `--token-file FILE` | read the token from FILE |
| `--save-token` | save the token to `/etc/puzuz/github-token` (root, 0600) for `puzuz update` and later runs |
| `--uninstall` | remove puzuz (see [Uninstalling](#uninstalling)) |
| `--purge` | with `--uninstall`, also delete `/etc/puzuz` and `/var/lib/puzuz` |
| `-y`, `--yes` | do not ask before purging |

The token is saved only with `--save-token`. Without it, an expired or revoked token cannot break
a later run without you noticing which token was used.

`install.sh` checks, before downloading anything, that it runs as root, that systemd is PID 1, that
the machine is x86_64 with glibc 2.34 or newer, and that puzuz is not installed by a package
manager or Nix (see [Install method](#install-method)). For the last check it asks an installed
puzuz (the one the unit runs, else the one in `PATH`) with `puzuz service install-method`;
without one it refuses only the obvious cases itself (NixOS by `/etc/os-release`, a unit a package
owns, a unit linked into `/nix/store`), and the downloaded binary decides the rest before it
installs anything. It then:

1. looks up the release (`/repos/kaito4681/puzuz/releases/latest`, or `/releases/tags/v<X>`),
2. downloads `puzuz-v<X>-x86_64-unknown-linux-gnu.tar.gz` and its `.sha256` file into a
   temporary directory and checks the checksum,
3. if `minisign` is installed, also checks the `.minisig` signature against the release key built
   into the script (`packaging/minisign.pub`). Without `minisign` it says that the signature was
   not checked,
4. extracts the tarball (refusing links and paths outside it), runs the new
   `puzuz service install --bin-dir <DIR>` (`--bin-dir`, else the directory the installed units
   run puzuz from, else `/usr/local/bin`), and prints its `puzuz doctor` report,
5. prints how to install CRIU and cuda-checkpoint if they are missing. It never installs them.

If only some `puzuz doctor` checks fail (for example CRIU is missing) but the services run,
`install.sh` succeeds with a warning.

Afterwards, update with `sudo puzuz update` (see [Upgrading](#upgrading)). Running `install.sh`
again also installs the latest (or the given) version over the installed one. Running jobs keep
running either way.

Other ways to install (apt, rpm, AUR, Nix) are coming later. Until then, build from source as
described below.

### Verifying a release by hand

Each release has `puzuz-v<X>-x86_64-unknown-linux-gnu.tar.gz`, `.sha256` and `.minisig` files
and `install.sh`. To check a tarball yourself:

```sh
sha256sum -c puzuz-v0.1.0-x86_64-unknown-linux-gnu.tar.gz.sha256
minisign -Vm puzuz-v0.1.0-x86_64-unknown-linux-gnu.tar.gz -p packaging/minisign.pub
```

### The release signing key (maintainers)

Releases are signed in `.github/workflows/release.yml` with the minisign secret key stored in the
repository secrets `MINISIGN_SECRET_KEY` and `MINISIGN_PASSWORD`. The public key is
`packaging/minisign.pub`, which is built into `puzuz`, and `MINISIGN_PUBKEY` in
`scripts/install.sh`. The repository carries a test key until the real one is created, and the
release workflow refuses to run with it. To create the real key once:

```sh
scripts/dist/gen-signing-key.sh        # asks for a password; writes the secret key under ~
gh secret set MINISIGN_SECRET_KEY -R kaito4681/puzuz < ~/puzuz-release-key-*/minisign.key
gh secret set MINISIGN_PASSWORD -R kaito4681/puzuz
git commit packaging/minisign.pub scripts/install.sh
```

Back up the secret key and its password. A new key needs a new release of `puzuz` before
`puzuz update` accepts releases signed with it. Releases are created as drafts: publish the
draft on GitHub, or `install.sh` (which asks for the latest published release) does not see it.

## Building

Both binaries are needed: `puzuz` (CLI and daemon) and `puzuz-helper` (the root helper). A plain
`cargo build` only builds `puzuz`, so name both packages:

```sh
cargo build --release -p puzuz -p puzuz-helper
# -> target/release/puzuz, target/release/puzuz-helper
```

Without a local Rust toolchain you can build in the project's Docker image (Rust 1.98.1 on Debian
bookworm; the binaries need glibc 2.34 or newer):

```sh
scripts/test-in-docker.sh build --release -p puzuz -p puzuz-helper
# -> target/docker/release/puzuz, target/docker/release/puzuz-helper
```

Tagged versions also come as prebuilt binaries for x86_64 Linux (built on Debian bookworm, glibc
2.34 or newer) on the GitHub releases page: `puzuz-v<version>-x86_64-unknown-linux-gnu.tar.gz`
holds `puzuz`, `puzuz-helper` and the documentation, with `.sha256` and `.minisig` files next to
it. `install.sh` installs them (see above). By hand:

```sh
sha256sum -c puzuz-v0.1.0-x86_64-unknown-linux-gnu.tar.gz.sha256
tar xzf puzuz-v0.1.0-x86_64-unknown-linux-gnu.tar.gz
sudo puzuz-v0.1.0-x86_64-unknown-linux-gnu/puzuz service install --bin-dir /usr/local/bin
```

## Installing

```sh
sudo target/release/puzuz service install --bin-dir /usr/local/bin
```

The units run the binaries from the directory the `puzuz` binary is in, but only if that is
`/usr/bin` or `/usr/local/bin`. Otherwise pass `--bin-dir <DIR>` to copy `puzuz` and
`puzuz-helper` there. `puzuz-helper` must sit next to `puzuz`. Because `puzuz-helper` runs as
root, the directory and every parent must be owned by root and not group- or world-writable
(directories that do not exist yet are created with mode 0755); install refuses anything else.
`--no-start` enables the services without starting or restarting them. `--force` overwrites unit,
sysusers.d and tmpfiles.d files that `puzuz service install` did not write (see step 2).

`puzuz service install` exits non-zero when the final `puzuz doctor` report has a FAIL. After
`--no-start`, the failures that only mean "not running yet" (both units, both sockets, the
daemon and the helper connection) are ignored.

`puzuz service install` is safe to run again (for example after an upgrade). It does the
following:

1. Copies `puzuz` and `puzuz-helper` into `--bin-dir`, if given (mode 0755, replaced
   atomically).
2. Writes these files from the packaging templates built into the binary. Each file starts with
   an "Installed by `puzuz service install`" marker line and is overwritten on the next install.
   If one of them exists without the marker (written by an administrator or a package), install
   stops before changing anything, unless `--force` is given. Directories it creates (such as
   `/etc/puzuz`) get mode 0755 whatever the umask:
   - `/etc/systemd/system/puzuz-helper.service`, the root helper (from
     [packaging/systemd/puzuz-helper.service](../packaging/systemd/puzuz-helper.service)),
   - `/etc/systemd/system/puzuz.service`, the daemon, which runs as user `puzuz` with a strict
     sandbox that allows only the NVIDIA devices NVML needs (from
     [packaging/systemd/puzuz.service](../packaging/systemd/puzuz.service)),
   - `/etc/sysusers.d/puzuz.conf`, which defines the system user `puzuz` and the group
     `puzuz-admin`,
   - `/etc/tmpfiles.d/puzuz.conf`, the directories listed below.
3. Runs `systemd-sysusers` for that file and, if needed, `groupadd --system puzuz-admin`.
4. Runs `systemd-tmpfiles --create` for that file, which creates:

   | Path | Mode, owner | Contents |
   | --- | --- | --- |
   | `/var/lib/puzuz` | 0755 root | base directory (`data_dir`) |
   | `/var/lib/puzuz/daemon` | 0700 puzuz | `state.sqlite`, the daemon's database |
   | `/var/lib/puzuz/helper` | 0711 root | the helper's job records and event queue |
   | `/var/lib/puzuz/checkpoints` | 0700 root | checkpoint images, `<id>/` per job |
   | `/var/lib/puzuz/logs` | 0755 root | job output, `<uid>/<id>.log` (the helper creates `<uid>/` with mode 2770, owned by the user, group `puzuz`) |
   | `/var/lib/puzuz/lib` | 0755 root | the fork guard library (`puzuz-forkguard-<hash>.so`) |
   | `/var/lib/puzuz/criu-no-plugins` | 0755 root | empty CRIU plugin directory for manual mode |
   | `/run/puzuz` | 0755 puzuz | `puzuz.sock` (daemon, mode 0666) and `helper.sock` (helper, owned by `puzuz`, mode 0600) |
   | `/run/puzuz-helper` | 0700 root | bind mount of `/` used by `criu restore --root` |

5. Creates `/etc/puzuz/config.toml` from
   [packaging/config.toml](../packaging/config.toml) **only if it does not exist yet**. If
   `nvidia-smi` works, it appends a `[[gpus]]` entry for each GPU it lists (a fallback for when
   NVML fails; see [GPU detection](#gpu-detection)).
6. Runs `systemctl daemon-reload`. If the units or the binaries changed, it runs
   `systemctl try-restart` on both services. Then it runs
   `systemctl enable --now puzuz-helper.service puzuz.service`, waits up to 10 seconds for the
   daemon to connect to the helper, and prints the `puzuz doctor` report.

Unit files owned by a distribution package (`/usr/lib/systemd/...`) are never touched. Customize
the units with drop-ins (`sudo systemctl edit puzuz-helper.service`). Edits to the files in
`/etc/systemd/system` are lost on the next install.

### Install method

`puzuz service install` must not share a machine with a package manager or Nix: its units in
`/etc/systemd/system` would shadow the packaged ones, and both would use `/etc/puzuz` and
`/var/lib/puzuz`. puzuz tells the ways apart by the unit files, `puzuz.service` and
`puzuz-helper.service` each:

| Install method | Recognized by |
| --- | --- |
| `puzuz service install` | `/etc/systemd/system/<unit>` starting with the install marker line |
| distribution package | `/usr/lib/systemd/system/<unit>` (or `/lib/systemd/system/...`) that `dpkg-query -S`, `rpm -qf` or `pacman -Qo` names an owner for (asked under both paths, since dpkg may have registered the `/lib` one on merged-`/usr` systems). If none of these tools exists, the unit counts as a package of unknown origin |
| unowned unit | such a unit that no package owns: left over from a removed package, or copied there by hand (`make install`) |
| Nix | `/etc/systemd/system/<unit>` resolving into `/nix/store` (the NixOS `services.puzuz` module, system-manager), or a packaged unit that resolves into or runs a binary from `/nix/store` |
| by hand | `/etc/systemd/system/<unit>` without the marker, and no packaged unit (also when its `ExecStart=` is below `/nix/store`) |

Next to a packaged (or unowned) unit, a `/etc/systemd/system/puzuz.service` without the marker is
the administrator's replacement of it (`systemctl edit --full puzuz`), and a symlink to
`/dev/null` there is a mask (`systemctl mask puzuz`). Neither is a second install method: puzuz
reports the package as "overridden" or "masked". A mask without a package is ignored.

The operating system is read from `/etc/os-release` (else `/usr/lib/os-release`), `ID` and
`ID_LIKE`. It only decides the wording of hints and that NixOS (`ID=nixos`) uses its module; it
never decides the install method (the same distribution can have puzuz from a package or from
`install.sh`). `/etc/NIXOS` is not looked at.

What each command does (the hidden `puzuz service install-method` prints the keyword, for
`install.sh`):

| Install method | `service install` | `service uninstall` | `update` | `doctor` | keyword |
| --- | --- | --- | --- | --- | --- |
| not installed | installs | says so | refuses: use `install.sh` | INFO | `none` |
| not installed, NixOS | refuses: use the `services.puzuz` module | says so | refuses: use the module | INFO "not installed (NixOS: use the services.puzuz module)" | `none` |
| `puzuz service install` | re-installs | uninstalls | updates | OK | `service-install` |
| distribution package (dpkg, rpm, pacman, or unknown) | refuses, even with `--force` | refuses | refuses | OK (INFO when masked) | `package` |
| unowned unit | only with `--force`; delete the file first instead | refuses | refuses | WARN | `unowned` |
| Nix | refuses, even with `--force` | refuses | refuses | OK | `nix` |
| by hand | only with `--force` | refuses | refuses | INFO | `manual` |
| mixed | refuses: uninstall the `service install` copy first | removes only what `service install` wrote (no `--purge`) | refuses | FAIL | `mixed` |

"Mixed" is `service install` together with a package or Nix. A `service install` next to an
unowned leftover in `/usr/lib` stays a `service install` (its units in `/etc` win); `doctor`
warns and tells you to delete the leftover. When `puzuz.service` and `puzuz-helper.service` are
installed in different ways, or only one of them exists, `doctor` warns, `service status` says
so, and `service install` refuses if either unit belongs to a package or Nix (otherwise it
installs both and so fixes it). `service install` also refuses when `/etc/systemd/system` itself
resolves into `/nix/store`, since it cannot write there.

The refusals name the way to update or remove puzuz that fits the install method:

| Install method | Update | Remove |
| --- | --- | --- |
| dpkg | `sudo apt update && sudo apt upgrade` | `sudo apt remove <package>` |
| rpm | `sudo dnf upgrade` | `sudo dnf remove <package>` |
| pacman | `yay -Syu` (AUR) or `sudo pacman -Syu` | `sudo pacman -R <package>` |
| package of unknown origin | your package manager (named after `ID`/`ID_LIKE` when known) | the same |
| Nix on NixOS | `nix flake update && sudo nixos-rebuild switch` | `services.puzuz.enable = false`, then `sudo nixos-rebuild switch` |
| Nix elsewhere | the Nix configuration that created the unit | the same |
| `puzuz service install` | `sudo puzuz update` | `sudo puzuz service uninstall` |

The daemon runs with `ProtectSystem=strict`: it can only write below the unit's
`ReadWritePaths=/var/lib/puzuz/daemon /var/lib/puzuz/logs /run/puzuz`. If you move `data_dir`,
`state_db`, `logs_dir` or `api_socket` elsewhere in `config.toml`, add the new directories in a
drop-in, or the daemon fails with "Read-only file system":

```sh
sudo systemctl edit puzuz.service
# [Service]
# ReadWritePaths=/srv/puzuz/daemon /srv/puzuz/logs
sudo systemctl restart puzuz.service
```

`puzuz doctor` warns (`daemon write paths`) when a configured directory is outside the unit's
effective `ReadWritePaths=`.

The helper's `TimeoutStopSec=30min` is the initial budget for checkpointing all jobs when the
system shuts down. While it dumps, the helper keeps asking systemd for more time
(`EXTEND_TIMEOUT_USEC`), but only up to the end of the running dump's deadline plus its rollback
(see [Checkpoint timeouts](#checkpoint-timeouts)), so a stuck dump cannot hold up the shutdown
forever. Jobs are dumped one after another. With systemd versions that ignore the extension, 30
minutes covers about 50 GiB of RSS + VRAM in total at the rate the deadline allows (30 s/GiB),
and several hundred GiB at the measured speed of an NVMe disk (~2.5 s/GiB); raise it in a
drop-in if your jobs are larger or your disk is slower.

### Checkpoint timeouts

A whole checkpoint (CUDA lock + checkpoint of every CUDA process, then `criu dump`) or restore
(`criu restore`, then CUDA restore + unlock) must finish within a deadline that scales with the
job's size:

```
deadline = checkpoint_deadline_base_secs + checkpoint_deadline_secs_per_gib * GiB
         = 180 s + 30 s * GiB            (defaults)
```

GiB is the job's RSS + VRAM, rounded up, for a checkpoint, and the image size for a restore. A
14 GiB job gets 180 + 30 * 14 = 600 s. Each step also has its own timeout (a lock: the lock
`--timeout` of 30 s + 15 s; a CUDA checkpoint or restore: 300 s; an unlock: 60 s), and
`criu_dump_timeout_secs` / `criu_restore_timeout_secs` can cap `criu` alone; a step gets
whichever ends first.

When the deadline passes, the running tool is killed together with its whole process group and
every process it forked. Then:

- **checkpoint**: the job is resumed - its stopped tasks get SIGCONT, frozen cgroups within the
  job's cgroup are thawed, CUDA processes that were (or may have been) checkpointed are restored
  and every locked one is unlocked - and the checkpoint fails with "the checkpoint did not finish
  within its deadline of ... s for ... GiB of memory; the job was resumed". The partial image is
  removed. The same rollback runs when `criu` is killed from outside (e.g. `kill -9`, the OOM
  killer). Each CUDA process is rolled back on its own: one that has exited meanwhile is skipped,
  and one that is stopped or frozen is left alone (and reported) without holding up the others.
  In plugin mode, CRIU's CUDA plugin locks and checkpoints the CUDA processes inside
  `criu dump` and cannot undo that once CRIU is killed, so the helper asks
  `cuda-checkpoint --get-state` for each of them and restores and unlocks them as needed.
  A `criu dump` that finishes in the instant its deadline passes counts as finished.

  Caveat: killing `criu dump` cannot always be undone. While it collects a process's state, CRIU
  runs parasite code inside that process; if CRIU is killed at that moment, the process is
  released in the middle of that code and may crash. "The job was resumed" therefore means that
  the rollback succeeded (the job was thawed and its CUDA state resumed), not that every process
  of the job survived; when `criu` was killed, the message says so.
- **restore**: the restored processes are killed and the restore fails; the image is kept.

Measured on an NVMe disk (RTX 5070 Ti, 38 GB RAM), cold and warm page cache:

| step | time |
| --- | --- |
| CUDA lock / unlock | 0.01-0.04 s |
| CUDA checkpoint (VRAM to host memory) | 0.6-1.0 s/GiB (1.3-11.8 GiB of VRAM; 11.8 GiB: 8-10 s) |
| CUDA restore | 0.5-0.9 s/GiB (11.8 GiB: 6-8 s) |
| `criu dump` | 0.3-1.8 s/GiB (1-14 GiB of RSS; 14 GiB: 18-25 s, slower under memory pressure) |
| `criu restore` | 0.3-0.7 s/GiB (14 GiB: 8.6 s warm, 9.2 s cold) |

A GPU job with 11.8 GiB of VRAM and 1.2 GiB of RSS (13 GiB, deadline 600 s) took 19-20 s to
checkpoint and 14-16 s to restore, in manual and in plugin mode alike (plugin mode: `criu dump`
20-21 s, `criu restore` 16 s, the CUDA steps included). So a job needs about 1.5-2.5 s per GiB
of RSS + VRAM end to end; the default deadline leaves a margin of more than 10x (about 30x for
that job; enough for a disk of about 35 MB/s). On slower storage (network file systems, HDDs),
raise `checkpoint_deadline_secs_per_gib`. The fixed 300 s timeouts of a CUDA checkpoint and
restore leave 10x for up to about 30 GiB of VRAM per process at the measured speed (an 80 GB
GPU at the same speed would still finish in about a quarter of them).

## Verifying

```sh
puzuz doctor           # every check, with a `fix:` line for each problem; exits non-zero on FAIL
puzuz service status   # both units, and whether the daemon is reachable and connected to the helper
```

`puzuz doctor` checks the configuration file and its ownership, the install method (see
[Install method](#install-method)), `criu` (version 4.2 or newer), the
NVIDIA driver, `cuda-checkpoint` (manual mode support, version matching the driver), the GPUs the
daemon detected (with NVML or from `[[gpus]]`) against `nvidia-smi` (see
[GPU detection](#gpu-detection)), the kernel, the systemd version, both units, the ownership of
the helper binary and of every directory above it (taken from the unit's effective `ExecStart=`,
drop-ins included), whether the daemon's directories are inside its `ReadWritePaths=`, both
sockets, the daemon and helper versions and their connection, the checkpoint
directory (exists, not tmpfs, free space) and the total checkpoint size, whether you are an
administrator, and (unless `update_check = false`) whether a newer release exists. That last
check is at most INFO, and so is not being able to reach GitHub (a user without a token cannot
read the private repository). It does not run `criu check`. Run `sudo criu check` yourself to see what CRIU
supports on this kernel.

Try a job:

```sh
puzuz add -- 'for i in $(seq 1000); do echo $i; sleep 1; done'
puzuz checkpoint 0 && puzuz status
puzuz restore -i 0 && puzuz follow 0
```

## Configuration

The daemon and the helper read `/etc/puzuz/config.toml`. Every key is optional. Unknown keys are
ignored. A missing file means all defaults. The installed file
([packaging/config.toml](../packaging/config.toml)) lists every key with comments: a commented
line `# key = value` shows the default, and lines indented below `# Example:` are examples for
keys that are unset by default (such as `checkpoint_size_limit_bytes`) and for `[[gpus]]`. After a
change, restart whatever uses the key. Restarting the helper does not affect running jobs:

```sh
sudo systemctl restart puzuz-helper.service puzuz.service
```

The helper runs as root and **refuses to start** unless the file and every parent directory
(`/etc/puzuz`, `/etc`, `/`) are owned by root, not group- or world-writable, and not symlinks.
The same rule applies to the directories it writes to or loads code from: `data_dir`,
`helper_state_dir`, `logs_dir`, `checkpoints_dir`, `<data_dir>/criu-no-plugins` (passed to
`criu -L`, and it must be empty), `<data_dir>/lib` (the fork guard) and all their parents must be
owned by root and not group- or world-writable (symlinks owned by root are followed and their
targets checked, unlike for the configuration file); the helper checks them before it creates
anything there. Keep the layout that
tmpfiles.d creates. The daemon stores its database in the `daemon/` subdirectory because
`data_dir` itself must belong to root.

Groups and GPU slot limits are not configured here. They are stored in the daemon's database and
changed with `puzuz group`, `puzuz parallel` and `puzuz gpu parallel`.

Paths of files, directories and sockets must be absolute. Programs (`criu_path`,
`cuda_checkpoint_path`, `nvidia_smi_path`, `nvml_library`, the first word of `shell`) are either
a bare name, looked up in `PATH`, or an absolute path; relative paths such as `bin/criu` are
rejected. Every `*_secs` key is at most one year (31536000).

| Key | Default | Used by | Meaning |
| --- | --- | --- | --- |
| `data_dir` | `"/var/lib/puzuz"` | both | base of `daemon/state.sqlite`, `helper/`, `logs/`, `checkpoints/`, `lib/`, `criu-no-plugins/` |
| `state_db` | `<data_dir>/daemon/state.sqlite` | daemon | the SQLite database |
| `helper_state_dir` | `<data_dir>/helper` | helper | job records and event queue (root-owned) |
| `logs_dir` | `<data_dir>/logs` | both | job output, `<uid>/<id>.log` (root-owned) |
| `checkpoints_dir` | `<data_dir>/checkpoints` | both | checkpoint images (root-owned, not tmpfs) |
| `api_socket` | `"/run/puzuz/puzuz.sock"` | daemon | the socket clients connect to (`puzuz --socket` / `PUZUZ_SOCKET` on the client side) |
| `helper_socket` | `"/run/puzuz/helper.sock"` | both | the helper's socket |
| `daemon_user` | `"puzuz"` | helper | the only user allowed to connect to the helper |
| `grace_secs` | `30` | both | seconds between SIGTERM and SIGKILL when a job is stopped |
| `shell` | `["sh", "-c"]` | daemon | command prefix that the job's command line is appended to |
| `checkpoint_size_limit_bytes` | unset (no limit) | daemon | warn when all images together exceed this size (images are never deleted automatically) |
| `checkpoint_size_warning_interval_secs` | `3600` | daemon | how often each administrator is reminded while the limit is exceeded |
| `checkpoint_tcp_close` | `true` | both | close TCP connections when checkpointing (`criu --tcp-close`) |
| `criu_path` | `"criu"` | helper | name (looked up in `PATH`) or root-owned absolute path |
| `cuda_checkpoint_path` | `"cuda-checkpoint"` | helper | same |
| `nvidia_smi_path` | `"nvidia-smi"` | helper | same |
| `cuda_checkpoint_mode` | `"manual"` | helper | `"manual"`: the helper calls cuda-checkpoint around criu. `"plugin"`: CRIU's CUDA plugin does it. |
| `checkpoint_deadline_base_secs` | `180` | helper | fixed part of the deadline of a whole checkpoint or restore (see [Checkpoint timeouts](#checkpoint-timeouts)) |
| `checkpoint_deadline_secs_per_gib` | `30` | helper | part of that deadline per GiB of RSS + VRAM (checkpoint) or image (restore) |
| `criu_dump_timeout_secs` | unset | helper | optional cap on `criu dump` alone; unset: only the deadline applies |
| `criu_restore_timeout_secs` | unset | helper | optional cap on `criu restore` alone; unset: only the deadline applies |
| `fork_guard` | `true` | helper | preload the fork guard into jobs (see the README's limitations) |
| `gpu_detection` | `"auto"` | daemon | `"auto"`: NVML, falling back to `[[gpus]]`. `"static"`: only `[[gpus]]`. |
| `nvml_library` | unset | daemon | NVML library path or name; by default `libnvidia-ml.so.1`, then `libnvidia-ml.so` |
| `update_source` | `"github:kaito4681/puzuz"` | CLI | where `puzuz update` and `puzuz doctor` look for releases: `github:<owner>/<repo>` |
| `update_check` | `true` | CLI | whether `puzuz doctor` asks `update_source` for a newer release |
| `[[gpus]]` | empty | daemon | static GPU list: `index` (required), `uuid`, `name`, `memory_total_bytes` (see [GPU detection](#gpu-detection)) |

`puzuz-helper serve` also accepts command-line flags (`puzuz-helper serve --help`). They override
the file. Prefer the file: `puzuz doctor` reports the file's values and warns when the unit's
`ExecStart=` passes flags.

## Administrators

Administrators are root and the members of `puzuz-admin` (primary or supplementary group). They
can act on every user's jobs and run the admin-only commands (`group add/remove`, `parallel`,
`gpu parallel`, pausing/resuming groups with `pause/start/kill -g` or `-a`, `shutdown`, `reset`
of all jobs).

```sh
sudo usermod -aG puzuz-admin alice
```

The daemon looks the group up on every request (through NSS), so the change applies at once,
without logging in again.

## GPU detection

The daemon detects GPUs once, when it starts. Restart `puzuz.service` after adding, removing or
replacing a GPU.

- With `gpu_detection = "auto"` (the default) it loads NVML (`libnvidia-ml.so.1`) at run time and
  ignores `[[gpus]]`. If NVML cannot be loaded or initialized, it logs why (`journalctl -u
  puzuz.service`, a warning starting with "GPU detection with NVML failed") and uses the
  `[[gpus]]` list from the configuration instead, or runs without GPUs if there is none.
- With `gpu_detection = "static"` it always uses the `[[gpus]]` list.

NVML needs read-write access to `/dev/nvidiactl` and `/dev/nvidia<N>`. These nodes are normally
mode 0666, so the `puzuz` user needs no extra group. The packaged `puzuz.service` therefore does
not use `PrivateDevices=yes` (its private `/dev` has no NVIDIA nodes). It uses
`DevicePolicy=closed` with `DeviceAllow=` for the NVIDIA control and GPU devices instead, so the
daemon can open only those and the standard pseudo devices. If you customize the unit, keep these
lines, or NVML fails with "NVIDIA driver is not loaded" (node hidden) or "internal driver error"
(node not allowed). The log message says which one.

`puzuz service install` also writes the `[[gpus]]` list from `nvidia-smi` when it creates the
configuration file, as the fallback. `puzuz doctor` asks the daemon which GPUs it detected and
how, and compares them with `nvidia-smi`. It warns when NVML failed in auto mode, when the daemon
has no GPUs while `nvidia-smi` lists some, when the daemon's GPUs (or the `[[gpus]]` fallback
list) differ from `nvidia-smi`, and when `gpu_detection = "static"` and `[[gpus]]` is empty. An
empty `[[gpus]]` is fine while NVML works. The list looks like this:

```toml
[[gpus]]
index = 0                 # PCI bus order, as nvidia-smi and CUDA_DEVICE_ORDER=PCI_BUS_ID
uuid = "GPU-..."          # nvidia-smi --query-gpu=index,uuid,name,memory.total --format=csv
name = "NVIDIA GeForce RTX 4090"
memory_total_bytes = 25757220864
```

| Field | Default | Meaning |
| --- | --- | --- |
| `index` | required | the GPU's index in PCI bus order, as nvidia-smi numbers it. Unique. `puzuz add --gpu` takes it, and jobs see it in `CUDA_VISIBLE_DEVICES` (with `CUDA_DEVICE_ORDER=PCI_BUS_ID`). |
| `uuid` | `""` | the GPU's UUID. The daemon warns at start-up when a GPU with a different UUID than before is found at an index. |
| `name` | `""` | the model, shown by `puzuz gpu` and recorded with each checkpoint. Restoring requires the same model. |
| `memory_total_bytes` | unset | the GPU's memory in bytes, shown by `puzuz gpu` |

`puzuz gpu` shows what the daemon uses: with NVML, each GPU's name, memory and driver version.
The GPU end-to-end test checks NVML detection inside the packaged unit.

A GPU that the daemon knew before but no longer finds stays listed as unavailable, and keeps its
slot limit. Jobs that need it stay queued, and new jobs that ask for it are rejected. Each GPU
starts with a limit of 1 job. Change it with `puzuz gpu parallel <N> [--gpu LIST]` (`0` means
unlimited).

## Checkpoint storage

- Images are stored in `checkpoints_dir` (default `/var/lib/puzuz/checkpoints/<job id>/`), which
  only root can read. Each directory holds `images/` (CRIU image files), `dump.log`,
  `restore.log`, `meta.json` (GPU, driver, size, time) and a `COMPLETE` marker that is written
  last. Directories without the marker are incomplete and are cleaned up when the helper starts.
  The helper also writes `shutdown-manifest.json` there after the dumps at system shutdown.
- Each job has at most one image. It is deleted when the restore succeeds, when the job is removed
  (`remove`, `clean`, `reset`), or when the dump fails. A failed restore keeps the image.
- After a failed dump the helper moves its `dump.log` to `failed-logs/<job id>-<unix ms>.log` in
  the same directory (root only) and keeps the newest 20 of them. The job's last error names the
  file.
- Space: before each dump the helper requires free space of at least (RSS + VRAM of the job) × 1.2.
  Otherwise the checkpoint is refused and the job keeps running. Images are not compressed. A job with 7 GB of VRAM produced an
  8.1 GB image in tests. Do not use tmpfs (`puzuz doctor` fails on it). It also warns below 20 GiB
  free, or below the space left under `checkpoint_size_limit_bytes`.
- Limit: with `checkpoint_size_limit_bytes`, the owner of a checkpoint that takes the total over
  the limit is warned once. Administrators are warned on their commands, at most every
  `checkpoint_size_warning_interval_secs`, and `puzuz doctor` reports it. Dumps are not refused
  and nothing is deleted automatically. Administrators see the image sizes of all jobs in
  `puzuz status`.
- To use another disk, create a root-owned directory (mode 0700, all parents root-owned and not
  group- or world-writable), set `checkpoints_dir`, and make sure the helper is stopped before
  that filesystem is unmounted at shutdown:

  ```sh
  sudo install -d -o root -g root -m 0700 /data/puzuz-checkpoints
  sudo systemctl edit puzuz-helper.service
  #   [Unit]
  #   RequiresMountsFor=/data/puzuz-checkpoints
  sudo systemctl restart puzuz-helper.service puzuz.service
  ```

  Move or restore existing checkpoints before you change the path.

## CUDA plugin mode

By default (`cuda_checkpoint_mode = "manual"`) the helper runs cuda-checkpoint itself (`lock`,
`checkpoint`, then `criu dump`; `criu restore`, then `restore` and `unlock`) and points CRIU at an
empty plugin directory. In plugin mode it runs only `criu`, and CRIU's CUDA plugin calls
cuda-checkpoint at the right moments of the dump and the restore. To enable it:

1. Build CRIU 4.0 or newer with the plugin (puzuz is tested with 4.2.1) and install the plugin in
   CRIU's built-in plugin directory `/usr/lib/criu`. In plugin mode the helper passes no `-L`, so
   CRIU looks there.

   ```sh
   git clone --branch v4.2.1 https://github.com/checkpoint-restore/criu && cd criu
   make -j criu cuda_plugin
   sudo install -m 0755 criu/criu /usr/local/sbin/criu
   sudo install -D -m 0755 plugins/cuda/cuda_plugin.so /usr/lib/criu/cuda_plugin.so
   ```

2. Put cuda-checkpoint in `/usr/local/bin` (or elsewhere on the default `PATH` of systemd
   services). The plugin runs `cuda-checkpoint` by name and ignores `cuda_checkpoint_path`.
3. Set `cuda_checkpoint_mode = "plugin"` in `/etc/puzuz/config.toml`, run
   `sudo systemctl restart puzuz-helper.service`, and check with `puzuz doctor` (the `cuda plugin`
   line).

Each image records its mode in `meta.json` and is restored in that mode, so existing checkpoints
survive a change of `cuda_checkpoint_mode`. Restoring a plugin-mode image needs the plugin, so
keep it installed until those images are restored. If the plugin is missing, the helper refuses to
dump CUDA jobs in plugin mode (they keep running) instead of letting the dump fail. CPU-only jobs
do not need it.

Trade-offs:

| | manual (default) | plugin |
| --- | --- | --- |
| CRIU | any CRIU 4.2 or newer, including distribution packages without the plugin | CRIU built with `cuda_plugin.so` |
| GPU on restore | same model and driver; a different card of the same model is remapped (`--device-map`) | same model and driver, and the same GPU UUIDs (CRIU 4.2.1's plugin cannot remap) |
| Timeouts | the whole-operation deadline, plus a timeout per cuda-checkpoint step (lock, checkpoint, restore, unlock) | the whole-operation deadline only (it covers the plugin's cuda-checkpoint calls inside `criu`) |
| Failure handling | the helper rolls back (restores and unlocks) the CUDA steps it took | CRIU's plugin resumes the processes when the dump fails |

Both modes behave the same for users. In both, the fork guard is what makes PyTorch DataLoader
workers forked from a CUDA process checkpointable: without it, plugin mode fails on them as well
(`handle_device_vma plugin failed`, `Can't handle non-regular mapping`). The guard is preloaded
into every job regardless of the mode. Paused jobs are resumed for the dump in both modes, because
cuda-checkpoint hangs on a stopped process.

## Filesystems and mounts

Each job runs in a mount namespace of its own. When the job starts, the shim copies the host's
mounts into it and then fixes them, so that what the job sees is what CRIU dumps and recreates on
restore:

- Automount points (`x-systemd.automount`, autofs direct maps) are triggered once, before the job
  runs, and the job's namespace gets the mounted filesystem itself instead of the automount point.
  An automount whose filesystem does not appear within 20 s (an NFS server that does not answer)
  is left as it is, and the job cannot use that path.
- The namespace is private from then on: filesystems that the administrator mounts or unmounts
  later do not show up in running jobs. Start new jobs to use them.
- Mounts below `/run` and `/media`, and mounts hidden under other mounts, are removed. Each job
  gets its own tmpfs on `/tmp` and `/dev/shm`, which is saved in the checkpoint.

CRIU recreates every mount of the job's namespace on restore by bind-mounting the same host path,
so **every filesystem that was mounted when the job started must be mounted again, at the same
path and with the same type, before the job can be restored**, even if the job never used it. The
filesystem may come back as a new mount (another device number, a new loop device, a new NFS
mount). At the dump the helper records the filesystem type and mount point of each of these
paths, and the files and working directories the job has open. Before a restore it checks that
they are still there (triggering automount points on the way) and refuses with a clear error
otherwise (see [Restore failures](#restore-failures)). The image is kept, so you can mount the
filesystem and run `puzuz restore` again.

Before a restored job runs, the helper checks that its owner could open each of its files the way
the job has it open, by the same path and without symlinks (see
[Restores and paths](architecture.md#restores-and-paths)). A file the job already had open at the
dump passes even if its owner could not open it that way now (a file the job created read-only,
or an sshfs mount on which every file shows as root's), as long as it is that very file:

- on the same mounted filesystem (no reboot or remount in between): the same device and inode, or
- after the filesystem was mounted again: the same owner, group, mode, size, modification time
  and change time as right after the dump. Device numbers change with a new mount, and inode
  numbers are not stable on every filesystem: FUSE filesystems such as sshfs number inodes in the
  order they are looked up, so after a reboot the job's files usually have other numbers (and
  another file may have the old one). The change time is set by the kernel (or, on sshfs, by the
  server) and cannot be set by the job's owner on a file that is not theirs.

So do not touch a checkpointed job's files until it is restored: a file whose times changed is
checked against its owner's permissions like any other, and with an sshfs mount that shows files
as root's (`root@server:`, `allow_other,default_permissions`) the restore is then refused. On
filesystems where the owner may open the files as the job does (the usual case), changes do not
matter.

Tested across a simulated reboot (`scripts/e2e-test.sh`, scenarios 15 and 16) with the job's
working directory and open files on each of these:

| Mount | Restore after a reboot |
|---|---|
| separate local filesystem (ext4 on a loop device) mounted at a fixed path, on another device after the reboot | works |
| bind mount (`/home/<user>` from another filesystem), read-only bind mount, bind mount with private propagation; `/` with shared propagation (the systemd default) | works |
| overlayfs | works |
| `x-systemd.automount` (job started before and after the trigger; not yet triggered at restore time) | works: the restore pre-check triggers it |
| FUSE network filesystem (sshfs, mounted by root with `allow_other,default_permissions`, so that files show as root's), with the job writing and renaming its own files there, and other inode numbers after the reboot | works while the FUSE daemon is up again before the restore |
| filesystem not mounted after the reboot (missing fstab entry, NFS server down) | refused: `<path> needs <type> on <mount point>` |
| tmpfs mounted by the administrator (e.g. `/scratch`) with the job's files or working directory on it | refused: its contents are gone after a reboot. Use a tmpfs only for jobs that do not need to survive one. A restore on the same boot works. |

Not tested, or not supported:

- **Kernel NFS** (NFSv3/v4) was not tested: the Docker test environment cannot load the host's
  `nfs` module. NFS behaves like any other filesystem in the checks above. Mount it from
  `/etc/fstab` (`_netdev`, or `x-systemd.automount`) so that it is mounted before the helper
  starts and unmounted only after the helper has dumped the jobs at shutdown
  (`puzuz-helper.service` is ordered after `remote-fs.target`). Files that the job had open are
  reopened by path, so they must still exist on the server.
- **autofs indirect maps** (an autofs daemon mounting `/net/<host>` or `/home/<user>` on demand):
  keys mounted when the job starts are visible to it; keys first accessed later are not (the
  namespace is private). Restoring such a namespace has not been tested.
- **Removable media and `/run`:** not visible to jobs at all.
- Filesystems that cannot be mounted again at the same path after a reboot (paths with a boot ID
  or a random name) make every job that started while they were mounted unrestorable.

## Upgrading

How to update depends on how puzuz was installed (`puzuz doctor` shows it as "install method",
and its "update" line says when a newer release exists):

| Installed with | Update with |
| --- | --- |
| `install.sh` (or `puzuz service install` of a release) | `sudo puzuz update` |
| built from source | `cargo build --release -p puzuz -p puzuz-helper`, then `sudo target/release/puzuz service install --bin-dir /usr/local/bin` |
| a distribution package or Nix (later) | the package manager or Nix (see [Install method](#install-method)); `puzuz update` refuses and names the command |

### puzuz update

```sh
puzuz update --check          # is there a newer release? (any user; without root, update only checks)
sudo puzuz update             # install the latest release
sudo puzuz update --version 0.2.0 --yes
```

`puzuz update` needs a GitHub token that can read the repository, like `install.sh`: `GH_TOKEN`
or `GITHUB_TOKEN` (`sudo GH_TOKEN=... puzuz update`), or `/etc/puzuz/github-token` (root, 0600),
which `install.sh --save-token` writes. It then:

1. checks that puzuz was installed by `puzuz service install` (not by a package, Nix or by
   hand, and that both units belong to it) and
   that it runs as the installed binary (the directory in `puzuz.service`'s `ExecStart=`),
2. looks up the latest release (or `--version`) and compares it with its own version. Nothing
   happens when it is up to date. An older version is installed only when asked for with
   `--version`,
3. asks for confirmation (`-y/--yes` skips it),
4. downloads the tarball, `.sha256` and `.minisig` into `/var/lib/puzuz/update` (root, 0700),
   checks the minisign signature with the release key built into `puzuz`, and the checksum,
5. extracts the tarball (regular files and directories only) and asks the new `puzuz` which
   database schema it supports. A release that cannot open the daemon's database (an older one,
   from before a schema change) is refused before anything changes,
6. runs the new `puzuz service install --bin-dir <the installed directory>`, waits until the
   daemon runs the new version and is connected to the helper, and deletes the download.

`install.sh` (run again, optionally with `--version`) works too and does the same, except that it
checks the signature only when `minisign` is installed.

`service install` (and so both ways above) replaces the binaries and units and restarts both
services if anything changed. The configuration file is kept.

- **Running jobs are not affected.** They run in their own systemd units
  (`puzuz-job-<id>.service`, or `puzuz-job-<id>-r<n>.service` after a restore). A new daemon picks
  up the state from its database, and a new helper finds the running jobs again.
- **Helper restart:** a plain `systemctl stop/restart puzuz-helper` (not a system shutdown) never
  checkpoints anything. The stopping helper refuses new spawns and restores and lets operations in
  flight finish first. A dump in progress can take a while, and a second SIGTERM makes it exit at
  once. Launches it refused or abandoned are queued again by the daemon and retried when the
  helper is back. While the helper is down, `pause`, `kill`, `checkpoint` and resuming a paused job
  fail with "helper unavailable". `add -i`, `start`, `restore -i` and `restart -i` are accepted,
  and the job is launched when the helper reconnects.
- **Versions:** the CLI and the daemon must be the same version. A mismatched client gets an
  error that says which side to update, and `puzuz doctor` warns about it. Update the `puzuz`
  binary that users run at the same time.
- Old fork guard libraries in `/var/lib/puzuz/lib` are kept on purpose: jobs checkpointed before
  the upgrade need the exact file to restore.
- Restore or remove checkpointed jobs before upgrading the NVIDIA driver. A restore with a
  different driver version is refused.

## Uninstalling

```sh
sudo puzuz service uninstall            # stop, disable and remove the units and snippets
sudo puzuz service uninstall --purge    # also delete /etc/puzuz, /var/lib/puzuz, /run/puzuz, /run/puzuz-helper
```

`install.sh --uninstall [--purge] [--yes]` runs the same command and then also deletes the
`puzuz` and `puzuz-helper` binaries from the directory the units ran them from:

```sh
gh release download -R kaito4681/puzuz -p install.sh -O - | sudo bash -s -- --uninstall
```

`--purge` asks for confirmation on the terminal. When there is no terminal, pass `--yes`.

- Only files that start with the install marker are removed. Others are reported and kept.
- Running jobs keep running in their own units. `uninstall` warns about them, and `--purge`
  refuses to run while any `puzuz-job-*` unit is active. Stop them first with
  `sudo systemctl stop 'puzuz-job-*'`, or checkpoint/kill them before uninstalling.
- `--purge` asks for confirmation (`-y/--yes` skips it). It does not descend into mount points
  (for example the bind mount of `/` below `/run/puzuz-helper`). Those are reported for you to
  unmount and delete by hand.
- The system user `puzuz`, the group `puzuz-admin` and the binaries are always kept.

## Troubleshooting

### Where to look

```sh
journalctl -u puzuz -u puzuz-helper          # daemon and helper
journalctl -u 'puzuz-job-*'                  # job units (start/stop, OOM kills)
puzuz status; puzuz log <id>                 # job state, last error, output
sudo ls /var/lib/puzuz/checkpoints/<id>/     # dump.log, restore.log (CRIU logs, root only)
sudo ls /var/lib/puzuz/checkpoints/failed-logs/   # dump.log of failed dumps (newest 20)
```

Job output is written to `/var/lib/puzuz/logs/<uid>/<id>.log`. When a checkpoint or restore fails,
the job's last error (shown by `puzuz status` and in the `puzuz log` header to the owner and
administrators) contains a short summary of the CRIU log. After a failed dump it also names the
full log, e.g. `(CRIU log: /var/lib/puzuz/checkpoints/failed-logs/12-1760000000000.log)`. The daemon logs at `info` level. For
more detail, set `Environment=RUST_LOG=debug` in a drop-in for `puzuz.service`.

If the daemon is not running, every command fails with "cannot connect to the daemon socket". Check
`puzuz service status` and `systemctl status puzuz`. After `puzuz shutdown` the daemon stays
stopped until `sudo systemctl start puzuz` or the next boot.

### Common `puzuz doctor` failures

| Check | Message | Fix |
| --- | --- | --- |
| `config` | not owned by root or group/world-writable | `sudo chown root:root /etc/puzuz/config.toml /etc/puzuz; sudo chmod go-w /etc/puzuz/config.toml /etc/puzuz` |
| `config` | invalid configuration | fix the TOML. The daemon does not start with it. |
| `config` | not readable by others / not searchable by others (WARN) | `sudo chmod 755 /etc/puzuz; sudo chmod 644 /etc/puzuz/config.toml`: the daemon runs as `puzuz` |
| `criu` | not found, or too old | install CRIU 4.2 or newer, or set `criu_path` |
| `cuda-checkpoint` | not found, or lacks `--action lock/checkpoint/restore/unlock` | install the binary for a driver of r570 or newer into `/usr/local/bin` |
| `cuda-checkpoint` | version X but the driver is Y | use the cuda-checkpoint that matches the driver's major version |
| `cuda plugin` | `cuda_checkpoint_mode` is `"plugin"` but `/usr/lib/criu/cuda_plugin.so` is missing | install the plugin (see [CUDA plugin mode](#cuda-plugin-mode)) or use the manual mode |
| `gpus` | nvidia-smi lists N GPU(s), the configuration M | update `[[gpus]]` (see [GPU detection](#gpu-detection)) |
| `kernel` | lacks `/proc/sys/kernel/ns_last_pid` | use a kernel with `CONFIG_CHECKPOINT_RESTORE=y` |
| `update` | puzuz X is available (INFO) | `sudo puzuz update`; `update_check = false` turns the check off |
| `install method` | mixed: two install methods at once | remove all but one, as the `fix:` lines say (e.g. `sudo puzuz service uninstall` to keep the package) |
| `install method` | WARN: a unit no package owns, a leftover unit, or units installed in different ways | delete the unowned `/usr/lib` unit (`sudo rm ...`), or re-run the installation that should own both units, as the `fix:` line says |
| `puzuz.service`, `puzuz-helper.service` | not installed / not active | `sudo puzuz service install`, or `sudo systemctl start <unit>` and read `journalctl -u <unit>` |
| `helper binary` | the binary or a directory above it is not owned by root or is writable by others | `sudo chown root:root <path>; sudo chmod 755 <path>` (and its directories), or reinstall with `--bin-dir /usr/local/bin` |
| `daemon write paths` | cannot write to a configured directory: not below its ReadWritePaths= | add a `ReadWritePaths=` drop-in for `puzuz.service` (see [Installing](#installing)) |
| `helper` | the daemon is not connected to the helper | `journalctl -u puzuz-helper`. Often an ownership problem with the configuration or a data directory. |
| `daemon` | runs version X while this puzuz is Y | `sudo systemctl restart puzuz-helper.service puzuz.service` after upgrading, or update the CLI |
| `checkpoint dir` | on tmpfs, or little free space | move `checkpoints_dir` to a disk with room for RAM + VRAM of your jobs |
| `helper settings` | ExecStart= passes flags | move the settings into `config.toml` and drop the flags |

### Checkpoint failures

The helper checks the job before dumping it. If a check fails, the dump is **refused**: the job is
not touched and keeps running, `puzuz checkpoint` fails with `checkpoint refused: <reason>; the job
keeps running`, and the reason becomes the job's last error. When the refusal only comes when the
dump is due (for example during `puzuz shutdown`, which then leaves the job running), the owner is
warned on their next command. Refusals:

- `not enough free space for the checkpoint: need N bytes, M available`: free space or move
  `checkpoints_dir`, then checkpoint again.
- `the job's mounts [...] are only reachable through mounts hidden under other mounts`: the job's
  mount namespace depends on a mount that is hidden under another one, so it could never be
  restored. Run the job again (`puzuz restart`). New jobs get a cleaned-up namespace.
- Mounts covering `/` itself cannot be handled. puzuz refuses such dumps.
- `<dir> is on tmpfs`: move `checkpoints_dir` to a disk.
- `cuda_checkpoint_mode is "plugin" but CRIU's CUDA plugin ... is missing`: install the plugin
  (see [CUDA plugin mode](#cuda-plugin-mode)) or switch to the manual mode.

A paused job that is checkpointed is resumed for the dump. If the dump is refused, it is paused
again.

A dump that is attempted and **fails** stops the job (SIGTERM, then SIGKILL after `grace_secs`),
because CRIU and cuda-checkpoint have already frozen it. The job is marked `Checkpoint failed`
with a summary of the CRIU log and the path of the full log. Common reasons:

- `Can't handle non-regular mapping` or `handle_device_vma plugin failed` in the CRIU summary: a
  process that a CUDA process forked without `exec` still maps `/dev/nvidia*`. The fork guard
  normally prevents this. Check that `fork_guard` is not `false`, that the program is not
  statically linked, and that the job was started after the guard was installed. Otherwise use
  the `spawn` or `forkserver` start method (`multiprocessing_context="forkserver"` for a PyTorch
  DataLoader).
- With `checkpoint_tcp_close = false`, CRIU refuses to dump jobs that have established TCP
  connections.

During an OS shutdown or reboot, a job whose dump fails or is refused is stopped by systemd. After
the boot it is shown as `Checkpoint failed` with the reason (refusals start with `checkpoint
refused:`), and its owner is warned.

### Restore failures

A failed restore keeps the job `Checkpointed` and keeps its image, so you can fix the cause and run
`puzuz restore` again. The owner is warned on their next command.

- `paths the checkpoint's mounts depend on are missing on this host: [...]`: CRIU would fail with
  `Can't stat mountpoint` or `Failed to open_tree <path>`. The job's namespace contained a mount
  point, or a bind mount from a host path, that does not exist now (for example a filesystem that
  is not mounted after the reboot). Mount it, or create the path, and restore again.
- `filesystems the checkpoint depends on are not mounted as at the dump (mount them and restore
  again): /data needs ext4 on /data (found overlay on /)`: a filesystem that was mounted when the
  job started is not mounted now, or another filesystem type is mounted there. CRIU would bind
  the empty mount point directory instead, and the job would go on writing to the filesystem
  underneath. Mount it (`systemctl start data.mount`, `mount -a`, start the NFS server, ...) and
  restore again. See [Filesystems and mounts](#filesystems-and-mounts).
- `files or directories the job used are missing on this host: [...]`: files the job had open,
  or its working directory, are gone, typically because they were on a tmpfs (`/scratch`) that
  is empty after the reboot. The job cannot be restored. Remove it (`puzuz remove`).
- `checkpoint is not compatible with this host`: a different GPU model or driver version, or a
  missing GPU. Restore with the original driver and GPU. In plugin mode the GPU UUIDs must match
  as well. In manual mode a different card of the same model is remapped.
- `not enough free GPU memory`: other jobs use the GPU. Wait, or free it.
- Files the process had open, or its executables and libraries, were changed or deleted (see
  `restore.log`). Restore them at the same paths.
