# puzuz

English | [日本語](README.ja.md) | [简体中文](README.zh-CN.md)

A GPU job queue for one Linux machine shared by several users. Its CLI works like
[pueue](https://github.com/Nukesor/pueue). puzuz adds two things on top: GPU slots, and
transparent checkpoint/restore of running jobs (CPU, memory and GPU state) with
[CRIU](https://criu.org) and NVIDIA [cuda-checkpoint](https://github.com/NVIDIA/cuda-checkpoint).
A training run that is still going when the machine reboots is saved to disk and resumes where it
stopped. It does not start over.

> **Status: pre-release.** Everything below is implemented and tested in Docker (systemd as PID 1,
> real CRIU, a real NVIDIA GPU). It has **not** been tested on a real host across a real reboot yet.
> See [Project status](#project-status).

> **No release has been published yet.** The download links, the APT repository and the `stable`
> branch below work from the first release on.

## Why

On a shared GPU workstation people usually run jobs with `nohup`, `tmux` or a per-user pueue, and
they argue over GPUs in chat. A reboot for kernel or driver updates kills every training run.
puzuz provides:

- **one queue for everyone**: a system-wide daemon schedules all users' jobs and shows who is using
  which GPU,
- **GPU slots**: a job asks for specific GPUs (`--gpu 0,1`) and only starts when they are free,
- **checkpoint/restore**: running jobs are saved when the OS shuts down, on `puzuz shutdown`, or on
  demand with `puzuz checkpoint`. They resume later with `puzuz restore`, with no changes to your
  code.

## Features

- pueue-compatible commands: `add`, `status`, `log`, `follow`, `wait`, `pause`, `start`, `kill`,
  `remove`, `restart`, `clean`, `reset`, `stash`, `enqueue`, `switch`, `group`, `parallel`,
  `shutdown`, `completions`, plus groups, dependencies (`--after`), priorities, delays and labels.
- GPU slots: per-GPU limits on how many jobs run at once (`puzuz gpu parallel`). Each job gets
  `CUDA_VISIBLE_DEVICES` and `CUDA_DEVICE_ORDER=PCI_BUS_ID`. GPUs are detected with NVML.
- Fair sharing between users: Slurm-style multifactor priority (fair share with decaying usage,
  age, nice, `top`; `puzuz prio` and `puzuz share` show the numbers).
- Transparent checkpoint/restore of CPU and CUDA processes. This includes PyTorch jobs whose
  DataLoader workers are forked (see [fork guard](#limitations)).
- Automatic checkpoint of all running jobs when the OS shuts down or reboots.
- Multi-user: jobs run as their owner. Other users can see that a job exists, but not its command,
  environment or log. Administrators are root and the `puzuz-admin` group.
- Privilege separation: the daemon (HTTP API, scheduler, SQLite) runs as the unprivileged system
  user `puzuz`. Only a small root helper starts jobs and runs CRIU.
- Jobs run in their own systemd units, so they keep running when the daemon or the helper is
  restarted (for example during an upgrade).
- `puzuz doctor` checks the installation and prints the command that fixes each problem.

## Quick start

See [docs/install.md](docs/install.md) for requirements and details.

### Install (administrator)

On x86_64 or aarch64 Linux with systemd and glibc 2.34 or newer (Ubuntu 22.04+, Debian 12+,
RHEL 9+), the installer downloads the latest release, checks it, and installs puzuz into
/usr/local/bin:

```sh
curl -fsSL https://github.com/kaito4681/puzuz/releases/latest/download/install.sh | sudo bash
```

The installer sets up the systemd units, the `puzuz` user, the `puzuz-admin` group, the
directories and a default /etc/puzuz/config.toml, starts the services and prints the
`puzuz doctor` report. It does not install CRIU or cuda-checkpoint; it tells you if they are
missing. Update later with `sudo puzuz update` (running jobs keep running). Remove it with
`sudo puzuz uninstall`, which keeps the data (`--purge` deletes it too). A GitHub token is not
needed (`GH_TOKEN` is used when set; it only raises the API rate limit).

On Debian and Ubuntu (amd64 or arm64) you can install the package from the release instead:

```sh
sudo apt install ./puzuz_<version>_amd64.deb     # or _arm64.deb
```

It installs and starts the same services and recommends `criu`. It also registers the puzuz APT
repository (in the GitHub release `apt`), so you install the `.deb` by hand only once: later
versions come with `sudo apt update && sudo apt upgrade`. `sudo apt remove puzuz` stops the
services; `sudo apt purge puzuz` also deletes /etc/puzuz. Both keep /var/lib/puzuz and running
jobs.

On Fedora, RHEL, Rocky and Alma Linux 9+ (x86_64 or aarch64), install the .rpm from the release
(a file or its URL):

```sh
sudo dnf install ./puzuz-<version>-1.x86_64.rpm   # or -1.aarch64.rpm, or the release URL
```

There is no dnf repository yet (planned, issue #23): update by installing the newer release's
.rpm with `sudo dnf upgrade <its URL>`. `sudo dnf remove puzuz` stops the services and keeps
/var/lib/puzuz and running jobs. It recommends `criu`; Fedora's is new enough, Rocky/RHEL 9 ship
only 3.19, so checkpointing there needs a newer CRIU (jobs run without it). You can also build
from source or use the Nix flake (see [docs/install.md](docs/install.md)).

On NixOS, import the flake's module (`github:kaito4681/puzuz/stable`, `nixosModules.default`) and
set `services.puzuz.enable = true` (see [NixOS](docs/install.md#nixos)); home-manager users can add
the CLI's completions with `homeManagerModules.default`. With Nix on another distribution
(x86_64 or aarch64), build the flake and install the service from the result (it is copied to
/usr/local/bin and kept from Nix's garbage collection; see
[Installing with Nix](docs/install.md#installing-with-nix)):

```sh
nix build github:kaito4681/puzuz/stable
sudo ./result/bin/puzuz service install
```

On Arch Linux (x86_64, aarch64), use the installer above for now: the AUR package `puzuz-bin` is
not published yet. CRIU (4.2 or newer, for checkpoints) is in the official repositories:

```sh
sudo pacman -S criu
curl -fsSL https://github.com/kaito4681/puzuz/releases/latest/download/install.sh | sudo bash
```

Update with `sudo puzuz update` and remove with `sudo puzuz uninstall`. Once the package is on
the AUR, you can install [`puzuz-bin`](https://aur.archlinux.org/packages/puzuz-bin) with any AUR
helper instead (see [docs/install.md](docs/install.md#arch-linux-aur)).

```sh
# Check everything: criu, cuda-checkpoint, driver, kernel, systemd, services, storage.
puzuz doctor

# Optional: make someone an administrator.
sudo usermod -aG puzuz-admin alice
```

Regular users do not need to install anything.

### Run a GPU job (any user)

```sh
# Queue a job on GPU 0. The command runs through `sh -c` in the current directory, with your
# environment.
puzuz add --gpu 0 -l resnet -- 'python train.py --epochs 100'

puzuz status          # all jobs of all users (others' commands are hidden)
puzuz gpu             # GPUs, slots in use, and which jobs hold them
puzuz log 0           # the last lines of job 0's output
puzuz follow 0        # stream the output, like tail -f
```

### Checkpoint and restore

```sh
puzuz checkpoint 0    # save job 0 to disk and stop it; frees the GPU. State: Checkpointed
puzuz restore 0       # queue it again; it resumes once its group and GPUs have a free slot
puzuz restore -i 0    # ...or resume right away, ahead of the queue (see "GPU slots")
```

A restored job continues from the moment it was checkpointed. It keeps its job id, and its output
is appended to the same log.

A paused job can be checkpointed as well, also on a reboot. It is restored paused, still holding
its GPU slots, and `puzuz start` resumes it. CRIU and cuda-checkpoint need a running process, so
the job runs for a moment during the dump and right after the restore. `puzuz pause` is refused
while a checkpoint is in progress.

### What happens on reboot

1. When the OS shuts down, `puzuz-helper.service` is stopped before the filesystems and NVIDIA
   services. It checkpoints the running jobs one at a time. `--no-checkpoint` jobs are simply
   terminated.
2. After boot, those jobs are listed as `Checkpointed`. Jobs that were queued stay queued and start
   as usual.
3. Each owner (or an administrator) resumes their jobs with `puzuz restore <id>` or
   `puzuz restore -a`. Restores are not started automatically.

A checkpoint can go wrong in two ways:

- **Refused:** the helper's checks fail before the job is touched (not enough free space, a mount
  that could never be restored, ...). The job keeps running. `puzuz checkpoint` fails with the
  reason. If the refusal only comes when the dump is due (for example during `puzuz shutdown`),
  the owner is warned on their next `puzuz` command. The reason is the job's last error in
  `puzuz status` and `puzuz log`.
- **Failed:** the dump itself fails. CRIU puts the job back, but it has been frozen, so it is
  stopped (SIGTERM, which gives it a chance to save its own checkpoint, then SIGKILL after
  `grace_secs`) and shown as `Checkpoint failed` with the reason. The owner is warned on their next
  `puzuz` command.

During an OS shutdown or reboot, systemd stops every job that could not be saved, whether its dump
was refused or failed. After the boot it is shown as `Checkpoint failed` with the reason, and the
owner is warned. Nothing is saved on a power loss or a kernel crash.

## pueue compatibility and differences

The command names, options and table layout follow pueue 4.0, so most pueue habits and scripts work
after replacing `pueue` with `puzuz`. puzuz only copies the CLI. It does not speak pueue's protocol
and does not read pueue's files. These are the differences:

| Topic | pueue | puzuz |
| --- | --- | --- |
| Daemon | one per user, started by the user (`pueued`) | one for the whole system, run by systemd (`puzuz daemon`, user `puzuz`). Users never start a daemon. |
| Selecting a daemon | `-c/--config`, `-p/--profile` | `--socket <path>` (env `PUZUZ_SOCKET`, default `/run/puzuz/puzuz.sock`) |
| `send` | sends input to a task's stdin | not supported: jobs' stdin is `/dev/null`. Make the command non-interactive. |
| `shutdown` | kills all tasks and stops the daemon | administrators only. Checkpoints all running jobs, then stops the daemon. A job whose checkpoint is refused keeps running. `--no-checkpoint` kills them instead. |
| `restart` | finished tasks | also works on `Checkpointed` jobs: they start over as a new job, and the image is kept |
| `start <id>`, `add -i`, `restart -i` (and `restore -i`) | start the task right away, ignoring every limit | skip the queue and the group's `parallel` limit, but wait for a free slot on the job's GPUs and for a paused group to be resumed; only administrators' ignore every limit (see [GPU slots](#gpu-slots)) |
| `reset`, `-a/--all`, `-g/--group` | all tasks | your own jobs. Administrators act on everyone's. |
| `pause/start/kill -a`, `-g` | also pause/resume the groups | only administrators pause/resume groups; for others, groups are left as they are |
| Wording | "task" | "job". Option names such as `--print-task-id` are kept for compatibility. |
| `--json` | pueue's internal state | puzuz's own schema, described below |
| `edit`, `env`, `restart --edit` | supported | accepted but not implemented yet |
| `add -o/--priority`, `switch` | order all tasks of the daemon | order only your own jobs; which user's job starts first is decided by a Slurm-style multifactor priority with fair share (see [Job priority](#job-priority)). `priority_type = "basic"` restores pueue's order. |
| Callbacks, aliases, `pueue.yml` | supported | not available |

Extra commands and options:

| Command | Purpose |
| --- | --- |
| `add --gpu <LIST>` | GPUs the job runs on, e.g. `0` or `0,1`. Without it the job is CPU-only (`CUDA_VISIBLE_DEVICES` is empty). |
| `add --no-checkpoint` | never checkpoint this job; on shutdown it is terminated |
| `checkpoint [IDS] [-g GROUP] [-a]` | save running jobs and stop them |
| `restore [IDS] [-g GROUP] [-a] [-i]` | resume `Checkpointed` jobs (`-i`: right away, ahead of the queue, like `start`) |
| `gpu [-j]`, `gpu parallel <N> [--gpu LIST]` | list GPUs, or set the per-GPU job limit (administrators) |
| `add --nice <N>`, `nice <IDS>... <N>` | lower a job's priority by `N` (Slurm's nice; negative values for administrators only) |
| `top <IDS>...` | move jobs to the top of your own queue (Slurm's `scontrol top`) |
| `prio [IDS] [-j]` | pending jobs in scheduling order with their priority factors (like Slurm's `sprio`) |
| `share [-j]`, `share set <USER> <N>` | fair-share state per user (like Slurm's `sshare`); set a user's shares (administrators) |
| `site <IDS>... <N>`, `group priority <GROUP> <N>` | a job's site factor, a group's priority (administrators) |
| `service install [--bin-dir DIR] [--no-start] [--force] / uninstall / status` | manage the systemd services; install exits non-zero if the final check fails (see [docs/install.md](docs/install.md)) |
| `uninstall [--purge] [--yes]` | remove the services and the binaries `service install` copied (root) |
| `update [--check] [--version V] [-y]` | update puzuz from the latest (or the given) release; needs root, without root it only checks (see [docs/install.md](docs/install.md#upgrading)) |
| `doctor` | check the installation |
| `daemon` | run the daemon in the foreground (systemd does this) |

Additional job states: `Checkpointing`, `Checkpointed`, `Restore queued`, `Restoring`, and
`Terminating` (stopping after a failed checkpoint). The `status` table has `Owner` and `GPU`
columns, and `Checkpointed` jobs show the image size.

### JSON output

`puzuz status --json` prints `{"groups": [...], "jobs": [...]}`:

```json
{
  "groups": [{"name": "default", "parallel": 1, "paused": false}],
  "jobs": [{
    "id": 0, "owner": 1000, "group": "default", "gpus": [0], "dependencies": [],
    "priority": 0, "label": "resnet", "no_checkpoint": false,
    "state": "done", "result": "failed", "exit_code": 1,
    "created_at": 1760000000000, "started_at": 1760000001000, "finished_at": 1760000100000,
    "checkpoint_size_bytes": 8100000000,
    "details": {"command": "python train.py", "cwd": "/home/alice/proj", "last_error": "..."}
  }]
}
```

- Timestamps are Unix epoch milliseconds. `owner` is a uid.
- The fields of `state` are flattened into the job object. `state` is one of `queued`
  (`enqueued_at`), `stashed` (`enqueue_at` or null), `running`, `paused`, `checkpointing`,
  `checkpointed`, `restore_queued` (`enqueued_at`), `restoring`, `terminating` and `done`.
  `terminating` and `done` carry `result`, which is one of `success`, `failed` (`exit_code`),
  `failed_to_spawn` (`reason`), `killed`, `dependency_failed`, `checkpoint_failed` (`reason`) and
  `lost`.
- `checkpoint_size_bytes`, `details`, `details.last_error`, a job's `nice` and a group's
  `priority` are omitted when they do not apply (or are 0).
  For other users' jobs, `label` is null, `details` is absent and `reason` is `"<redacted>"`.
- `waiting` is present on a queued job that was launched right away by a non-administrator but
  waits for a GPU slot or its paused group: `{"reason": "gpu_slots", "gpus": [0]}`,
  `{"reason": "group_paused"}` or `{"reason": "unavailable"}`.
- `puzuz log --json` prints `{"<id>": {"job": <job>, "output": "..."}}`. `puzuz group --json`
  prints `{"groups": [...]}`. `puzuz gpu --json` prints
  `{"gpus": [{"index", "uuid", "name", "memory_total_bytes", "driver_version", "parallel",
  "used", "jobs": [{"id", "owner"}]}]}`, where `"available": false` marks a GPU that has
  disappeared.
- `puzuz prio --json` prints `{"priority_type", "weights": {"age", "fairshare", "job_size",
  "partition", "tres"}, "jobs": [{"id", "owner", "group", "priority", "factors": {"age",
  "fairshare", "job_size", "partition", "tres_cpu", "tres_gpu"}, "weighted": {"age", "fairshare",
  "job_size", "partition", "tres"}, "site", "nice"}]}`, the jobs in scheduling order.
  `puzuz share --json` prints `{"users": [{"uid", "shares", "norm_shares", "raw_usage",
  "effective_usage", "fairshare", "active"}], "decay_half_life_secs", "calc_period_secs",
  "usage_reset_period", "billing_weights", "usage_calc_at"}`.

## Multi-user model and authorization

- The daemon identifies callers by the kernel-reported uid of the socket peer (`SO_PEERCRED`). A
  client cannot claim to be someone else.
- Each job belongs to the user who added it. It runs with that user's uid, gid and supplementary
  groups, with `no_new_privs` set and all capabilities dropped. The command runs in the directory
  it was added from, with the environment of the `puzuz add` invocation.
- The root helper refuses jobs of system accounts (uid below `job_min_uid`, by default `UID_MIN`
  of `/etc/login.defs`), of users missing from `job_allowed_users` when it is set, and of users
  whose primary group is root-equivalent (`job_denied_groups`: `docker`, `lxd`, `incus-admin`,
  `libvirt`, `disk`, `sudo`, `wheel`, `admin` by default). Those groups are left out of every
  job's supplementary groups, so a job cannot use Docker or sudo even if its owner can (the
  helper's journal records what it left out). A refused job fails to spawn with `puzuz refuses to run jobs as <user>: <reason>`.
  See [Who jobs may run as](docs/install.md#who-jobs-may-run-as).
- **Owner or administrator only:** changing a job (`kill`, `pause`, `start`, `remove`,
  `checkpoint`, `restore`, `restart`, `stash`, `enqueue`, `switch`, ...) and seeing its command,
  working directory, environment, label, failure reasons and log. Likewise, `add --after` only
  accepts jobs the caller could remove (their own; any job for administrators): a job blocks the
  removal of the jobs it depends on, so depending on someone else's job would keep them from
  removing or cleaning it. A dependency given twice is recorded once.
- **Everyone:** seeing that a job exists, its owner, group, GPUs, state, priority, dependencies and
  times (`status`, `gpu`), the priority factors of pending jobs (`prio`) and everyone's fair-share
  usage (`share`). This shows who is using which GPU.
- **Owner:** raising `nice` (never below 0), and `top` (unless `enable_user_top = false`).
- **Administrators only:** `share set`, `group priority`, `site`, negative `nice`, `top` of other
  users' jobs, `group add/remove`, `parallel`, `gpu parallel`, pausing or resuming
  whole groups (`pause/start/kill -g` for one group, `pause/start/kill -a` for all groups),
  `shutdown`, `reset` of other users' jobs, and `kill <id>` of an
  [orphan](docs/architecture.md#orphans) (a process the helper runs that the daemon does not
  account for; `status` lists them to administrators). As in pueue, `pause` and `kill` pause the selected
  groups, `start` resumes them, and `pause --wait` pauses only the groups and lets running jobs
  finish. Unlike pueue, `kill -g/-a` pauses the groups even when nothing is queued in them.
  When a regular user runs `pause/start/kill -g` or `-a`, only their own jobs are affected and no
  group's paused state changes (`pause --wait` then does nothing). The output says which happened
  (`All groups are being paused.` versus `Your jobs are being paused: 3, 4`).
- **Administrators' immediate launches** (`start <id>`, `add -i`, `restart -i`, `restore -i`)
  ignore GPU slots and paused groups; everybody else's wait for them (see [GPU slots](#gpu-slots)).
- Administrators are root and members of the `puzuz-admin` group (primary or supplementary). The
  check is made on every request, so `usermod -aG puzuz-admin <user>` takes effect immediately.

## GPU slots

- `puzuz add --gpu 0,1 ...` starts the job only when its group has a free slot and **every** listed
  GPU has a free slot. The daemon rejects GPU indices it does not know.
- Each GPU allows 1 job at a time by default. An administrator changes this with
  `puzuz gpu parallel 2` (all GPUs) or `puzuz gpu parallel 2 --gpu 1`. `0` means unlimited.
  Slots count jobs, not VRAM.
- GPU indices follow PCI bus order, the same order as `nvidia-smi` and
  `CUDA_DEVICE_ORDER=PCI_BUS_ID`. Inside the job, `CUDA_VISIBLE_DEVICES` lists the requested
  GPUs, so the program sees them as `cuda:0`, `cuda:1`, ...
- **GPU isolation:** a job can open only the device nodes of the GPUs it was given. Every job
  unit has systemd's `DevicePolicy=closed`, with `DeviceAllow=` for the assigned GPUs'
  `/dev/nvidia<N>` and the shared NVIDIA nodes; a CPU-only job gets no NVIDIA device at all.
  Changing `CUDA_VISIBLE_DEVICES` or opening another GPU's node fails with `EPERM`, and
  `nvidia-smi` inside a job shows only its own GPUs (in a CPU-only job it fails with
  `Failed to initialize NVML: Unknown Error`). Administrators can turn this off with
  `gpu_device_isolation = false`, e.g. for MIG (see
  [GPU isolation](docs/install.md#gpu-isolation)).
- Groups work as in pueue (the `default` group starts with `parallel 1`). Both limits apply.
- Launching right away (`start <id>`, `add -i`, `restart -i`, `restore -i`) skips the queue and the
  group's `parallel` limit, as in pueue, but not GPU slots or a paused group: if one of the job's
  GPUs has no free slot or its group is paused, the job waits and starts ahead of every other
  queued job as soon as that changes. The command says so, and `status` shows e.g.
  `Queued (waiting for a GPU slot)`. Only administrators' immediate launches ignore GPU slots and
  paused groups, so a user cannot push a GPU past the limit an administrator set.
- GPUs are detected with NVML when the daemon starts (the `[[gpus]]` list in the configuration is
  only a fallback). Restart `puzuz.service` after adding or removing a GPU. `puzuz doctor`
  compares what the daemon detected with `nvidia-smi`.

## Job priority

When jobs wait for the same slots, the one with the highest priority among those that fit starts
first. The priority follows Slurm's multifactor plugin: by default it is the sum of a fair-share
factor (users who used less of the machine recently, with usage decaying with a half-life of 7
days, go first) and an age factor (time spent waiting), minus the job's nice value. Job size,
group (partition) and per-GPU (TRES) factors can be weighted in as well. Your own `-o`
priority, `switch` and `puzuz top` reorder only your own jobs. `puzuz prio` shows why a job is
where it is, `puzuz share` shows everyone's usage. See
[Priority and fair share](docs/install.md#priority-and-fair-share) for the formula and the
settings.

## Requirements

- Linux with systemd 240 or newer (tested: 259). Releases are built for x86_64 and aarch64; only
  x86_64 has been tested on real hardware. The prebuilt binaries need glibc 2.34 or newer.
- A kernel with `CONFIG_CHECKPOINT_RESTORE`, 5.5 or newer recommended (tested: 7.0).
- CRIU 4.2 or newer. The distribution package works where it is new enough (Ubuntu 26.04, Fedora,
  Arch); on older releases build it from source (see [docs/install.md](docs/install.md#criu)).
- For GPU jobs: an NVIDIA driver whose `cuda-checkpoint` supports `--action lock/checkpoint/
  restore/unlock` (r570 or newer; tested: 595), the `cuda-checkpoint` binary, and `nvidia-smi`.

Details are in [docs/install.md](docs/install.md).

## Limitations

- **Network connections:** established TCP connections (to a dataset server, a database, an
  experiment tracker, ...) are closed when a job is checkpointed (`criu --tcp-close`). After a
  restore the program sees a broken connection and must reconnect.
- **Private `/tmp` and `/dev/shm`:** each job has its own empty tmpfs on `/tmp` and `/dev/shm`. It
  is saved in the checkpoint and is not shared with other jobs or with your shell. Mounts that do
  not survive a reboot (anything below `/run` and `/media`) are not visible inside jobs. Use a
  path on a persistent filesystem to exchange files.
- **Mounts are fixed at the start:** a job sees the filesystems that were mounted when it started
  (automount points are triggered then), not those mounted later. To restore a job, every one of
  those filesystems must be mounted again at the same path; otherwise the restore is refused and
  the image is kept. Files on a tmpfs such as `/scratch` do not survive a reboot. See
  [Filesystems and mounts](docs/install.md#filesystems-and-mounts).
- **No crash safety:** checkpoints are taken only on an orderly shutdown, `puzuz shutdown` and
  `puzuz checkpoint`. Nothing is saved on a power loss, a kernel panic or a hard reset. Periodic
  checkpoints are not available yet. Keep saving your own training checkpoints.
- **Fork guard:** children that a CUDA process forks without `exec` (for example PyTorch
  DataLoader workers with the `fork` start method) inherit NVIDIA device mappings that CRIU cannot
  save. The helper preloads a small library (`LD_PRELOAD`) into every job that removes them in the
  child. It cannot work for statically linked programs, or for processes created with raw
  `clone(2)` or `_Fork()`. With the guard, NVML cannot be initialized in such a forked child.
  PyTorch prints a warning and carries on. It can be turned off with `fork_guard = false`.
- **Same GPU model and driver:** a job can only be restored on a GPU of the same model, with the
  same driver version and at the same GPU index as when it was checkpointed. Otherwise the restore
  is refused and the image is kept. Upgrade the NVIDIA driver only after restoring (or removing)
  every checkpointed job.
- **Same files:** CRIU reopens the job's files by path and checks executables and libraries. Do
  not change or delete the job's virtualenv, binaries, working directory or log before it is
  restored. Do not replace a directory on the way to the job's files with a symlink, and keep the
  groups you were in: the restore checks that you could open every file the job had open the
  same way, through the same directories, and is refused otherwise (see
  [Restores and paths](docs/architecture.md#restores-and-paths)).
- **Unix sockets:** a job with a Unix socket bound to a path outside its own `/tmp` and
  `/dev/shm` (e.g. in your home directory), or to a relative name such as `ctl.sock`, cannot be
  checkpointed; the checkpoint is refused and the job keeps running. Bind sockets to an absolute
  path below `/tmp`, or use an abstract name.
- **`SO_REUSEPORT`:** a job with a TCP or UDP socket that has `SO_REUSEPORT` set cannot be
  checkpointed either (CRIU would restore the socket as root's, and it could then share its port
  with a root service). Some servers set it by default; turn it off or use `--no-checkpoint`.
- **Python and PyTorch:**
  - Python 3.14 and newer default to the `forkserver` start method on Linux, so DataLoader workers
    are not forked and do not depend on the fork guard. Python 3.12 and 3.13 (or an explicit
    `multiprocessing_context="fork"`) rely on the fork guard.
  - Sharing CUDA tensors between processes (`torch.multiprocessing` CUDA IPC) cannot be
    checkpointed with current drivers. Multi-GPU jobs that use NCCL have not been tested.
  - cuda-checkpoint waits for submitted GPU work to finish before saving, so very long kernels
    delay a checkpoint.
  - Checkpoints take roughly RSS + VRAM of disk space per job, plus what the job keeps in its
    `/tmp` and `/dev/shm`, and are written one job at a time.
    In tests, a job with 7 GB of VRAM took about 10 to 20 s to dump.
- **Mounts covering `/`:** a job whose root directory is covered by another mount cannot be
  checkpointed. puzuz refuses the dump and tells you why. The job keeps running.
- Checkpoint images are never deleted automatically while their job exists. They are removed when
  the job is restored successfully, removed (`remove`, `clean`, `reset`), or when its dump fails.
  Above `checkpoint_size_limit_bytes`, puzuz warns but does not refuse.
- Not available yet: `edit`, `env`, periodic checkpoints, checkpointing a job while keeping it
  running, freeing a GPU without a full checkpoint ("GPU suspend"), a web UI or TCP API, and slots
  counted by VRAM.

## Project status

Pre-release (version 0.1.0). Tested so far:

- Unit and integration tests for every crate (`scripts/test-in-docker.sh`).
- Full-stack end-to-end tests in a privileged Docker container with systemd 259 as PID 1, CRIU 4.2
  and the binaries installed with `puzuz service install` (`scripts/e2e-test.sh`). The tests
  cover basic queue operations, three-user authorization, groups and GPU limits,
  checkpoint/restore, daemon and helper restarts, many jobs added at once, `puzuz shutdown`, and a
  simulated reboot: the container is shut down with systemd and the jobs are restored in a new
  container, also with jobs working on a separate ext4 filesystem, bind mounts, an
  `x-systemd.automount`, overlayfs, a tmpfs and sshfs that come back as new mounts.
- With a real GPU in Docker (driver 595, CRIU 4.2.1 in manual mode, PyTorch 2.8 on Python 3.12):
  checkpoint/restore of a CUDA counter and of PyTorch training with forked DataLoader workers,
  with results identical to an uninterrupted run, and NVML GPU detection by the daemon inside the
  packaged, sandboxed `puzuz.service`.

Not tested yet:

- A real OS reboot on a host: driver reload, and the stop order relative to the filesystems and
  `nvidia-persistenced`.
- Installing with `install.sh` from the real GitHub releases (tested against a fake release
  server, `scripts/dist-test.sh`, on Ubuntu 22.04/24.04/26.04 and Debian 12 containers).
- The `.deb` under a running systemd (its install, reinstall, remove and purge are tested in
  plain Debian 12 and Ubuntu containers without systemd, `scripts/deb-test.sh`, and so is an
  upgrade through a local copy of the APT repository, `scripts/apt-repo-test.sh`), the same for the
  `.rpm` (install, upgrade and remove are tested in plain Rocky Linux 9 and Fedora containers,
  `scripts/rpm-test.sh`), the APT repository in the real `apt` release, and arm64 on real hardware.
- Kernel NFS and autofs indirect maps (FUSE network filesystems and `x-systemd.automount` are
  tested).
- Multi-GPU and NCCL jobs, and GPU models other than the one used for testing.

Please report what works and what does not on your hardware.

## Documentation

- [docs/install.md](docs/install.md): requirements, building, installing, configuration,
  priority and fair share, upgrading, uninstalling and troubleshooting
- [docs/architecture.md](docs/architecture.md): components, job flow, shutdown ordering, state
  storage and crate layout

## Contributing

Read [AGENTS.md](AGENTS.md) for the workspace layout and conventions. Code, comments, docs and
commit messages are in English. Tests run inside Docker:

```sh
scripts/test-in-docker.sh                                   # cargo test --workspace
scripts/test-in-docker.sh clippy --workspace --all-targets
scripts/test-in-docker.sh fmt --all
scripts/e2e-test.sh               # full stack under systemd in a privileged container
scripts/service-install-test.sh   # service install / status / uninstall and doctor
scripts/helper-systemd-test.sh    # the helper alone under systemd
scripts/dist-test.sh              # install.sh against a fake GitHub API (PUZUZ_DIST_BASE=debian:12 ...)
nix build -L .#checks.x86_64-linux.nixos         # NixOS module in a VM (needs KVM)
nix build -L .#checks.x86_64-linux.home-manager  # home-manager module evaluation
nix build -L .#checks.x86_64-linux.helper-tests  # all puzuz-helper tests in a VM (needs KVM)
```

`scripts/e2e-test.sh` documents its environment variables at the top, for example
`PUZUZ_E2E_ONLY`, and `PUZUZ_E2E_GPU=1` for the GPU scenarios. `PUZUZ_HELPER_GPU=1
scripts/helper-systemd-test.sh` (with a CUDA base image, see the script) forces GPU checkpoints
past their deadline and kills `criu` / `cuda-checkpoint` mid-dump, in either
`PUZUZ_HELPER_CUDA_MODE`. `scripts/measure-checkpoint.sh` measures how long each checkpoint step
takes. None of the scripts run anything privileged on the host.

## License

[MIT](LICENSE)
