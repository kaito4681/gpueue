# Using gpueue

English | [日本語](ja/usage.md) | [简体中文](zh-CN/usage.md)

The reference for running jobs with gpueue once it is installed. The [README](../README.md) has
the overview and a quick start; [install.md](install.md) covers installing, configuring and
operating gpueue.

- [pueue compatibility and differences](#pueue-compatibility-and-differences)
- [JSON output](#json-output)
- [Checkpoint and restore](#checkpoint-and-restore)
  - [What happens on reboot](#what-happens-on-reboot)
- [Multi-user model and authorization](#multi-user-model-and-authorization)
- [GPU slots](#gpu-slots)
- [Job priority](#job-priority)
- [Limitations](#limitations)
- [Test coverage](#test-coverage)

## pueue compatibility and differences

The command names, options and table layout follow pueue 4.0, so most pueue habits and scripts work
after replacing `pueue` with `gpueue`. gpueue only copies the CLI. It does not speak pueue's protocol
and does not read pueue's files. These are the differences:

| Topic | pueue | gpueue |
| --- | --- | --- |
| Daemon | one per user, started by the user (`pueued`) | one for the whole system, run by systemd (`gpueue daemon`, user `gpueue`). Users never start a daemon. |
| Selecting a daemon | `-c/--config`, `-p/--profile` | `--socket <path>` (env `GPUEUE_SOCKET`, default `/run/gpueue/gpueue.sock`) |
| `send` | sends input to a task's stdin | not supported: jobs' stdin is `/dev/null`. Make the command non-interactive. |
| `shutdown` | kills all tasks and stops the daemon | administrators only. Checkpoints all running jobs, then stops the daemon. A job whose checkpoint is refused keeps running. `--no-checkpoint` kills them instead. |
| `restart` | finished tasks | also works on `Checkpointed` jobs: they start over as a new job, and the image is kept |
| `start <id>`, `add -i`, `restart -i` (and `restore -i`) | start the task right away, ignoring every limit | only administrators' start right away, ignoring every limit; anybody else's job is queued first among their own jobs (like `gpueue top`) and waits for its turn like any queued job (see [Job priority](#job-priority)) |
| `reset`, `-a/--all`, `-g/--group` | all tasks | your own jobs. Administrators act on everyone's. |
| `pause/start/kill -a`, `-g` | also pause/resume the groups | only administrators pause/resume groups; for others, groups are left as they are |
| Wording | "task" | "job". Option names such as `--print-task-id` are kept for compatibility. |
| `--json` | pueue's internal state | gpueue's own schema, described below |
| `edit`, `env`, `restart --edit` | supported | accepted but not implemented yet |
| `add -o/--priority`, `switch` | order all tasks of the daemon | order only your own jobs; which user's job starts first is decided by a Slurm-style multifactor priority with fair share (see [Job priority](#job-priority)). `priority_type = "basic"` restores pueue's order, except that `-o` still only orders your own jobs. |
| Callbacks, aliases, `pueue.yml` | supported | not available |

Extra commands and options:

| Command | Purpose |
| --- | --- |
| `add --gpu <LIST>` | GPUs the job runs on, e.g. `0` or `0,1`. Without it the job is CPU-only (`CUDA_VISIBLE_DEVICES` is empty). |
| `add --no-checkpoint` | never checkpoint this job; on shutdown it is terminated |
| `reset --keep-checkpoints` | reset everything except `Checkpointed` (and restore-queued) jobs, which keep their checkpoint images |
| `checkpoint [IDS] [-g GROUP] [-a]` | save running jobs and stop them |
| `restore [IDS] [-g GROUP] [-a] [-i]` | resume `Checkpointed` jobs (`-i`: like `start`, right away for administrators, first among your own jobs for anybody else) |
| `gpu [-j]`, `gpu parallel <N> [--gpu LIST]` | list GPUs, or set the per-GPU job limit (administrators) |
| `add --nice <N>`, `nice <IDS>... <N>` | lower a job's priority by `N` (Slurm's nice; negative values for administrators only) |
| `top <IDS>...` | move jobs to the top of your own queue (Slurm's `scontrol top`) |
| `prio [IDS] [-j]` | pending jobs in scheduling order with their priority factors (like Slurm's `sprio`) |
| `share [-j]`, `share set <USER> <N>` | fair-share state per user (like Slurm's `sshare`); set a user's shares (administrators) |
| `site <IDS>... <N>`, `group priority <GROUP> <N>` | a job's site factor, a group's priority (administrators) |
| `service install [--bin-dir DIR] [--no-start] [--force] / uninstall / status` | manage the systemd services; install exits non-zero if the final check fails (see [install.md](install.md)) |
| `uninstall [--purge] [--yes]` | remove the services and the binaries `service install` copied (root) |
| `update [--check] [--version V] [-y]` | update gpueue from the latest (or the given) release; needs root, without root it only checks (see [install.md](install.md#upgrading)) |
| `doctor` | check the installation |
| `daemon` | run the daemon in the foreground (systemd does this) |

Additional job states: `Checkpointing`, `Checkpointed`, `Restore queued`, `Restoring`, and
`Terminating` (stopping after a failed checkpoint). The `status` table has `Owner` and `GPU`
columns, and `Checkpointed` jobs show the image size.

## JSON output

`gpueue status --json` (or just `gpueue --json`) prints `{"groups": [...], "jobs": [...]}`. Every
`--json` output is indented when stdout is a terminal and on one line otherwise (in a pipe, for
`jq` or a script):

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
- `gpueue log --json` prints `{"<id>": {"job": <job>, "output": "..."}}`. `gpueue group --json`
  prints `{"groups": [...]}`. `gpueue gpu --json` prints
  `{"gpus": [{"index", "uuid", "name", "memory_total_bytes", "driver_version", "parallel",
  "used", "jobs": [{"id", "owner"}]}]}`, where `"available": false` marks a GPU that has
  disappeared.
- `gpueue prio --json` prints `{"priority_type", "weights": {"age", "fairshare", "job_size",
  "partition", "tres"}, "jobs": [{"id", "owner", "group", "priority", "factors": {"age",
  "fairshare", "job_size", "partition", "tres_cpu", "tres_gpu"}, "weighted": {"age", "fairshare",
  "job_size", "partition", "tres"}, "site", "nice"}]}`, the jobs in scheduling order.
  `gpueue share --json` prints `{"users": [{"uid", "shares", "norm_shares", "raw_usage",
  "effective_usage", "fairshare", "active"}], "decay_half_life_secs", "calc_period_secs",
  "usage_reset_period", "billing_weights", "usage_calc_at"}`.

## Checkpoint and restore

`gpueue checkpoint <id>` saves a running (or paused) job to disk and stops it, which frees its GPUs; the
job is then `Checkpointed`. `gpueue restore <id>` queues it again, and it resumes once its group and
GPUs have a free slot (`restore -i`: first among your queued jobs, or right away for
administrators; see [Job priority](#job-priority)).

A restored job continues from the moment it was checkpointed. It keeps its job id, and its output
is appended to the same log.

A paused job can be checkpointed as well, also on a reboot. It is restored paused, still holding
its GPU slots, and `gpueue start` resumes it. CRIU and cuda-checkpoint need a running process, so
the job runs for a moment during the dump. The restore leaves it stopped, so it runs nothing (not
even for a moment) until `gpueue start`. `gpueue pause` is refused while a checkpoint is in
progress.

### What happens on reboot

1. When the OS shuts down, `gpueue-helper.service` is stopped before the filesystems and NVIDIA
   services. It checkpoints the running jobs one at a time. `--no-checkpoint` jobs are simply
   terminated.
2. After boot, those jobs are listed as `Checkpointed`. Jobs that were queued stay queued and start
   as usual.
3. Each owner (or an administrator) resumes their jobs with `gpueue restore <id>` or
   `gpueue restore -a`. Restores are not started automatically.

A checkpoint can go wrong in two ways:

- **Refused:** the helper's checks fail before the job is touched (not enough free space, a mount
  that could never be restored, ...). The job keeps running. `gpueue checkpoint` fails with the
  reason. If the refusal only comes when the dump is due (for example during `gpueue shutdown`),
  the owner is warned on their next `gpueue` command. The reason is the job's last error in
  `gpueue status` and `gpueue log`.
- **Failed:** the dump itself fails (for example, a deleted file larger than CRIU's limit or a
  file lock the job holds). CRIU puts the job back and the helper undoes what it did to the job's
  CUDA state; when all of that succeeds, the job keeps running, and the reason (starting with
  `checkpoint failed, the job keeps running:`) is its last error in `gpueue status` and
  `gpueue log`. If the job cannot be put back cleanly (its CUDA state cannot be resumed, or CRIU
  was killed mid-dump at the deadline), it is stopped (SIGTERM, which gives it a chance to save
  its own checkpoint, then SIGKILL after `grace_secs`) and shown as `Checkpoint failed` with the
  reason. Either way, the owner is warned on their next `gpueue` command.

During an OS shutdown or reboot, systemd stops every job that could not be saved, whether its dump
was refused or failed. After the boot it is shown as `Checkpoint failed` with the reason, and the
owner is warned. Nothing is saved on a power loss or a kernel crash.

## Multi-user model and authorization

- The daemon identifies callers by the kernel-reported uid of the socket peer (`SO_PEERCRED`). A
  client cannot claim to be someone else.
- Each job belongs to the user who added it. It runs with that user's uid, gid and supplementary
  groups, with `no_new_privs` set and all capabilities dropped. The command runs in the directory
  it was added from, with the environment of the `gpueue add` invocation.
- The root helper refuses jobs of system accounts (uid below `job_min_uid`, by default `UID_MIN`
  of `/etc/login.defs`), of `nobody`, systemd dynamic-user uids (61184-65519) and, if `job_max_uid`
  is set, uids above it, of users missing from `job_allowed_users` when it is set, and of users
  whose primary group is root-equivalent (`job_denied_groups`: `docker`, `lxd`, `incus-admin`,
  `libvirt`, `libvirtd`, `disk`, `sudo`, `wheel`, `admin` by default). Those groups are left out of every
  job's supplementary groups, so a job cannot use Docker or sudo even if its owner can (the
  helper's journal records what it left out). A refused job fails to spawn with `gpueue refuses to run jobs as <user>: <reason>`.
  See [Who jobs may run as](install.md#who-jobs-may-run-as).
- A job cannot become root or act as root, even when its owner can: setuid programs (`sudo`,
  `su`, `pkexec`) do not gain privileges under `no_new_privs`, the system D-Bus is inaccessible in
  job units (so no `systemctl`, `run0`, `machinectl` or other polkit-authorized action, whatever
  the polkit rules grant the owner's groups; `job_system_dbus = true` lifts this), neither are
  systemd's polkit-authorized Varlink services under `/run/systemd` (`job_systemd_varlink`), nor
  daemon sockets that check the owner's groups in the user database (libvirt's in `/run/libvirt`,
  authorized through polkit, and CUPS's in `/run/cups`; `job_hidden_socket_dirs`), root-equivalent
  groups are left out, and only the assigned GPUs' device nodes can be opened. The same holds
  after a restore. See
  [What a job can and cannot do](install.md#what-a-job-can-and-cannot-do).
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
  [orphan](architecture.md#orphans) (a process the helper runs that the daemon does not
  account for; `status` lists them to administrators). As in pueue, `pause` and `kill` pause the selected
  groups, `start` resumes them, and `pause --wait` pauses only the groups and lets running jobs
  finish. Unlike pueue, `kill -g/-a` pauses the groups even when nothing is queued in them.
  When a regular user runs `pause/start/kill -g` or `-a`, only their own jobs are affected and no
  group's paused state changes (`pause --wait` then does nothing). The output says which happened
  (`All groups are being paused.` versus `Your jobs are being paused: 3, 4`).
- **Only administrators launch jobs immediately** (`start <id>`, `add -i`, `restart -i`,
  `restore -i`), ignoring every limit; everybody else's job is queued first among their own jobs
  (see [Job priority](#job-priority)).
- Administrators are root and members of the `gpueue-admin` group (primary or supplementary). The
  check is made on every request, so `usermod -aG gpueue-admin <user>` takes effect immediately.

## GPU slots

- `gpueue add --gpu 0,1 ...` starts the job only when its group has a free slot and **every** listed
  GPU has a free slot. The daemon rejects GPU indices it does not know.
- Each GPU allows 1 job at a time by default. An administrator changes this with
  `gpueue gpu parallel 2` (all GPUs) or `gpueue gpu parallel 2 --gpu 1`. `0` means unlimited.
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
  [GPU isolation](install.md#gpu-isolation)).
- Groups work as in pueue (the `default` group starts with `parallel 1`). Both limits apply.
- GPU slots, the group's `parallel` limit and a paused group apply to every job a regular user
  launches, also with `start <id>`, `add -i`, `restart -i` or `restore -i` (see
  [Job priority](#job-priority)); only administrators' immediate launches ignore them.
- GPUs are detected with NVML when the daemon starts (the `[[gpus]]` list in the configuration is
  only a fallback). Restart `gpueue.service` after adding or removing a GPU. `gpueue doctor`
  compares what the daemon detected with `nvidia-smi`.

## Job priority

When jobs wait for the same slots, the one with the highest priority among those that fit starts
first. The priority follows Slurm's multifactor plugin: by default it is the sum of a fair-share
factor (users who used less of the machine recently, with usage decaying with a half-life of 7
days, go first) and an age factor (time spent waiting), minus the job's nice value. Job size,
group (partition) and per-GPU (TRES) factors can be weighted in as well. Your own `-o`
priority, `switch` and `gpueue top` reorder only your own jobs. So do pueue's immediate launches
(`start <id>` of a queued or stashed job, `add -i`, `restart -i`, `restore -i`) for anybody but an
administrator: the job is queued first among your own jobs, exactly like `gpueue top`, and then
waits for its turn against other users' jobs, within the group's `parallel` limit, GPU slots, a
paused group and its dependencies; the command says it was queued, not started. With
`enable_user_top = false` (or `priority_type = "basic"`) it is just queued. `start` of a paused
job resumes it as before. Administrators' immediate launches start right away, ignoring every
limit. `gpueue prio` shows why a job is
where it is, `gpueue share` shows everyone's usage. See
[Priority and fair share](install.md#priority-and-fair-share) for the formula and the
settings.

## Limitations

- **Network connections:** established TCP connections (to a dataset server, a database, an
  experiment tracker, ...) are closed when a job is checkpointed (`criu --tcp-close`). After a
  restore the program sees a broken connection and must reconnect.
- **Private `/tmp` and `/dev/shm`:** each job has its own empty tmpfs on `/tmp` and `/dev/shm`. It
  is saved in the checkpoint and is not shared with other jobs or with your shell. Mounts that do
  not survive a reboot (anything below `/run` and `/media`) and other containers' mounts (below
  `/var/lib/docker/` and the like) are not visible inside jobs. Use a
  path on a persistent filesystem to exchange files.
- **Mounts are fixed at the start:** a job sees the filesystems that were mounted when it started
  (automount points are triggered then), not those mounted later. To restore a job, every one of
  those filesystems must be mounted again at the same path; otherwise the restore is refused and
  the image is kept. Files on a tmpfs such as `/scratch` do not survive a reboot. See
  [Filesystems and mounts](install.md#filesystems-and-mounts).
- **No crash safety:** checkpoints are taken only on an orderly shutdown, `gpueue shutdown` and
  `gpueue checkpoint`. Nothing is saved on a power loss, a kernel panic or a hard reset. Periodic
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
  [Restores and paths](architecture.md#restores-and-paths)).
- **Unix sockets:** a job with a Unix socket bound to a path outside its own `/tmp` and
  `/dev/shm` (e.g. in your home directory), or to a relative name such as `ctl.sock`, cannot be
  checkpointed; the checkpoint is refused and the job keeps running. Bind sockets to an absolute
  path below `/tmp` (without `..` or symlinks on the way), or use an abstract name for sockets
  that do not listen. A socket *listening* on an abstract name cannot be checkpointed (CRIU
  would set it up as root, and its clients would see root as the listener), except the one
  libcuda listens on in every CUDA process (`@cuda-uvmfd-<pid namespace inode>-<pid>@`).
- **inotify, fanotify and descriptors in flight:** a job with an inotify or fanotify descriptor
  (file watchers), or one that is passing descriptors over a Unix socket (`SCM_RIGHTS`) at the
  moment of the checkpoint, cannot be checkpointed (CRIU would recreate those as root).
- **Listening ports:** a job whose TCP or UDP socket is bound to a port can be checkpointed
  (Jupyter, TensorBoard), but it is restored only while no other socket on the host has that
  port; otherwise the restore is refused and can be retried once the port is free.
- **`SO_REUSEPORT`:** a job with a TCP or UDP socket that has `SO_REUSEPORT` set cannot be
  checkpointed either (CRIU would restore the socket as root's, and it could then share its port
  with a root service). Some servers set it by default; turn it off or use `--no-checkpoint`.
- **Deleted files:** files the job deleted while it still has them open (temporary files,
  multiprocessing semaphores, PyTorch DataLoader batches in `/dev/shm`) are saved in the image,
  however large, as long as they fit in the free space of the checkpoint directory. A job that holds a deleted file that still
  has another hard link, a file deleted on NFS while open (`.nfs<hex>`), or a deleted program
  with the setuid/setgid bit or file capabilities cannot be checkpointed; the checkpoint is
  refused and the job keeps running. Libraries that a package upgrade replaced while the job
  runs are fine.
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
  checkpointed. gpueue refuses the dump and tells you why. The job keeps running.
- Checkpoint images are never deleted automatically while their job exists. They are removed when
  the job is restored successfully, removed (`remove`, `clean`, `reset`), or when its dump fails.
  Before removing them, `reset` (unless `-f`) and `remove` (on a terminal) ask `[y/N]` whether the
  `Checkpointed` jobs go too; answering no (the default) keeps those jobs and their images and
  removes the others. `reset` also asks `[y/N]` before it kills and removes anything.
  Above `checkpoint_size_limit_bytes`, gpueue warns but does not refuse.
- Not available yet: `edit`, `env`, periodic checkpoints, checkpointing a job while keeping it
  running, freeing a GPU without a full checkpoint ("GPU suspend"), a web UI or TCP API, and slots
  counted by VRAM.

## Test coverage

Tested so far:

- Unit and integration tests for every crate (`scripts/test-in-docker.sh`).
- Full-stack end-to-end tests in a privileged Docker container with systemd as PID 1, CRIU
  and the binaries installed with `gpueue service install` (`scripts/e2e-test.sh`). The tests
  cover basic queue operations, three-user authorization, groups and GPU limits,
  checkpoint/restore, daemon and helper restarts, many jobs added at once, `gpueue shutdown`, and a
  simulated reboot: the container is shut down with systemd and the jobs are restored in a new
  container, also with jobs working on a separate ext4 filesystem, bind mounts, an
  `x-systemd.automount`, overlayfs, a tmpfs and sshfs that come back as new mounts.
- With a real NVIDIA GPU in Docker (CRIU in manual mode, PyTorch):
  checkpoint/restore of a CUDA counter and of PyTorch training with forked DataLoader workers,
  with results identical to an uninterrupted run, and NVML GPU detection by the daemon inside the
  packaged, sandboxed `gpueue.service`.
- Real operating systems in QEMU/KVM virtual machines (`scripts/vm-reboot-test.sh`), booted from
  the distributions' cloud images, with gpueue installed as users install it: the `.deb` with
  `apt install ./gpueue_<version>_amd64.deb` (the `.rpm` with `dnf` on Fedora), or
  `gpueue service install`, and `/var/lib/gpueue` on an ext4 disk of its own. On the quick set's
  distribution: `systemctl reboot`, `systemctl poweroff` followed by a fresh boot, and a
  reboot while `gpueue checkpoint` of a 1 GiB job is still running, with CPU jobs of two users plus
  a `--no-checkpoint` job. After each boot, the previous boot's journal must show the stop order
  (the daemon stops first, the helper dumps the jobs before it stops, the `--no-checkpoint` job's
  unit stops only after the helper, and `/var/lib/gpueue` is unmounted last) with no timeout or
  SIGKILL, the `--no-checkpoint` job must have received SIGTERM, and the restored jobs must
  continue counting where they stopped. A desktop-like host: systemd-resolved running, `/tmp` a
  tmpfs, a user's FUSE mounts without `allow_other` under `/tmp` (as an AppImage's) and in the home
  directory, checkpoints and restores of running and paused jobs, a job that keeps its state in
  its private `/tmp`, and a reboot with checkpointed, running and paused jobs. An upgrade from the
  previous release's `.deb` (downloaded from GitHub and verified like `install.sh` does) with
  checkpointed and running jobs. The `all` set adds more Debian and Ubuntu releases (where CRIU is
  missing or too old: the install and jobs without checkpoints only) and Fedora with the `.rpm`;
  `scripts/vm-reboot-test.sh list` shows both sets. CI runs the quick set on pull requests
  that touch the helper, the daemon, checkpointing or packaging, and on every push to `main`; a
  release is not created, not even as a draft, unless it passes with the release's own `.deb`.

Not tested yet:

- A real reboot on bare metal, and the GPU parts of a real reboot: GPU jobs checkpointed and
  restored across it, the driver reload, and the stop order relative to `nvidia-persistenced`.
- Installing with `install.sh` from the real GitHub releases (tested against a fake release
  server, `scripts/dist-test.sh`, in Debian and Ubuntu containers).
- The `.deb`'s reinstall, remove and purge under a running systemd (they are tested in plain
  Debian and Ubuntu containers without systemd, `scripts/deb-test.sh`, and so is an upgrade
  through a local copy of the APT repository, `scripts/apt-repo-test.sh`; install and upgrade are
  tested in the VMs above), the `.rpm` on Rocky Linux 9 under systemd (install, upgrade and remove
  are tested in plain containers, `scripts/rpm-test.sh`), the APT repository in the real `apt`
  release, and arm64 on real hardware.
- Kernel NFS and autofs indirect maps (FUSE network filesystems and `x-systemd.automount` are
  tested).
- Multi-GPU and NCCL jobs, and GPU models other than the one used for testing.

Please report what works and what does not on your hardware.
