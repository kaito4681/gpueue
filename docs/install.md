# Installing and operating gpueue

English | [日本語](ja/install.md) | [简体中文](zh-CN/install.md)

This guide is for the administrator of the machine. Regular users need nothing beyond the `gpueue`
command on their `PATH`.

- [Requirements](#requirements)
- [Installing with install.sh](#installing-with-installsh)
- [Installing the Debian/Ubuntu package](#installing-the-debianubuntu-package)
- [Installing the Fedora/RHEL/Rocky package](#installing-the-fedorarhelrocky-package)
- [Installing with Nix](#installing-with-nix)
- [Arch Linux (AUR)](#arch-linux-aur)
- [Building](#building)
- [Installing](#installing)
- [Verifying](#verifying)
- [Configuration](#configuration)
- [Administrators](#administrators)
- [Who jobs may run as](#who-jobs-may-run-as)
- [What a job can and cannot do](#what-a-job-can-and-cannot-do)
- [Priority and fair share](#priority-and-fair-share)
- [GPU detection](#gpu-detection)
- [GPU isolation](#gpu-isolation)
- [Checkpoint storage](#checkpoint-storage)
- [CUDA plugin mode](#cuda-plugin-mode)
- [Filesystems and mounts](#filesystems-and-mounts)
- [Upgrading](#upgrading)
- [Uninstalling](#uninstalling)
- [Troubleshooting](#troubleshooting)

## Requirements

| Component | Requirement | Tested with |
| --- | --- | --- |
| OS | Linux with systemd as init, x86_64 or aarch64 (releases are built for both) | Ubuntu (Docker image with systemd as PID 1), x86_64 |
| systemd | 240 or newer (`gpueue doctor` checks this) | 259 |
| Kernel | `CONFIG_CHECKPOINT_RESTORE=y` (`/proc/sys/kernel/ns_last_pid` must exist), PID and mount namespaces, 5.5 or newer recommended (`clone3` with `set_tid`, used by CRIU to restore PIDs), 5.2 or newer for `open_tree`/`move_mount` (used to clean up hidden mounts), 5.8 or newer to restore (`openat2`, `faccessat2` and `statx` mount ids, used to check what a restore opens) | 7.0 |
| CRIU | 4.2 or newer | 4.2 (distribution package), 4.2.1 |
| NVIDIA driver (GPU jobs) | a driver whose `cuda-checkpoint` supports `--action lock/checkpoint/restore/unlock` and `--timeout`: r570 or newer (cuda-checkpoint itself supports r550, but only with `--toggle`, which gpueue does not use) | 595 |
| cuda-checkpoint (GPU jobs) | the binary from NVIDIA, with the same major version as the driver | 595 |
| nvidia-smi (GPU jobs) | the helper uses it to find CUDA processes and GPU memory | from the driver |
| Rust (building only) | 1.88 or newer, plus a C compiler (`cc`) for the helper's fork guard library | 1.98.1 |

Without an NVIDIA driver gpueue still runs CPU-only jobs, and checkpoints them as well.

### CRIU

gpueue needs CRIU 4.2 or newer to checkpoint; jobs run without it. `gpueue doctor` reports a
missing or older CRIU. The distribution's package is new enough on Ubuntu 26.04
(`sudo apt install criu`, universe), Fedora (`sudo dnf install criu`, 4.2) and Arch
(`sudo pacman -S criu`). Ubuntu 22.04 (3.16), Ubuntu 24.04 (no package), Debian 12 (3.17),
Debian 13 (4.1) and RHEL/Rocky/Alma 9 (3.19) do not have it: build CRIU from source following
<https://criu.org/Installation> (the upstream release tarball, `make` and `sudo make install`
into `/usr/local/sbin`), or point `criu_path` in `/etc/gpueue/config.toml` at a root-owned build.
The `.deb` and the `.rpm` only recommend `criu`, so on those releases apt or dnf installs the old
package or none; a CRIU in `/usr/local/sbin` comes first in the helper's `PATH`. In the default **manual** mode
(`cuda_checkpoint_mode = "manual"`) the helper calls cuda-checkpoint itself and runs CRIU without
plugins. The distribution package works for this, even though it ships without CRIU's CUDA plugin.

The **plugin** mode (`cuda_checkpoint_mode = "plugin"`) lets CRIU's CUDA plugin (`cuda_plugin.so`,
CRIU 4.0 or newer) drive cuda-checkpoint instead. It is optional; see
[CUDA plugin mode](#cuda-plugin-mode).

### cuda-checkpoint

NVIDIA distributes cuda-checkpoint as a prebuilt binary in
<https://github.com/NVIDIA/cuda-checkpoint>, under `bin/x86_64_Linux/cuda-checkpoint` (or
`bin/aarch64_Linux/` on arm64). Its major version must be the driver's (`gpueue doctor` warns
otherwise). Put it where root's `PATH` finds it, owned by root and not writable by others
(`uname -m` prints `x86_64` or `aarch64`, which picks the directory):

```sh
git clone --depth 1 https://github.com/NVIDIA/cuda-checkpoint /tmp/cuda-checkpoint
sudo install -o root -g root -m 0755 "/tmp/cuda-checkpoint/bin/$(uname -m)_Linux/cuda-checkpoint" \
  /usr/local/bin/cuda-checkpoint
```

You can put it somewhere else and set `cuda_checkpoint_path` in `/etc/gpueue/config.toml`. Absolute
paths of `criu`, `cuda-checkpoint` and `nvidia-smi` must be owned by root, because the helper runs
them as root. Symlinks owned by root are followed and their targets checked the same way, and
`/nix/store` (owned by root, sticky, group `nixbld`) is accepted although it is group-writable, so
paths like `/run/current-system/sw/bin/criu` on NixOS work. `gpueue doctor` run as root applies the
same rule before it runs one of these programs: it reports a FAIL instead of running one that
fails it (bare names are looked up in systemd's default `PATH`), and it runs none of the programs
a configuration file names if that file failed its own ownership check.

The helper looks programs given by name up once, when it starts. **Restart it after installing
cuda-checkpoint (or CRIU)** (`sudo systemctl restart gpueue-helper`): until then it cannot use
them, refuses to checkpoint GPU jobs (`finding CUDA processes: the helper did not find
cuda-checkpoint when it started ...`), and `gpueue doctor` warns (`helper programs`).

### Dependencies that are not installed automatically

No install method installs the NVIDIA driver or cuda-checkpoint, and only the NixOS module brings
a CRIU that is new enough everywhere. What each method installs besides gpueue itself:

| Install method | Installs | Leaves to you |
| --- | --- | --- |
| [install.sh](#installing-with-installsh), [Nix build](#other-distributions) + `gpueue service install` | nothing else (install.sh prints how to install CRIU and cuda-checkpoint if they are missing) | CRIU, the NVIDIA driver, cuda-checkpoint |
| [.deb](#installing-the-debianubuntu-package) | its dependencies (libc6, libgcc-s1, systemd 240+) and the recommended `criu` the release has | CRIU 4.2+ where the release's is missing or older, the NVIDIA driver, cuda-checkpoint |
| [.rpm](#installing-the-fedorarhelrocky-package) | its dependencies (glibc 2.34+, libgcc, systemd) and the recommended `criu` (weak dependency) | CRIU 4.2+ on RHEL/Rocky/Alma 9 (3.19), the NVIDIA driver, cuda-checkpoint |
| [AUR](#arch-linux-aur) | its dependencies (systemd, glibc, libgcc); `criu` and `cuda-checkpoint` are only optional dependencies | CRIU, the NVIDIA driver, cuda-checkpoint |
| [NixOS module](#nixos) | CRIU 4.2.1; `nvidia-smi` and NVML from `hardware.nvidia.package` | the NVIDIA driver (`hardware.nvidia`), cuda-checkpoint ([`cudaCheckpointPackage`](#cuda-checkpoint-on-nixos)) |

Jobs run without CRIU and cuda-checkpoint; checkpoints need them. To add them (not on NixOS):

```sh
# CRIU 4.2 or newer, where the distribution has it (otherwise build it, see CRIU above)
sudo apt install criu       # Ubuntu 26.04
sudo dnf install criu       # Fedora
sudo pacman -S criu         # Arch Linux

# For GPU jobs, with an NVIDIA driver r570 or newer installed: cuda-checkpoint
git clone --depth 1 https://github.com/NVIDIA/cuda-checkpoint /tmp/cuda-checkpoint
sudo install -o root -g root -m 0755 "/tmp/cuda-checkpoint/bin/$(uname -m)_Linux/cuda-checkpoint" \
  /usr/local/bin/cuda-checkpoint

# The helper looks the programs up only when it starts (see cuda-checkpoint above)
sudo systemctl restart gpueue-helper
gpueue doctor
```

## Installing with install.sh

`scripts/install.sh` is the usual way to install gpueue on x86_64 and aarch64 Linux (on Debian and
Ubuntu, the [package](#installing-the-debianubuntu-package) is an alternative). It is attached to every
release. It downloads a release tarball, checks it, and runs that release's
`gpueue service install`, which copies the binaries into `/usr/local/bin` (see
[Installing](#installing) for what that does).

### GitHub token (optional)

Downloads need no GitHub token. A token only raises the GitHub API rate limit for anonymous
requests (60 an hour per IP address, which a shared NAT can use up). Any token works; a
fine-grained one needs no permissions. `install.sh` uses one when it finds one, in this order:

1. the `GH_TOKEN` or `GITHUB_TOKEN` environment variable,
2. `--token-file FILE` (the first line of FILE),
3. `gh auth token`, run as the user who ran `sudo` (if the GitHub CLI is installed and logged in),
4. `/etc/gpueue/github-token`, which `--save-token` writes (owned by root, mode 0600).

The token is passed to `curl` in a header file, so it does not show up in `ps` or in the output.
A rejected token fails with a message that says so; a 404 for `--version X` means that release
does not exist.

### Running the installer

```sh
curl -fsSL https://github.com/kaito4681/gpueue/releases/latest/download/install.sh | sudo bash
```

Options go after `bash -s --` when the script comes from a pipe (for example
`... | sudo bash -s -- --version 0.2.0`), or after the file name with a downloaded copy
(`sudo bash install.sh --version 0.2.0`; to check its signature before it runs as root, see
[Verifying a release by hand](#verifying-a-release-by-hand)):

| Option | Meaning |
| --- | --- |
| `--version X` | install version X instead of the latest release |
| `--bin-dir DIR` | install somewhere else than the default (the current installation's directory, else `/usr/local/bin`) |
| `--token-file FILE` | read the token from FILE |
| `--save-token` | save the token to `/etc/gpueue/github-token` (root, 0600) for `gpueue update` and later runs |
| `--uninstall` | remove gpueue with the installed `gpueue uninstall` (see [Uninstalling](#uninstalling)) |
| `--purge` | with `--uninstall`, also delete `/etc/gpueue` and `/var/lib/gpueue` |
| `-y`, `--yes` | do not ask before purging |

A token is saved only with `--save-token`. Without it, an expired or revoked token cannot break
a later run without you noticing which token was used.

`install.sh` checks, before downloading anything, that it runs as root, that systemd is PID 1, that
the machine is x86_64 or aarch64 with glibc 2.34 or newer, and that gpueue is not installed by a
package manager or Nix (see [Install method](#install-method)). For the last check it asks an
installed gpueue (the one the unit runs, else the one in `PATH`) with
`gpueue service install-method`; without one it refuses only the obvious cases itself (NixOS by
`/etc/os-release`, a unit a package owns, a unit linked into `/nix/store`), and the downloaded
binary decides the rest before it installs anything. It then:

1. looks up the release (`/repos/kaito4681/gpueue/releases/latest`, or `/releases/tags/v<X>`),
2. downloads `gpueue-v<X>-<target>.tar.gz` (`x86_64-unknown-linux-gnu` or
   `aarch64-unknown-linux-gnu`) and its `.sha256` and `.minisig` files into a
   temporary directory (over HTTPS only, also for redirects), checks that the `.sha256` file is
   the checksum of that tarball by name, and checks the checksum,
3. checks the `.minisig` signature against the release key built into the script
   (`packaging/minisign.pub`). It uses the `minisign` in `PATH`; without one it downloads the
   static minisign 0.12 release from GitHub into the temporary directory, checks it against the
   SHA-256 checksum built into the script (the same pin as `scripts/dist/get-minisign.sh`), and
   uses that. If that download fails, it stops and asks you to install `minisign` from your
   distribution (e.g. `sudo apt install minisign`) and re-run it; the signature is never skipped.
   The signature's trusted comment must be `gpueue <tag> <file>` for the requested release and
   tarball, so an older release's (validly signed) tarball served under a newer name is refused,
4. extracts the tarball (refusing links and paths outside it), runs the new
   `gpueue service install` (which keeps the directory of the current installation, else uses
   `/usr/local/bin`; `--bin-dir` is passed on only if you gave it), and prints its
   `gpueue doctor` report,
5. prints how to install CRIU and cuda-checkpoint if they are missing. It never installs them.

If only some `gpueue doctor` checks fail (for example CRIU is missing) but the services run,
`install.sh` succeeds with a warning.

Afterwards, update with `sudo gpueue update` (see [Upgrading](#upgrading)). Running `install.sh`
again also installs the latest (or the given) version over the installed one. Running jobs keep
running either way.

On Debian and Ubuntu you can use the [.deb](#installing-the-debianubuntu-package) instead, on
Fedora, RHEL, Rocky and Alma Linux the [.rpm](#installing-the-fedorarhelrocky-package), or
[Nix](#installing-with-nix) anywhere, and on Arch Linux the [AUR package](#arch-linux-aur) once it
is published (until then, use `install.sh` as above). You can also build from source as described
below.

### Verifying a release by hand

Each release has, for each target (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`),
`gpueue-v<X>-<target>.tar.gz` with `.sha256` and `.minisig` files, the packages
`gpueue_<X>_amd64.deb`, `gpueue_<X>_arm64.deb`, `gpueue-<X>-1.x86_64.rpm` and
`gpueue-<X>-1.aarch64.rpm` with the same two files each, the same packages again under version-less
names (`gpueue_amd64.deb`, `gpueue_arm64.deb`, `gpueue.x86_64.rpm`, `gpueue.aarch64.rpm`, for
`releases/latest/download/<name>`) with their own two files each, `install.sh` with
`install.sh.minisig`, and, when the `.rpm` files are GPG-signed, `RPM-GPG-KEY-gpueue` with
`RPM-GPG-KEY-gpueue.minisig`. To check a tarball (or a package) yourself:

```sh
sha256sum -c gpueue-v0.1.0-x86_64-unknown-linux-gnu.tar.gz.sha256
minisign -Vm gpueue-v0.1.0-x86_64-unknown-linux-gnu.tar.gz -P RWQhfodV2C486vD6glCl5MvvRupK9WboEzB2GUxcdmp2+NNqaL9W+UFP
```

minisign prints the signature's trusted comment, which the signature covers: it names the release
and the file (`Trusted comment: gpueue v0.1.0 gpueue-v0.1.0-x86_64-unknown-linux-gnu.tar.gz`). Check
that it is the release and file you meant to download.

`curl ... | sudo bash` runs whatever the download contains. To check `install.sh` first, download
it with its signature, verify it with the release key (`packaging/minisign.pub`), and run the
checked copy:

```sh
curl -fsSLO https://github.com/kaito4681/gpueue/releases/latest/download/install.sh
curl -fsSLO https://github.com/kaito4681/gpueue/releases/latest/download/install.sh.minisig
minisign -Vm install.sh -P RWQhfodV2C486vD6glCl5MvvRupK9WboEzB2GUxcdmp2+NNqaL9W+UFP
sudo bash install.sh
```

### The release signing key (maintainers)

Releases are signed in `.github/workflows/release.yml` with the minisign secret key stored in the
repository secrets `MINISIGN_SECRET_KEY` and `MINISIGN_PASSWORD`. The public key is
`packaging/minisign.pub`, which is built into `gpueue`, and `MINISIGN_PUBKEY` in
`scripts/install.sh`. The key in use is the real release key (id `EA3C2ED855877E21`). To rotate
it (a leaked key, or a new maintainer), create a new key pair as follows, replace the public key
in both places in the same commit, and update the two secrets. Installed copies of
`gpueue` carry the old public key and cannot verify releases signed with the new one, so they need
a manual reinstall with `install.sh`:

```sh
scripts/dist/gen-signing-key.sh        # asks for a password; writes the secret key under ~
gh secret set MINISIGN_SECRET_KEY -R kaito4681/gpueue < ~/gpueue-release-key-*/minisign.key
gh secret set MINISIGN_PASSWORD -R kaito4681/gpueue
git commit packaging/minisign.pub scripts/install.sh
```

Back up the secret key and its password. A new key needs a new release of `gpueue` before
`gpueue update` accepts releases signed with it. Releases are created as drafts: publish the
draft on GitHub, or `install.sh` (which asks for the latest published release) does not see it.

## Installing the Debian/Ubuntu package

Each release has a `.deb` for amd64 and arm64, for Debian 12 and Ubuntu 22.04, 24.04 and 26.04.
The stable releases are also in the gpueue APT repository, a flat repository in the GitHub release
`apt` (`https://github.com/kaito4681/gpueue/releases/download/apt/`, signed with the key in
`/etc/apt/keyrings/gpueue.gpg`).

**Prerequisites.**

- `curl` and `ca-certificates`, for the commands below (`sudo apt install curl ca-certificates`).
  `/etc/apt/keyrings` comes with apt on every supported release.
- systemd 240 or newer as PID 1 (the package depends on it; without systemd as PID 1 the services
  are only enabled, see below).
- Optional: CRIU 4.2 or newer for checkpoints (the distribution's is often missing or too old, see
  [CRIU](#criu)), and for GPU jobs the NVIDIA driver and cuda-checkpoint (see
  [cuda-checkpoint](#cuda-checkpoint)). Jobs run without them.

**From the APT repository (recommended).** Add the repository and install from it; apt picks the
newest stable version and the architecture. Use these exact file names and contents (they are what
the package installs, so the package does not ask about them later):

```sh
sudo curl -fsSLo /etc/apt/keyrings/gpueue.gpg \
  https://github.com/kaito4681/gpueue/releases/download/apt/gpueue.gpg
sudo curl -fsSLo /etc/apt/sources.list.d/gpueue.sources \
  https://github.com/kaito4681/gpueue/releases/download/apt/gpueue.sources
sudo apt update && sudo apt install gpueue
```

**The `.deb` directly.** Or download the package for this machine's architecture from the latest
release and install it with apt, which also pulls in what it depends on; the package registers the
same repository:

```sh
arch=$(dpkg --print-architecture)
curl -fLO "https://github.com/kaito4681/gpueue/releases/latest/download/gpueue_${arch}.deb"
sudo apt install "./gpueue_${arch}.deb"
```

`releases/latest` is the newest stable release (pre-releases are skipped), and
`gpueue_<arch>.deb` is a version-less copy of its `gpueue_<version>_<arch>.deb`. The versioned
files are attached too, for a specific version
(`https://github.com/kaito4681/gpueue/releases/download/v0.1.0/gpueue_0.1.0_amd64.deb`). To verify
the download before installing it (see [Verifying a release by hand](#verifying-a-release-by-hand)):

```sh
curl -fLO "https://github.com/kaito4681/gpueue/releases/latest/download/gpueue_${arch}.deb.sha256"
curl -fLO "https://github.com/kaito4681/gpueue/releases/latest/download/gpueue_${arch}.deb.minisig"
sha256sum -c "gpueue_${arch}.deb.sha256"
minisign -Vm "gpueue_${arch}.deb" -P RWQhfodV2C486vD6glCl5MvvRupK9WboEzB2GUxcdmp2+NNqaL9W+UFP
```

The trusted comment names the release and the version-less file
(`Trusted comment: gpueue v0.1.0 gpueue_amd64.deb`).

The package contains:

| Path | |
| --- | --- |
| `/usr/bin/gpueue`, `/usr/bin/gpueue-helper` | the binaries |
| `/usr/lib/systemd/system/gpueue.service`, `gpueue-helper.service` | the units (`ExecStart=/usr/bin/...`) |
| `/usr/lib/sysusers.d/gpueue.conf`, `/usr/lib/tmpfiles.d/gpueue.conf` | the `gpueue` user, the `gpueue-admin` group, the directories |
| `/etc/gpueue/config.toml` | the default configuration (a conffile: your edits are kept on upgrades) |
| `/etc/apt/sources.list.d/gpueue.sources`, `/etc/apt/keyrings/gpueue.gpg` | the gpueue APT repository and its signing key (conffiles, see below) |
| `/usr/share/bash-completion`, `zsh/vendor-completions`, `fish/vendor_completions.d` | shell completions |
| `/usr/share/doc/gpueue` | README, this guide, the architecture notes, the license |

On the first installation it creates the user, the group and the directories, then enables and
starts `gpueue-helper.service` and `gpueue.service`; check the result with `gpueue doctor`. Without
systemd as PID 1 (a container image being built, a chroot) the services are only enabled, so they
start on the next boot. Unlike `install.sh`, the package does not add `[[gpus]]` entries to the
configuration; NVML finds the GPUs at start-up (see [GPU detection](#gpu-detection)).

It depends on systemd 240 or newer and only **recommends** `criu`: on most supported releases the
distribution's CRIU is missing or older than 4.2 (see [CRIU](#criu)). Jobs run without it, but
checkpoints need it, and `gpueue doctor` reports a missing or old CRIU. cuda-checkpoint for GPU
jobs is installed by hand as described in [cuda-checkpoint](#cuda-checkpoint).

The package refuses to install on a machine where `gpueue service install` (or `install.sh`)
installed gpueue: remove that first with `sudo gpueue uninstall` (with an older gpueue that lacks
the command, `install.sh --uninstall`). Running jobs, `/etc/gpueue` and `/var/lib/gpueue` are kept. `gpueue doctor`
and `gpueue service install-method` report the package's installation as a distribution
`package`, and `gpueue service install`, `install.sh` and `gpueue update` refuse to touch it.

**Updates.** Either way, later versions come from the repository with the rest of the system:

```sh
sudo apt update && sudo apt upgrade
```

The repository has the stable releases (not pre-releases) for amd64 and arm64, the newest five
versions of each. An upgrade restarts the running services. Running jobs keep running, as with
`gpueue update` (see [Upgrading](#upgrading)). Your edits to `/etc/gpueue/config.toml` and
`/etc/apt/sources.list.d/gpueue.sources` are kept; `/etc/apt/keyrings/gpueue.gpg` is replaced by a
new version's keyring when you have not changed it (that is how a new signing key arrives). While a new release is being uploaded,
`apt update` may briefly fail with a hash mismatch or a missing file; running it again a minute
later fixes it.

**The repository key.** The packages, the repository and the `.rpm` files are signed with one
GPG key (`packaging/apt/gpueue-archive-keyring.gpg` in the source tree), fingerprint
`9B4F 59D2 F414 B76B 626B 3BE0 4FB3 D15B 2389 5621`. To check a downloaded key before trusting it:

```console
$ gpg --show-keys /etc/apt/keyrings/gpueue.gpg
pub   rsa4096 2026-10-15 [SC]
      9B4F59D2F414B76B626B3BE04FB3D15B23895621
uid                      gpueue APT repository <https://github.com/kaito4681/gpueue>
```

**Removing.**

```sh
sudo apt remove gpueue    # stop and disable the services, remove the binaries and units
sudo apt purge gpueue     # also delete /etc/gpueue and the APT repository entry and key
```

`remove` keeps the repository entry and its key (both conffiles), so `apt update` keeps working
and `sudo apt install gpueue` brings gpueue back. Both keep `/var/lib/gpueue` (state, job logs, checkpoint images), the `gpueue` user and the
`gpueue-admin` group, and never touch running jobs: they keep running in their own
`gpueue-job-*` units. `purge` prints how to delete `/var/lib/gpueue`
(`sudo systemctl stop 'gpueue-job-*'` first if jobs still run, then `sudo rm -rf /var/lib/gpueue`).

### The APT repository (maintainers)

The repository is the set of assets of the release `apt` (a pre-release, so it never becomes the
"latest" release that `install.sh` and `gpueue update` follow): the `.deb` files, `Packages`,
`Packages.gz`, `Release`, `InRelease` and `Release.gpg`, the public key `gpueue.gpg` and
`gpueue.sources`. `.github/workflows/release-apt.yml` updates it with
`scripts/dist/apt-repo.sh github <tag>` when a release is **published as a stable release** (the
`released` event: publishing a stable release, or turning a pre-release into one): it adds the release's
`.deb` files (after checking their checksums and their minisign signatures, with the public key
of the tag's commit), checks the packages already in the repository against its signed index
(`InRelease` must verify with `packaging/apt/gpueue-archive-keyring.gpg`, and each `.deb` must
have the SHA256 the signed `Packages` lists; on a mismatch the job fails and changes nothing, and a
`.deb` the index does not list, left by an interrupted run, is dropped), deletes all but the newest
five versions per architecture (in Debian version order), regenerates and signs the index (and
copies `gpueue.gpg` and `gpueue.sources` out of the newest package it keeps), and uploads the
packages first and the index last, `InRelease` at the very end; old `.deb` files are deleted after
that. Each upload is tried three times; when the index still cannot be uploaded, the previous
index files are put back and the job fails, saying to run it again. It creates the `apt` release when it is missing. `apt-repo.sh` decides what is published:
drafts, pre-releases and tags other than `v*` are skipped with a notice. The index has no
`by-hash` files (they need a subdirectory, and release asset names cannot contain `/`), hence the
short window described above.

`release.yml` only creates drafts, so the repository is updated when a person publishes the draft
on GitHub (or a tool does so with a personal access token). A release published with a workflow's
`GITHUB_TOKEN` does not start other workflows; then run it by hand, which is also how to retry a
failed upload:

```sh
gh workflow run release-apt.yml -R kaito4681/gpueue -f tag=v0.1.0
```

To try it without a real release, use a fork: create a key there with
`scripts/dist/gen-apt-key.sh` (and the minisign key, for `release.yml`), commit the public keys,
set the secrets, push a tag, publish the draft, and run `apt update` against the fork's `apt`
release. Or run the local test, `scripts/apt-repo-test.sh` (`GPUEUE_APT_BASES="debian:12
ubuntu:22.04 ubuntu:24.04 ubuntu:26.04"`), which builds a repository with
`apt-repo.sh update DIR DEB...` and a throwaway key and upgrades through it in containers.

**The archive key.** `packaging/apt/gpueue-archive-keyring.gpg` is the real key, fingerprint
`9B4F 59D2 F414 B76B 626B 3BE0 4FB3 D15B 2389 5621` (see "The repository key" in
[Installing the Debian/Ubuntu package](#installing-the-debianubuntu-package)); its secret key and
passphrase are the repository secrets `APT_GPG_PRIVATE_KEY` and `APT_GPG_PASSPHRASE`.
`release.yml` and `apt-repo.sh` refuse a keyring whose user ID says "TEST KEY" (written by
`scripts/dist/gen-apt-key.sh --test`). A key is created with:

```sh
scripts/dist/gen-apt-key.sh     # asks for a passphrase; writes the secret key under ~
gh secret set APT_GPG_PRIVATE_KEY -R kaito4681/gpueue < ~/gpueue-apt-key-*/apt-signing-key.asc
gh secret set APT_GPG_PASSPHRASE -R kaito4681/gpueue
# then commit packaging/apt/gpueue-archive-keyring.gpg
```

Without the two secrets, `release-apt.yml` skips the repository (with a notice). The index is
signed only with a key that the newest package in the repository ships (`apt-repo.sh` reads
`/etc/apt/keyrings/gpueue.gpg` out of the `.deb`), so a mismatch between the secret and the committed public key fails
the run instead of breaking `apt update` for everyone.

Back up the secret key and its passphrase. Every installed package trusts this key, so a new key
can only be rolled out by a package version that ships it, through the repository signed with the
old key:

1. Create the new key (`scripts/dist/gen-apt-key.sh` in a scratch checkout) and append its public
   key to `packaging/apt/gpueue-archive-keyring.gpg`, so the keyring holds both keys
   (`cat old.gpg new.gpg >packaging/apt/gpueue-archive-keyring.gpg`). Keep the secrets on the old
   key and release a version with the combined keyring.
2. Wait until users have upgraded to it (at least one more release cycle).
3. Switch the secrets `APT_GPG_PRIVATE_KEY` and `APT_GPG_PASSPHRASE` to the new key, and in the
   same release remove the old key from the keyring. The keyring is a conffile: dpkg replaces it
   on upgrade unless a user changed it (then dpkg asks). Users who skipped step 1's version get
   `NO_PUBKEY` from `apt update` and install the next `.deb` by hand once, as on a first
   installation.

If the secret key leaks, do the same at once and tell users to install the new `.deb` by hand.

The packages are built with [nfpm](https://nfpm.goreleaser.com) from `packaging/nfpm.yaml`
(`scripts/dist/build-package.sh BIN_DIR OUT_DIR [deb|rpm]` builds a `.deb` or an `.rpm` from built
binaries; `scripts/deb-test.sh` and `scripts/rpm-test.sh` check them in containers).

## Installing the Fedora/RHEL/Rocky package

Each release has an `.rpm` for x86_64 and aarch64 (`uname -m` tells which), for Fedora and
RHEL-compatible 9 or newer (RHEL, Rocky Linux, AlmaLinux; glibc 2.34 or newer).

**Prerequisites.**

- systemd as PID 1 (the package requires it; without systemd as PID 1 the services are only
  enabled). dnf downloads the package itself; `curl` (with `ca-certificates`) is needed only to
  download the files for verifying it.
- Optional: CRIU 4.2 or newer for checkpoints (see [CRIU](#criu)), and for GPU jobs the NVIDIA
  driver and cuda-checkpoint (see [cuda-checkpoint](#cuda-checkpoint)). Jobs run without them.

Install the `.rpm` of the latest release with dnf, which takes the URL directly and pulls in what
it depends on:

```sh
sudo dnf install "https://github.com/kaito4681/gpueue/releases/latest/download/gpueue.$(uname -m).rpm"
```

As for the `.deb`, `gpueue.<arch>.rpm` is a version-less copy of the newest stable release's
`gpueue-<version>-1.<arch>.rpm`, which is attached too, for a specific version
(`sudo dnf install https://github.com/kaito4681/gpueue/releases/download/v0.1.0/gpueue-0.1.0-1.x86_64.rpm`).
To verify it first, download `gpueue.$(uname -m).rpm` with its `.sha256` and `.minisig` files and check them as for the
[`.deb`](#installing-the-debianubuntu-package) (the trusted comment names the version-less file),
then install the checked file with `sudo dnf install "./gpueue.$(uname -m).rpm"`.

(A pre-release such as `v0.2.0-rc.1` is `gpueue-0.2.0-rc.1-1.<arch>.rpm`; inside the package its
version is `0.2.0~rc.1`, which sorts before `0.2.0`.)

It installs the same files as the `.deb` (binaries, units in `/usr/lib/systemd/system`,
sysusers.d and tmpfiles.d entries, `/etc/gpueue/config.toml`, shell completions, documentation,
and the license in `/usr/share/licenses/gpueue`), but no repository configuration. The
configuration is `%config(noreplace)`: your edits are kept on upgrades. The first installation
sets up and starts the services exactly like the `.deb` (only enables them without systemd as
PID 1), and refuses to install next to an installation made by `gpueue service install` or
`install.sh`. It requires systemd and glibc 2.34 and only **recommends** `criu` (a weak
dependency, which dnf installs by default when the distribution has it): Fedora's CRIU (4.2) is
new enough, the 3.19 of RHEL/Rocky/Alma 9 is not, so checkpointing there needs a newer CRIU (see
[CRIU](#criu)). Running jobs works without it.

**Verifying.** Like every release asset, the `.rpm` has `.sha256` and `.minisig` files (see
[Verifying a release by hand](#verifying-a-release-by-hand)). Releases built with the GPG key also
sign the `.rpm` itself, and attach that key as `RPM-GPG-KEY-gpueue` (the APT repository's key),
minisign-signed like the other assets. Check the key file with the release key before importing it:

```sh
curl -fsSLO https://github.com/kaito4681/gpueue/releases/download/v0.1.0/RPM-GPG-KEY-gpueue
curl -fsSLO https://github.com/kaito4681/gpueue/releases/download/v0.1.0/RPM-GPG-KEY-gpueue.minisig
minisign -Vm RPM-GPG-KEY-gpueue -P RWQhfodV2C486vD6glCl5MvvRupK9WboEzB2GUxcdmp2+NNqaL9W+UFP
gpg --show-keys RPM-GPG-KEY-gpueue                   # 9B4F59D2F414B76B626B3BE04FB3D15B23895621
sudo rpm --import RPM-GPG-KEY-gpueue
rpm -K gpueue-0.1.0-1.x86_64.rpm                     # "digests signatures OK"
# or let dnf refuse an unsigned or badly signed file:
sudo dnf install --setopt=localpkg_gpgcheck=1 ./gpueue-0.1.0-1.x86_64.rpm
```

**Updates.** There is no dnf repository yet (it is planned, see
[issue #23](https://github.com/kaito4681/gpueue/issues/23)), so `dnf upgrade` alone does not see new
versions. Install the newer `.rpm` of the release instead; dnf takes the URL directly:

```sh
sudo dnf upgrade "https://github.com/kaito4681/gpueue/releases/latest/download/gpueue.$(uname -m).rpm"
```

When a newer release exists, the "update" line of `gpueue doctor` (and `gpueue update --check`)
prints such a command with the release's versioned URL for the machine's architecture, and how to
check the signature first. An upgrade restarts the running services. Running jobs keep running.

**Removing.**

```sh
sudo dnf remove gpueue
```

It stops and disables the services and removes the binaries and units. It keeps `/var/lib/gpueue`
(state, job logs, checkpoint images), the `gpueue` user and the `gpueue-admin` group, and never
touches running jobs. rpm has no purge: an edited `config.toml` is kept as
`/etc/gpueue/config.toml.rpmsave`; delete `/etc/gpueue` and `/var/lib/gpueue` by hand if you no
longer need them (`sudo systemctl stop 'gpueue-job-*'` first if jobs still run).

## Installing with Nix

The repository is a Nix flake (`flake.nix`). It builds gpueue from source for x86_64 and aarch64
Linux; nothing is downloaded from the GitHub releases. Its outputs:

| Output | Contents |
| --- | --- |
| `packages.<system>.default` (also `gpueue`) | `bin/gpueue`, `bin/gpueue-helper`, bash/zsh/fish completions, the docs |
| `packages.<system>.criu` | CRIU 4.2.1 (nixpkgs has an older version), for the NixOS module |
| `devShells.<system>.default` | a development shell with the Rust toolchain (`nix develop`) |
| `nixosModules.default` | the NixOS module `services.gpueue` (see [NixOS](#nixos)) |
| `homeManagerModules.default` | the home-manager module `programs.gpueue` (see [home-manager](#home-manager)) |
| `packages.<system>.helper-tests` | the gpueue-helper test executables (built, not run), for the `helper-tests` check |
| `checks.<system>.*` | `home-manager` (module evaluation) and, on x86_64, two NixOS VM tests that need KVM: `nixos` (the module) and `helper-tests` (every gpueue-helper test; the package build itself runs no tests, since the helper's job-spawning tests cannot pass in the Nix build sandbox) |

Use `github:kaito4681/gpueue/stable` to follow releases: the `stable` branch moves to a release's
tag when the release is published as a stable release, if it is the highest version (never to a
draft, a pre-release, or a patch release of an older line; `.github/workflows/release-nix.yml`).
Plain `github:kaito4681/gpueue` is the development head (`main`).

### NixOS

The flake's NixOS module (`nixosModules.default`, file `packaging/nix/nixos-module.nix`) sets up the
daemon, the root helper, the CLI, the `gpueue` system user, the `gpueue-admin` group,
`/etc/gpueue/config.toml` and the directories below `/var/lib/gpueue`. Do not run
`gpueue service install` or `gpueue update` on NixOS; both refuse to (NixOS is recognized by
`ID=nixos` in `/etc/os-release`, see [Install method](#install-method)).

Add the flake to the system's flake (the `stable` branch follows the latest release) and enable
the service:

```nix
# flake.nix
{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  inputs.gpueue.url = "github:kaito4681/gpueue/stable";
  inputs.gpueue.inputs.nixpkgs.follows = "nixpkgs";

  outputs = { nixpkgs, gpueue, ... }: {
    nixosConfigurations.gpu-server = nixpkgs.lib.nixosSystem {
      modules = [
        gpueue.nixosModules.default
        ./configuration.nix
      ];
    };
  };
}
```

```nix
# configuration.nix
{
  services.gpueue = {
    enable = true;
    admins = [ "alice" ];
    settings = {
      grace_secs = 60;
      checkpoint_size_limit_bytes = 500000000000;
    };
  };
}
```

Then `sudo nixos-rebuild switch`. `gpueue doctor` checks the result.

| Option | Default | Meaning |
| --- | --- | --- |
| `services.gpueue.enable` | `false` | Install and start gpueue. |
| `services.gpueue.package` | the flake's `gpueue` | The package that runs the daemon (`bin/gpueue`) and the helper (`bin/gpueue-helper`); also added to `environment.systemPackages`. |
| `services.gpueue.admins` | `[ ]` | Users added to the `gpueue-admin` group (see [Administrators](#administrators)). |
| `services.gpueue.settings` | `{ }` | The contents of `/etc/gpueue/config.toml` as an attribute set (see [Configuration](#configuration) for the keys). |
| `services.gpueue.criuPackage` | the flake's `criu` (4.2.1) | CRIU for the helper; sets `criu_path`. nixpkgs' own `criu` is older than 4.2. |
| `services.gpueue.cudaCheckpointPackage` | `null` | A package with `bin/cuda-checkpoint`; sets `cuda_checkpoint_path`. With `null`, GPU jobs cannot be checkpointed. |
| `services.gpueue.nvidiaSmiPath` | `nvidia-smi` of `hardware.nvidia.package` when `hardware.nvidia.enabled`, else `null` | Sets `nvidia_smi_path` (the helper finds a job's CUDA processes with it). |
| `services.gpueue.nvmlLibrary` | `libnvidia-ml.so.1` of `hardware.nvidia.package` when `hardware.nvidia.enabled`, else `null` | Sets `nvml_library`, which the daemon loads to detect the GPUs. |

The module sets `criu_path`, `cuda_checkpoint_path`, `nvidia_smi_path`, `nvml_library` and
`shell` with `lib.mkDefault`, so a value in `settings` wins. Notes for NixOS:

- **Units.** `gpueue.service` and `gpueue-helper.service` are the files in `packaging/systemd` of
  the module's flake revision, with `ExecStart=` pointing into the store, and the tmpfiles.d
  entries are `packaging/tmpfiles.d/gpueue.conf`. If you set `package` to another revision, the
  units still come from the module's revision; keep the two together.
- **Configuration file.** The helper runs as root and only reads a regular file that nobody but
  root can change, never a symlink. The module therefore has NixOS copy the file into
  `/etc/gpueue/config.toml` (mode 0644, owned by root) instead of linking it into the store.
- **Restarts on `nixos-rebuild switch`.** A change to `settings` (or to the package) restarts
  both the daemon and the helper. Running jobs are not stopped: they run in their own units. The
  restarting helper first refuses new launches and lets the operations in flight finish, so a
  switch during a checkpoint or restore waits for it (up to the helper's `TimeoutStopSec=`,
  30 minutes). Jobs the daemon tries to start or restore in that window are put back in the
  queue, not failed, and start once the new helper is up.
- **Updates and Nix garbage collection.** A job's unit runs the helper binary that started it,
  and its checkpoint image contains that binary too (restoring needs it). After an update, the
  old store path is no longer part of the system, so the helper keeps it alive with a GC root,
  `/nix/var/nix/gcroots/gpueue/job-<id>`, for as long as the job runs or its checkpoint image is
  kept. The root is removed when the job ends or its checkpoint is deleted. `nix-collect-garbage`
  then frees the old version. A job started, or a checkpoint taken, by a helper older than this
  feature has no root: restoring it after the old version was collected fails, so restore or
  finish such jobs before collecting garbage.
- **The job environment.** A job runs with the environment of the `gpueue add` that submitted it,
  so its `PATH` is the user's (`/run/wrappers/bin`, `/run/current-system/sw/bin`,
  `~/.nix-profile/bin`, ...). NixOS keeps nothing in `/usr/bin` and only `sh` in `/bin`; the
  module sets `shell = [ "/bin/sh" "-c" ]` so that starting a job does not depend on the `PATH`
  at all.
- **CRIU.** The module uses the flake's CRIU 4.2.1 by its store path. The NixOS kernels have
  `CONFIG_CHECKPOINT_RESTORE=y`.
- **NVIDIA.** With the NVIDIA driver enabled the usual NixOS way
  (`services.xserver.videoDrivers = [ "nvidia" ]`, also on headless machines, or
  `hardware.nvidia.datacenter.enable`), `nvidia-smi` and NVML are taken from
  `hardware.nvidia.package`: the dynamic loader does not search the driver's directory on NixOS.
  Otherwise set `nvidiaSmiPath` and `nvmlLibrary` (or `settings.gpus`, see
  [GPU detection](#gpu-detection)). After a driver update, the new
  store paths restart the daemon.
- **Root-owned paths.** Programs in `/nix/store` pass the helper's and `gpueue doctor`'s ownership
  checks (see [cuda-checkpoint](#cuda-checkpoint)). In a VM without a GPU, `gpueue doctor` reports
  no failure: the install method is "Nix", and only the NVIDIA driver and cuda-checkpoint checks
  warn (the latter's hint names `services.gpueue.cudaCheckpointPackage`).

#### cuda-checkpoint on NixOS

cuda-checkpoint is not in nixpkgs and gpueue does not package it (it is NVIDIA's binary). A
derivation for it can look like this; take the release whose major version matches the driver,
and fill in the hash that the first build reports:

```nix
# cuda-checkpoint.nix
{ lib, stdenv, fetchFromGitHub, autoPatchelfHook, autoAddDriverRunpath }:
stdenv.mkDerivation {
  pname = "cuda-checkpoint";
  version = "595";  # match the driver's major version
  src = fetchFromGitHub {
    owner = "NVIDIA";
    repo = "cuda-checkpoint";
    rev = "<commit>";
    hash = lib.fakeHash;
  };
  # Sets the ELF interpreter, and adds /run/opengl-driver/lib so that libcuda.so.1 is found.
  nativeBuildInputs = [ autoPatchelfHook autoAddDriverRunpath ];
  dontBuild = true;
  installPhase = ''
    install -Dm755 bin/x86_64_Linux/cuda-checkpoint $out/bin/cuda-checkpoint
  '';
  meta.license = lib.licenses.unfree;
}
```

```nix
services.gpueue.cudaCheckpointPackage = pkgs.callPackage ./cuda-checkpoint.nix { };
```

(`lib.licenses.unfree` needs `nixpkgs.config.allowUnfree = true` or an
`allowUnfreePredicate`.)

#### Updating and removing on NixOS

- Update: `nix flake update gpueue && sudo nixos-rebuild switch`. The changed units restart the
  daemon and the helper (see "Restarts on `nixos-rebuild switch`" above). Running jobs keep
  running on the old helper binary, and checkpoints taken before the update can be restored,
  also after `nix-collect-garbage` (see "Updates and Nix garbage collection").
- Roll back: `sudo nixos-rebuild switch --rollback` (or boot an older generation) brings back the
  older binaries, but not the older database: after an update that migrated the database, the
  older daemon refuses to start with the newer database.
- Remove: set `services.gpueue.enable = false` and run `sudo nixos-rebuild switch`. The units,
  `/etc/gpueue/config.toml` and the CLI go away; `/var/lib/gpueue` (state, logs, checkpoint
  images) and the `gpueue` user's files stay. Kill or finish running jobs first: the helper
  stops, but job units that are still running are left alone.

### home-manager

home-manager manages one user's environment, so it cannot install the daemon or the helper
(they need root and the system's systemd). Its module (`homeManagerModules.default`) sets up the
CLI's shell completions for a machine where an administrator installed gpueue
(`homeManagerModules.default`, file `packaging/nix/home-manager-module.nix`):

```nix
{
  imports = [ inputs.gpueue.homeManagerModules.default ];
  programs.gpueue = {
    enable = true;
    # package = null;  # the default: use the system's gpueue
  };
}
```

| Option | Default | Meaning |
| --- | --- | --- |
| `programs.gpueue.enable` | `false` | Enable the module. |
| `programs.gpueue.package` | `null` | `null`: install nothing, and load the completions of the system's `gpueue` (the one in `PATH`) when the shell starts: the completion files installed next to it (`<prefix>/share/...`, as on NixOS and in the packages), or, when there are none, the output of `gpueue completions <shell>`. A package (e.g. `inputs.gpueue.packages.${pkgs.stdenv.hostPlatform.system}.default`) is installed into the profile with its completion files. |
| `programs.gpueue.enableBashIntegration` | `true` | Load the completions in Bash (`programs.bash.enable`), with `package = null`. |
| `programs.gpueue.enableZshIntegration` | `true` | The same for Zsh (`programs.zsh.enable` with `programs.zsh.enableCompletion`, which runs `compinit`; home-manager 25.05 or newer). |
| `programs.gpueue.enableFishIntegration` | `true` | The same for Fish (`programs.fish.enable`). |

The CLI and the daemon must have the same version. Prefer `package = null`: the system's
`gpueue` always matches the daemon. A `gpueue` from home-manager (or `nix profile`) comes from the
revision your home-manager flake locks, so it can differ; commands then fail with a version
mismatch that names the store path and, on NixOS or for a home-manager profile, suggests
`programs.gpueue.package = null`. Update with
`nix flake update && home-manager switch`; the daemon is the administrator's to update.

### Other distributions

Build the flake, then install the service from the result like a tarball:

```sh
nix build github:kaito4681/gpueue/stable
sudo ./result/bin/gpueue service install
```

`service install` copies the binaries to `/usr/local/bin` and, because they still load their
libraries from the Nix store, registers the build as a garbage-collector root
(`/nix/var/nix/gcroots/gpueue`; see [Installing](#installing)). If `nix-store` is not in root's
`PATH` (sudo's `secure_path` often leaves out the Nix profile), install warns and prints the
command to register it yourself. With the root registered you can delete `./result`. The units run the copies in `/usr/local/bin`, so the install method is
`service-install`, not `nix` (which means units that resolve into `/nix/store`, such as the NixOS
module's). Install CRIU as described in [CRIU](#criu).

To update, run the same two commands again (Nix fetches the current `stable`; add `--refresh` if
it was fetched less than an hour ago); running jobs keep running. `sudo gpueue update` works too,
but replaces the Nix build with the release tarball's binaries. Remove gpueue with
`sudo gpueue uninstall` (see [Uninstalling](#uninstalling)), which also removes the GC root.

`nix profile install github:kaito4681/gpueue/stable` gives a user the `gpueue` command (with
completions) only (see also [home-manager](#home-manager)). The helper runs as root and cannot run from a user profile, so the service
must be installed as above.

## Arch Linux (AUR)

**The AUR package is not published yet.** Until it is, install gpueue on Arch Linux with
`install.sh` (it works on Arch: glibc 2.34 or newer, systemd, x86_64 or aarch64, and it verifies
the minisign signature). CRIU 4.2 or newer comes from the official repositories:

```sh
sudo pacman -S criu
curl -fsSL https://github.com/kaito4681/gpueue/releases/latest/download/install.sh | sudo bash
```

Update with `sudo gpueue update` and remove with `sudo gpueue uninstall` (see
[Upgrading](#upgrading) and [Uninstalling](#uninstalling)). The rest of this section describes
the AUR package for once it is on the AUR (the maintainer setup is below).

The AUR package [`gpueue-bin`](https://aur.archlinux.org/packages/gpueue-bin) (x86_64 and aarch64)
repackages the release tarballs. Once it is published, install it with any AUR helper, for
example yay:

```sh
yay -S gpueue-bin
```

or by hand:

```sh
git clone https://aur.archlinux.org/gpueue-bin.git
cd gpueue-bin
makepkg -si
```

It installs `/usr/bin/gpueue` and `/usr/bin/gpueue-helper`, the units in
`/usr/lib/systemd/system`, the `sysusers.d` and `tmpfiles.d` snippets in `/usr/lib`, the default
`/etc/gpueue/config.toml` (kept on upgrades and removal when you changed it), and completions for
bash, zsh and fish. pacman's systemd hooks create the `gpueue` user, the `gpueue-admin` group and
the directories. As usual on Arch, the services are not enabled or started for you:

```sh
sudo systemctl enable --now gpueue-helper.service gpueue.service
gpueue doctor
```

Checkpoints need CRIU 4.2 or newer, which is in the official repositories
(`sudo pacman -S criu`). GPU jobs also need NVIDIA's cuda-checkpoint (see
[cuda-checkpoint](#cuda-checkpoint)). Both are optional dependencies of the package.

- **Update:** `yay -Syu` (or rebuild with `git pull && makepkg -si`). The package does not
  restart the services: run `sudo systemctl restart gpueue-helper.service gpueue.service`
  afterwards, so that the daemon and the `gpueue` command are the same version again. Running jobs
  keep running (see [Upgrading](#upgrading)). `gpueue update` and `gpueue service install` refuse a
  package installation.
- **Remove:** `sudo systemctl disable --now gpueue.service gpueue-helper.service`, then
  `sudo pacman -R gpueue-bin`. Jobs still running in `gpueue-job-*` units keep running until you
  stop them. `/var/lib/gpueue` (state, logs, checkpoint images) and a changed
  `/etc/gpueue/config.toml` (saved as `config.toml.pacsave`) are kept; delete them by hand to
  remove everything.

### The AUR package (maintainers)

The PKGBUILD lives in `packaging/aur/gpueue-bin`. When a release is published as a stable release
(the `released` event, like the APT repository; never for a draft or a pre-release),
`.github/workflows/release-aur.yml` runs `scripts/dist/update-aur.sh <version>`, which downloads
the release's tarballs and their `.minisig` files, checks them with the tag's
`packaging/minisign.pub` (and the trusted comment `gpueue v<version> <file>`, as `install.sh` does),
computes the PKGBUILD's checksums from the verified tarballs and from the tag's source archive (for
the units and snippets, which no signed asset carries), and regenerates `.SRCINFO` in an Arch
Linux container.
It then pushes `PKGBUILD`, `.SRCINFO` and `gpueue-bin.install` to
`ssh://aur@aur.archlinux.org/gpueue-bin.git`. The release must have both the x86_64 and the aarch64
tarball. To change the packaging without a new release, bump `pkgrel` in the PKGBUILD and run the
workflow by hand (`gh workflow run release-aur.yml -f version=<X.Y.Z>`).

One-time setup:

1. Create an account on <https://aur.archlinux.org>.
2. Create a dedicated SSH key and add its public half to the account ("My Account", "SSH Public
   Key"):

   ```sh
   ssh-keygen -t ed25519 -N '' -C gpueue-aur -f aur_gpueue
   ```

3. Store the private half as the repository secret `AUR_SSH_PRIVATE_KEY`, then delete the local
   copy (or keep it somewhere safe):

   ```sh
   gh secret set AUR_SSH_PRIVATE_KEY -R kaito4681/gpueue < aur_gpueue
   ```

The first push creates the package on the AUR. Without the secret the workflow only prints a
notice. To test the package locally (makepkg, pacman, the services, a job, an upgrade and
removal in an Arch Linux container with systemd): `scripts/dist/aur-test.sh`.

## Building

Both binaries are needed: `gpueue` (CLI and daemon) and `gpueue-helper` (the root helper). A plain
`cargo build` only builds `gpueue`, so name both packages:

```sh
cargo build --release -p gpueue -p gpueue-helper
# -> target/release/gpueue, target/release/gpueue-helper
```

Without a local Rust toolchain you can build in the project's Docker image (Rust 1.98.1 on Debian
bookworm; the binaries need glibc 2.34 or newer):

```sh
scripts/test-in-docker.sh build --release -p gpueue -p gpueue-helper
# -> target/docker/release/gpueue, target/docker/release/gpueue-helper
```

Tagged versions also come as prebuilt binaries for x86_64 and aarch64 Linux (built natively on
Debian bookworm, glibc 2.34 or newer) on the GitHub releases page:
`gpueue-v<version>-<target>.tar.gz` (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`)
holds `gpueue`, `gpueue-helper` and the documentation, with `.sha256` and `.minisig` files next to
it. `install.sh` installs them (see above). By hand:

```sh
sha256sum -c gpueue-v0.1.0-x86_64-unknown-linux-gnu.tar.gz.sha256
tar xzf gpueue-v0.1.0-x86_64-unknown-linux-gnu.tar.gz
sudo gpueue-v0.1.0-x86_64-unknown-linux-gnu/gpueue service install
```

## Installing

```sh
sudo target/release/gpueue service install
```

`gpueue-helper` must sit next to `gpueue`. The units run the binaries from:

1. the directory of the current `service install` installation, when re-installing or updating,
2. else the directory the `gpueue` binary is in, if that is `/usr/bin` or `/usr/local/bin`
   (used in place, nothing is copied; both binaries, and anything a symlink there resolves
   through, must then be owned by root and not group- or world-writable),
3. else `/usr/local/bin`, where `gpueue` and `gpueue-helper` are copied (it says so). That is the
   usual place for software the administrator installs: package managers leave it alone, and it
   is in systemd's and every user's default `PATH`.

`--bin-dir DIR` installs somewhere else. Because `gpueue-helper` runs as root, the directory and
every parent must be owned by root and not group- or world-writable (directories that do not
exist yet are created with mode 0755); install refuses anything else, and a path with `.` or `..`
components. `--no-start` enables the
services without starting or restarting them. `--force` overwrites unit, sysusers.d and
tmpfiles.d files that `gpueue service install` did not write (see step 2).

On RHEL, CentOS, Rocky, Alma and Fedora, sudo's `secure_path` does not include `/usr/local/bin`,
so `sudo gpueue ...` says "command not found". Run `sudo /usr/local/bin/gpueue ...`, or add
`/usr/local/bin` to `secure_path` with `sudo visudo`; `gpueue doctor` (run as root) points this
out. Scripts with a minimal `PATH` (such as cron's `/usr/bin:/bin`) need the full path too.

Built with Nix outside NixOS (`nix build`, then `sudo ./result/bin/gpueue service install`), the
binaries are copied to `/usr/local/bin` like any other build. They still load their libraries
from the Nix store, so install also registers the store path as a garbage-collector root,
`/nix/var/nix/gcroots/gpueue` (with `nix-store --add-root /nix/var/nix/gcroots/gpueue --realise
<store path>`). Only a `nix-store` that nobody but root can replace is run (the one in root's
`PATH`, else `/nix/var/nix/profiles/default/bin/nix-store` or
`/run/current-system/sw/bin/nix-store`). If none is found or it fails, install warns and prints
that command, and a root recorded earlier is kept. Since the helper runs as root with that
loader and those libraries, install refuses binaries whose loader is in a store someone else can
change: the loader and every directory above it must be owned by root and not writable by group
or others, except `/nix/store` itself, which may be group-writable when it is sticky and its
group is `nixbld` (a multi-user Nix installation; a single-user installation's store belongs to
its user). Whether the root is still needed is decided from the installed
binaries themselves (their dynamic loader is in `/nix/store`), so moving a Nix-built copy with
`--bin-dir` keeps it. `gpueue uninstall` removes the root, and so does installing binaries that
do not use the store (for example `gpueue update`).

`gpueue service install` exits non-zero when the final `gpueue doctor` report has a FAIL. After
`--no-start`, the failures that only mean "not running yet" (both units, both sockets, the
daemon and the helper connection) are ignored.

`gpueue service install` is safe to run again (for example after an upgrade). It does the
following:

1. Copies `gpueue` and `gpueue-helper` into the directory chosen above unless they are already
   there as regular files owned by root with the same contents (anything else, such as a symlink
   to a build directory, is replaced; mode 0755, replaced atomically), registers the Nix GC root
   if they come from the
   store, and writes `/var/lib/gpueue/install-receipt` (root, 0644). The receipt records the
   directory, the binaries it copied (none when they are used in place), the version and the GC
   root, so that `gpueue uninstall` removes exactly those. A re-install into another directory
   (with `--bin-dir`) removes the copies the previous one made; another name of the same
   directory (such as `/bin` for `/usr/bin` on merged-`/usr` systems) counts as the same
   directory, and a copy that is the same file as a new binary is never removed. If the receipt
   exists but cannot
   be used (not owned by root, writable by others, or not valid), install stops without
   changing anything: fix the file, or delete it after checking what it lists.
2. Writes these files from the packaging templates built into the binary. Each file starts with
   an "Installed by `gpueue service install`" marker line and is overwritten on the next install.
   If one of them exists without the marker (written by an administrator or a package), install
   stops before changing anything, unless `--force` is given. Directories it creates (such as
   `/etc/gpueue`) get mode 0755 whatever the umask:
   - `/etc/systemd/system/gpueue-helper.service`, the root helper (from
     [packaging/systemd/gpueue-helper.service](../packaging/systemd/gpueue-helper.service)),
   - `/etc/systemd/system/gpueue.service`, the daemon, which runs as user `gpueue` with a strict
     sandbox that allows only the NVIDIA devices NVML needs (from
     [packaging/systemd/gpueue.service](../packaging/systemd/gpueue.service)),
   - `/etc/sysusers.d/gpueue.conf`, which defines the system user `gpueue` and the group
     `gpueue-admin`,
   - `/etc/tmpfiles.d/gpueue.conf`, the directories listed below.
3. Runs `systemd-sysusers` for that file and, if needed, `groupadd --system gpueue-admin`.
4. Runs `systemd-tmpfiles --create` for that file, which creates:

   | Path | Mode, owner | Contents |
   | --- | --- | --- |
   | `/var/lib/gpueue` | 0755 root | base directory (`data_dir`) |
   | `/var/lib/gpueue/daemon` | 0700 gpueue | `state.sqlite`, the daemon's database |
   | `/var/lib/gpueue/helper` | 0711 root | the helper's job records and event queue |
   | `/var/lib/gpueue/checkpoints` | 0700 root | checkpoint images, `<id>/` per job |
   | `/var/lib/gpueue/logs` | 0755 root | job output, `<uid>/<id>.log` (the helper creates `<uid>/` with mode 2770, owned by the user, group `gpueue`) |
   | `/var/lib/gpueue/lib` | 0755 root | the fork guard library (`gpueue-forkguard-<hash>.so`) |
   | `/var/lib/gpueue/criu-no-plugins` | 0755 root | empty CRIU plugin directory for manual mode |
   | `/run/gpueue` | 0755 gpueue | `gpueue.sock` (daemon, mode 0666) and `helper.sock` (helper, owned by `gpueue`, mode 0600) |
   | `/run/gpueue-helper` | 0700 root | bind mount of `/` used by `criu restore --root` |

5. Creates `/etc/gpueue/config.toml` from
   [packaging/config.toml](../packaging/config.toml) **only if it does not exist yet**. If
   `nvidia-smi` works, it appends a `[[gpus]]` entry for each GPU it lists (a fallback for when
   NVML fails; see [GPU detection](#gpu-detection)).
6. Runs `systemctl daemon-reload`. If the units or the binaries changed, it runs
   `systemctl try-restart` on both services. Then it runs
   `systemctl enable --now gpueue-helper.service gpueue.service`, waits up to 10 seconds for the
   daemon to connect to the helper, and prints the `gpueue doctor` report.

Unit files owned by a distribution package (`/usr/lib/systemd/...`) are never touched. Customize
the units with drop-ins (`sudo systemctl edit gpueue-helper.service`). Edits to the files in
`/etc/systemd/system` are lost on the next install.

### Install method

`gpueue service install` must not share a machine with a package manager or Nix: its units in
`/etc/systemd/system` would shadow the packaged ones, and both would use `/etc/gpueue` and
`/var/lib/gpueue`. gpueue tells the ways apart by the unit files, `gpueue.service` and
`gpueue-helper.service` each:

| Install method | Recognized by |
| --- | --- |
| `gpueue service install` | `/etc/systemd/system/<unit>` starting with the install marker line |
| distribution package | `/usr/lib/systemd/system/<unit>` (or `/lib/systemd/system/...`) that `dpkg-query -S`, `rpm -qf` or `pacman -Qo` names an owner for (asked under both paths where both exist, since dpkg may have registered the `/lib` one on merged-`/usr` systems; the distribution's own package manager first). A tool that names an owner decides; the unit counts as unowned only when every installed tool says that no package owns it. If no tool can answer (none installed, or one fails, e.g. with a locked database), the unit counts as a package of unknown origin |
| unowned unit | such a unit that a package manager definitely reports as not owned (`no path found matching pattern`, `is not owned by any package`, `No package owns`): left over from a removed package, or copied there by hand (`make install`) |
| Nix | `/etc/systemd/system/<unit>` resolving into `/nix/store` (the NixOS `services.gpueue` module, system-manager), or a packaged unit that resolves into or runs a binary from `/nix/store` |
| by hand | `/etc/systemd/system/<unit>` without the marker, and no packaged unit (also when its `ExecStart=` is below `/nix/store`) |

Next to a packaged (or unowned) unit, a `/etc/systemd/system/gpueue.service` without the marker is
the administrator's replacement of it (`systemctl edit --full gpueue`), and a symlink to
`/dev/null` there is a mask (`systemctl mask gpueue`). Neither is a second install method: gpueue
reports the package as "overridden" or "masked". A mask without a package is ignored.

The operating system is read from `/etc/os-release` (else `/usr/lib/os-release`), `ID` and
`ID_LIKE`. It only decides the wording of hints and that NixOS (`ID=nixos`) uses its module; it
never decides the install method (the same distribution can have gpueue from a package or from
`install.sh`). `/etc/NIXOS` is not looked at.

What each command does (the hidden `gpueue service install-method` prints the keyword, for
`install.sh`):

| Install method | `service install` | `uninstall` | `update` | `doctor` | keyword |
| --- | --- | --- | --- | --- | --- |
| not installed | installs | says so | refuses: use `install.sh` | INFO | `none` |
| not installed, NixOS | refuses: use the `services.gpueue` module | says so | refuses: use the module | INFO "not installed (NixOS: use the services.gpueue module)" | `none` |
| `gpueue service install` | re-installs | uninstalls | updates | OK | `service-install` |
| distribution package (dpkg, rpm, pacman, or unknown) | refuses, even with `--force` | refuses | refuses | OK (INFO when masked) | `package` |
| unowned unit | only with `--force`; delete the file first instead | refuses | refuses | WARN | `unowned` |
| Nix | refuses, even with `--force` | refuses | refuses | OK | `nix` |
| by hand | only with `--force` | refuses | refuses | INFO | `manual` |
| mixed | refuses: uninstall the `service install` copy first | removes only what `service install` wrote (no `--purge`) | refuses | FAIL | `mixed` |

"Mixed" is `service install` together with a package or Nix, on one unit or across
`gpueue.service` and `gpueue-helper.service` (e.g. the daemon unit from `service install`, the
helper unit from a package): `doctor` fails, `service status` reports it as unhealthy, and the
keyword is `mixed`. A `service install` next to an unowned unit in `/usr/lib` stays a
`service install` (its units in `/etc` win), and re-running `service install` needs no
`--force`; `doctor` warns and tells you to delete the leftover. When the two units are installed
in other different ways (for example, one of them is missing, or they come from different
packages of different package managers), `doctor` warns and `service status` says so;
`service install` installs both and so fixes it. `service install` also refuses when `/etc/systemd/system` itself
resolves into `/nix/store`, since it cannot write there.

The refusals name the way to update or remove gpueue that fits the install method:

| Install method | Update | Remove |
| --- | --- | --- |
| dpkg | `sudo apt update && sudo apt upgrade` | `sudo apt remove <package>` |
| rpm | `sudo dnf upgrade` | `sudo dnf remove <package>` |
| pacman | `yay -Syu` (AUR) or `sudo pacman -Syu` | `sudo pacman -R <package>` |
| package of unknown origin | your package manager (named after `ID`/`ID_LIKE` when known) | the same |
| Nix on NixOS | `nix flake update && sudo nixos-rebuild switch` | `services.gpueue.enable = false`, then `sudo nixos-rebuild switch` |
| Nix elsewhere | the Nix configuration that created the unit | the same |
| `gpueue service install` | `sudo gpueue update` | `sudo gpueue uninstall` |

The daemon runs with `ProtectSystem=strict`: it can only write below the unit's
`ReadWritePaths=/var/lib/gpueue/daemon /var/lib/gpueue/logs /run/gpueue`. If you move `data_dir`,
`state_db`, `logs_dir` or `api_socket` elsewhere in `config.toml`, add the new directories in a
drop-in, or the daemon fails with "Read-only file system":

```sh
sudo systemctl edit gpueue.service
# [Service]
# ReadWritePaths=/srv/gpueue/daemon /srv/gpueue/logs
sudo systemctl restart gpueue.service
```

`gpueue doctor` warns (`daemon write paths`) when a configured directory is outside the unit's
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
gpueue doctor           # every check, with a `fix:` line for each problem; exits non-zero on FAIL
gpueue service status   # both units, and whether the daemon is reachable and connected to the helper
```

`gpueue doctor` checks, in this order (the check's name in parentheses):

- the configuration file and its ownership (`config`)
- the install method (`install method`, see [Install method](#install-method))
- on the RHEL family, with gpueue in `/usr/local/bin`: whether sudo's `secure_path` includes that
  directory (`sudo PATH`)
- `criu`, version 4.2 or newer (`criu`)
- the NVIDIA driver (`nvidia driver`)
- `cuda-checkpoint`: manual mode support, version matching the driver (`cuda-checkpoint`)
- in plugin mode only, CRIU's CUDA plugin (`cuda plugin`, see
  [CUDA plugin mode](#cuda-plugin-mode))
- with a driver, the GPUs the daemon detected (with NVML or from `[[gpus]]`) against
  `nvidia-smi` (`gpus`, see [GPU detection](#gpu-detection))
- the kernel (`kernel`) and the systemd version (`systemd`)
- both units (`gpueue-helper.service`, `gpueue.service`)
- the ownership of the helper binary and of every directory above it, taken from the unit's
  effective `ExecStart=`, drop-ins included (`helper binary`)
- the settings the helper uses: those of `config.toml`, with a warning when flags in its unit
  override them (`helper settings`)
- whether job units may open only their own GPUs' device nodes (`gpu isolation`, see
  [GPU isolation](#gpu-isolation))
- whether jobs can reach the system D-Bus or systemd's Varlink services, a warning when
  `job_system_dbus` or `job_systemd_varlink` is on (`job d-bus`, see
  [What a job can and cannot do](#what-a-job-can-and-cannot-do))
- which daemon socket directories are hidden from jobs, a warning when a default of
  `job_hidden_socket_dirs` is left out, when an entry is not a directory, or when nix-daemon's
  `trusted-users` names anyone besides root while `/nix/var/nix/daemon-socket` is not hidden
  (`job sockets`, see [What a job can and cannot do](#what-a-job-can-and-cannot-do))
- which users jobs may run as: the minimum uid, the denied groups and the allowed users
  (`job policy`, see [Who jobs may run as](#who-jobs-may-run-as))
- whether the daemon's directories are inside its `ReadWritePaths=` (`daemon write paths`)
- the daemon and helper sockets (`daemon socket`, `helper socket`)
- the daemon and helper versions and their connection (`daemon`, `helper`)
- programs installed after the helper started (it did not find them then and uses them only
  after a restart), a warning (`helper programs`, only when there are some)
- jobs the helper runs that the daemon does not account for (`orphaned jobs`, only when there
  are some)
- the checkpoint directory (exists, not tmpfs, free space) and the total checkpoint size
  (`checkpoint dir`, `checkpoint usage`)
- whether you are an administrator (`admin`)
- unless `update_check = false`, whether a newer release exists (`update`). This check is at most
  INFO, and so is not being able to reach GitHub.

It does not run `criu check`. Run `sudo criu check` yourself to see what CRIU supports on this
kernel.

Try a job:

```sh
gpueue add -- 'for i in $(seq 1000); do echo $i; sleep 1; done'
gpueue checkpoint 0 && gpueue status
gpueue restore -i 0 && gpueue follow 0
```

## Configuration

The daemon and the helper read `/etc/gpueue/config.toml`. Every key is optional. Unknown keys are
ignored. A missing file means all defaults. The installed file
([packaging/config.toml](../packaging/config.toml)) lists every key with comments: a commented
line `# key = value` shows the default, and lines indented below `# Example:` are examples for
keys that are unset by default (such as `checkpoint_size_limit_bytes`) and for `[[gpus]]`. After a
change, restart whatever uses the key. Restarting the helper does not affect running jobs:

```sh
sudo systemctl restart gpueue-helper.service gpueue.service
```

The helper runs as root and **refuses to start** unless the file and every parent directory
(`/etc/gpueue`, `/etc`, `/`) are owned by root, not group- or world-writable, and not symlinks.
The same rule applies to the directories it writes to or loads code from: `data_dir`,
`helper_state_dir`, `logs_dir`, `checkpoints_dir`, `<data_dir>/criu-no-plugins` (passed to
`criu -L`, and it must be empty), `<data_dir>/lib` (the fork guard) and all their parents must be
owned by root and not group- or world-writable (symlinks owned by root are followed and their
targets checked, unlike for the configuration file); the helper checks them before it creates
anything there. Keep the layout that
tmpfiles.d creates. The daemon stores its database in the `daemon/` subdirectory because
`data_dir` itself must belong to root.

Groups and GPU slot limits are not configured here. They are stored in the daemon's database and
changed with `gpueue group`, `gpueue parallel` and `gpueue gpu parallel`.

Paths of files, directories and sockets must be absolute. Programs (`criu_path`,
`cuda_checkpoint_path`, `nvidia_smi_path`, `nvml_library`, the first word of `shell`) are either
a bare name, looked up in `PATH`, or an absolute path; relative paths such as `bin/criu` are
rejected. The helper looks up `criu_path`, `cuda_checkpoint_path` and `nvidia_smi_path` once, when
it starts, in its service's `PATH`, checks the file it found like an absolute path (owned by root
and reached only through directories that only root can change) and uses that path from then on.
A program that was not found stays unavailable until the helper is restarted (its log says so).
Every `*_secs` key is at most one year (31536000).

| Key | Default | Used by | Meaning |
| --- | --- | --- | --- |
| `data_dir` | `"/var/lib/gpueue"` | both | base of `daemon/state.sqlite`, `helper/`, `logs/`, `checkpoints/`, `lib/`, `criu-no-plugins/` |
| `state_db` | `<data_dir>/daemon/state.sqlite` | daemon | the SQLite database |
| `helper_state_dir` | `<data_dir>/helper` | helper | job records and event queue (root-owned) |
| `logs_dir` | `<data_dir>/logs` | both | job output, `<uid>/<id>.log` (root-owned) |
| `checkpoints_dir` | `<data_dir>/checkpoints` | both | checkpoint images (root-owned, not tmpfs) |
| `api_socket` | `"/run/gpueue/gpueue.sock"` | daemon | the socket clients connect to (`gpueue --socket` / `GPUEUE_SOCKET` on the client side) |
| `helper_socket` | `"/run/gpueue/helper.sock"` | both | the helper's socket |
| `daemon_user` | `"gpueue"` | helper | the only user allowed to connect to the helper |
| `grace_secs` | `30` | both | seconds between SIGTERM and SIGKILL when a job is stopped |
| `shell` | `["sh", "-c"]` | daemon | command prefix that the job's command line is appended to |
| `checkpoint_size_limit_bytes` | unset (no limit) | daemon | warn when all images together exceed this size (images are never deleted automatically) |
| `checkpoint_size_warning_interval_secs` | `3600` | daemon | how often each administrator is reminded while the limit is exceeded |
| `checkpoint_tcp_close` | `true` | both | close TCP connections when checkpointing (`criu --tcp-close`) |
| `criu_path` | `"criu"` | helper | name (looked up in `PATH` when the helper starts) or root-owned absolute path |
| `cuda_checkpoint_path` | `"cuda-checkpoint"` | helper | same |
| `nvidia_smi_path` | `"nvidia-smi"` | helper | same |
| `cuda_checkpoint_mode` | `"manual"` | helper | `"manual"`: the helper calls cuda-checkpoint around criu. `"plugin"`: CRIU's CUDA plugin does it. |
| `checkpoint_deadline_base_secs` | `180` | helper | fixed part of the deadline of a whole checkpoint or restore (see [Checkpoint timeouts](#checkpoint-timeouts)) |
| `checkpoint_deadline_secs_per_gib` | `30` | helper | part of that deadline per GiB of RSS + VRAM (checkpoint) or image (restore) |
| `criu_dump_timeout_secs` | unset | helper | optional cap on `criu dump` alone; unset: only the deadline applies |
| `criu_restore_timeout_secs` | unset | helper | optional cap on `criu restore` alone; unset: only the deadline applies |
| `fork_guard` | `true` | helper | preload the fork guard into jobs (see the README's limitations) |
| `gpu_device_isolation` | `true` | helper | each job unit may open only its assigned GPUs' device nodes (`DevicePolicy=closed`); `false` turns this off (see [GPU isolation](#gpu-isolation)) |
| `job_system_dbus` | `false` | helper | `true` lets jobs reach the system D-Bus (systemd, polkit); by default `/run/dbus` is hidden from jobs (see [What a job can and cannot do](#what-a-job-can-and-cannot-do)) |
| `job_systemd_varlink` | `false` | helper | `true` lets jobs reach systemd's Varlink services under `/run/systemd`; by default the directory is hidden from jobs except journald's log sockets, resolved's resolution socket and `resolv.conf` files, and `userdb/` (see [What a job can and cannot do](#what-a-job-can-and-cannot-do)) |
| `job_min_uid` | `UID_MIN` of `/etc/login.defs`, else `1000` | helper | jobs of users with a smaller uid are refused (see [Who jobs may run as](#who-jobs-may-run-as)) |
| `job_denied_groups` | `["docker", "lxd", "incus-admin", "libvirt", "libvirtd", "disk", "sudo", "wheel", "admin"]` | helper | root-equivalent groups: dropped from jobs' supplementary groups; a job whose primary group is one of them is refused |
| `job_hidden_socket_dirs` | `["/run/libvirt", "/run/cups"]` | helper | directories hidden from jobs behind an empty root-owned tmpfs (created first if missing): those of daemon sockets that check the caller's groups in the user database, which dropping groups does not defeat; absolute paths at least two levels deep (see [What a job can and cannot do](#what-a-job-can-and-cannot-do)) |
| `job_allowed_users` | `[]` (everyone) | helper | if not empty, only these users (by name) may run jobs |
| `gpu_detection` | `"auto"` | daemon | `"auto"`: NVML, falling back to `[[gpus]]`. `"static"`: only `[[gpus]]`. |
| `nvml_library` | unset | daemon | NVML library path or name; by default `libnvidia-ml.so.1`, then `libnvidia-ml.so` |
| `update_source` | `"github:kaito4681/gpueue"` | CLI | where `gpueue update` and `gpueue doctor` look for releases: `github:<owner>/<repo>` |
| `update_check` | `true` | CLI | whether `gpueue doctor` asks `update_source` for a newer release |
| `[[gpus]]` | empty | daemon | static GPU list: `index` (required), `uuid`, `name`, `memory_total_bytes` (see [GPU detection](#gpu-detection)) |
| `[priority]` | multifactor priority with fair share | daemon | how queued jobs are ordered (see [Priority and fair share](#priority-and-fair-share)) |

`gpueue-helper serve` also accepts command-line flags (`gpueue-helper serve --help`). They override
the file. Prefer the file: `gpueue doctor` reports the file's values and warns when the unit's
`ExecStart=` passes flags.

## Administrators

Administrators are root and the members of `gpueue-admin` (primary or supplementary group). They
can act on every user's jobs and run the admin-only commands (`group add/remove`, `parallel`,
`gpu parallel`, pausing/resuming groups with `pause/start/kill -g` or `-a`, `shutdown`, `reset`
of all jobs, and the priority settings `share set`, `group priority`, `site` and negative
`nice`).

```sh
sudo usermod -aG gpueue-admin alice
```

The daemon looks the group up on every request (through NSS), so the change applies at once,
without logging in again.

## Who jobs may run as

The root helper starts every job as its owner, so it decides whose jobs it runs; the daemon cannot
change that. Besides never running a job as root (uid 0 or gid 0) or with a group its owner is not
in, the helper applies three settings to every spawn and every restore:

- `job_min_uid`: users with a smaller uid are refused. Unset, it is `UID_MIN` of
  `/etc/login.defs` (1000 on most distributions), or 1000 when the file has no valid `UID_MIN`.
  This keeps system accounts (`www-data`, `postgres`, the daemon's own `gpueue` user, ...) from
  running jobs.
- `job_denied_groups`: groups whose members can act as root without a password. They are
  removed from a job's supplementary groups, so a job of a user in `docker` runs without
  `docker` (the helper's journal records which groups it left out; the job's output is not
  touched). A job whose primary group is one of them is refused, and so is restoring a
  checkpoint whose processes had one of them (CRIU would restore the group). Names are looked up
  in `/etc/group` at each spawn and restore; names that are not listed there are ignored. gid 0 is
  always left out. The defaults and why each is listed:

  | Group | Why it is root-equivalent |
  | --- | --- |
  | `docker` | the Docker socket starts containers that bind-mount `/` and run as root |
  | `lxd`, `incus-admin` | the same through LXD/Incus (privileged containers, host mounts); Incus's restricted `incus` group is not listed |
  | `libvirt`, `libvirtd` (NixOS) | manages `qemu:///system`, whose VMs can be given host block devices and directories |
  | `disk` | raw read/write access to block devices, and so to every file on them |
  | `sudo`, `wheel`, `admin` | sudo rights: sudo normally asks for the password, which an unattended job cannot type, but `NOPASSWD` rules or cached credentials would let the job become root |

  Not listed, because they do not amount to root: `shadow` (password hashes, at most cracked
  offline), `adm` (logs), `kvm`, and `video`/`render` (devices; GPU jobs need the latter two).
  Add groups that are root-equivalent on your hosts (e.g. `microk8s`), or remove one to let jobs
  use it again (e.g. `job_denied_groups = ["lxd", "incus-admin", "libvirt", "libvirtd", "disk", "sudo",
  "wheel", "admin"]` lets jobs use Docker).
- `job_allowed_users`: if not empty, only the users listed (by name) may run jobs.

A refused job fails to spawn with a reason that starts with `gpueue refuses to run jobs as
<user>:`, followed by e.g. `uid 999 is below job_min_uid 1000 (UID_MIN in /etc/login.defs)`,
`primary group docker is in job_denied_groups` or `not in job_allowed_users`. `gpueue status`
shows it as `Failed to spawn`, and `gpueue log <id>` and `gpueue status --json` show the reason. A
refused restore is a restore failure (see [Restore failures](#restore-failures)): the job stays
checkpointed. `gpueue doctor` prints the effective policy (the minimum uid
and where it comes from, the denied groups and which of them exist on the host, the allowed
users), and the helper logs it when it starts. Restart the helper after changing these keys.

## What a job can and cannot do

A job runs as its owner and can do what its owner can do on files, the network and its assigned
GPUs. gpueue guarantees that it cannot gain more, even when the owner could outside gpueue (a
member of `sudo` with a `NOPASSWD` rule, of `docker`, or of a group that polkit trusts). This
holds for a job started from a checkpoint as well: the restored unit is set up the same way, and
the restored processes keep their credentials. e2e scenario 19 checks each item below, before
and after a checkpoint and restore.

| A job ... | because | switch |
| --- | --- | --- |
| runs with its owner's uid and primary gid, never root | the helper refuses uid 0, system accounts and denied primary groups ([Who jobs may run as](#who-jobs-may-run-as)) | `job_min_uid`, `job_denied_groups`, `job_allowed_users` |
| has no capabilities (`CapInh`, `CapPrm`, `CapEff`, `CapBnd` and `CapAmb` are all 0) and `NoNewPrivs: 1` | the shim empties the bounding set and sets `no_new_privs` before it runs the command | none |
| cannot gain privileges with `sudo`, `su`, `pkexec` or any other setuid or file-capability program | `no_new_privs` makes the kernel ignore setuid bits and file capabilities | none |
| cannot reach the system D-Bus, so no `systemctl`, `run0`, `machinectl`, `busctl --system` or other request to systemd, polkit or system services | the shim hides `/run/dbus` (also `/var/run/dbus` if `/var/run` is not a link to `/run`) behind an empty root-owned tmpfs in the job's mount namespace, creating the directory if the host has none yet, so a bus socket that appears later is out of reach as well; CRIU saves the tmpfs and recreates it on restore; polkit takes group membership from the user database, not from the job's groups, so dropping groups alone would not stop it | `job_system_dbus = true` |
| cannot reach systemd's Varlink services (`io.systemd.Manager` reload and reexecute, `io.systemd.Credentials` decryption of system credentials, mountfsd, nsresourced, machined, logind, networkd, sysupdate, ...), which authorize unprivileged callers through polkit without D-Bus | the shim hides `/run/systemd` behind an empty root-owned tmpfs that holds only journald's log sockets (`journal/socket`, `journal/stdout`, `journal/dev-log`), resolved's `resolve/io.systemd.Resolve`, `resolve/stub-resolv.conf` and `resolve/resolv.conf` (bound one by one, so sockets created there later stay out of reach), the `userdb/` directory (user and group lookups only, see below) and an empty `system/`; CRIU saves and recreates this on restore | `job_systemd_varlink = true` |
| cannot reach daemon sockets that authorize callers by their groups in the user database: libvirt's (`/run/libvirt/libvirt-sock`, `virtqemud-sock`, `virtstoraged-sock`, ... with their `-ro` and `-admin` variants), which are world-connectable on Fedora, RHEL, Debian, Arch and NixOS and authorized through polkit, whose shipped rule grants `org.libvirt.unix.manage` to group `libvirt` (`libvirtd` on NixOS), so a member's job could otherwise define a VM or storage pool on the host's disks; and CUPS's `/run/cups/cups.sock`, where cupsd identifies the caller by its peer credentials and lets `SystemGroup` members (`lpadmin`, `wheel`) administer printers | the shim hides each directory of `job_hidden_socket_dirs` behind an empty root-owned tmpfs, as for `/run/dbus` (creating it if missing, so sockets that appear later stay out of reach; CRIU saves and recreates this on restore); printing still works, as libcups falls back to `localhost:631`, where administration needs a password | `job_hidden_socket_dirs` |
| does not have the groups `docker`, `lxd`, `incus-admin`, `libvirt`, `libvirtd`, `disk`, `sudo`, `wheel`, `admin` (root daemons' sockets, raw block devices) | the helper drops them from the supplementary groups | `job_denied_groups` |
| cannot open device nodes except the standard pseudo devices and its assigned GPUs' | `DevicePolicy=closed` with `DeviceAllow=` ([GPU isolation](#gpu-isolation)) | `gpu_device_isolation` |

Each switch weakens one guarantee; `gpueue doctor` warns when `job_system_dbus = true` or `job_systemd_varlink = true`, and when `job_hidden_socket_dirs` leaves out a default. What gpueue
does not limit: whatever the owner's uid itself may do (files, user services over the session bus,
network access), the amount of CPU, memory or disk a job uses, and sockets of other services
that check credentials on their own (for example a daemon socket that allows any user).
The job's mounts are private (no propagation from the host), so a socket the host creates later
in `/run/dbus`, in `/run/systemd` or in its `journal/` and `resolve/` subdirectories does not show
up in the job, before or after a restore (e2e scenario 19 checks this). The one directory bound
whole is `userdb/`: services there implement only `io.systemd.UserDatabase` (`GetUserRecord`,
`GetGroupRecord`, `GetMemberships`), read-only lookups that `getent` answers anyway, so a socket
added there later grants nothing. Other root sockets stay reachable but grant nothing to an
unprivileged caller: udev's `/run/udev/io.systemd.Udev` only accepts root (socket mode 0644),
polkit's `/run/polkit/agent-helper.socket` only completes a polkit authentication that a client
started over D-Bus (which a job cannot reach), and the system bus listens on no other address
(`<listen>unix:path=/run/dbus/system_bus_socket</listen>` only). Name resolution keeps working,
but a job sees `resolv.conf` as it was when the job started (the files are bound, and resolved
replaces them on changes).

Daemon sockets elsewhere were checked on Ubuntu 22.04/24.04/26.04, Debian 12, Fedora, RHEL 9,
Arch and NixOS. Most decide by the socket file's group, which dropping the group already defeats:
Docker (`/run/docker.sock`, group `docker`), Incus (`/var/lib/incus/unix.socket`, group
`incus-admin`; `unix.socket.user`, group `incus`, gives only restricted projects), LXD
(`/var/snap/lxd/common/lxd/unix.socket` and Ubuntu's `/run/lxd-installer.socket`, group `lxd`),
and libvirt on Ubuntu (`libvirt-sock` of group `libvirt` with `auth_unix_rw = "none"`; hidden
anyway). Rootful Podman's `/run/podman/podman.sock`, containerd, virtlogd and virtlockd, and
Cockpit's internal sockets accept only root or a system group; Cockpit itself (port 9090) and
SSH ask for the user's password or key, like any login. udisks2, NetworkManager, PackageKit,
accounts-daemon, fwupd and firewalld only have D-Bus interfaces. snapd's `/run/snapd.socket`
allows anyone, but installing, removing or configuring snaps needs `auth_admin` in polkit, which
needs an authentication agent on the system bus, so a job cannot (and snaps do not run under
`no_new_privs` anyway). nix-daemon (`/nix/var/nix/daemon-socket/socket`) checks `@group` entries
of `trusted-users` in the group database, and a trusted user is root-equivalent: the default
(`root` only) is safe, but if you trust `@wheel` or other users, add
`/nix/var/nix/daemon-socket` to `job_hidden_socket_dirs` (`gpueue doctor` warns), which takes
the Nix daemon (building, fetching) away from jobs. Add the directory of any other site-specific
socket that trusts groups or polkit rules the same way. A mask on a directory goes away if the
host removes that directory (the kernel detaches mounts on a removed directory in every
namespace), so list directories that the host keeps.

## Priority and fair share

When several queued jobs wait for the same group or GPU slots, gpueue starts the one with the
highest priority among those that fit (a job that does not fit never blocks smaller ones behind
it). The priority follows Slurm's
[multifactor priority plugin](https://slurm.schedmd.com/priority_multifactor.html):

```text
priority = site factor
         + priority_weight_age       * age factor
         + priority_weight_fairshare * fair-share factor
         + priority_weight_job_size  * job size factor
         + priority_weight_partition * partition factor
         + priority_weight_tres      * TRES factors
         - nice
```

Each factor is between 0 and 1:

- **Age:** the time since the job became eligible (queued, with every dependency finished
  successfully), divided by `priority_max_age_secs`, at most 1. Stashed jobs and jobs waiting for a
  dependency do not age.
- **Fair share:** Slurm's classic formula `F = 2^(-U/S)`. `S` (normalised shares) is the user's
  shares divided by the sum of the shares of all *active* users, that is users with an unfinished
  job or recorded usage (users have 1 share unless an administrator sets another number). `U`
  (effective usage) is the user's usage divided by every user's usage (0 while nobody has used
  anything). A user who used exactly their share gets 0.5, less than their share more, more less;
  a user with 0 shares gets 0.
- **Job size:** the job's GPUs divided by the machine's GPUs; for CPU-only jobs one CPU divided by
  the machine's CPUs (gpueue does not allocate CPUs, so every job counts as one CPU). With
  `priority_favor_small = true` it is `(total - requested + 1) / total` instead, so the smallest
  jobs get 1.
- **Partition:** gpueue groups play the role of Slurm partitions. An administrator gives a group a
  priority with `gpueue group priority <group> <n>`; the factor is that priority divided by the
  highest group priority (0 while every group has priority 0).
- **TRES:** for each resource weighted in `priority_weight_tres` (`CPU`, `GRES/gpu`), the job's
  request divided by the machine's total.

**Usage.** Only the time a job spends running is billed: its billing (from
`tres_billing_weights`, by default 1 per CPU plus 10 per GPU) times the seconds it ran is added
to its owner's usage. A job is charged when it stops running (it is paused, checkpointed, exits,
is killed or lost) and, while it runs, every `priority_calc_period_secs` (and when the daemon
starts). Time spent queued, paused, checkpointed or waiting to be restored costs nothing, and one
charge covers at most two calculation periods, so time the daemon was not running is not billed.
Every calculation also decays all usage with a half-life of `priority_decay_half_life_secs` (0:
no decay). The scheduler uses fair-share factors computed at each calculation and when shares
change; a user who submits their first job in between counts as having no usage until the next
calculation. Usage is kept in the database across restarts and reset as `priority_usage_reset_period`
says (`"now"` resets it whenever the daemon starts; the calendar periods reset at 00:00 local
time, weeks on Sunday). Usage below 0.001 billing-seconds is dropped.

**A user's own order.** pueue's `-o/--priority`, `gpueue switch` and `gpueue top` only order the
jobs of one user, like HTCondor's `condor_prio` and Slurm's `scontrol top`: the order is jobs
moved to the top first (the most recent `top` first), then higher `-o` priority, then lower id.
gpueue applies it by pooling the ages of a user's pending jobs that could launch now and handing
them out in that order (the oldest age to the first job), so the sum of the user's priorities,
and every other user's position, stays the same. A job could launch now when its group exists
and is not paused, every GPU it asks for was detected, and its dependencies are done; waiting
for a free group or GPU slot is ordinary queueing and does not exclude a job. A job that cannot
launch now lends no age to the user's other jobs: it keeps its own age (a job waiting for a
dependency has an age of 0, counted from when the dependency finished) and joins the pool once
it can launch, so an old job parked in a paused group cannot lift the user's newest job ahead of
other users' jobs. Remaining ties go by the user's order, then by id. **This is a change from pueue,**
where a higher `-o` priority wins over every other job. `priority_type = "basic"` restores
pueue's order (higher `-o` priority first, then lower id, for all jobs; nice, site factors, `top`
and fair share are then ignored).

**Immediate launches.** In pueue, `start <id>` of a queued or stashed job, `add -i`, `restart -i`
and `restore -i` start the job right away. In gpueue only administrators' do: their job starts at
once, ignoring the group's `parallel` limit, GPU slots, a paused group and dependencies. For
anybody else these commands put the job (for `restart -i`, the new copy; for `restore -i`, the
restore) first among their own queued jobs, exactly like `gpueue top`. The job then competes with
other users' jobs by the priority above, and the group's `parallel` limit, GPU slots, a paused
group and its dependencies apply as for any queued job; the command says that the job was queued
first among your jobs rather than started, and the API reply carries `"queued": "first"`. With
`enable_user_top = false`, or with `priority_type = "basic"` (where the owner's order does not
apply), the job is queued like any other job without being moved (`"queued": "in_line"`).
`start` of a paused job resumes it, as before.

**Commands.**

| Command | Who | Slurm |
| --- | --- | --- |
| `gpueue prio [IDS] [--json]` | everyone | `sprio`: each pending job in scheduling order with its priority and weighted factors (no commands are shown) |
| `gpueue share [--json]` | everyone | `sshare`: shares, normalised shares, raw and effective usage and fair-share factor per user |
| `gpueue add --nice N`, `gpueue nice <IDS>... <N>` | owner (only raising it, never below 0), administrators (any `N`) | `--nice`, `scontrol update nice=` |
| `gpueue top <IDS>...` | owner (with `enable_user_top`), administrators | `scontrol top` |
| `gpueue site <IDS>... <N>` | administrators | the site factor |
| `gpueue share set <USER> <N>` | administrators | `sacctmgr modify user set fairshare=` |
| `gpueue group priority <GROUP> <N>` | administrators | partition `PriorityJobFactor` |

As in Slurm, users may only raise a nice value, so they cannot undo a value an administrator
set. A restarted job, a copy or `restart --in-place`, keeps a nice value of 0 or more, but not a
negative one, the site factor or its `top` position. `gpueue prio` lists exactly the jobs the
scheduler considers, in its order. `gpueue status --json` shows a job's `nice` and a group's `priority` when they are not 0.

No release has been published yet, so this only matters for development builds: the database of
a build from before priorities is migrated to schema version 6 when the daemon starts. Existing
jobs keep their `-o` priority, which from then on orders only their owner's jobs, and usage
starts at zero. Older builds cannot open the migrated database (`gpueue update` refuses to
install them).

**Settings.** The `[priority]` table of `/etc/gpueue/config.toml` (restart `gpueue.service` after a
change). Keys below the `[priority]` line belong to the table, so keep other keys above it.

| Key | Default | Slurm |
| --- | --- | --- |
| `priority_type` | `"multifactor"` (`"basic"`: pueue's order) | `PriorityType` |
| `priority_weight_age` | `1000` | `PriorityWeightAge` |
| `priority_weight_fairshare` | `10000` | `PriorityWeightFairshare` |
| `priority_weight_job_size` | `0` | `PriorityWeightJobSize` |
| `priority_weight_partition` | `0` | `PriorityWeightPartition` |
| `priority_weight_tres` | `""` (e.g. `"GRES/gpu=1000"`) | `PriorityWeightTRES` |
| `priority_max_age_secs` | `604800` (7 days) | `PriorityMaxAge` |
| `priority_decay_half_life_secs` | `604800` (7 days; 0: no decay) | `PriorityDecayHalfLife` |
| `priority_calc_period_secs` | `300` | `PriorityCalcPeriod` |
| `priority_usage_reset_period` | `"none"` (`"now"`, `"daily"`, `"weekly"`, `"monthly"`, `"quarterly"`, `"yearly"`) | `PriorityUsageResetPeriod` |
| `priority_favor_small` | `false` | `PriorityFavorSmall` |
| `tres_billing_weights` | `"CPU=1.0,GRES/gpu=10.0"` | `TRESBillingWeights` |
| `enable_user_top` | `true` | `SchedulerParameters=enable_user_top` |

For example, to favour jobs that use more GPUs and give the `urgent` group a boost:

```toml
[priority]
priority_weight_tres = "GRES/gpu=2000"
priority_weight_partition = 5000
```

```sh
sudo gpueue group priority urgent 10
```

## GPU detection

The daemon detects GPUs once, when it starts. Restart `gpueue.service` after adding, removing or
replacing a GPU.

- With `gpu_detection = "auto"` (the default) it loads NVML (`libnvidia-ml.so.1`) at run time and
  ignores `[[gpus]]`. If NVML cannot be loaded or initialized, it logs why (`journalctl -u
  gpueue.service`, a warning starting with "GPU detection with NVML failed") and uses the
  `[[gpus]]` list from the configuration instead, or runs without GPUs if there is none.
- With `gpu_detection = "static"` it always uses the `[[gpus]]` list.

NVML needs read-write access to `/dev/nvidiactl` and `/dev/nvidia<N>`. These nodes are normally
mode 0666, so the `gpueue` user needs no extra group. The packaged `gpueue.service` therefore does
not use `PrivateDevices=yes` (its private `/dev` has no NVIDIA nodes). It uses
`DevicePolicy=closed` with `DeviceAllow=` for the NVIDIA control and GPU devices instead, so the
daemon can open only those and the standard pseudo devices. If you customize the unit, keep these
lines, or NVML fails with "NVIDIA driver is not loaded" (node hidden) or "internal driver error"
(node not allowed). The log message says which one.

`gpueue service install` also writes the `[[gpus]]` list from `nvidia-smi` when it creates the
configuration file, as the fallback. `gpueue doctor` asks the daemon which GPUs it detected and
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
| `index` | required | the GPU's index in PCI bus order, as nvidia-smi numbers it. Unique. `gpueue add --gpu` takes it, and jobs see it in `CUDA_VISIBLE_DEVICES` (with `CUDA_DEVICE_ORDER=PCI_BUS_ID`). |
| `uuid` | `""` | the GPU's UUID. The daemon warns at start-up when a GPU with a different UUID than before is found at an index. |
| `name` | `""` | the model, shown by `gpueue gpu` and recorded with each checkpoint. Restoring requires the same model. |
| `memory_total_bytes` | unset | the GPU's memory in bytes, shown by `gpueue gpu` |

`gpueue gpu` shows what the daemon uses: with NVML, each GPU's name, memory and driver version.
The GPU end-to-end test checks NVML detection inside the packaged unit.

A GPU that the daemon knew before but no longer finds stays listed as unavailable, and keeps its
slot limit. Jobs that need it stay queued, and new jobs that ask for it are rejected. Each GPU
starts with a limit of 1 job. Change it with `gpueue gpu parallel <N> [--gpu LIST]` (`0` means
unlimited).

## GPU isolation

`CUDA_VISIBLE_DEVICES` only tells CUDA which GPUs to use, and a job can change it. With
`gpu_device_isolation = true` (the default) the helper also restricts the device nodes each job
can open, with the cgroup device controller of the job's systemd unit:

- Every job unit, spawned (`gpueue-job-<id>.service`) or restored
  (`gpueue-job-<id>-r<n>.service`), gets `DevicePolicy=closed`. That allows the standard pseudo
  devices (`/dev/null`, `/dev/zero`, `/dev/full`, `/dev/random`, `/dev/urandom`, `/dev/tty`,
  `/dev/ptmx` and the `/dev/pts/*` pseudo terminals) and nothing else that is not listed.
- A job with GPUs gets `DeviceAllow=` (read-write) for each assigned GPU's
  `/dev/nvidia<minor>`, and for `/dev/nvidiactl`, `/dev/nvidia-uvm`, `/dev/nvidia-uvm-tools`,
  `/dev/nvidia-modeset` and `char-nvidia-uvm` (the unified memory devices by major number, in
  case the driver creates their nodes only after the unit started).
- A CPU-only job gets no NVIDIA device at all.
- A restored job gets the nodes of the GPUs it is restored on (the same indices as at the
  checkpoint). `criu restore` and, in plugin mode, CRIU's CUDA plugin run inside the restore
  unit; in manual mode the driver reopens the device nodes in the restored processes. Checkpoint
  and restore of CUDA jobs work with isolation on (the GPU end-to-end test checks it).

The filter applies to every process of the unit, root included. Opening another GPU's
`/dev/nvidia<N>` fails with `EPERM`. Inside a job, `nvidia-smi` shows only the GPUs the job may
open. In a CPU-only job it fails with `Failed to initialize NVML: Unknown Error`, and CUDA finds
no device. To see what a unit got:

```sh
systemctl show -p DevicePolicy -p DeviceAllow gpueue-job-<id>.service
```

GPU indices follow PCI bus order, but device nodes are named after the driver's minor number,
which can differ. At every spawn and restore the helper reads the minors from
`/proc/driver/nvidia/gpus/<PCI address>/information` (`Device Minor`), in PCI bus order and
without GPUs marked `GPU Excluded: Yes`, as NVML and CUDA count them. A GPU index the driver does
not know (a `[[gpus]]` entry for a GPU that is not there, or no driver loaded) gets no node of
its own, and the helper logs it.

Set `gpu_device_isolation = false` and restart `gpueue-helper.service` to turn isolation off.
Jobs started or restored after that get no device restrictions; running jobs keep the settings of
their unit. You need to turn it off:

- on hosts that use **MIG**: CUDA also opens `/dev/nvidia-caps/nvidia-cap<N>` of the MIG instance,
  which is not allowed;
- if jobs need other device nodes, such as `/dev/fuse`, `/dev/kvm` or RDMA devices under
  `/dev/infiniband`.

`gpueue doctor` shows whether isolation is on (`gpu isolation`).

## Checkpoint storage

- Images are stored in `checkpoints_dir` (default `/var/lib/gpueue/checkpoints/<job id>/`), which
  only root can read. Each directory holds `images/` (CRIU image files), `dump.log`,
  `restore.log`, `meta.json` (GPU, driver, size, time) and a `COMPLETE` marker that is written
  last. Directories without the marker are incomplete and are cleaned up when the helper starts,
  except one whose dump succeeded (a `DUMPED` marker, or CRIU's log says so): the job's
  processes are gone then and the image is all that is left of it. The helper finalizes such a
  directory when it starts or before the job is restored, and never deletes it. If writing the
  metadata fails right after a successful dump (a full or failing disk), the job counts as
  checkpointed, the helper logs the reason and the daemon logs a warning; a restore fails, keeping
  the image, until the metadata can be written.
  The helper also writes `shutdown-manifest.json` there after the dumps at system shutdown.
- Each job has at most one image. It is deleted when the restore succeeds, when the job is removed
  (`remove`, `clean`, `reset`), or when the dump fails. A failed restore keeps the image.
- After a failed dump the helper moves its `dump.log` to `failed-logs/<job id>-<unix ms>.log` in
  the same directory (root only) and keeps the newest 20 of them. The job's last error names the
  file.
- Space: before each dump the helper requires free space of at least (RSS + VRAM of the job +
  what is stored in its private `/tmp` and `/dev/shm`) × 1.2.
  Otherwise the checkpoint is refused and the job keeps running. Images are not compressed. A job with 7 GB of VRAM produced an
  8.1 GB image in tests. Do not use tmpfs (`gpueue doctor` fails on it). It also warns below 20 GiB
  free, or below the space left under `checkpoint_size_limit_bytes`.
- Limit: with `checkpoint_size_limit_bytes`, the owner of a checkpoint that takes the total over
  the limit is warned once. Administrators are warned on their commands, at most every
  `checkpoint_size_warning_interval_secs`, and `gpueue doctor` reports it. Dumps are not refused
  and nothing is deleted automatically. Administrators see the image sizes of all jobs in
  `gpueue status`.
- To use another disk, create a root-owned directory (mode 0700, all parents root-owned and not
  group- or world-writable), set `checkpoints_dir`, and make sure the helper is stopped before
  that filesystem is unmounted at shutdown:

  ```sh
  sudo install -d -o root -g root -m 0700 /data/gpueue-checkpoints
  sudo systemctl edit gpueue-helper.service
  #   [Unit]
  #   RequiresMountsFor=/data/gpueue-checkpoints
  sudo systemctl restart gpueue-helper.service gpueue.service
  ```

  Move or restore existing checkpoints before you change the path.

## CUDA plugin mode

By default (`cuda_checkpoint_mode = "manual"`) the helper runs cuda-checkpoint itself (`lock`,
`checkpoint`, then `criu dump`; `criu restore`, then `restore` and `unlock`) and points CRIU at an
empty plugin directory. In plugin mode it runs only `criu`, and CRIU's CUDA plugin calls
cuda-checkpoint at the right moments of the dump and the restore. To enable it:

1. Build CRIU 4.0 or newer with the plugin (gpueue is tested with 4.2.1) and install the plugin in
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
3. Set `cuda_checkpoint_mode = "plugin"` in `/etc/gpueue/config.toml`, run
   `sudo systemctl restart gpueue-helper.service`, and check with `gpueue doctor` (the `cuda plugin`
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
cuda-checkpoint hangs on a stopped process. They are restored stopped in both modes: in plugin
mode CRIU's plugin restores the CUDA state before the helper's restore hook stops the processes,
and in manual mode the helper lets only the CUDA restore threads run (with `ptrace`) while it runs
cuda-checkpoint.

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
filesystem and run `gpueue restore` again. The exception are FUSE and network filesystems that are
already gone, or do not answer, when the job is dumped: a job that does not use them is dumped
without them (see
[Network and FUSE filesystems that stop answering](#network-and-fuse-filesystems-that-stop-answering)).

Before a restored job runs, the helper checks that its owner could open each of its files the way
the job has it open, by the same path and without symlinks (see
[Restores and paths](architecture.md#restores-and-paths)). A file the job already had open at the
dump passes even if its owner could not open it that way now (a file the job created read-only,
or an sshfs mount on which every file shows as root's; this includes files it opened a moment
before the dump), as long as it is that very file:

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

Tested across a simulated reboot (`scripts/e2e-test.sh`, scenarios 15, 16 and 20) with the job's
working directory and open files on each of these:

| Mount | Restore after a reboot |
|---|---|
| separate local filesystem (ext4 on a loop device) mounted at a fixed path, on another device after the reboot | works |
| bind mount (`/home/<user>` from another filesystem), read-only bind mount, bind mount with private propagation; `/` with shared propagation (the systemd default) | works |
| overlayfs | works |
| `x-systemd.automount` (job started before and after the trigger; not yet triggered at restore time) | works: the restore pre-check triggers it |
| FUSE network filesystem (sshfs, mounted by root with `allow_other,default_permissions`, so that files show as root's), with the job writing and renaming its own files there, and other inode numbers after the reboot | works while the FUSE daemon is up again before the restore |
| filesystem not mounted after the reboot (missing fstab entry, NFS server down) | refused: `<path> needs <type> on <mount point>` |
| FUSE filesystem whose daemon died while jobs ran (sshfs killed: the mount stays but does not answer; then unmounted on the host), with jobs that do not use it | works: they are checkpointed without it (also on the same boot), and get an empty, inaccessible directory there until the filesystem is mounted again, then the filesystem itself |
| the same, with the job's working directory on it | not checkpointed (also not at the shutdown): `the job depends on a filesystem that is gone, does not answer or root cannot access: /rnfs (fuse.sshfs) does not answer (...); the job uses it: the working directory /rnfs` |
| FUSE filesystem that the job's owner mounted without `allow_other` (sshfs; also what AppImages mount under `/tmp`), with a job that does not use it | works: it is checkpointed without it and gets an empty, inaccessible directory there |
| the same, with the job's working directory on it | not checkpointed: `... /home/alice/pfuse (fuse.sshfs) cannot be accessed by root (a FUSE mount without `allow_other`); the job uses it: the working directory /home/alice/pfuse` |
| tmpfs mounted by the administrator (e.g. `/scratch`) with the job's files or working directory on it | refused: its contents are gone after a reboot. Use a tmpfs only for jobs that do not need to survive one. A restore on the same boot works. |

Not tested, or not supported:

- **Kernel NFS** (NFSv3/v4) was not tested: the Docker test environment cannot load the host's
  `nfs` module. NFS behaves like any other filesystem in the checks above. Mount it from
  `/etc/fstab` (`_netdev`, or `x-systemd.automount`) so that it is mounted before the helper
  starts and unmounted only after the helper has dumped the jobs at shutdown
  (`gpueue-helper.service` is ordered after `remote-fs.target`, `network.target` and
  `network-online.target`). Files that the job had open are reopened by path, so they must still
  exist on the server.
- **autofs indirect maps** (an autofs daemon mounting `/net/<host>` or `/home/<user>` on demand):
  keys mounted when the job starts are visible to it; keys first accessed later are not (the
  namespace is private). Restoring such a namespace has not been tested.
- **Removable media and `/run`:** not visible to jobs at all.
- Filesystems that cannot be mounted again at the same path after a reboot (paths with a boot ID
  or a random name) make every job that started while they were mounted unrestorable.

### Network and FUSE filesystems that stop answering

A job's namespace holds every FUSE and network filesystem that was mounted when the job started,
whether the job uses it or not, and CRIU can only save such a mount by binding the host's mount
again on restore. When one goes away while jobs run, the helper notices it before each dump: its
FUSE daemon exited (sshfs whose SSH connection dropped unmounts its mount on the host, but every
job keeps a dead copy), or it does not answer (its server is down). A FUSE or network mount
(`fuse.*`, `nfs`, `nfs4`, `cifs`, `smb3`, `9p`, `ceph`, `glusterfs`, ...) counts as dead if the host
no longer has it, or if `statfs` on it fails with a connection error (`ENOTCONN`, `EIO`, `ESTALE`,
...) or does not return within 5 s (asked in a child process, so a hung NFS server cannot hold up
the helper). A FUSE mount that refuses root is treated the same way: one mounted by a user without
`allow_other` (a user's own sshfs, an AppImage's mount under `/tmp`) answers, but only to that
user, so CRIU (which runs as root) could not bind it again on restore.

- A job that uses a dead mount (a file it has open there, its working or root directory, its
  executable, a file it has mapped, or a Unix socket bound there) is **not checkpointed**. The
  dump is refused before the job is touched (see [Checkpoint failures](#checkpoint-failures)),
  with a reason that names the mount and what the job has there, for example `the job depends on
  a filesystem that is gone, does not answer or root cannot access: /nfs (fuse.sshfs) is no
  longer mounted on the host; the job uses it: the working directory /nfs/run`. Mount the
  filesystem again (with `allow_other` if root cannot access it) and checkpoint it then. At a shutdown such a job is stopped by systemd and shown as `Checkpoint
  failed` with that reason.
- **Every other job is checkpointed without the mount**. On restore its mount point gets the
  filesystem again if a filesystem of the same type is mounted there and answers, and an empty
  directory that the job cannot open (root's, mode 0,
  `/var/lib/gpueue/checkpoints/detached-mount`) otherwise. The job never sees what lies under the
  mount point on the host. Mounts below a dead mount are left out. The restore needs the mount
  point directory itself; if it is gone (a mount point created at boot), it is refused until the
  directory exists again.

The helper logs each mount it leaves out (`journalctl -u gpueue-helper`: `job N: /nfs
(fuse.sshfs) is no longer mounted on the host; the job does not use it, so it is checkpointed
without it`).

At a shutdown, `gpueue-helper.service` is stopped (and dumps the jobs) before `remote-fs.target`,
`network-online.target` and `network.target`, so mounts from `/etc/fstab` and the network stay
until the dumps are done. FUSE daemons that run in a **login session** are another matter: an
sshfs that a user started over SSH, or the `sftp-server` that serves a local sshfs, is stopped
with its session (`session-<n>.scope`) right at the beginning of the shutdown, in parallel with
the services. A service cannot order itself against units that logind creates at each login, but
a drop-in for all session scopes can make systemd end the sessions only after the helper has
dumped the jobs:

```ini
# /etc/systemd/system/session-.scope.d/50-gpueue.conf
[Unit]
Before=gpueue-helper.service
```

Run `systemctl daemon-reload` and check a session with `systemctl show -p Before
session-<n>.scope`. A shutdown then waits for the dumps before it stops the users' processes.
Processes of the users' `systemd --user` services run in `user@<uid>.service` and are stopped at
the same time; the same line in `/etc/systemd/system/user@.service.d/50-gpueue.conf` covers them.
gpueue does not install these drop-ins. Without them, jobs that use such a mount are not
checkpointed when its daemon is gone first, and the other jobs are.

## Upgrading

How to update depends on how gpueue was installed (`gpueue doctor` shows it as "install method",
and its "update" line says when a newer release exists):

| Installed with | Update with |
| --- | --- |
| `install.sh` (or `gpueue service install` of a release) | `sudo gpueue update` |
| the `.deb` | `sudo apt update && sudo apt upgrade` (or install a newer `.deb` from a release with `sudo apt install ./gpueue_<new version>_<arch>.deb`) |
| the `.rpm` | `sudo dnf upgrade <URL of gpueue-<new version>-1.<arch>.rpm>` (no dnf repository yet) |
| built from source | `cargo build --release -p gpueue -p gpueue-helper`, then `sudo target/release/gpueue service install` |
| `gpueue service install` from a Nix build (not NixOS) | the same `nix build` and `service install` again (see [Installing with Nix](#installing-with-nix)) |
| the AUR package `gpueue-bin` (once published; until then Arch uses `install.sh` and `sudo gpueue update`) | `yay -Syu` (any AUR helper), then `sudo systemctl restart gpueue-helper.service gpueue.service`; `gpueue update` refuses |
| NixOS (`services.gpueue`) | `nix flake update gpueue && sudo nixos-rebuild switch` (see [NixOS](#nixos)); `gpueue update` refuses |
| another distribution package | the package manager (see [Install method](#install-method)); `gpueue update` refuses and names the command |

### gpueue update

```sh
gpueue update --check          # is there a newer release? (any user; without root, update only checks)
sudo gpueue update             # install the latest release
sudo gpueue update --version 0.2.0 --yes
```

`gpueue update` needs no GitHub token. Like `install.sh`, it uses one when given (it raises the API
rate limit): `GH_TOKEN` or `GITHUB_TOKEN` (`sudo GH_TOKEN=... gpueue update`), or
`/etc/gpueue/github-token` (root, 0600), which `install.sh --save-token` writes. It then:

1. checks that gpueue was installed by `gpueue service install` (not by a package, Nix or by
   hand, and that both units belong to it: a helper unit installed another way is refused
   before anything is downloaded, a missing one is written by the install) and
   that it runs as the installed binary (the directory in `gpueue.service`'s `ExecStart=`),
2. looks up the latest release (or `--version`) and compares it with its own version. Nothing
   happens when it is up to date, unless the installed `gpueue-helper` is missing or reports
   another version (an interrupted earlier update): then the release is installed again to
   repair the pair. An older version is installed only when asked for with `--version`,
3. asks for confirmation (`-y/--yes` skips it),
4. downloads the tarball, `.sha256` and `.minisig` into `/var/lib/gpueue/update` (root, 0700),
   checks the minisign signature with the release key built into `gpueue` (its trusted comment
   must be `gpueue <tag> <file>` of that release, as with `install.sh`), and the checksum,
5. extracts the tarball (regular files and directories only) and asks the new `gpueue` which
   database schema it supports. A release that cannot open the daemon's database (an older one,
   from before a schema change) is refused before anything changes,
6. runs the new `gpueue service install` for the installed directory, removes the Nix GC root of
   a Nix-built installation (the release's binaries do not need the store), waits until the
   daemon runs the new version and is connected to the helper, and deletes the download. If
   `service install` fails, `gpueue update` looks at each installed binary: if neither is the
   release's, it stops with that error; if only one is, it stops and says to run it again
   (which repairs the pair); if both are (only the final checks failed), it warns and goes on.

`install.sh` (run again, optionally with `--version`) works too and does the same; it checks the
signature with the `minisign` in `PATH`, or downloads a pinned one (see
[Running the installer](#running-the-installer)).

`service install` (and so both ways above) replaces the binaries and units and restarts both
services if anything changed. It writes both binaries first and then puts them in place, the
helper first; if one cannot be put in place, the other is restored, so a failure (a full disk,
for example) never leaves a new `gpueue` next to an old `gpueue-helper`. The configuration file is kept.

- **Running jobs are not affected.** They run in their own systemd units
  (`gpueue-job-<id>.service`, or `gpueue-job-<id>-r<n>.service` after a restore). A new daemon picks
  up the state from its database, and a new helper finds the running jobs again.
- **Helper restart:** a plain `systemctl stop/restart gpueue-helper` (not a system shutdown) never
  checkpoints anything. The stopping helper refuses new spawns, restores and checkpoints and lets
  operations in flight finish first. A dump in progress can take a while, and a second SIGTERM
  makes it exit at once. Launches it refused or abandoned are queued again by the daemon and
  retried when the helper is back; a `checkpoint` it refused fails with "helper unavailable" and
  leaves the job running (or paused). While the helper is down, `pause`, `kill`, `checkpoint` and
  resuming a paused job fail with "helper unavailable". `add -i`, `start`, `restore -i` and
  `restart -i` are accepted, and the job is launched when the helper reconnects.
- **Versions:** the CLI and the daemon must be the same version. A mismatched client gets an
  error that says which side to update, and `gpueue doctor` warns about it. Update the `gpueue`
  binary that users run at the same time.
- Old fork guard libraries in `/var/lib/gpueue/lib` are kept on purpose: jobs checkpointed before
  the upgrade need the exact file to restore.
- Restore or remove checkpointed jobs before upgrading the NVIDIA driver. A restore with a
  different driver version is refused.

## Uninstalling

On NixOS, set `services.gpueue.enable = false` and run `sudo nixos-rebuild switch` instead (see
[Updating and removing on NixOS](#updating-and-removing-on-nixos)).

For the `.deb`, use `sudo apt remove gpueue` or `sudo apt purge gpueue` (see
[Installing the Debian/Ubuntu package](#installing-the-debianubuntu-package)); for the `.rpm`,
`sudo dnf remove gpueue` (see
[Installing the Fedora/RHEL/Rocky package](#installing-the-fedorarhelrocky-package)); for the AUR
package (once it is published), `sudo pacman -R gpueue-bin` (see
[Arch Linux (AUR)](#arch-linux-aur)). On Arch Linux for now, and for an installation made by
`install.sh` or `gpueue service install`:

```sh
sudo gpueue uninstall            # stop, disable and remove the units, snippets and copied binaries
sudo gpueue uninstall --purge    # also delete /etc/gpueue, /var/lib/gpueue, /run/gpueue, /run/gpueue-helper
```

`gpueue service uninstall` is the same command, and `install.sh --uninstall [--purge] [--yes]`
runs it. `--purge` asks for confirmation on the terminal. When there is no terminal, pass
`--yes`.

- Only files that start with the install marker are removed. Others are reported and kept.
- The binaries are removed only if `/var/lib/gpueue/install-receipt` lists them as copied by
  `gpueue service install` (only `gpueue` and `gpueue-helper` in the recorded directory). Binaries
  that were used in place, for example in `/usr/bin`, are kept. An installation made before the
  receipt existed keeps its binaries, and `uninstall` prints which to remove by hand; so does an
  unusable receipt. A listed binary that a package owns by now (asked of dpkg, rpm and pacman)
  is kept, and so is one whose owner a package manager could not tell. The Nix GC root and the
  receipt are removed as well; if a binary could not be removed, the receipt is kept listing it
  (re-run `sudo gpueue uninstall` later).
- `install.sh --uninstall` with an installed `gpueue` older than `gpueue uninstall` runs its
  `service uninstall` and then deletes `gpueue` and `gpueue-helper` from the units' directory.
- Running jobs keep running in their own units. `uninstall` warns about them, and `--purge`
  refuses to run while any `gpueue-job-*` unit is active, or when systemd cannot list them. Stop
  them first with
  `sudo systemctl stop 'gpueue-job-*'`, or checkpoint/kill them before uninstalling.
- `--purge` asks for confirmation (`-y/--yes` skips it). It does not descend into mount points
  (for example the bind mount of `/` below `/run/gpueue-helper`). Those are reported for you to
  unmount and delete by hand. A purge directory that is itself a mount point (for example
  `/var/lib/gpueue` on its own filesystem) is emptied and kept. A directory that is not empty any
  more when it is to be removed (a user's log directory a running process keeps writing to) is
  kept and reported too, and the other directories are still deleted. The deletion never follows
  symlinks, including ones users place in their log directories.
- The system user `gpueue` and the group `gpueue-admin` are always kept.

## Troubleshooting

### Where to look

```sh
journalctl -u gpueue -u gpueue-helper          # daemon and helper
journalctl -u 'gpueue-job-*'                  # job units (start/stop, OOM kills)
gpueue status; gpueue log <id>                 # job state, last error, output
sudo ls /var/lib/gpueue/checkpoints/<id>/     # dump.log, restore.log (CRIU logs, root only)
sudo ls /var/lib/gpueue/checkpoints/failed-logs/   # dump.log of failed dumps (newest 20)
```

Job output is written to `/var/lib/gpueue/logs/<uid>/<id>.log`. When a checkpoint or restore fails,
the job's last error (shown by `gpueue status` and in the `gpueue log` header to the owner and
administrators) contains a short summary of the CRIU log. After a failed dump it also names the
full log, e.g. `(CRIU log: /var/lib/gpueue/checkpoints/failed-logs/12-1760000000000.log)`. The daemon logs at `info` level. For
more detail, set `Environment=RUST_LOG=debug` in a drop-in for `gpueue.service`.

If the daemon is not running, every command fails with "cannot connect to the daemon socket". Check
`gpueue service status` and `systemctl status gpueue`. After `gpueue shutdown` the daemon stays
stopped until `sudo systemctl start gpueue` or the next boot.

### Common `gpueue doctor` failures

| Check | Message | Fix |
| --- | --- | --- |
| `config` | not owned by root or group/world-writable | `sudo chown root:root /etc/gpueue/config.toml /etc/gpueue; sudo chmod go-w /etc/gpueue/config.toml /etc/gpueue` |
| `config` | invalid configuration | fix the TOML. The daemon does not start with it. |
| `config` | not readable by others / not searchable by others (WARN) | `sudo chmod 755 /etc/gpueue; sudo chmod 644 /etc/gpueue/config.toml`: the daemon runs as `gpueue` |
| `criu` | not found, or too old | install CRIU 4.2 or newer, or set `criu_path` |
| `criu`, `cuda-checkpoint`, `gpus` | was not run as root (someone other than root could replace it, or the configuration failed the trust check) | make the program and its directories owned by root and not group- or world-writable, or fix the `config` check first |
| `cuda-checkpoint` | not found, or lacks `--action lock/checkpoint/restore/unlock` | install the binary for a driver of r570 or newer into `/usr/local/bin` |
| `cuda-checkpoint` | version X but the driver is Y | use the cuda-checkpoint that matches the driver's major version |
| `cuda plugin` | `cuda_checkpoint_mode` is `"plugin"` but `/usr/lib/criu/cuda_plugin.so` is missing | install the plugin (see [CUDA plugin mode](#cuda-plugin-mode)) or use the manual mode |
| `gpus` | nvidia-smi lists N GPU(s), the configuration M | update `[[gpus]]` (see [GPU detection](#gpu-detection)) |
| `kernel` | lacks `/proc/sys/kernel/ns_last_pid` | use a kernel with `CONFIG_CHECKPOINT_RESTORE=y` |
| `update` | gpueue X is available (INFO) | `sudo gpueue update`; `update_check = false` turns the check off |
| `install method` | mixed: two install methods at once | remove all but one, as the `fix:` lines say (e.g. `sudo gpueue uninstall` to keep the package) |
| `install method` | WARN: a unit no package owns, a leftover unit, or units installed in different ways | delete the unowned `/usr/lib` unit (`sudo rm ...`), or re-run the installation that should own both units, as the `fix:` line says |
| `gpueue.service`, `gpueue-helper.service` | not installed / not active | `sudo gpueue service install`, or `sudo systemctl start <unit>` and read `journalctl -u <unit>` |
| `helper binary` | the binary or a directory above it is not owned by root or is writable by others | `sudo chown root:root <path>; sudo chmod 755 <path>` (and its directories), or reinstall into another directory |
| `sudo PATH` | sudo's secure_path does not include /usr/local/bin (INFO, RHEL family) | `sudo /usr/local/bin/gpueue ...`, or add `/usr/local/bin` to `secure_path` with `sudo visudo` |
| `daemon write paths` | cannot write to a configured directory: not below its ReadWritePaths= | add a `ReadWritePaths=` drop-in for `gpueue.service` (see [Installing](#installing)) |
| `helper` | the daemon is not connected to the helper | `journalctl -u gpueue-helper`. Often an ownership problem with the configuration or a data directory. |
| `daemon` | runs version X while this gpueue is Y | `sudo systemctl restart gpueue-helper.service gpueue.service` after upgrading, or update the CLI |
| `checkpoint dir` | on tmpfs, or little free space | move `checkpoints_dir` to a disk with room for RAM + VRAM of your jobs |
| `helper settings` | ExecStart= passes flags | move the settings into `config.toml` and drop the flags |
| `job d-bus` | allowed (`job_system_dbus = true` or `job_systemd_varlink = true`): jobs can ask systemd and polkit for actions (WARN) | unless jobs need it, remove the key from `/etc/gpueue/config.toml` and `sudo systemctl restart gpueue-helper.service` |
| `job sockets` | a default of `job_hidden_socket_dirs` is left out (`/run/libvirt not hidden`), an entry is not a directory (every job fails to start), or nix-daemon trusts users or groups besides root while `/nix/var/nix/daemon-socket` is not hidden (WARN) | set `job_hidden_socket_dirs` to the list the hint shows (or remove the key for the defaults) and `sudo systemctl restart gpueue-helper.service` |
| `orphaned jobs` | the helper runs jobs the daemon does not account for (WARN; their GPUs stay taken) | `gpueue status` shows them to administrators; an administrator can stop one with `gpueue kill <id>` |

On NixOS (`ID=nixos` in `/etc/os-release`), the `fix:` lines name the [module options](#nixos)
instead of steps that do not apply there, each followed by `sudo nixos-rebuild switch`:
`services.gpueue.settings` instead of editing `/etc/gpueue/config.toml` (which the module
rewrites), `services.gpueue.criuPackage` and `services.gpueue.cudaCheckpointPackage` instead of
installing CRIU or cuda-checkpoint into `/usr/local/bin`, `services.gpueue.nvidiaSmiPath` and
`services.gpueue.nvmlLibrary` for nvidia-smi and NVML, `services.gpueue.enable = true` instead of
`sudo gpueue service install` or `systemctl enable`, `services.gpueue.admins` instead of `usermod`,
and `systemd.services.gpueue.serviceConfig.ReadWritePaths` instead of a `systemctl edit` drop-in.

### Checkpoint failures

The helper checks the job before dumping it. If a check fails, the dump is **refused**: the job is
not touched and keeps running, `gpueue checkpoint` fails with `checkpoint refused: <reason>; the job
keeps running`, and the reason becomes the job's last error. When the refusal only comes when the
dump is due (for example during `gpueue shutdown`, which then leaves the job running), the owner is
warned on their next command. Refusals:

- `not enough free space for the checkpoint: need N bytes, M available`: free space or move
  `checkpoints_dir`, then checkpoint again. The need includes what the job stores in its own
  `/tmp` and `/dev/shm` (the message then says how much).
- `finding CUDA processes: the helper did not find cuda-checkpoint when it started ...`: a GPU job
  cannot be checkpointed without cuda-checkpoint. Install it (see
  [cuda-checkpoint](#cuda-checkpoint)) and restart the helper. ``... process N uses an NVIDIA
  device but `cuda-checkpoint --get-state` does not know its CUDA state: ...``: cuda-checkpoint
  cannot handle that process (for example a driver/cuda-checkpoint version mismatch, see
  `gpueue doctor`), and CRIU could not dump its device mappings either.
- `the job has Unix sockets bound to [...] on the host's filesystems (or to relative names ...)`:
  a restore would make CRIU create, unlink and chown those paths as root. Bind the socket to an
  absolute path below `/tmp` or to an abstract name. A relative name counts as a host path
  because the directory it was bound in is not known.
- `the job has internet sockets with SO_REUSEPORT (...)`: CRIU recreates sockets as root before it
  gives the job its credentials back, so a restored `SO_REUSEPORT` socket would belong to root and
  could join a root service's group on the same port (and receive part of its connections). Turn
  `SO_REUSEPORT` off in the program, or run the job with `--no-checkpoint`.
- `the job depends on a filesystem that is gone, does not answer or root cannot access: <mount
  point> (<type>) ...; the job uses it: ...`: a FUSE or network filesystem that the job uses is no
  longer mounted on the host, does not answer, or is a FUSE mount without `allow_other`. Mount it
  again (restart the sshfs, bring the NFS server back; mount a user's FUSE filesystem with
  `-o allow_other`, which needs `user_allow_other` in `/etc/fuse.conf`) and checkpoint again. See
  [Network and FUSE filesystems that stop answering](#network-and-fuse-filesystems-that-stop-answering).
- `the job's mounts [...] are only reachable through mounts hidden under other mounts`: the job's
  mount namespace depends on a mount that is hidden under another one, so it could never be
  restored. Run the job again (`gpueue restart`). New jobs get a cleaned-up namespace.
- Mounts covering `/` itself cannot be handled. gpueue refuses such dumps.
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
`gpueue restore` again. The owner is warned on their next command.

- `gpueue refuses to run jobs as <user>: ...`: the job's owner or group is refused by
  `job_min_uid`, `job_denied_groups` or `job_allowed_users` (see
  [Who jobs may run as](#who-jobs-may-run-as)); `the checkpointed job ran with the groups
  ["docker"], which are in job_denied_groups` means the job was checkpointed with a group that is
  denied now. Change the setting (and restart the helper) or remove the job.

- `the checkpoint of job N was dumped but not finalized, and finalizing it failed again: ...`: the
  dump succeeded, but the helper could not write the checkpoint's metadata (see
  [Checkpoint storage](#checkpoint-storage)), typically because the disk with `checkpoints_dir`
  is full or failing. Fix that and restore again.

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
- `the mount points [...] of filesystems that were left out of the checkpoint (they were gone,
  did not answer or refused root at the dump) are missing on this host`: CRIU needs the directory to mount
  something on. Create it (or mount the filesystem there) and restore again. See
  [Network and FUSE filesystems that stop answering](#network-and-fuse-filesystems-that-stop-answering).
- `files or directories the job used are missing on this host: [...]`: files the job had open,
  or its working directory, are gone, typically because they were on a tmpfs (`/scratch`) that
  is empty after the reboot. The job cannot be restored. Remove it (`gpueue remove`).
- `checkpoint is not compatible with this host`: a different GPU model or driver version, or a
  missing GPU. Restore with the original driver and GPU. In plugin mode the GPU UUIDs must match
  as well. In manual mode a different card of the same model is remapped.
- `not enough free GPU memory`: other jobs use the GPU. Wait, or free it.
- Files the process had open, or its executables and libraries, were changed or deleted (see
  `restore.log`). Restore them at the same paths.
