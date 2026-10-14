# Architecture

English | [日本語](ja/architecture.md) | [简体中文](zh-CN/architecture.md)

## Components

| Process | Runs as | Role |
| --- | --- | --- |
| `puzuz` (CLI) | the calling user | sends requests to the daemon over HTTP on a Unix socket |
| `puzuz daemon` (`puzuz.service`) | system user `puzuz`, sandboxed, no capabilities | HTTP API, authorization, scheduler, SQLite state. Decides when jobs start, stop, checkpoint and restore. |
| `puzuz-helper` (`puzuz-helper.service`) | root | starts jobs as their owner, signals them, and runs CRIU and cuda-checkpoint. Accepts connections only from the daemon's user. |
| job units (`puzuz-job-<id>.service`, `puzuz-job-<id>-r<n>.service` after the n-th restore) | root shim, then the job owner | one transient systemd service per job |

The HTTP server and the database run unprivileged. The root helper has a small dependency tree (no
HTTP server, no SQLite) and validates every request itself: the uid (never 0), the working
directory and log opened with the owner's permissions, and image paths below its checkpoint
directory.

## How a job flows

```
puzuz add ──HTTP /v1 over /run/puzuz/puzuz.sock──▶ puzuz daemon ──NDJSON over /run/puzuz/helper.sock──▶ puzuz-helper
  (SO_PEERCRED identifies the caller)                (SQLite, scheduler)                                   │ D-Bus
                                                                                                           ▼
                                                                         systemd: puzuz-job-<id>.service ─▶ shim ─▶ sh -c '<command>'
```

1. **CLI → daemon.** The CLI sends REST/JSON requests under `/v1` (for example `POST /v1/jobs`) to
   `/run/puzuz/puzuz.sock`, with an `x-puzuz-version` header. The daemon reads the caller's uid
   from `SO_PEERCRED`, checks authorization and stores the job. Logs are streamed as raw bytes and
   state changes as Server-Sent Events (`/v1/events`). Pending warnings, such as a failed dump, come
   back in an `x-puzuz-warnings` header and the CLI prints them on stderr.
   <a id="api-limits"></a>Every user can reach the socket, so the daemon limits what one user keeps
   open: at most 64 connections and 16 streams (`log --follow`, `/v1/events`) per uid, and 4096
   connections in total (`puzuz.service` raises `LimitNOFILE` well above that). A request over a
   limit is answered with `too_many_requests`. A connection must send its request headers within
   20 seconds, and an idle keep-alive connection is closed after 20 seconds. `log --tail` looks
   for line starts in the last 4 MiB of the log at most (the output starts there if they hold
   fewer lines), and only a few such scans run at once.
