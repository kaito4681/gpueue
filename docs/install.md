# Installing and operating puzuz

English | [日本語](ja/install.md) | [简体中文](zh-CN/install.md)

This guide is for the administrator of the machine. Regular users need nothing beyond the `puzuz`
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
- [GPU detection](#gpu-detection)
- [Checkpoint storage](#checkpoint-storage)
- [Upgrading](#upgrading)
- [Uninstalling](#uninstalling)
- [Troubleshooting](#troubleshooting)

## Requirements

| Component | Requirement | Tested with |
| --- | --- | --- |
| OS | Linux with systemd as init, x86_64 or aarch64 (releases are built for both) | Ubuntu (Docker image with systemd as PID 1), x86_64 |
| systemd | 240 or newer (`puzuz doctor` checks this) | 259 |
| Kernel | `CONFIG_CHECKPOINT_RESTORE=y` (`/proc/sys/kernel/ns_last_pid` must exist), PID and mount namespaces, 5.5 or newer recommended (`clone3` with `set_tid`, used by CRIU to restore PIDs), 5.2 or newer for `open_tree`/`move_mount` (used to clean up hidden mounts), 5.8 or newer to restore (`openat2`, `faccessat2` and `statx` mount ids, used to check what a restore opens) | 7.0 |
| CRIU | 4.2 or newer | 4.2 (distribution package), 4.2.1 |
| NVIDIA driver (GPU jobs) | a driver whose `cuda-checkpoint` supports `--action lock/checkpoint/restore/unlock` and `--timeout`: r570 or newer (cuda-checkpoint itself supports r550, but only with `--toggle`, which puzuz does not use) | 595 |
| cuda-checkpoint (GPU jobs) | the binary from NVIDIA, with the same major version as the driver | 595 |
| nvidia-smi (GPU jobs) | the helper uses it to find CUDA processes and GPU memory | from the driver |
| Rust (building only) | 1.88 or newer, plus a C compiler (`cc`) for the helper's fork guard library | 1.98.1 |

Without an NVIDIA driver puzuz still runs CPU-only jobs, and checkpoints them as well.

### CRIU

puzuz needs CRIU 4.2 or newer to checkpoint; jobs run without it. `puzuz doctor` reports a
missing or older CRIU. The distribution's package is new enough on Ubuntu 26.04
(`sudo apt install criu`, universe), Fedora (`sudo dnf install criu`, 4.2) and Arch
(`sudo pacman -S criu`). Ubuntu 22.04 (3.16), Ubuntu 24.04 (no package), Debian 12 (3.17),
Debian 13 (4.1) and RHEL/Rocky/Alma 9 (3.19) do not have it: build CRIU from source following
<https://criu.org/Installation> (the upstream release tarball, `make` and `sudo make install`
into `/usr/local/sbin`), or point `criu_path` in `/etc/puzuz/config.toml` at a root-owned build.
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
`bin/aarch64_Linux/` on arm64). Put it
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

`scripts/install.sh` is the usual way to install puzuz on x86_64 and aarch64 Linux (on Debian and
Ubuntu, the [package](#installing-the-debianubuntu-package) is an alternative). It is attached to every
release. It downloads a release tarball, checks it, and runs that release's
`puzuz service install`, which copies the binaries into `/usr/local/bin` (see
[Installing](#installing) for what that does).

### GitHub token (optional)

Downloads need no GitHub token. A token only raises the GitHub API rate limit for anonymous
requests (60 an hour per IP address, which a shared NAT can use up). Any token works; a
fine-grained one needs no permissions. `install.sh` uses one when it finds one, in this order:

1. the `GH_TOKEN` or `GITHUB_TOKEN` environment variable,
2. `--token-file FILE` (the first line of FILE),
3. `gh auth token`, run as the user who ran `sudo` (if the GitHub CLI is installed and logged in),
4. `/etc/puzuz/github-token`, which `--save-token` writes (owned by root, mode 0600).

The token is passed to `curl` in a header file, so it does not show up in `ps` or in the output.
A rejected token fails with a message that says so; a 404 for `--version X` means that release
does not exist.

### Running the installer

```sh
curl -fsSL https://github.com/kaito4681/puzuz/releases/latest/download/install.sh | sudo bash
```

Options go after `bash -s --` when the script comes from a pipe (for example
`... | sudo bash -s -- --version 0.2.0`), or after the file name with a downloaded copy
(`sudo bash install.sh --version 0.2.0`):

| Option | Meaning |
| --- | --- |
| `--version X` | install version X instead of the latest release |
| `--bin-dir DIR` | install somewhere else than the default (the current installation's directory, else `/usr/local/bin`) |
| `--token-file FILE` | read the token from FILE |
| `--save-token` | save the token to `/etc/puzuz/github-token` (root, 0600) for `puzuz update` and later runs |
| `--uninstall` | remove puzuz with the installed `puzuz uninstall` (see [Uninstalling](#uninstalling)) |
| `--purge` | with `--uninstall`, also delete `/etc/puzuz` and `/var/lib/puzuz` |
| `-y`, `--yes` | do not ask before purging |

A token is saved only with `--save-token`. Without it, an expired or revoked token cannot break
a later run without you noticing which token was used.

`install.sh` checks, before downloading anything, that it runs as root, that systemd is PID 1, that
the machine is x86_64 or aarch64 with glibc 2.34 or newer, and that puzuz is not installed by a
package manager or Nix (see [Install method](#install-method)). For the last check it asks an
installed puzuz (the one the unit runs, else the one in `PATH`) with
`puzuz service install-method`; without one it refuses only the obvious cases itself (NixOS by
`/etc/os-release`, a unit a package owns, a unit linked into `/nix/store`), and the downloaded
binary decides the rest before it installs anything. It then:

1. looks up the release (`/repos/kaito4681/puzuz/releases/latest`, or `/releases/tags/v<X>`),
2. downloads `puzuz-v<X>-<target>.tar.gz` (`x86_64-unknown-linux-gnu` or
   `aarch64-unknown-linux-gnu`) and its `.sha256` file into a
   temporary directory and checks the checksum,
3. if `minisign` is installed, also checks the `.minisig` signature against the release key built
   into the script (`packaging/minisign.pub`). Without `minisign` it says that the signature was
   not checked,
4. extracts the tarball (refusing links and paths outside it), runs the new
   `puzuz service install` (which keeps the directory of the current installation, else uses
   `/usr/local/bin`; `--bin-dir` is passed on only if you gave it), and prints its
   `puzuz doctor` report,
5. prints how to install CRIU and cuda-checkpoint if they are missing. It never installs them.

If only some `puzuz doctor` checks fail (for example CRIU is missing) but the services run,
`install.sh` succeeds with a warning.

Afterwards, update with `sudo puzuz update` (see [Upgrading](#upgrading)). Running `install.sh`
again also installs the latest (or the given) version over the installed one. Running jobs keep
running either way.

On Debian and Ubuntu you can use the [.deb](#installing-the-debianubuntu-package) instead, on
Fedora, RHEL, Rocky and Alma Linux the [.rpm](#installing-the-fedorarhelrocky-package), or
[Nix](#installing-with-nix) anywhere, and on Arch Linux the [AUR package](#arch-linux-aur) once it
is published (until then, use `install.sh` as above). You can also build from source as described
below.

### Verifying a release by hand

Each release has, for each target (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`),
`puzuz-v<X>-<target>.tar.gz` with `.sha256` and `.minisig` files, the packages
`puzuz_<X>_amd64.deb`, `puzuz_<X>_arm64.deb`, `puzuz-<X>-1.x86_64.rpm` and
`puzuz-<X>-1.aarch64.rpm` with the same two files each, and `install.sh`. To check a tarball (or a
package) yourself:

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

## Installing the Debian/Ubuntu package

Each release has a `.deb` for amd64 and arm64 (`dpkg --print-architecture` tells which), for
Debian 12 and Ubuntu 22.04, 24.04 and 26.04. Download `puzuz_<version>_<arch>.deb` from the
release and install it with apt, which also pulls in what it depends on:

```sh
sudo apt install ./puzuz_0.1.0_amd64.deb
```

The package contains:

| Path | |
| --- | --- |
| `/usr/bin/puzuz`, `/usr/bin/puzuz-helper` | the binaries |
| `/usr/lib/systemd/system/puzuz.service`, `puzuz-helper.service` | the units (`ExecStart=/usr/bin/...`) |
| `/usr/lib/sysusers.d/puzuz.conf`, `/usr/lib/tmpfiles.d/puzuz.conf` | the `puzuz` user, the `puzuz-admin` group, the directories |
| `/etc/puzuz/config.toml` | the default configuration (a conffile: your edits are kept on upgrades) |
| `/etc/apt/sources.list.d/puzuz.sources`, `/etc/apt/keyrings/puzuz.gpg` | the puzuz APT repository and its signing key (conffiles, see below) |
| `/usr/share/bash-completion`, `zsh/vendor-completions`, `fish/vendor_completions.d` | shell completions |
| `/usr/share/doc/puzuz` | README, this guide, the architecture notes, the license |

On the first installation it creates the user, the group and the directories, then enables and
starts `puzuz-helper.service` and `puzuz.service`; check the result with `puzuz doctor`. Without
systemd as PID 1 (a container image being built, a chroot) the services are only enabled, so they
start on the next boot. Unlike `install.sh`, the package does not add `[[gpus]]` entries to the
configuration; NVML finds the GPUs at start-up (see [GPU detection](#gpu-detection)).

It depends on systemd 240 or newer and only **recommends** `criu`: on most supported releases the
distribution's CRIU is missing or older than 4.2 (see [CRIU](#criu)). Jobs run without it, but
checkpoints need it, and `puzuz doctor` reports a missing or old CRIU. cuda-checkpoint for GPU
jobs is installed by hand as described in [cuda-checkpoint](#cuda-checkpoint).

The package refuses to install on a machine where `puzuz service install` (or `install.sh`)
installed puzuz: remove that first with `sudo puzuz uninstall` (with an older puzuz that lacks
the command, `install.sh --uninstall`). Running jobs, `/etc/puzuz` and `/var/lib/puzuz` are kept. `puzuz doctor`
and `puzuz service install-method` report the package's installation as a distribution
`package`, and `puzuz service install`, `install.sh` and `puzuz update` refuse to touch it.

**Updates.** The package registers the puzuz APT repository, a flat repository in the GitHub
release `apt` (`https://github.com/kaito4681/puzuz/releases/download/apt/`, signed with the key in
`/etc/apt/keyrings/puzuz.gpg`). So the `.deb` is installed by hand only once;
later versions come with the rest of the system:

```sh
sudo apt update && sudo apt upgrade
```

The repository has the stable releases (not pre-releases) for amd64 and arm64, the newest five
versions of each. An upgrade restarts the running services. Running jobs keep running, as with
`puzuz update` (see [Upgrading](#upgrading)). Your edits to `/etc/puzuz/config.toml` and
`/etc/apt/sources.list.d/puzuz.sources` are kept; `/etc/apt/keyrings/puzuz.gpg` is replaced by a
new version's keyring when you have not changed it (that is how a new signing key arrives). While a new release is being uploaded,
`apt update` may briefly fail with a hash mismatch or a missing file; running it again a minute
later fixes it.

**Adding the repository by hand.** Instead of installing the first `.deb` from the release page,
you can add the repository and install from it. Use these exact file names and contents (they are
what the package installs, so the package does not ask about them later):

```sh
sudo curl -fsSLo /etc/apt/keyrings/puzuz.gpg \
  https://github.com/kaito4681/puzuz/releases/download/apt/puzuz.gpg
sudo curl -fsSLo /etc/apt/sources.list.d/puzuz.sources \
  https://github.com/kaito4681/puzuz/releases/download/apt/puzuz.sources
sudo apt update && sudo apt install puzuz
```

**The repository key.** The packages, the repository and the `.rpm` files are signed with one
GPG key (`packaging/apt/puzuz-archive-keyring.gpg` in the source tree), fingerprint
`E539 7032 5A39 7378 537B C776 20B6 B72F 9F55 85C2`. To check a downloaded key before trusting it:

```console
$ gpg --show-keys /etc/apt/keyrings/puzuz.gpg
pub   rsa4096 2026-10-13 [SC]
      E53970325A397378537BC77620B6B72F9F5585C2
uid                      puzuz APT repository <https://github.com/kaito4681/puzuz>
```

**Removing.**

```sh
sudo apt remove puzuz    # stop and disable the services, remove the binaries and units
sudo apt purge puzuz     # also delete /etc/puzuz and the APT repository entry and key
```

`remove` keeps the repository entry and its key (both conffiles), so `apt update` keeps working
and `sudo apt install puzuz` brings puzuz back. Both keep `/var/lib/puzuz` (state, job logs, checkpoint images), the `puzuz` user and the
`puzuz-admin` group, and never touch running jobs: they keep running in their own
`puzuz-job-*` units. `purge` prints how to delete `/var/lib/puzuz`
(`sudo systemctl stop 'puzuz-job-*'` first if jobs still run, then `sudo rm -rf /var/lib/puzuz`).

### The APT repository (maintainers)

The repository is the set of assets of the release `apt` (a pre-release, so it never becomes the
"latest" release that `install.sh` and `puzuz update` follow): the `.deb` files, `Packages`,
`Packages.gz`, `Release`, `InRelease` and `Release.gpg`, the public key `puzuz.gpg` and
`puzuz.sources`. `.github/workflows/release-apt.yml` updates it with
`scripts/dist/apt-repo.sh github <tag>` when a release is **published as a stable release** (the
`released` event: publishing a stable release, or turning a pre-release into one): it adds the release's
`.deb` files (after checking their checksums and their minisign signatures, with the public key
of the tag's commit), deletes all but the newest
five versions per architecture (in Debian version order), regenerates and signs the index (and
copies `puzuz.gpg` and `puzuz.sources` out of the newest package it keeps), and uploads the
packages first and the index last, `InRelease` at the very end; old `.deb` files are deleted after
that. It creates the `apt` release when it is missing. `apt-repo.sh` decides what is published:
drafts, pre-releases and tags other than `v*` are skipped with a notice. The index has no
`by-hash` files (they need a subdirectory, and release asset names cannot contain `/`), hence the
short window described above.

`release.yml` only creates drafts, so the repository is updated when a person publishes the draft
on GitHub (or a tool does so with a personal access token). A release published with a workflow's
`GITHUB_TOKEN` does not start other workflows; then run it by hand, which is also how to retry a
failed upload:

```sh
gh workflow run release-apt.yml -R kaito4681/puzuz -f tag=v0.1.0
```

To try it without a real release, use a fork: create a key there with
`scripts/dist/gen-apt-key.sh` (and the minisign key, for `release.yml`), commit the public keys,
set the secrets, push a tag, publish the draft, and run `apt update` against the fork's `apt`
release. Or run the local test, `scripts/apt-repo-test.sh` (`PUZUZ_APT_BASES="debian:12
ubuntu:22.04 ubuntu:24.04 ubuntu:26.04"`), which builds a repository with
`apt-repo.sh update DIR DEB...` and a throwaway key and upgrades through it in containers.

**The archive key.** `packaging/apt/puzuz-archive-keyring.gpg` is the real key, fingerprint
`E539 7032 5A39 7378 537B C776 20B6 B72F 9F55 85C2` (see "The repository key" in
[Installing the Debian/Ubuntu package](#installing-the-debianubuntu-package)); its secret key and
passphrase are the repository secrets `APT_GPG_PRIVATE_KEY` and `APT_GPG_PASSPHRASE`.
`release.yml` and `apt-repo.sh` refuse a keyring whose user ID says "TEST KEY" (written by
`scripts/dist/gen-apt-key.sh --test`). A key is created with:

```sh
scripts/dist/gen-apt-key.sh     # asks for a passphrase; writes the secret key under ~
gh secret set APT_GPG_PRIVATE_KEY -R kaito4681/puzuz < ~/puzuz-apt-key-*/apt-signing-key.asc
gh secret set APT_GPG_PASSPHRASE -R kaito4681/puzuz
# then commit packaging/apt/puzuz-archive-keyring.gpg
```

Without the two secrets, `release-apt.yml` skips the repository (with a notice). The index is
signed only with a key that the newest package in the repository ships (`apt-repo.sh` reads
`/etc/apt/keyrings/puzuz.gpg` out of the `.deb`), so a mismatch between the secret and the committed public key fails
the run instead of breaking `apt update` for everyone.

Back up the secret key and its passphrase. Every installed package trusts this key, so a new key
can only be rolled out by a package version that ships it, through the repository signed with the
old key:

1. Create the new key (`scripts/dist/gen-apt-key.sh` in a scratch checkout) and append its public
   key to `packaging/apt/puzuz-archive-keyring.gpg`, so the keyring holds both keys
   (`cat old.gpg new.gpg >packaging/apt/puzuz-archive-keyring.gpg`). Keep the secrets on the old
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
RHEL-compatible 9 or newer (RHEL, Rocky Linux, AlmaLinux; glibc 2.34 or newer). Install it with
dnf, from a downloaded file or straight from the release URL; dnf pulls in what it depends on:

```sh
sudo dnf install ./puzuz-0.1.0-1.x86_64.rpm
# or
sudo dnf install https://github.com/kaito4681/puzuz/releases/download/v0.1.0/puzuz-0.1.0-1.x86_64.rpm
```

(A pre-release such as `v0.2.0-rc.1` is `puzuz-0.2.0-rc.1-1.<arch>.rpm`; inside the package its
version is `0.2.0~rc.1`, which sorts before `0.2.0`.)

It installs the same files as the `.deb` (binaries, units in `/usr/lib/systemd/system`,
sysusers.d and tmpfiles.d entries, `/etc/puzuz/config.toml`, shell completions, documentation,
and the license in `/usr/share/licenses/puzuz`), but no repository configuration. The
configuration is `%config(noreplace)`: your edits are kept on upgrades. The first installation
sets up and starts the services exactly like the `.deb` (only enables them without systemd as
PID 1), and refuses to install next to an installation made by `puzuz service install` or
`install.sh`. It requires systemd and glibc 2.34 and only **recommends** `criu` (a weak
dependency, which dnf installs by default when the distribution has it): Fedora's CRIU (4.2) is
new enough, the 3.19 of RHEL/Rocky/Alma 9 is not, so checkpointing there needs a newer CRIU (see
[CRIU](#criu)). Running jobs works without it.

**Verifying.** Like every release asset, the `.rpm` has `.sha256` and `.minisig` files (see
[Verifying a release by hand](#verifying-a-release-by-hand)). Releases built with the GPG key also
sign the `.rpm` itself, and attach that key as `RPM-GPG-KEY-puzuz` (the APT repository's key):

```sh
curl -fsSLO https://github.com/kaito4681/puzuz/releases/download/v0.1.0/RPM-GPG-KEY-puzuz
gpg --show-keys RPM-GPG-KEY-puzuz                   # E53970325A397378537BC77620B6B72F9F5585C2
sudo rpm --import RPM-GPG-KEY-puzuz
rpm -K puzuz-0.1.0-1.x86_64.rpm                     # "digests signatures OK"
# or let dnf refuse an unsigned or badly signed file:
sudo dnf install --setopt=localpkg_gpgcheck=1 ./puzuz-0.1.0-1.x86_64.rpm
```

**Updates.** There is no dnf repository yet (it is planned, see
[issue #23](https://github.com/kaito4681/puzuz/issues/23)), so `dnf upgrade` alone does not see new
versions. Install the newer `.rpm` of the release instead; dnf takes the URL directly:

```sh
sudo dnf upgrade https://github.com/kaito4681/puzuz/releases/download/v<new version>/puzuz-<new version>-1.x86_64.rpm
```

When a newer release exists, the "update" line of `puzuz doctor` (and `puzuz update --check`)
prints this command with the release's URL for the machine's architecture, and how to check the
signature first. An upgrade restarts the running services. Running jobs keep running.

**Removing.**

```sh
sudo dnf remove puzuz
```

It stops and disables the services and removes the binaries and units. It keeps `/var/lib/puzuz`
(state, job logs, checkpoint images), the `puzuz` user and the `puzuz-admin` group, and never
touches running jobs. rpm has no purge: an edited `config.toml` is kept as
`/etc/puzuz/config.toml.rpmsave`; delete `/etc/puzuz` and `/var/lib/puzuz` by hand if you no
longer need them (`sudo systemctl stop 'puzuz-job-*'` first if jobs still run).

## Installing with Nix

The repository is a Nix flake (`flake.nix`). It builds puzuz from source for x86_64 and aarch64
Linux; nothing is downloaded from the GitHub releases. Its outputs:

| Output | Contents |
| --- | --- |
| `packages.<system>.default` (also `puzuz`) | `bin/puzuz`, `bin/puzuz-helper`, bash/zsh/fish completions, the docs |
| `packages.<system>.criu` | CRIU 4.2.1 (nixpkgs has an older version), for the NixOS module |
| `devShells.<system>.default` | a development shell with the Rust toolchain (`nix develop`) |
| `nixosModules.default` | the NixOS module `services.puzuz` (see [NixOS](#nixos)) |
| `homeManagerModules.default` | the home-manager module `programs.puzuz` (see [home-manager](#home-manager)) |
| `packages.<system>.helper-tests` | the puzuz-helper test executables (built, not run), for the `helper-tests` check |
| `checks.<system>.*` | `home-manager` (module evaluation) and, on x86_64, two NixOS VM tests that need KVM: `nixos` (the module) and `helper-tests` (every puzuz-helper test, including those the sandboxed package build skips) |

Use `github:kaito4681/puzuz/stable` to follow releases: the `stable` branch moves to every
release tag (not to pre-releases). Plain `github:kaito4681/puzuz` is the development head
(`main`).

### NixOS

The flake's NixOS module (`nixosModules.default`, file `nix/nixos-module.nix`) sets up the
daemon, the root helper, the CLI, the `puzuz` system user, the `puzuz-admin` group,
`/etc/puzuz/config.toml` and the directories below `/var/lib/puzuz`. Do not run
`puzuz service install` or `puzuz update` on NixOS; both refuse to (NixOS is recognized by
`ID=nixos` in `/etc/os-release`, see [Install method](#install-method)).

Add the flake to the system's flake (the `stable` branch follows the latest release) and enable
the service:

```nix
# flake.nix
{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  inputs.puzuz.url = "github:kaito4681/puzuz/stable";
  inputs.puzuz.inputs.nixpkgs.follows = "nixpkgs";

  outputs = { nixpkgs, puzuz, ... }: {
    nixosConfigurations.gpu-server = nixpkgs.lib.nixosSystem {
      modules = [
        puzuz.nixosModules.default
        ./configuration.nix
      ];
    };
  };
}
```

```nix
# configuration.nix
{
  services.puzuz = {
    enable = true;
    admins = [ "alice" ];
    settings = {
      grace_secs = 60;
      checkpoint_size_limit_bytes = 500000000000;
    };
  };
}
```

Then `sudo nixos-rebuild switch`. `puzuz doctor` checks the result.

| Option | Default | Meaning |
| --- | --- | --- |
| `services.puzuz.enable` | `false` | Install and start puzuz. |
| `services.puzuz.package` | the flake's `puzuz` | The package that runs the daemon (`bin/puzuz`) and the helper (`bin/puzuz-helper`); also added to `environment.systemPackages`. |
| `services.puzuz.admins` | `[ ]` | Users added to the `puzuz-admin` group (see [Administrators](#administrators)). |
| `services.puzuz.settings` | `{ }` | The contents of `/etc/puzuz/config.toml` as an attribute set (see [Configuration](#configuration) for the keys). |
| `services.puzuz.criuPackage` | the flake's `criu` (4.2.1) | CRIU for the helper; sets `criu_path`. nixpkgs' own `criu` is older than 4.2. |
| `services.puzuz.cudaCheckpointPackage` | `null` | A package with `bin/cuda-checkpoint`; sets `cuda_checkpoint_path`. With `null`, GPU jobs cannot be checkpointed. |
| `services.puzuz.nvidiaSmiPath` | `nvidia-smi` of `hardware.nvidia.package` when `hardware.nvidia.enabled`, else `null` | Sets `nvidia_smi_path` (the helper finds a job's CUDA processes with it). |
| `services.puzuz.nvmlLibrary` | `libnvidia-ml.so.1` of `hardware.nvidia.package` when `hardware.nvidia.enabled`, else `null` | Sets `nvml_library`, which the daemon loads to detect the GPUs. |

The module sets `criu_path`, `cuda_checkpoint_path`, `nvidia_smi_path`, `nvml_library` and
`shell` with `lib.mkDefault`, so a value in `settings` wins. Notes for NixOS:

- **Units.** `puzuz.service` and `puzuz-helper.service` are the files in `packaging/systemd` of
  the module's flake revision, with `ExecStart=` pointing into the store, and the tmpfiles.d
  entries are `packaging/tmpfiles.d/puzuz.conf`. If you set `package` to another revision, the
  units still come from the module's revision; keep the two together.
- **Configuration file.** The helper runs as root and only reads a regular file that nobody but
  root can change, never a symlink. The module therefore has NixOS copy the file into
  `/etc/puzuz/config.toml` (mode 0644, owned by root) instead of linking it into the store.
- **Restarts on `nixos-rebuild switch`.** A change to `settings` (or to the package) restarts
  both the daemon and the helper. Running jobs are not stopped: they run in their own units. The
  restarting helper first refuses new launches and lets the operations in flight finish, so a
  switch during a checkpoint or restore waits for it (up to the helper's `TimeoutStopSec=`,
  30 minutes). Jobs the daemon tries to start or restore in that window are put back in the
  queue, not failed, and start once the new helper is up.
- **Updates and Nix garbage collection.** A job's unit runs the helper binary that started it,
  and its checkpoint image contains that binary too (restoring needs it). After an update, the
  old store path is no longer part of the system, so the helper keeps it alive with a GC root,
  `/nix/var/nix/gcroots/puzuz/job-<id>`, for as long as the job runs or its checkpoint image is
  kept. The root is removed when the job ends or its checkpoint is deleted. `nix-collect-garbage`
  then frees the old version. A job started, or a checkpoint taken, by a helper older than this
  feature has no root: restoring it after the old version was collected fails, so restore or
  finish such jobs before collecting garbage.
- **The job environment.** A job runs with the environment of the `puzuz add` that submitted it,
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
- **Root-owned paths.** Programs in `/nix/store` pass the helper's and `puzuz doctor`'s ownership
  checks (see [cuda-checkpoint](#cuda-checkpoint)). In a VM without a GPU, `puzuz doctor` reports
  no failure: the install method is "Nix", and only the NVIDIA driver and cuda-checkpoint checks
  warn (the latter's hint names `services.puzuz.cudaCheckpointPackage`).

#### cuda-checkpoint on NixOS

cuda-checkpoint is not in nixpkgs and puzuz does not package it (it is NVIDIA's binary). A
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
services.puzuz.cudaCheckpointPackage = pkgs.callPackage ./cuda-checkpoint.nix { };
```

(`lib.licenses.unfree` needs `nixpkgs.config.allowUnfree = true` or an
`allowUnfreePredicate`.)

#### Updating and removing on NixOS

- Update: `nix flake update puzuz && sudo nixos-rebuild switch`. The changed units restart the
  daemon and the helper (see "Restarts on `nixos-rebuild switch`" above). Running jobs keep
  running on the old helper binary, and checkpoints taken before the update can be restored,
  also after `nix-collect-garbage` (see "Updates and Nix garbage collection").
- Roll back: `sudo nixos-rebuild switch --rollback` (or boot an older generation) brings back the
  older binaries, but not the older database: after an update that migrated the database, the
  older daemon refuses to start with the newer database.
- Remove: set `services.puzuz.enable = false` and run `sudo nixos-rebuild switch`. The units,
  `/etc/puzuz/config.toml` and the CLI go away; `/var/lib/puzuz` (state, logs, checkpoint
  images) and the `puzuz` user's files stay. Kill or finish running jobs first: the helper
  stops, but job units that are still running are left alone.

### home-manager

home-manager manages one user's environment, so it cannot install the daemon or the helper
(they need root and the system's systemd). Its module (`homeManagerModules.default`) sets up the
CLI's shell completions for a machine where an administrator installed puzuz
(`homeManagerModules.default`, file `nix/home-manager-module.nix`):

```nix
{
  imports = [ inputs.puzuz.homeManagerModules.default ];
  programs.puzuz = {
    enable = true;
    # package = null;  # the default: use the system's puzuz
  };
}
```

| Option | Default | Meaning |
| --- | --- | --- |
| `programs.puzuz.enable` | `false` | Enable the module. |
| `programs.puzuz.package` | `null` | `null`: install nothing, and load the completions of the system's `puzuz` (the one in `PATH`) when the shell starts: the completion files installed next to it (`<prefix>/share/...`, as on NixOS and in the packages), or, when there are none, the output of `puzuz completions <shell>`. A package (e.g. `inputs.puzuz.packages.${pkgs.stdenv.hostPlatform.system}.default`) is installed into the profile with its completion files. |
| `programs.puzuz.enableBashIntegration` | `true` | Load the completions in Bash (`programs.bash.enable`), with `package = null`. |
| `programs.puzuz.enableZshIntegration` | `true` | The same for Zsh (`programs.zsh.enable` with `programs.zsh.enableCompletion`, which runs `compinit`; home-manager 25.05 or newer). |
| `programs.puzuz.enableFishIntegration` | `true` | The same for Fish (`programs.fish.enable`). |

The CLI and the daemon must have the same version. Prefer `package = null`: the system's
`puzuz` always matches the daemon. A `puzuz` from home-manager (or `nix profile`) comes from the
revision your home-manager flake locks, so it can differ; commands then fail with a version
mismatch that names the store path and, on NixOS or for a home-manager profile, suggests
`programs.puzuz.package = null`. Update with
`nix flake update && home-manager switch`; the daemon is the administrator's to update.

### Other distributions

Build the flake, then install the service from the result like a tarball:

```sh
nix build github:kaito4681/puzuz/stable
sudo ./result/bin/puzuz service install
```

`service install` copies the binaries to `/usr/local/bin` and, because they still load their
libraries from the Nix store, registers the build as a garbage-collector root
(`/nix/var/nix/gcroots/puzuz`; see [Installing](#installing)). If `nix-store` is not in root's
`PATH` (sudo's `secure_path` often leaves out the Nix profile), install warns and prints the
command to register it yourself. With the root registered you can delete `./result`. The units run the copies in `/usr/local/bin`, so the install method is
`service-install`, not `nix` (which means units that resolve into `/nix/store`, such as the NixOS
module's). Install CRIU as described in [CRIU](#criu).

To update, run the same two commands again (Nix fetches the current `stable`; add `--refresh` if
it was fetched less than an hour ago); running jobs keep running. `sudo puzuz update` works too,
but replaces the Nix build with the release tarball's binaries. Remove puzuz with
`sudo puzuz uninstall` (see [Uninstalling](#uninstalling)), which also removes the GC root.

`nix profile install github:kaito4681/puzuz/stable` gives a user the `puzuz` command (with
completions) only (see also [home-manager](#home-manager)). The helper runs as root and cannot run from a user profile, so the service
must be installed as above.

## Arch Linux (AUR)

**The AUR package is not published yet.** Until it is, install puzuz on Arch Linux with
`install.sh` (it works on Arch: glibc 2.34 or newer, systemd, x86_64 or aarch64, and it verifies
the minisign signature). CRIU 4.2 or newer comes from the official repositories:

```sh
sudo pacman -S criu
curl -fsSL https://github.com/kaito4681/puzuz/releases/latest/download/install.sh | sudo bash
```

Update with `sudo puzuz update` and remove with `sudo puzuz uninstall` (see
[Upgrading](#upgrading) and [Uninstalling](#uninstalling)). The rest of this section describes
the AUR package for once it is on the AUR (the maintainer setup is below).

The AUR package [`puzuz-bin`](https://aur.archlinux.org/packages/puzuz-bin) (x86_64 and aarch64)
repackages the release tarballs. Once it is published, install it with any AUR helper, for
example yay:

```sh
yay -S puzuz-bin
```

or by hand:

```sh
git clone https://aur.archlinux.org/puzuz-bin.git
cd puzuz-bin
makepkg -si
```

It installs `/usr/bin/puzuz` and `/usr/bin/puzuz-helper`, the units in
`/usr/lib/systemd/system`, the `sysusers.d` and `tmpfiles.d` snippets in `/usr/lib`, the default
`/etc/puzuz/config.toml` (kept on upgrades and removal when you changed it), and completions for
bash, zsh and fish. pacman's systemd hooks create the `puzuz` user, the `puzuz-admin` group and
the directories. As usual on Arch, the services are not enabled or started for you:

```sh
sudo systemctl enable --now puzuz-helper.service puzuz.service
puzuz doctor
```

Checkpoints need CRIU 4.2 or newer, which is in the official repositories
(`sudo pacman -S criu`). GPU jobs also need NVIDIA's cuda-checkpoint (see
[cuda-checkpoint](#cuda-checkpoint)). Both are optional dependencies of the package.

- **Update:** `yay -Syu` (or rebuild with `git pull && makepkg -si`). The package does not
  restart the services: run `sudo systemctl restart puzuz-helper.service puzuz.service`
  afterwards, so that the daemon and the `puzuz` command are the same version again. Running jobs
  keep running (see [Upgrading](#upgrading)). `puzuz update` and `puzuz service install` refuse a
  package installation.
- **Remove:** `sudo systemctl disable --now puzuz.service puzuz-helper.service`, then
  `sudo pacman -R puzuz-bin`. Jobs still running in `puzuz-job-*` units keep running until you
  stop them. `/var/lib/puzuz` (state, logs, checkpoint images) and a changed
  `/etc/puzuz/config.toml` (saved as `config.toml.pacsave`) are kept; delete them by hand to
  remove everything.

### The AUR package (maintainers)

The PKGBUILD lives in `packaging/aur/puzuz-bin`. When a release is published (not a prerelease),
`.github/workflows/release-aur.yml` runs `scripts/dist/update-aur.sh <version>`, which takes the
tarballs' checksums from the release's `.sha256` files, computes the checksum of the tag's source
archive (for the units and snippets), and regenerates `.SRCINFO` in an Arch Linux container.
It then pushes `PKGBUILD`, `.SRCINFO` and `puzuz-bin.install` to
`ssh://aur@aur.archlinux.org/puzuz-bin.git`. The release must have both the x86_64 and the aarch64
tarball. To change the packaging without a new release, bump `pkgrel` in the PKGBUILD and run the
workflow by hand (`gh workflow run release-aur.yml -f version=<X.Y.Z>`).

One-time setup:

1. Create an account on <https://aur.archlinux.org>.
2. Create a dedicated SSH key and add its public half to the account ("My Account", "SSH Public
   Key"):

   ```sh
   ssh-keygen -t ed25519 -N '' -C puzuz-aur -f aur_puzuz
   ```

3. Store the private half as the repository secret `AUR_SSH_PRIVATE_KEY`, then delete the local
   copy (or keep it somewhere safe):

   ```sh
   gh secret set AUR_SSH_PRIVATE_KEY -R kaito4681/puzuz < aur_puzuz
   ```

The first push creates the package on the AUR. Without the secret the workflow only prints a
notice. To test the package locally (makepkg, pacman, the services, a job, an upgrade and
removal in an Arch Linux container with systemd): `scripts/dist/aur-test.sh`.

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

Tagged versions also come as prebuilt binaries for x86_64 and aarch64 Linux (built natively on
Debian bookworm, glibc 2.34 or newer) on the GitHub releases page:
`puzuz-v<version>-<target>.tar.gz` (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`)
holds `puzuz`, `puzuz-helper` and the documentation, with `.sha256` and `.minisig` files next to
it. `install.sh` installs them (see above). By hand:

```sh
sha256sum -c puzuz-v0.1.0-x86_64-unknown-linux-gnu.tar.gz.sha256
tar xzf puzuz-v0.1.0-x86_64-unknown-linux-gnu.tar.gz
sudo puzuz-v0.1.0-x86_64-unknown-linux-gnu/puzuz service install
```

## Installing

```sh
sudo target/release/puzuz service install
```

`puzuz-helper` must sit next to `puzuz`. The units run the binaries from:

1. the directory of the current `service install` installation, when re-installing or updating,
2. else the directory the `puzuz` binary is in, if that is `/usr/bin` or `/usr/local/bin`
   (used in place, nothing is copied),
3. else `/usr/local/bin`, where `puzuz` and `puzuz-helper` are copied (it says so). That is the
   usual place for software the administrator installs: package managers leave it alone, and it
   is in systemd's and every user's default `PATH`.

`--bin-dir DIR` installs somewhere else. Because `puzuz-helper` runs as root, the directory and
every parent must be owned by root and not group- or world-writable (directories that do not
exist yet are created with mode 0755); install refuses anything else. `--no-start` enables the
services without starting or restarting them. `--force` overwrites unit, sysusers.d and
tmpfiles.d files that `puzuz service install` did not write (see step 2).

On RHEL, CentOS, Rocky, Alma and Fedora, sudo's `secure_path` does not include `/usr/local/bin`,
so `sudo puzuz ...` says "command not found". Run `sudo /usr/local/bin/puzuz ...`, or add
`/usr/local/bin` to `secure_path` with `sudo visudo`; `puzuz doctor` (run as root) points this
out. Scripts with a minimal `PATH` (such as cron's `/usr/bin:/bin`) need the full path too.

Built with Nix outside NixOS (`nix build`, then `sudo ./result/bin/puzuz service install`), the
binaries are copied to `/usr/local/bin` like any other build. They still load their libraries
from the Nix store, so install also registers the store path as a garbage-collector root,
`/nix/var/nix/gcroots/puzuz` (with `nix-store --add-root /nix/var/nix/gcroots/puzuz --realise
<store path>`). If `nix-store` is not found or fails, install warns and prints that command, and
a root recorded earlier is kept. Whether the root is still needed is decided from the installed
binaries themselves (their dynamic loader is in `/nix/store`), so moving a Nix-built copy with
`--bin-dir` keeps it. `puzuz uninstall` removes the root, and so does installing binaries that
do not use the store (for example `puzuz update`).

`puzuz service install` exits non-zero when the final `puzuz doctor` report has a FAIL. After
`--no-start`, the failures that only mean "not running yet" (both units, both sockets, the
daemon and the helper connection) are ignored.

`puzuz service install` is safe to run again (for example after an upgrade). It does the
following:

1. Copies `puzuz` and `puzuz-helper` into the directory chosen above unless they are already
   there (mode 0755, replaced atomically), registers the Nix GC root if they come from the
   store, and writes `/var/lib/puzuz/install-receipt` (root, 0644). The receipt records the
   directory, the binaries it copied (none when they are used in place), the version and the GC
   root, so that `puzuz uninstall` removes exactly those. A re-install into another directory
   (with `--bin-dir`) removes the copies the previous one made. If the receipt exists but cannot
   be used (not owned by root, writable by others, or not valid), install stops without
   changing anything: fix the file, or delete it after checking what it lists.
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
| distribution package | `/usr/lib/systemd/system/<unit>` (or `/lib/systemd/system/...`) that `dpkg-query -S`, `rpm -qf` or `pacman -Qo` names an owner for (asked under both paths, since dpkg may have registered the `/lib` one on merged-`/usr` systems; the first tool with a definite answer decides). If no tool can answer (none installed, or each fails, e.g. with a locked database), the unit counts as a package of unknown origin |
| unowned unit | such a unit that a package manager definitely reports as not owned (`no path found matching pattern`, `is not owned by any package`, `No package owns`): left over from a removed package, or copied there by hand (`make install`) |
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

| Install method | `service install` | `uninstall` | `update` | `doctor` | keyword |
| --- | --- | --- | --- | --- | --- |
| not installed | installs | says so | refuses: use `install.sh` | INFO | `none` |
| not installed, NixOS | refuses: use the `services.puzuz` module | says so | refuses: use the module | INFO "not installed (NixOS: use the services.puzuz module)" | `none` |
| `puzuz service install` | re-installs | uninstalls | updates | OK | `service-install` |
| distribution package (dpkg, rpm, pacman, or unknown) | refuses, even with `--force` | refuses | refuses | OK (INFO when masked) | `package` |
| unowned unit | only with `--force`; delete the file first instead | refuses | refuses | WARN | `unowned` |
| Nix | refuses, even with `--force` | refuses | refuses | OK | `nix` |
| by hand | only with `--force` | refuses | refuses | INFO | `manual` |
| mixed | refuses: uninstall the `service install` copy first | removes only what `service install` wrote (no `--purge`) | refuses | FAIL | `mixed` |

"Mixed" is `service install` together with a package or Nix, on one unit or across
`puzuz.service` and `puzuz-helper.service` (e.g. the daemon unit from `service install`, the
helper unit from a package): `doctor` fails, `service status` reports it as unhealthy, and the
keyword is `mixed`. A `service install` next to an unowned unit in `/usr/lib` stays a
`service install` (its units in `/etc` win), and re-running `service install` needs no
`--force`; `doctor` warns and tells you to delete the leftover. When the two units are installed
in other different ways (for example, one of them is missing, or they come from different
packages of different package managers), `doctor` warns and `service status` says so;
`service install` installs both and so fixes it. `service install` also refuses when `/etc/systemd/system` itself
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
| `puzuz service install` | `sudo puzuz update` | `sudo puzuz uninstall` |

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
check is at most INFO, and so is not being able to reach GitHub. It does not run `criu check`. Run `sudo criu check` yourself to see what CRIU
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
| the `.deb` | `sudo apt update && sudo apt upgrade` (or install a newer `.deb` from a release with `sudo apt install ./puzuz_<new version>_<arch>.deb`) |
| the `.rpm` | `sudo dnf upgrade <URL of puzuz-<new version>-1.<arch>.rpm>` (no dnf repository yet) |
| built from source | `cargo build --release -p puzuz -p puzuz-helper`, then `sudo target/release/puzuz service install` |
| `puzuz service install` from a Nix build (not NixOS) | the same `nix build` and `service install` again (see [Installing with Nix](#installing-with-nix)) |
| the AUR package `puzuz-bin` (once published; until then Arch uses `install.sh` and `sudo puzuz update`) | `yay -Syu` (any AUR helper), then `sudo systemctl restart puzuz-helper.service puzuz.service`; `puzuz update` refuses |
| NixOS (`services.puzuz`) | `nix flake update puzuz && sudo nixos-rebuild switch` (see [NixOS](#nixos)); `puzuz update` refuses |
| another distribution package | the package manager (see [Install method](#install-method)); `puzuz update` refuses and names the command |

### puzuz update

```sh
puzuz update --check          # is there a newer release? (any user; without root, update only checks)
sudo puzuz update             # install the latest release
sudo puzuz update --version 0.2.0 --yes
```

`puzuz update` needs no GitHub token. Like `install.sh`, it uses one when given (it raises the API
rate limit): `GH_TOKEN` or `GITHUB_TOKEN` (`sudo GH_TOKEN=... puzuz update`), or
`/etc/puzuz/github-token` (root, 0600), which `install.sh --save-token` writes. It then:

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
6. runs the new `puzuz service install` for the installed directory, removes the Nix GC root of
   a Nix-built installation (the release's binaries do not need the store), waits until the
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

On NixOS, set `services.puzuz.enable = false` and run `sudo nixos-rebuild switch` instead (see
[Updating and removing on NixOS](#updating-and-removing-on-nixos)).

For the `.deb`, use `sudo apt remove puzuz` or `sudo apt purge puzuz` (see
[Installing the Debian/Ubuntu package](#installing-the-debianubuntu-package)); for the `.rpm`,
`sudo dnf remove puzuz` (see
[Installing the Fedora/RHEL/Rocky package](#installing-the-fedorarhelrocky-package)); for the AUR
package (once it is published), `sudo pacman -R puzuz-bin` (see
[Arch Linux (AUR)](#arch-linux-aur)). On Arch Linux for now, and for an installation made by
`install.sh` or `puzuz service install`:

```sh
sudo puzuz uninstall            # stop, disable and remove the units, snippets and copied binaries
sudo puzuz uninstall --purge    # also delete /etc/puzuz, /var/lib/puzuz, /run/puzuz, /run/puzuz-helper
```

`puzuz service uninstall` is the same command, and `install.sh --uninstall [--purge] [--yes]`
runs it. `--purge` asks for confirmation on the terminal. When there is no terminal, pass
`--yes`.

- Only files that start with the install marker are removed. Others are reported and kept.
- The binaries are removed only if `/var/lib/puzuz/install-receipt` lists them as copied by
  `puzuz service install` (only `puzuz` and `puzuz-helper` in the recorded directory). Binaries
  that were used in place, for example in `/usr/bin`, are kept. An installation made before the
  receipt existed keeps its binaries, and `uninstall` prints which to remove by hand; so does an
  unusable receipt. A listed binary that a package owns by now (asked of dpkg, rpm and pacman)
  is kept, and so is one whose owner a package manager could not tell. The Nix GC root and the
  receipt are removed as well; if a binary could not be removed, the receipt is kept listing it
  (re-run `sudo puzuz uninstall` later).
- `install.sh --uninstall` with an installed `puzuz` older than `puzuz uninstall` runs its
  `service uninstall` and then deletes `puzuz` and `puzuz-helper` from the units' directory.
- Running jobs keep running in their own units. `uninstall` warns about them, and `--purge`
  refuses to run while any `puzuz-job-*` unit is active. Stop them first with
  `sudo systemctl stop 'puzuz-job-*'`, or checkpoint/kill them before uninstalling.
- `--purge` asks for confirmation (`-y/--yes` skips it). It does not descend into mount points
  (for example the bind mount of `/` below `/run/puzuz-helper`). Those are reported for you to
  unmount and delete by hand.
- The system user `puzuz` and the group `puzuz-admin` are always kept.

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
| `install method` | mixed: two install methods at once | remove all but one, as the `fix:` lines say (e.g. `sudo puzuz uninstall` to keep the package) |
| `install method` | WARN: a unit no package owns, a leftover unit, or units installed in different ways | delete the unowned `/usr/lib` unit (`sudo rm ...`), or re-run the installation that should own both units, as the `fix:` line says |
| `puzuz.service`, `puzuz-helper.service` | not installed / not active | `sudo puzuz service install`, or `sudo systemctl start <unit>` and read `journalctl -u <unit>` |
| `helper binary` | the binary or a directory above it is not owned by root or is writable by others | `sudo chown root:root <path>; sudo chmod 755 <path>` (and its directories), or reinstall into another directory |
| `sudo PATH` | sudo's secure_path does not include /usr/local/bin (INFO, RHEL family) | `sudo /usr/local/bin/puzuz ...`, or add `/usr/local/bin` to `secure_path` with `sudo visudo` |
| `daemon write paths` | cannot write to a configured directory: not below its ReadWritePaths= | add a `ReadWritePaths=` drop-in for `puzuz.service` (see [Installing](#installing)) |
| `helper` | the daemon is not connected to the helper | `journalctl -u puzuz-helper`. Often an ownership problem with the configuration or a data directory. |
| `daemon` | runs version X while this puzuz is Y | `sudo systemctl restart puzuz-helper.service puzuz.service` after upgrading, or update the CLI |
| `checkpoint dir` | on tmpfs, or little free space | move `checkpoints_dir` to a disk with room for RAM + VRAM of your jobs |
| `helper settings` | ExecStart= passes flags | move the settings into `config.toml` and drop the flags |

On NixOS (`ID=nixos` in `/etc/os-release`), the `fix:` lines name the [module options](#nixos)
instead of steps that do not apply there, each followed by `sudo nixos-rebuild switch`:
`services.puzuz.settings` instead of editing `/etc/puzuz/config.toml` (which the module
rewrites), `services.puzuz.criuPackage` and `services.puzuz.cudaCheckpointPackage` instead of
installing CRIU or cuda-checkpoint into `/usr/local/bin`, `services.puzuz.nvidiaSmiPath` and
`services.puzuz.nvmlLibrary` for nvidia-smi and NVML, `services.puzuz.enable = true` instead of
`sudo puzuz service install` or `systemctl enable`, `services.puzuz.admins` instead of `usermod`,
and `systemd.services.puzuz.serviceConfig.ReadWritePaths` instead of a `systemctl edit` drop-in.

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
