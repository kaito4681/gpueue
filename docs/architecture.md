# Architecture

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
2. **Scheduling.** A queued job starts when its group has a free slot and every requested GPU has
   a free slot. The daemon then asks the helper to spawn it.
3. **Daemon → helper.** The daemon keeps one connection to `/run/puzuz/helper.sock` and exchanges
   newline-delimited JSON. A Hello message checks the protocol version first. Then come requests
   with ids, and numbered events from the helper (job exited, checkpoint or restore finished).
   Events are persisted and replayed until the daemon acknowledges them, so nothing is lost while
   the daemon is down. After a reconnect both sides reconcile their lists of jobs.
4. **Helper → systemd.** Over D-Bus the helper starts a transient service whose main process is
   `puzuz-helper shim`. The unit has `Before=puzuz-helper.service` and `KillMode=mixed`, stdin is
   `/dev/null`, and the stop timeout is `grace_secs`. Each job lives in its own cgroup, outside the
   daemon, the helper and the user's session, so it survives restarts of the daemon and the
   helper and the user logging out.
5. **Shim.** Still root, the shim creates a PID namespace and a mount namespace and starts the
   namespace's init. The init makes the mounts private and mounts a fresh `/proc`. It removes
   mounts hidden under other mounts and mounts that do not survive a reboot (`/run/*`, `/mnt/*`,
   `/media/*`), and mounts private tmpfs on `/tmp` and `/dev/shm`. Then it drops to the owner's
   uid, gid and groups with `no_new_privs`, opens the log
   (`/var/lib/puzuz/logs/<uid>/<id>.log`) and the working directory as the owner, and runs the
   command through `sh -c`. The environment is the client's, plus `CUDA_VISIBLE_DEVICES`,
   `CUDA_DEVICE_ORDER=PCI_BUS_ID` and the fork guard in `LD_PRELOAD`.
6. **Checkpoint.** The helper checks free space and the mounts. If a check fails, it refuses the
   dump before touching the job, which keeps running, and the `checkpoint` request fails with the
   reason (a refusal found only when the dump is due is reported with the outcome instead, and the
   daemon warns the owner). In manual mode it then runs
   `cuda-checkpoint --action lock` and `checkpoint` on each CUDA process, followed by
   `criu dump` of the namespace's init with `--tcp-close` and the image directory under
   `/var/lib/puzuz/checkpoints/<id>/`. In plugin mode it runs only `criu dump`, with CRIU's
   default plugin directory, and CRIU's `cuda_plugin.so` calls cuda-checkpoint (in plugin mode a
   CUDA job is refused when the plugin is missing). It writes `meta.json` (including the mode)
   and the `COMPLETE` marker last. A successful dump stops the process. If the dump fails, CRIU
   leaves the process running and the daemon then terminates it. The helper moves CRIU's
   `dump.log` to `/var/lib/puzuz/checkpoints/failed-logs/` and removes the image directory.
   A **paused** job is resumed (SIGCONT) before the dump, because cuda-checkpoint hangs on a
   stopped process. The daemon records in the checkpoint record that the job was paused, and
   pauses it again (SIGSTOP) as soon as it is restored, or when the dump is refused. The job
   therefore runs for a moment during the dump and right after the restore. `pause` is refused
   while a dump is in progress.
7. **Restore.** The helper checks the GPU model, driver, free VRAM and mount paths. It starts a
   new unit `puzuz-job-<id>-r<n>.service` that runs `criu restore --root` on a bind mount of `/`
   (`/run/puzuz-helper/criu-root`). In manual mode it then runs cuda-checkpoint `restore` and
   `unlock` on the CUDA processes. In plugin mode CRIU's plugin does this before `criu restore`
   returns. Each image is restored in the mode it was dumped in, whatever `cuda_checkpoint_mode`
   says now. The job keeps its id and log, and the image is deleted afterwards.

## Shutdown ordering

- **OS shutdown or reboot.** systemd stops units in the reverse order of starting.
  `puzuz.service` runs `After=puzuz-helper.service`, so the daemon stops first. Stopping the daemon
  never stops jobs. `puzuz-helper.service` runs after the local and remote filesystems, D-Bus,
  `nvidia-persistenced` and `nvidia-fabricmanager`, so those are still available while it stops.
  On SIGTERM the helper asks systemd whether the whole system is stopping. If so, it refuses new
  work, dumps every running job one at a time (except `--no-checkpoint` jobs), extends its stop
  timeout before each job, and writes `shutdown-manifest.json`. Paused jobs are dumped too
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

## State

| Data | Location | Owner |
| --- | --- | --- |
| jobs, groups, GPU slot limits, checkpoint records, pending warnings | `/var/lib/puzuz/daemon/state.sqlite` (WAL, schema versioned with `PRAGMA user_version`) | daemon |
| job records, event queue | `/var/lib/puzuz/helper/` | helper |
| job output | `/var/lib/puzuz/logs/<uid>/<id>.log` | the helper creates the directory. The shim writes the log as the owner. |
| checkpoint images | `/var/lib/puzuz/checkpoints/<id>/` | helper (root only) |
| configuration | `/etc/puzuz/config.toml` | root |

Only the daemon writes the database. The CLI always goes through the daemon.

## Crate layout

| Crate | Contents |
| --- | --- |
| `crates/puzuz` | the `puzuz` binary: pueue-compatible CLI, `puzuz daemon`, `service`, `doctor` |
| `crates/puzuz-core` | domain model, job lifecycle, scheduling decisions, authorization, SQLite storage (no other I/O) |
| `crates/puzuz-protocol` | wire types: CLI ⇄ daemon (HTTP API and client), daemon ⇄ helper (NDJSON messages) |
| `crates/puzuz-config` | the `/etc/puzuz/config.toml` schema, shared by the daemon, the helper (with root-ownership checks) and `doctor` |
| `crates/puzuz-daemon` | the daemon: axum API, scheduler loop, helper client, GPU detection (NVML) |
| `crates/puzuz-checkpoint` | CRIU and cuda-checkpoint command plans, execution with timeouts and rollback, image layout |
| `crates/puzuz-helper` | the `puzuz-helper` binary: socket server, job engine, systemd launcher, shim, mount analysis, fork guard (`forkguard/forkguard.c`, embedded in the binary) |

The systemd units, sysusers.d and tmpfiles.d snippets and the default configuration live in
[`packaging/`](../packaging) and are embedded in the `puzuz` binary for `puzuz service install`.