2. **Scheduling.** A queued job starts when its group has a free slot and every requested GPU has
   a free slot. Candidates are considered in priority order (see
   [Priority and fair share](#priority-and-fair-share)). The daemon then asks the helper to spawn
   it. Immediate launches (`start <id>`, `add -i`, `restart -i`, `restore -i`) are marked as
   forced, in memory. An administrator's is launched at once, ignoring every limit. Anybody else's
   is part of the plan (`scheduler::Reserved::forced`): it goes before every other candidate and
   skips the group's `parallel` limit and its dependencies, but it still needs a free slot on each
   of its GPUs and an unpaused group. The action checks this in the same transaction, so a launch
   that fits starts right away and one that does not is queued with the reason
   (`scheduler::LaunchWait`), which the reply and the job's view (`waiting`) carry.
3. **Daemon → helper.** The daemon keeps one connection to `/run/puzuz/helper.sock` and exchanges
   newline-delimited JSON. A Hello message checks the protocol version first. Then come requests
   with ids, and numbered events from the helper (job exited, checkpoint or restore finished).
   Events are persisted and replayed until the daemon acknowledges them, so nothing is lost while
   the daemon is down. The daemon stores the number of the last event it applied with its effect
   and skips replayed events up to it, so an event is applied once even if the daemon stopped
   before acknowledging it. An event it cannot decode (from a newer helper) is logged and
   acknowledged rather than ending the connection. After a reconnect both sides reconcile their
   lists of jobs: a job the helper no longer runs is lost, and a job the daemon believes is being
   checkpointed but the helper runs normally lost its checkpoint request and goes back to running
   (or paused) with a failed checkpoint. A checkpoint request that times out is followed up the
   same way. Before the daemon marks a job lost because the helper no longer knows it, it applies
   every event the helper sent before saying so, so a job that has just exited keeps its result.
   <a id="orphans"></a>A job the helper runs that the daemon has as finished, queued, checkpointed
   or not at all (for example after restoring the database from a backup) is an *orphan*. It is
   left running, its GPUs count as taken, and a queued job with its id is not started until it
   ends. `puzuz status` lists orphans to administrators, `puzuz doctor` warns about them, and an
   administrator can stop one with `puzuz kill <id>`.
4. **Helper → systemd.** Over D-Bus the helper starts a transient service whose main process is
   `puzuz-helper shim`. The unit has `Before=puzuz-helper.service` and `KillMode=mixed`, stdin is
   `/dev/null`, and the stop timeout is `grace_secs`. Each job lives in its own cgroup, outside the
   daemon, the helper and the user's session, so it survives restarts of the daemon and the
   helper and the user logging out. With `gpu_device_isolation` (the default) the unit also has
   `DevicePolicy=closed` and `DeviceAllow=` for the job's GPUs only (see
   [GPU isolation](#gpu-isolation)).
5. **Shim.** Still root, the shim creates a PID namespace and a mount namespace and starts the
   namespace's init, after it triggered the host's automount points (so that the namespace gets
   the automounted filesystems). The init makes the mounts private and mounts a fresh `/proc`. It removes mounts hidden under other mounts and mounts that do not survive a
   reboot (`/run/*`, `/media/*`), replaces each automounted filesystem stacked on its `autofs`
   mount with a plain copy, and mounts private tmpfs on `/tmp` and `/dev/shm`. Unless
   `job_system_dbus = true`, it bind-mounts systemd's inaccessible socket node over the system
   D-Bus socket, so the job cannot ask systemd or polkit for anything; CRIU dumps that mount and
   recreates it on restore (see
   [What a job can and cannot do](install.md#what-a-job-can-and-cannot-do)). Then it drops to
   the owner's
   uid, gid and groups with `no_new_privs`, opens the log
   (`/var/lib/puzuz/logs/<uid>/<id>.log`) and the working directory as the owner, and runs the
   command through `sh -c`. The environment is the client's, plus `CUDA_VISIBLE_DEVICES`,
   `CUDA_DEVICE_ORDER=PCI_BUS_ID` and the fork guard in `LD_PRELOAD`. Signals for the job (TERM,
   INT, HUP, QUIT, USR1, USR2, TSTP, ALRM, WINCH and the other non-fault signals) go to the
   unit's main process, the outer shim (or, after a restore, the init), and are forwarded to the
   job's process group; STOP, CONT and KILL go to every process of the unit. The helper refuses
   any other signal, which would reach the root shim instead of the job.
6. **Checkpoint.** The helper checks free space (for RSS and VRAM, deleted files saved in the
   image and the contents of the job's `/tmp` and `/dev/shm`), the mounts and the job's sockets.
   If a check fails, it refuses the
   dump before touching the job, which keeps running, and the `checkpoint` request fails with the
   reason (a refusal found only when the dump is due is reported with the outcome instead, and the
   daemon warns the owner). In manual mode it then runs
   `cuda-checkpoint --action lock` and `checkpoint` on each CUDA process, followed by
   `criu dump` of the namespace's init with `--tcp-close` and the image directory under
   `/var/lib/puzuz/checkpoints/<id>/`. In plugin mode it runs only `criu dump`, with CRIU's
   default plugin directory, and CRIU's `cuda_plugin.so` calls cuda-checkpoint (in plugin mode a
   CUDA job is refused when the plugin is missing). Before the dump it writes what a restore needs
   besides CRIU's images (the owner record, the job's mounts and files, a draft `meta.json`);
   right after a successful dump a `DUMPED` marker, then the external mounts from CRIU's log,
   the final `meta.json` (including the mode) and the `COMPLETE` marker last. A successful dump
   stops the process, so from then on the image is never deleted: if finishing it fails, the
   helper retries, keeps the image and reports the checkpoint as not finalized (the daemon counts
   the job as checkpointed); it finalizes the image before restoring it or when it starts again,
   also after a crash between the dump and the marker. If the dump fails, CRIU
   leaves the process running and the daemon then terminates it. The helper moves CRIU's
   `dump.log` to `/var/lib/puzuz/checkpoints/failed-logs/` and removes the image directory.
   A **paused** job is resumed (SIGCONT) before the dump, because cuda-checkpoint hangs on a
   stopped process. The daemon records in the checkpoint record that the job was paused, and
   pauses it again (SIGSTOP) as soon as it is restored, or when the dump is refused. The job
   therefore runs for a moment during the dump and right after the restore. `pause` is refused
   while a dump is in progress.
7. **Restore.** The helper claims the job (a delete of its checkpoint is refused from then on)
   and checks, off its request loop and with every file system lookup in a child process with a
   deadline (so that an NFS server that is down cannot hold it up), the GPU model, driver, free
   VRAM and mounts: the paths CRIU
   binds must exist, the filesystems they were on at the dump (recorded in `helper-mounts.json`)
   must be mounted at the same place with the same type (automount points are triggered), and
   the files and working directories the job had open must exist. It starts a
   new unit `puzuz-job-<id>-r<n>.service` that runs `criu restore --root` on a bind mount of `/`
   (`/run/puzuz-helper/criu-root`), with the device access of the GPUs of the restore. In manual
   mode it then runs cuda-checkpoint `restore` and
   `unlock` on the CUDA processes. In plugin mode CRIU's plugin does this before `criu restore`
   returns. Each image is restored in the mode it was dumped in, whatever `cuda_checkpoint_mode`
   says now. The job keeps its id and log, and the image is deleted afterwards.
   CRIU re-opens the job's files as root by their dump-time paths, so the restore is guarded
   against an owner who changes those paths after the dump (see
   [Restores and paths](#restores-and-paths)).

## Restores and paths

CRIU runs as root and reopens, creates or binds everything of a restored job by its dump-time
path. After the dump the owner can change what such a path leads to, for example replace a
directory of theirs with a symlink to `/etc`; without precautions the restored job would then
hold a root file opened with the dump-time mode. The helper therefore:

- **at the dump** refuses a job with Unix sockets bound on the host's filesystems (CRIU binds,
  unlinks and chowns those paths as root on restore; sockets below the job's own `/tmp` are
  fine, and a socket bound to a relative name counts as bound on the host, since `/proc` does
  not tell in which directory), refuses a job with internet sockets that have `SO_REUSEPORT`
  (CRIU creates sockets as root before it restores the job's credentials, so the restored socket
  would be root's and could join the `SO_REUSEPORT` group of a root service on the same port;
  confirmed with CRIU 4.2), saves deleted files on the host's filesystems in the image instead of linking them
  back with `--link-remap` (which CRIU does as root, by path, while the dump runs), and records
  the groups the job runs with;
- **before the restore** refuses it if the owner left one of those groups, if the image has a
  socket bound on a host filesystem, or if a path in the image (`files.img`) can no longer be
  reached by the owner without a symlink on the way (checked as the owner, with
  `openat2(RESOLVE_NO_SYMLINKS)`);
- **before the restored job runs**, checks what CRIU actually opened: CRIU runs
  `puzuz-helper verify-restore-hook` as its `pre-resume` action script (`--action-script`),
  when the restored tasks are complete but still stopped under its control; the hook's child
  process (`puzuz-helper verify-access`) pins every open file, working and root
  directory, executable and file mapping of the tasks through `/proc`, enters the job's
  mount namespace, drops to the owner, and requires that the owner may access each of them the
  way it is open (`faccessat2` on the pinned file) and reaches it by its path, without
  symlinks, as the same inode. It also requires that every task runs as the owner, with groups
  the owner is still in and without capabilities. Files on the job's own mounts (`/proc`, the
  private tmpfs) come from the image and are skipped. A file the job held at the dump only
  needs its path to lead to that very file: a job may hold a file in a way its owner could not
  open it again (created read-only, or on an sshfs mount that shows files as root's), and
  getting that same file back gives it nothing new. The files held are listed before the
  dump and again by `puzuz-helper dump-hook`, CRIU's `post-dump` action script, which runs
  while the dumped tasks are still stopped under CRIU's control (so a file the job opened
  just before CRIU stopped it counts too). It is that file if it has the device and
  inode recorded at the dump, or (after a remount, when device numbers change and FUSE
  filesystems such as sshfs have renumbered their inodes) the owner, group, mode, size,
  modification and change times recorded right after the dump; the change time cannot be set by
  users, and renaming or linking another file there changes it (see
  [Filesystems and mounts](install.md#filesystems-and-mounts)). If the hook finds a problem, it
  kills the tasks while they are
  still stopped (CRIU itself ignores a failed `pre-resume` script), so they never run; the
  restore fails with the reason and the image is kept. If the hook did not run at all, the
  helper kills the restored job as soon as `criu restore` returns.

The last check looks at the objects CRIU opened, so racing it by changing paths does not help.
What remains is the short window in which CRIU itself works on paths as root before that
check: re-creating a deleted file that was saved in the image (a file named
`<name>.cr.<id>.ghost` that exists only for a moment, owned by the job's owner) or creating the
missing directories on its way. An owner who swaps a directory between the pre-check and that
moment can make such a transient file appear in another directory.

After a crash of the helper during a restore, its next start kills whatever the restore unit
runs (it was not checked, and in manual mode its CUDA state may not be restored) and reports the
restore as failed, also when the unit is already gone; the image is kept.

## Other privilege boundaries

- <a id="job-credentials"></a>The helper, not the daemon, decides whose jobs run, so a
  compromised daemon cannot get around it. For every spawn and restore it looks the owner up
  through NSS and refuses uid 0, gid 0, groups the owner is not in, users below `job_min_uid`
  (default `UID_MIN` of `/etc/login.defs`), users missing from a non-empty `job_allowed_users`,
  and a job group listed in `job_denied_groups` (root-equivalent groups such as `docker`, `disk`
  or `sudo`, looked up in `/etc/group`), with an error of code `policy_denied` whose message
  (`puzuz refuses to run jobs as <user>: <reason>`) becomes the job's spawn failure or restore
  error. It computes the job's supplementary groups itself (what `initgroups` would set, without
  denied groups and gid 0) and passes them to the shim; the helper's journal records the groups it
  left out. A restore is refused when the checkpointed processes had a denied
  group, and the check after the restore allows only the owner's groups minus the denied ones. See
  [Who jobs may run as](install.md#who-jobs-may-run-as).
- The helper binds its socket in `/run/puzuz`, which the daemon user owns, through a private
  root-owned staging directory that it uses only by file descriptor. It serves one daemon session
  at a time (a new one replaces the old), at most a few connections waiting for their Hello, and
  bounds the queues and requests in flight of a session.
- The shim works on the job's mount namespace as root before it drops privileges. It moves and detaches mounts (including
  the volatile ones below `/run` and `/media`) only through file descriptors, reached once
  through directories that only root can change and without following symlinks, so the owner
  cannot redirect a mount operation onto a system directory. A mount below a directory that
  someone else can change is left alone. The supplementary groups come from the helper (see
  above), so the shim does not look them up again.
- At start-up the helper looks up programs given by name (`criu`, `cuda-checkpoint`,
  `nvidia-smi`) once in its `PATH`, requires the file found, like a program given by an
  absolute path, to belong to root and to be reached only through directories that only root can
  change, and uses that absolute path from then on.
- <a id="gpu-isolation"></a>**GPU isolation.** `CUDA_VISIBLE_DEVICES` only tells CUDA which
  GPUs to use; any job could change it. With `gpu_device_isolation = true` (the default) the
  helper gives every job unit, spawned or restored, `DevicePolicy=closed` and a `DeviceAllow=`
  list (read-write): `/dev/nvidia<minor>` of each assigned GPU plus `/dev/nvidiactl`,
  `/dev/nvidia-uvm`, `/dev/nvidia-uvm-tools`, `/dev/nvidia-modeset` and `char-nvidia-uvm`, and
  nothing for a CPU-only job. systemd enforces it with a cgroup device program for every process
  of the unit, root included, so opening any other NVIDIA device fails with `EPERM`; `closed`
  still allows the standard pseudo devices (`/dev/null`, `/dev/zero`, `/dev/full`,
  `/dev/random`, `/dev/urandom`, `/dev/tty`, `/dev/ptmx`, `/dev/pts/*`). GPU indices (PCI bus
  order) are mapped to device minors with the driver's
  `/proc/driver/nvidia/gpus/<PCI address>/information` (`Device Minor`, without excluded GPUs)
  at every spawn and restore. A restore unit gets the GPUs of the restore: `criu restore` and, in
  plugin mode, CRIU's CUDA plugin run inside it, and in manual mode the driver reopens the
  device nodes in the restored processes when the helper runs `cuda-checkpoint --action restore`
  on them. See [GPU isolation](install.md#gpu-isolation) in the installation guide.
- At start-up the helper refuses directories it writes to or loads code from (including
  `<data_dir>/criu-no-plugins`, passed to `criu -L`, and the fork guard's `<data_dir>/lib`) unless
  they and their parents belong to root.

## Shutdown ordering

- **OS shutdown or reboot.** systemd stops units in the reverse order of starting.
  `puzuz.service` runs `After=puzuz-helper.service`, so the daemon stops first. Stopping the daemon
  never stops jobs. `puzuz-helper.service` runs after the local and remote filesystems, D-Bus,
  `nvidia-persistenced` and `nvidia-fabricmanager`, so those are still available while it stops.
  On SIGTERM the helper asks systemd whether the whole system is stopping. If so, it refuses new
  work, dumps every running job one at a time (except `--no-checkpoint` jobs), keeps extending
  its stop timeout up to the end of the running dump's deadline (plus its rollback), and writes
  `shutdown-manifest.json`. Every checkpoint and restore has a deadline that scales with the
  job's size; when it passes, the tool's whole process tree is killed and a checkpoint is rolled
  back (the job is thawed and its CUDA state restored and unlocked; in plugin mode, where the
  killed CRIU's plugin may have left the CUDA processes locked or checkpointed, the helper asks
  `cuda-checkpoint --get-state` and restores and unlocks them as needed). Paused jobs are dumped too
  (resumed first), and come back paused after `puzuz restore`. Job units are
  `Before=puzuz-helper.service`, so systemd stops them only after the helper has finished. On the
  next boot the daemon gets the results from the helper's replayed events and marks the jobs
  `Checkpointed`. Jobs whose dump failed or was refused were stopped by systemd. They are marked
  `Checkpoint failed` with the reason, and their owners are warned.
- **`puzuz shutdown`** (administrators) does not rely on unit ordering. The daemon checkpoints
  running jobs one at a time (or terminates them with `--no-checkpoint`), waits for them to settle,
  and exits. The helper keeps running. A job whose dump fails is terminated. A job whose dump the
  helper refuses is left running, because terminating it would lose the work. The daemon logs a
  warning and the owner is warned. Stop it with `puzuz kill` or `puzuz shutdown --no-checkpoint`
  if needed.
- **`systemctl stop/restart puzuz-helper` or `puzuz`** on its own leaves jobs running.

## Priority and fair share

The order of queued jobs is Slurm's multifactor priority (the formula and settings are in
[docs/install.md](install.md#priority-and-fair-share)). The work is split like the rest of the
scheduler:

- `puzuz-core::priority` is pure: the factors, the formula, the owner's order (top, `-o`
  priority, id; applied by handing out the ages of a user's eligible jobs in that order), the
  classic fair-share
  factor `2^(-U/S)`, the decay, the billing of a job and the reset-period boundaries (computed
  from the local UTC offset the daemon passes in). `scheduler::plan_prioritized` orders the
  candidates with it and then applies the usual slot, pause and dependency rules unchanged, so a
  high-priority job that does not fit never blocks jobs behind it. `plan`/`plan_with` keep pueue's
  order (also used for `priority_type = "basic"`). `scheduler::pending_priorities` (behind
  `puzuz prio`) uses the same candidate selection, so it lists what the plan considers.
- The storage keeps `jobs.nice`, `jobs.site_factor`, `jobs.top_seq`, `jobs.run_since`, `groups.priority`, the
  `user_shares` and `user_usage` tables and, in `meta`, when usage was last calculated and reset
  (schema version 6).
- Only running time is billed. Every state change goes through one place in the daemon
  (`Core::apply_in_tx`), which sets `jobs.run_since` when a job starts running and, when it stops
  running, charges the time since to its owner's usage in the same transaction. A usage task runs
  every `priority_calc_period_secs`: in one transaction it decays every user's usage by the time
  since the last calculation, charges running jobs up to now (moving their `run_since`), applies a
  due reset and recomputes the fair-share factors, which the daemon caches. Scheduling passes use
  the cache and do not read the usage tables. One charge covers at most two periods, so daemon
  downtime is not billed.
- The API adds `GET /v1/priority` (pending jobs with their factors, no commands, for everyone),
  `GET /v1/shares`, `PUT /v1/shares/{uid}` and `PUT /v1/groups/{name}/priority` (administrators),
  and `POST /v1/jobs/nice`, `/v1/jobs/top` and `/v1/jobs/site`, which report per job like the
  bulk endpoint. Negative nice values, site factors, shares and group priorities are checked in
  `puzuz-core::authz`.

## State

| Data | Location | Owner |
| --- | --- | --- |
| jobs, groups, GPU slot limits, checkpoint records, pending warnings, fair-share shares and usage | `/var/lib/puzuz/daemon/state.sqlite` (WAL with `synchronous=FULL`, so an acknowledged helper event survives a power failure; schema versioned with `PRAGMA user_version`) | daemon |
| job records, event queue | `/var/lib/puzuz/helper/` | helper |
| job output | `/var/lib/puzuz/logs/<uid>/<id>.log` | the helper creates the directory (owned by the user, group `puzuz`, mode 2770). The shim writes the log as the owner. The daemon reads and truncates it without following symlinks and only if it is a regular file owned by the job's owner. |
| checkpoint images | `/var/lib/puzuz/checkpoints/<id>/` | helper (root only) |
| configuration | `/etc/puzuz/config.toml` | root |

Only the daemon writes the database. The CLI always goes through the daemon.

## Crate layout

| Crate | Contents |
| --- | --- |
| `crates/puzuz` | the `puzuz` binary: pueue-compatible CLI, `puzuz daemon`, `service`, `doctor`, `update` (the only crate with an HTTPS client) |
| `crates/puzuz-core` | domain model, job lifecycle, scheduling decisions, authorization, SQLite storage (no other I/O) |
| `crates/puzuz-protocol` | wire types: CLI ⇄ daemon (HTTP API and client), daemon ⇄ helper (NDJSON messages) |
| `crates/puzuz-config` | the `/etc/puzuz/config.toml` schema, shared by the daemon, the helper (with root-ownership checks) and `doctor` |
| `crates/puzuz-daemon` | the daemon: axum API, scheduler loop, helper client, GPU detection (NVML) |
| `crates/puzuz-checkpoint` | CRIU and cuda-checkpoint command plans, execution with timeouts and rollback, image layout |
| `crates/puzuz-helper` | the `puzuz-helper` binary: socket server, job engine, systemd launcher, shim, mount analysis, fork guard (`forkguard/forkguard.c`, embedded in the binary) |

The systemd units, sysusers.d and tmpfiles.d snippets and the default configuration live in
[`packaging/`](../packaging) and are embedded in the `puzuz` binary for `puzuz service install`.
The AUR package (`packaging/aur/puzuz-bin`) installs the same files unchanged.
