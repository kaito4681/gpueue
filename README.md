# gpueue

[![CI](https://github.com/kaito4681/gpueue/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/kaito4681/gpueue/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/kaito4681/gpueue)](https://github.com/kaito4681/gpueue/releases/latest)
[![License: MIT](https://img.shields.io/github/license/kaito4681/gpueue)](LICENSE)

English | [日本語](README.ja.md) | [简体中文](README.zh-CN.md)

A GPU job queue for one Linux machine shared by several users. Its CLI works like
[pueue](https://github.com/Nukesor/pueue). gpueue adds two things on top: GPU slots, and
transparent checkpoint/restore of running jobs (CPU, memory and GPU state) with
[CRIU](https://criu.org) and NVIDIA [cuda-checkpoint](https://github.com/NVIDIA/cuda-checkpoint).
A training run that is still going when the machine reboots is saved to disk and resumes where it
stopped. It does not start over.

> [!NOTE]
> **Status: pre-release.** Everything below is implemented and tested in Docker (systemd as PID 1,
> real CRIU, a real NVIDIA GPU), and across real reboots and power-offs of a QEMU/KVM virtual
> machine with CPU jobs. It has **not** been tested across a real reboot on bare metal or with GPU
> jobs yet.
> See [Project status](#project-status).

## Contents

- [Why](#why)
- [Features](#features)
- [Install](#install)
  - [1. Prerequisites](#1-prerequisites)
  - [2. Install gpueue](#2-install-gpueue)
  - [3. Check the installation](#3-check-the-installation)
- [Usage](#usage)
- [Limitations](#limitations)
- [Project status](#project-status)
- [Agent Skill for coding agents](#agent-skill)
- [Documentation](#documentation)
- [Contributing](#contributing)
- [License](#license)

## Why

On a shared GPU workstation people usually run jobs with `nohup`, `tmux` or a per-user pueue, and
they argue over GPUs in chat. A reboot for kernel or driver updates kills every training run.
gpueue gives everyone one queue: a system-wide daemon schedules all users' jobs and shows who is
using which GPU. A job asks for specific GPUs (`--gpu 0,1`) and only starts when they are free.
Running jobs are saved when the OS shuts down, on `gpueue shutdown`, or on demand with
`gpueue checkpoint`, and resume later with `gpueue restore`, with no changes to your code.

## Features

| Feature | What it means |
| --- | --- |
| pueue-compatible CLI | `add`, `status`, `log`, `follow`, `wait`, `pause`, `start`, `kill`, `remove`, `restart`, `clean`, `reset`, `stash`, `enqueue`, `switch`, `group`, `parallel`, `shutdown`, `completions`, plus groups, dependencies (`--after`), priorities, delays and labels |
| GPU slots | per-GPU limits on how many jobs run at once (`gpueue gpu parallel`); each job gets `CUDA_VISIBLE_DEVICES` and `CUDA_DEVICE_ORDER=PCI_BUS_ID`; GPUs are detected with NVML |
| Fair sharing | Slurm-style multifactor priority (fair share with decaying usage, age, nice, `top`); `gpueue prio` and `gpueue share` show the numbers |
| Checkpoint/restore | transparent, for CPU and CUDA processes, including PyTorch jobs whose DataLoader workers are forked (see [fork guard](docs/usage.md#limitations)); automatic for all running jobs when the OS shuts down or reboots |
| Multi-user | jobs run as their owner; other users can see that a job exists, but not its command, environment or log; administrators are root and the `gpueue-admin` group |
| Privilege separation | the daemon (HTTP API, scheduler, SQLite) runs as the unprivileged system user `gpueue`; only a small root helper starts jobs and runs CRIU |
| Robust jobs | jobs run in their own systemd units, so they keep running when the daemon or the helper is restarted (for example during an upgrade) |
| `gpueue doctor` | checks the installation and prints the command that fixes each problem |

## Install

Administrators install gpueue once per machine; regular users do not need to install anything.
[docs/install.md](docs/install.md) has the details for every step.

### 1. Prerequisites

gpueue needs Linux with systemd 240 or newer (tested: 259) and a kernel with
`CONFIG_CHECKPOINT_RESTORE`, 5.5 or newer recommended (tested: 7.0). Releases are built for x86_64
and aarch64 and need glibc 2.34 or newer (Ubuntu 22.04+, Debian 12+, RHEL 9+); only x86_64 has
been tested on real hardware.

Jobs run without the programs below, but checkpoints need them, and no install method installs
them for you: the packages only recommend CRIU, which is too old or missing on many releases, and
`install.sh` only lists what is missing (see
[what each method installs](docs/install.md#dependencies-that-are-not-installed-automatically)).
The NixOS module brings its own CRIU.

<a id="dependencies-that-are-not-installed-automatically"></a>

| Dependency | Needed for | How to get it |
| --- | --- | --- |
| CRIU 4.2 or newer | every checkpoint | `sudo apt install criu` (Ubuntu 26.04), `sudo dnf install criu` (Fedora), `sudo pacman -S criu` (Arch Linux); elsewhere [build it from source](docs/install.md#criu) |
| NVIDIA driver r570 or newer, with `nvidia-smi` (tested: 595) | GPU jobs | your distribution; its `cuda-checkpoint` must support `--action lock/checkpoint/restore/unlock` |
| NVIDIA's prebuilt cuda-checkpoint with the driver's major version, owned by root | GPU checkpoints | the commands below (on NixOS see [cuda-checkpoint on NixOS](docs/install.md#cuda-checkpoint-on-nixos)) |

```sh
git clone --depth 1 https://github.com/NVIDIA/cuda-checkpoint /tmp/cuda-checkpoint
sudo install -o root -g root -m 0755 "/tmp/cuda-checkpoint/bin/$(uname -m)_Linux/cuda-checkpoint" \
  /usr/local/bin/cuda-checkpoint
```

The helper looks cuda-checkpoint and CRIU up only when it starts: if you install either after
gpueue, restart it with `sudo systemctl restart gpueue-helper` (see
[cuda-checkpoint](docs/install.md#cuda-checkpoint)) and run `gpueue doctor` again.

### 2. Install gpueue

The installer downloads the latest release, checks it, installs gpueue into /usr/local/bin, sets
up the systemd units, the `gpueue` user, the `gpueue-admin` group, the directories and a default
/etc/gpueue/config.toml, starts the services and prints the `gpueue doctor` report:

```sh
curl -fsSL https://github.com/kaito4681/gpueue/releases/latest/download/install.sh | sudo bash
```

Update later with `sudo gpueue update` (running jobs keep running). Remove it with
`sudo gpueue uninstall`, which keeps the data (`--purge` deletes it too). A GitHub token is not
needed (`GH_TOKEN` is used when set; it only raises the API rate limit).

<details>
<summary><b>Other methods: APT / .deb, .rpm, NixOS and Nix, Arch Linux</b></summary>

**Debian and Ubuntu** (amd64 or arm64): install the package from the gpueue APT repository (in the
GitHub release `apt`); this needs `curl` and `ca-certificates`, and apt picks the newest stable
version and the architecture. Or install the `.deb` of the latest release directly (it registers
the same repository). Both install and start the same services. Either way, later versions come with `sudo apt update && sudo apt upgrade`.
`sudo apt remove gpueue` stops the services; `sudo apt purge gpueue` also deletes /etc/gpueue. Both
keep /var/lib/gpueue and running jobs. The package recommends `criu` (see
[the prerequisites](docs/install.md#installing-the-debianubuntu-package)).

```sh
sudo curl -fsSLo /etc/apt/keyrings/gpueue.gpg https://github.com/kaito4681/gpueue/releases/download/apt/gpueue.gpg
sudo curl -fsSLo /etc/apt/sources.list.d/gpueue.sources https://github.com/kaito4681/gpueue/releases/download/apt/gpueue.sources
sudo apt update && sudo apt install gpueue

# or the .deb of the latest release:
arch=$(dpkg --print-architecture)
curl -fLO "https://github.com/kaito4681/gpueue/releases/latest/download/gpueue_${arch}.deb"
sudo apt install "./gpueue_${arch}.deb"
```

**Fedora, RHEL, Rocky and Alma Linux 9+** (x86_64 or aarch64): dnf takes the release's .rpm URL
directly. There is no dnf repository yet (planned, issue #23): update by running the same command
with `sudo dnf upgrade`. `sudo dnf remove gpueue` stops the services and keeps /var/lib/gpueue and
running jobs. It recommends `criu`; Fedora's is new enough, Rocky/RHEL 9 ship only 3.19, so
checkpointing there needs a newer CRIU (jobs run without it).

```sh
sudo dnf install "https://github.com/kaito4681/gpueue/releases/latest/download/gpueue.$(uname -m).rpm"
```

These URLs always give the newest stable release (`releases/latest` skips pre-releases); the
versioned files (`gpueue_<version>_<arch>.deb`, `gpueue-<version>-1.<arch>.rpm`) are attached too,
for a specific version. Every file has `.sha256` and `.minisig` files (see
[Verifying a release by hand](docs/install.md#verifying-a-release-by-hand)). You can also build
from source (see [docs/install.md](docs/install.md)).

**NixOS:** import the flake's module (`github:kaito4681/gpueue/stable`, `nixosModules.default`) and
set `services.gpueue.enable = true` (see [NixOS](docs/install.md#nixos)); home-manager users can add
the CLI's completions with `homeManagerModules.default`. **Nix on another distribution** (x86_64
or aarch64): build the flake and install the service from the result (it is copied to
/usr/local/bin and kept from Nix's garbage collection; see
[Installing with Nix](docs/install.md#installing-with-nix)):

```sh
nix build github:kaito4681/gpueue/stable
sudo ./result/bin/gpueue service install
```

**Arch Linux** (x86_64, aarch64): use the installer above for now (`sudo pacman -S criu` first for
checkpoints); update with `sudo gpueue update` and remove with `sudo gpueue uninstall`. The AUR
package [`gpueue-bin`](https://aur.archlinux.org/packages/gpueue-bin) is planned and not published
yet ([#159](https://github.com/kaito4681/gpueue/issues/159)); once it is, you can install it with
any AUR helper instead (see [docs/install.md](docs/install.md#arch-linux-aur)).

</details>

### 3. Check the installation

```sh
# Check everything: criu, cuda-checkpoint, driver, kernel, systemd, services, storage.
gpueue doctor

# Optional: make someone an administrator.
sudo usermod -aG gpueue-admin alice
```

## Usage

`gpueue --help` lists every command. Commands without a `(gpueue)` tag work like pueue's (the
[differences](docs/usage.md#pueue-compatibility-and-differences) are mostly about one shared
daemon and who may do what); the tagged ones are gpueue's additions. `gpueue <command> --help`
shows each command's options, for example `--gpu` and `--no-checkpoint` of `add`.

```console
$ gpueue --help
Interact with the gpueue GPU job queue (a pueue-compatible CLI)

Use the `--help` long form to get detailed help output on each subcommand!

Usage: gpueue [OPTIONS] [COMMAND]

Commands:
  add          Enqueue a job for execution
  remove       Remove jobs from the list
  switch       Swap the queue position of two jobs
  stash        Stash jobs so they won't be started automatically
  enqueue      Enqueue stashed jobs again
  start        Resume paused jobs or groups, or force-start queued jobs
  restart      Restart finished jobs
  pause        Pause running jobs or whole groups
  kill         Kill running jobs or all jobs of a group
  send         Not supported by gpueue: jobs have no stdin to send input to
  edit         Edit properties of stashed or queued jobs in $EDITOR (not implemented yet)
  env          Add or remove environment variables of a job (not implemented yet)
  group        Add or remove groups; without a subcommand, list all groups
  status       Show the status of all jobs
  log          Show the output of jobs
  follow       Follow the output of a running job, like "tail -f"
  wait         Wait until jobs are finished
  clean        Remove all finished jobs from the list
  reset        Kill all jobs, remove them and reset the groups
  shutdown     Checkpoint all running jobs and stop the daemon (administrators only)
  parallel     Set the number of jobs a group may run in parallel
  completions  Generate shell completion files
  checkpoint   (gpueue) Checkpoint running jobs and stop them
  restore      (gpueue) Resume Checkpointed jobs
  gpu          (gpueue) Show the GPUs and their slots; see subcommands to change limits
  prio         (gpueue) Show the priority of pending jobs and its factors (like Slurm's `sprio`)
  nice         (gpueue) Set the nice value of jobs (like Slurm's nice option): it is subtracted from
               their priority
  top          (gpueue) Move jobs to the top of their owner's queue (Slurm's `scontrol top`)
  site         (gpueue) Set the site factor of jobs, which is added to their priority
               (administrators only)
  share        (gpueue) Show the fair-share state of each user (like Slurm's `sshare`)
  daemon       (gpueue) Run the daemon in the foreground (normally started by systemd)
  service      (gpueue) Install, uninstall or inspect the systemd services
  uninstall    (gpueue) Uninstall gpueue (requires root)
  doctor       (gpueue) Check the installation and print how to fix problems
  update       (gpueue) Update gpueue itself (root; without root it only checks)
  help         Print this message or the help of the given subcommand(s)

Options:
  -v, --verbose...
          Verbose mode (-v, -vv, -vvv)

      --color <COLOR>
          Colorize the output; `auto` enables colors when stdout is a terminal
          
          [default: auto]
          [possible values: auto, never, always]

      --socket <SOCKET>
          Path of the daemon's Unix socket.
          
          Clients connect to it and `gpueue daemon` listens on it. Replaces pueue's
          `--config`/`--profile`, which do not apply to a system-wide daemon.
          
          [env: GPUEUE_SOCKET=]
          [default: /run/gpueue/gpueue.sock]

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version
```

A typical session: `add` queues a job on the GPUs it names, `status` and `gpu` show who runs what
where, `log` and `follow` show its output, and `checkpoint` / `restore` take it off the GPU and
bring it back later.

```sh
# Queue a job on GPU 0. The command runs through `sh -c` in the current directory, with your
# environment.
gpueue add --gpu 0 -l resnet -- 'python train.py --epochs 100'

gpueue status          # all jobs of all users (others' commands are hidden)
gpueue gpu             # GPUs, slots in use, and which jobs hold them
gpueue log 0           # the last lines of job 0's output
gpueue follow 0        # stream the output, like tail -f

gpueue checkpoint 0    # save job 0 to disk and stop it; frees the GPU. State: Checkpointed
gpueue restore 0       # queue it again; it resumes once its group and GPUs have a free slot
gpueue restore -i 0    # ...or put it first among your queued jobs (administrators: resume right away; see "Job priority")
```

A restored job continues from the moment it was checkpointed, keeps its job id and appends to the
same log. When the OS shuts down, the helper checkpoints the running jobs one at a time
(`--no-checkpoint` jobs are simply terminated); after the boot they are `Checkpointed`, and each
owner resumes them with `gpueue restore <id>` or `gpueue restore -a`: restores are not started
automatically. Only administrators start a job right away with `start`, `add -i`, `restart -i` or
`restore -i`; anybody else's job is queued first among their own jobs.

[docs/usage.md](docs/usage.md) is the reference for the rest:
[differences from pueue and the extra commands](docs/usage.md#pueue-compatibility-and-differences),
[JSON output](docs/usage.md#json-output),
[paused jobs, reboots and failed checkpoints](docs/usage.md#checkpoint-and-restore),
[who may do what](docs/usage.md#multi-user-model-and-authorization),
[GPU slots and isolation](docs/usage.md#gpu-slots) and
[job priority](docs/usage.md#job-priority).

## Limitations

Checkpoint/restore has limits. Established TCP connections are closed at a checkpoint, and nothing
is saved on a power loss or a kernel crash. A job is restored only on the same GPU model and
driver, with the same files and mounts. Some sockets and descriptors (Unix sockets outside the
job's `/tmp`, inotify, `SO_REUSEPORT`, ...) make a job impossible to checkpoint; the checkpoint is
then refused and the job keeps running. See [all limitations](docs/usage.md#limitations).

## Project status

Pre-release (version 0.1.4). v0.1.4 is a security release: sites running v0.1.3 or earlier should
upgrade (see the [v0.1.4 release notes](https://github.com/kaito4681/gpueue/releases/tag/v0.1.4)).
It is tested by unit and integration tests, full-stack end-to-end tests under systemd in Docker
(with a real GPU too), and real reboots of QEMU/KVM virtual machines; a real reboot on bare metal
and the GPU parts of a real reboot are not tested yet (see
[Test coverage](docs/usage.md#test-coverage)). Please report what works and what does not on your
hardware.

<details>
<summary><b>Behaviour changes in v0.1.4 to check before upgrading</b></summary>

- These jobs can no longer be checkpointed (the refusal message says why): jobs holding
  inotify/fanotify watches, listening on an abstract Unix socket (other than libcuda's own),
  holding a descriptor in a Unix socket queue, or holding a deleted setuid/setgid file.
- A restore is refused while another program on the host uses a port the job had bound; it can be
  retried once the port is free.
- `gpueue daemon` refuses to start if `/etc/gpueue/config.toml` (or a directory above it) is not
  root-owned or is group/world-writable; `gpueue doctor` reports this.
- `sudo gpueue service install` refuses to copy a `gpueue-helper` from a directory a non-root user
  can write; unpack releases as root.
- Checkpoints taken by v0.1.3 restore with v0.1.4.

</details>

<a id="agent-skill"></a>
## Agent Skill for coding agents

The repository ships an [Agent Skill](https://agentskills.io) in
[skills/gpueue](skills/gpueue/SKILL.md) that teaches coding agents (Claude Code, Codex, Cursor and
others) how to use gpueue as a regular user: checking that the daemon is up, adding jobs with the
right working directory, environment and `--gpu`, following logs, waiting with `gpueue wait`
instead of polling, pause/kill/restart, checkpoint/restore, `--json` output, common errors, and
safety rules (only touch your own jobs, never use `sudo` or administrator commands). Install it
with the [skills](https://github.com/vercel-labs/skills) CLI:

```sh
npx skills add kaito4681/gpueue              # this project only; add -g for all projects
npx skills add kaito4681/gpueue -g -a claude-code -y
```

## Documentation

| Document | Contents |
| --- | --- |
| [docs/install.md](docs/install.md) | requirements, building, installing, configuration, priority and fair share, upgrading, uninstalling and troubleshooting |
| [docs/usage.md](docs/usage.md) | pueue differences, JSON output, checkpoint/restore, multi-user model, GPU slots, job priority, limitations and test coverage |
| [docs/architecture.md](docs/architecture.md) | components, job flow, shutdown ordering, state storage and crate layout |

## Contributing

Read [AGENTS.md](AGENTS.md) for the workspace layout and conventions. Code, comments, docs and
commit messages are in English. Tests run inside Docker:

```sh
scripts/test-in-docker.sh                                   # cargo test --workspace
scripts/test-in-docker.sh clippy --workspace --all-targets
scripts/test-in-docker.sh fmt --all
```

<details>
<summary><b>End-to-end, VM, GPU and Nix tests</b></summary>

```sh
scripts/e2e-test.sh               # full stack under systemd in a privileged container
scripts/vm-reboot-test.sh         # real OS in QEMU/KVM VMs: install, reboots, upgrade (needs /dev/kvm; `all` for every distro)
scripts/gpu-host-test.sh          # checkpoint/restore of real CUDA jobs on the installed gpueue (GPU host, no root)
scripts/service-install-test.sh   # service install / status / uninstall and doctor
scripts/helper-systemd-test.sh    # the helper alone under systemd
scripts/dist-test.sh              # install.sh against a fake GitHub API (GPUEUE_DIST_BASE=debian:12 ...)
nix build -L .#checks.x86_64-linux.nixos         # NixOS module in a VM (needs KVM)
nix build -L .#checks.x86_64-linux.home-manager  # home-manager module evaluation
nix build -L .#checks.x86_64-linux.helper-tests  # all gpueue-helper tests in a VM (needs KVM)
```

`scripts/e2e-test.sh` documents its environment variables at the top, for example
`GPUEUE_E2E_ONLY`, and `GPUEUE_E2E_GPU=1` for the GPU scenarios. `GPUEUE_HELPER_GPU=1
scripts/helper-systemd-test.sh` (with a CUDA base image, see the script) forces GPU checkpoints
past their deadline and kills `criu` / `cuda-checkpoint` mid-dump, in either
`GPUEUE_HELPER_CUDA_MODE`. `scripts/measure-checkpoint.sh` measures how long each checkpoint step
takes. None of the scripts run anything privileged on the host.

</details>

## License

[MIT](LICENSE)
