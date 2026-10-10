# puzuz

A GPU job queue for one Linux machine shared by several users. Its CLI works like
[pueue](https://github.com/Nukesor/pueue). puzuz adds two things on top: GPU slots, and
transparent checkpoint/restore of running jobs (CPU, memory and GPU state) with
[CRIU](https://criu.org) and NVIDIA [cuda-checkpoint](https://github.com/NVIDIA/cuda-checkpoint).
A training run that is still going when the machine reboots is saved to disk and resumes where it
stopped. It does not start over.

> **Status: pre-release.** Everything below is implemented and tested in Docker (systemd as PID 1,
> real CRIU, a real NVIDIA GPU). It has **not** been tested on a real host across a real reboot yet.
> See [Project status](#project-status).

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

```sh
# Build both binaries (puzuz and puzuz-helper).
cargo build --release -p puzuz -p puzuz-helper

# Install the systemd units, the `puzuz` user, the `puzuz-admin` group, the directories and a
# default /etc/puzuz/config.toml, then start the services. --bin-dir copies both binaries
# there (needed unless they already live in /usr/bin or /usr/local/bin).
sudo target/release/puzuz service install --bin-dir /usr/local/bin

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
puzuz restore -i 0    # ...or resume right away, ignoring slot limits
```

A restored job continues from the moment it was checkpointed. It keeps its job id, and its output
is appended to the same log.

### What happens on reboot

1. When the OS shuts down, `puzuz-helper.service` is stopped before the filesystems and NVIDIA
   services. It checkpoints the running jobs one at a time. `--no-checkpoint` jobs are simply
   terminated.
2. After boot, those jobs are listed as `Checkpointed`. Jobs that were queued stay queued and start
   as usual.
3. Each owner (or an administrator) resumes their jobs with `puzuz restore <id>` or
   `puzuz restore -a`. Restores are not started automatically.

If a job cannot be checkpointed, it is stopped (SIGTERM, which gives it a chance to save its own
checkpoint, then SIGKILL after `grace_secs`). The owner is warned on their next `puzuz` command.
After a `puzuz checkpoint` the job is shown as `Checkpoint failed`, and the reason appears in
`puzuz status` and `puzuz log`. Nothing is saved on a power loss or a kernel crash.

## pueue compatibility and differences

The command names, options and table layout follow pueue 4.0, so most pueue habits and scripts work
after replacing `pueue` with `puzuz`. puzuz only copies the CLI. It does not speak pueue's protocol
and does not read pueue's files. These are the differences:

| Topic | pueue | puzuz |
| --- | --- | --- |
| Daemon | one per user, started by the user (`pueued`) | one for the whole system, run by systemd (`puzuz daemon`, user `puzuz`). Users never start a daemon. |
| Selecting a daemon | `-c/--config`, `-p/--profile` | `--socket <path>` (env `PUZUZ_SOCKET`, default `/run/puzuz/puzuz.sock`) |
| `send` | sends input to a task's stdin | not supported: jobs' stdin is `/dev/null`. Make the command non-interactive. |
| `shutdown` | kills all tasks and stops the daemon | administrators only. Checkpoints all running jobs, then stops the daemon. `--no-checkpoint` kills them instead. |
| `restart` | finished tasks | also works on `Checkpointed` jobs: they start over as a new job, and the image is kept |
| `reset`, `-a/--all`, `-g/--group` | all tasks | your own jobs. Administrators act on everyone's. |
| Wording | "task" | "job". Option names such as `--print-task-id` are kept for compatibility. |
| `--json` | pueue's internal state | puzuz's own schema, described below |
| `edit`, `env`, `restart --edit` | supported | accepted but not implemented yet |
| Callbacks, aliases, `pueue.yml` | supported | not available |

Extra commands and options:

| Command | Purpose |
| --- | --- |
| `add --gpu <LIST>` | GPUs the job runs on, e.g. `0` or `0,1`. Without it the job is CPU-only (`CUDA_VISIBLE_DEVICES` is empty). |
| `add --no-checkpoint` | never checkpoint this job; on shutdown it is terminated |
| `checkpoint [IDS] [-g GROUP] [-a]` | save running jobs and stop them |
| `restore [IDS] [-g GROUP] [-a] [-i]` | resume `Checkpointed` jobs (`-i`: right away, ignoring free slots) |
| `gpu [-j]`, `gpu parallel <N> [--gpu LIST]` | list GPUs, or set the per-GPU job limit (administrators) |
| `service install / uninstall / status` | manage the systemd services (see [docs/install.md](docs/install.md)) |
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
- `checkpoint_size_bytes`, `details` and `details.last_error` are omitted when they do not apply.
  For other users' jobs, `label` is null, `details` is absent and `reason` is `"<redacted>"`.
- `puzuz log --json` prints `{"<id>": {"job": <job>, "output": "..."}}`. `puzuz group --json`
  prints `{"groups": [...]}`. `puzuz gpu --json` prints
  `{"gpus": [{"index", "uuid", "name", "memory_total_bytes", "driver_version", "parallel",
  "used", "jobs": [{"id", "owner"}]}]}`, where `"available": false` marks a GPU that has
  disappeared.

## Multi-user model and authorization

- The daemon identifies callers by the kernel-reported uid of the socket peer (`SO_PEERCRED`). A
  client cannot claim to be someone else.
- Each job belongs to the user who added it. It runs with that user's uid, gid and supplementary
  groups, with `no_new_privs` set and all capabilities dropped. The command runs in the directory
  it was added from, with the environment of the `puzuz add` invocation.
- **Owner or administrator only:** changing a job (`kill`, `pause`, `start`, `remove`,
  `checkpoint`, `restore`, `restart`, `stash`, `enqueue`, `switch`, ...) and seeing its command,
  working directory, environment, label, failure reasons and log.
- **Everyone:** seeing that a job exists, its owner, group, GPUs, state, priority, dependencies and
  times (`status`, `gpu`). This shows who is using which GPU.
- **Administrators only:** `group add/remove`, `parallel`, `gpu parallel`, pausing or resuming a
  whole group (`pause -g`, `start -g`, `kill -g`), `shutdown`, and `reset` of other users' jobs.
  When a regular user runs `pause/start/kill -g`, only their own jobs in the group are affected.
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
- Groups work as in pueue (the `default` group starts with `parallel 1`). Both limits apply.
- GPUs are detected when the daemon starts. Restart `puzuz.service` after adding or removing a GPU.

## Requirements

- Linux with systemd 240 or newer (tested: 259). Only x86_64 has been tested.
- A kernel with `CONFIG_CHECKPOINT_RESTORE`, 5.5 or newer recommended (tested: 7.0).
- CRIU 4.2 or newer. The distribution package works.
- For GPU jobs: an NVIDIA driver whose `cuda-checkpoint` supports `--action lock/checkpoint/
  restore/unlock` (r570 or newer; tested: 595), the `cuda-checkpoint` binary, and `nvidia-smi`.

Details are in [docs/install.md](docs/install.md).

## Limitations

- **Network connections:** established TCP connections (to a dataset server, a database, an
  experiment tracker, ...) are closed when a job is checkpointed (`criu --tcp-close`). After a
  restore the program sees a broken connection and must reconnect.
- **Private `/tmp` and `/dev/shm`:** each job has its own empty tmpfs on `/tmp` and `/dev/shm`. It
  is saved in the checkpoint and is not shared with other jobs or with your shell. Mounts that do
  not survive a reboot (anything below `/run`, `/mnt` and `/media`) are not visible inside jobs.
  Use a path on a persistent filesystem to exchange files.
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
  restored.
- **Python and PyTorch:**
  - Python 3.14 and newer default to the `forkserver` start method on Linux, so DataLoader workers
    are not forked and do not depend on the fork guard. Python 3.12 and 3.13 (or an explicit
    `multiprocessing_context="fork"`) rely on the fork guard.
  - Sharing CUDA tensors between processes (`torch.multiprocessing` CUDA IPC) cannot be
    checkpointed with current drivers. Multi-GPU jobs that use NCCL have not been tested.
  - cuda-checkpoint waits for submitted GPU work to finish before saving, so very long kernels
    delay a checkpoint.
  - Checkpoints take roughly RSS + VRAM of disk space per job and are written one job at a time.
    In tests, a job with 7 GB of VRAM took about 10 to 20 s to dump.
- **Mounts covering `/`:** a job whose root directory is covered by another mount cannot be
  checkpointed. puzuz refuses the dump and tells you why.
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
  container.
- With a real GPU in Docker (driver 595, CRIU 4.2.1 in manual mode, PyTorch 2.8 on Python 3.12):
  checkpoint/restore of a CUDA counter and of PyTorch training with forked DataLoader workers,
  with results identical to an uninterrupted run.

Not tested yet:

- A real OS reboot on a host: driver reload, and the stop order relative to the filesystems and
  `nvidia-persistenced`.
- Restoring with a real host's mount layout (NFS, autofs, ...).
- `cuda_checkpoint_mode = "plugin"`, multi-GPU and NCCL jobs, and GPU models other than the one
  used for testing.

Please report what works and what does not on your hardware.

## Documentation

- [docs/install.md](docs/install.md): requirements, building, installing, configuration,
  upgrading, uninstalling and troubleshooting
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
```

`scripts/e2e-test.sh` documents its environment variables at the top, for example
`PUZUZ_E2E_ONLY`, and `PUZUZ_E2E_GPU=1` for the GPU scenarios. None of the scripts run anything
privileged on the host.

## License

[MIT](LICENSE)
