# Architecture

English | [日本語](ja/architecture.md) | [简体中文](zh-CN/architecture.md)

## Components

| Process | Runs as | Role |
| --- | --- | --- |
| `gpueue` (CLI) | the calling user | sends requests to the daemon over HTTP on a Unix socket |
| `gpueue daemon` (`gpueue.service`) | system user `gpueue`, sandboxed, no capabilities | HTTP API, authorization, scheduler, SQLite state. Decides when jobs start, stop, checkpoint and restore. |
| `gpueue-helper` (`gpueue-helper.service`) | root | starts jobs as their owner, signals them, and runs CRIU and cuda-checkpoint. Accepts connections only from the daemon's user. |
| job units (`gpueue-job-<id>.service`, `gpueue-job-<id>-r<n>.service` after the n-th restore) | root shim, then the job owner | one transient systemd service per job |

The HTTP server and the database run unprivileged. The root helper has a small dependency tree (no
HTTP server, no SQLite) and validates every request itself: the uid (never 0), the working
directory and log opened with the owner's permissions, and image paths below its checkpoint
directory.

## How a job flows

```
gpueue add ──HTTP /v1 over /run/gpueue/gpueue.sock──▶ gpueue daemon ──NDJSON over /run/gpueue/helper.sock──▶ gpueue-helper
  (SO_PEERCRED identifies the caller)                (SQLite, scheduler)                                   │ D-Bus
                                                                                                           ▼
                                                                         systemd: gpueue-job-<id>.service ─▶ shim ─▶ sh -c '<command>'
```

1. **CLI → daemon.** The CLI sends REST/JSON requests under `/v1` (for example `POST /v1/jobs`) to
   `/run/gpueue/gpueue.sock`, with an `x-gpueue-version` header. The daemon reads the caller's uid
   from `SO_PEERCRED`, checks authorization and stores the job. Logs are streamed as raw bytes and
   state changes as Server-Sent Events (`/v1/events`). Pending warnings, such as a failed dump, come
   back in an `x-gpueue-warnings` header and the CLI prints them on stderr.
   <a id="api-limits"></a>Every user can reach the socket, so the daemon limits what one user keeps
   open: at most 64 connections and 16 streams (`log --follow`, `/v1/events`) per uid, and 4096
   connections in total (`gpueue.service` raises `LimitNOFILE` well above that). A request over a
   limit is answered with `too_many_requests`. A connection must send its request headers within
   20 seconds, and an idle keep-alive connection is closed after 20 seconds. A job operation (`add`,
   job actions, bulk actions, `clean`, `reset`) runs to completion even if its client goes away,
   and keeps counting as one of the caller's connections until it has, so abandoned operations
   cannot pile up. `log --tail` looks
   for line starts in the last 4 MiB of the log at most (the output starts there if they hold
   fewer lines), and only a few such scans run at once. What one user stores is limited too: at
   most `max_unfinished_jobs_per_user` jobs that have not finished (`add` and a `restart` that
   brings a job back are refused with `too_many_requests` beyond it), and at most
   `max_job_spec_bytes` for a job's command, working directory, label and environment together
   (refused with `bad_request`; each dependency counts 8 bytes, and a job may depend on at most 1000 jobs). A `/v1/events` subscriber that falls too far behind gets a
   `resync` event instead of the events it missed; `gpueue wait` then reads the job list again.
2. **Scheduling.** A queued job starts when its group has a free slot and every requested GPU has
   a free slot. Candidates are considered in priority order (see
   [Priority and fair share](#priority-and-fair-share)). A scheduling pass reads the jobs that
   have not finished without their commands and environments, and only the jobs it launches in
   full. The daemon then asks the helper to spawn
   it (at most 8 spawn or restore requests at a time; the job is recorded as running before its
   request waits for its turn). Immediate launches (`start <id>`, `add -i`, `restart -i`, `restore -i`) by an
   administrator are marked as forced, in memory, and launched at once, ignoring every limit.
   Anybody else's are not forced: the action queues the job (or queues it for restore) and, in
   the same transaction, moves it first among the owner's jobs like `gpueue top`
   (`Core::queue_first`; not with `enable_user_top = false` or `priority_type = "basic"`). The
   plan then treats it like any queued job; the reply's `queued` says so.
3. **Daemon → helper.** The daemon keeps one connection to `/run/gpueue/helper.sock` and exchanges
   newline-delimited JSON. A Hello message checks the protocol version first. Then come requests
   with ids, and numbered events from the helper (job exited, checkpoint or restore finished).
   Events are persisted and replayed until the daemon acknowledges them, so nothing is lost while
   the daemon is down. The daemon stores the number of the last event it applied with its effect
   and skips replayed events up to it, so an event is applied once even if the daemon stopped
   before acknowledging it. An event the daemon cannot record (its database is busy, full or
   failing) is not acknowledged: the daemon tries it again, waiting longer each time, and applies
   the events after it only once it is recorded. The helper's Hello carries the epoch of its
   event numbering, which changes when the helper's state is wiped and its numbering starts
   over; the daemon then applies the helper's events as new, whatever their numbers. (From a
   helper that reports no epoch, a replay that ends below the last number applied is taken for
   such a reset.) An event it cannot decode (from a newer helper) is logged and
   acknowledged rather than ending the connection. After a reconnect both sides reconcile their
   lists of jobs: a job the helper no longer runs is lost, and a job the daemon believes is being
   checkpointed but the helper runs normally lost its checkpoint request and goes back to running
   (or paused) with a failed checkpoint. Should the outcome of that dump arrive after all (the
   helper resumes a job before it reports a failed dump), it is applied: a dump CRIU attempted
   terminates the job, as after any failed dump, and a refused one leaves it alone. The daemon
   launches jobs only once it has reconciled the current connection; should the helper not
   answer its request for the live jobs, it asks again. If that checkpoint request is still unanswered, or was
   answered since the previous reconnect, the list may predate it, so the daemon checks again once
   the answer is in. A checkpoint request that times out is followed up the same way. Before the daemon marks a job lost because the helper no longer knows it, it applies
   every event the helper sent before saying so, so a job that has just exited keeps its result.
   <a id="orphans"></a>A job the helper runs that the daemon has as finished, queued, checkpointed
   or not at all (for example after restoring the database from a backup) is an *orphan*. It is
   left running, its GPUs count as taken, and a queued job with its id is not started until it
   ends. `gpueue status` lists orphans to administrators, `gpueue doctor` warns about them, and an
   administrator can stop one with `gpueue kill <id>`.
4. **Helper → systemd.** Over D-Bus the helper starts a transient service whose main process is
   `gpueue-helper shim`, run from a content-addressed copy of the helper binary
   (`<data_dir>/lib/gpueue-helper-<hash>`, see [Helper binary copies](#helper-binary-copies)). The
   unit's description names the job id and the owner's uid only (not the command line, since
   unit descriptions are readable by every local user and logged). Its capability bounding set
   is limited to what the shim needs before it drops privileges (`CAP_SYS_ADMIN`, `CAP_SETUID`,
   `CAP_SETGID`, `CAP_SETPCAP`, `CAP_DAC_OVERRIDE`, `CAP_DAC_READ_SEARCH`, `CAP_KILL`); once the
   namespace init is started, the outer shim keeps `CAP_KILL` alone (to forward signals to the
   init, which then runs as the owner). The unit has `Before=gpueue-helper.service`, `After=remote-fs.target network.target` (ordering
   only) and `KillMode=mixed`, stdin is `/dev/null`, and the stop timeout is `grace_secs`. Each job lives in its own cgroup, outside the
   daemon, the helper and the user's session, so it survives restarts of the daemon and the
   helper and the user logging out. With `gpu_device_isolation` (the default) the unit also has
   `DevicePolicy=closed` and `DeviceAllow=` for the job's GPUs only (see
   [GPU isolation](#gpu-isolation)).
5. **Shim.** Still root, the shim creates a PID namespace and a mount namespace and starts the
   namespace's init, after it triggered the host's automount points (so that the namespace gets
   the automounted filesystems). The init makes the mounts private and mounts a fresh `/proc`. It removes mounts hidden under other mounts and mounts that do not survive a
   reboot (`/run/*`, `/media/*`) or that belong to other containers (below `/var/lib/docker/`,
   `/var/lib/containers/`, `/var/lib/containerd/`, `/var/lib/kubelet/pods/` and the Docker snap's
   and k3s's equivalents; they come and go with their containers), replaces each automounted filesystem stacked on its `autofs`
   mount with a plain copy, and mounts private tmpfs on `/tmp` and `/dev/shm`, after detaching
   whatever is mounted at or below them (the host's tmpfs `/tmp` and mounts on it, such as an
   AppImage's FUSE mount, which would otherwise stay hidden in the namespace). A mount it cannot
   reach that way, because it lies below a directory someone other than root can change (a
   user's FUSE mount in `/tmp/<their dir>/` when `/tmp` is a plain directory), is removed after
   the private tmpfs is mounted, by setting that aside as for other hidden mounts. A hidden mount
   whose covering mount it cannot set aside that way stays (the job then cannot be checkpointed);
   the other hidden mounts are still removed. Unless
   `job_system_dbus = true`, it hides `/run/dbus` behind an empty tmpfs and, unless
   `job_systemd_varlink = true`, hides `/run/systemd` behind a tmpfs that holds only journald's log
   sockets, resolved's resolution socket and `resolv.conf` files, and `userdb/`, so the job cannot
   ask systemd or polkit for anything over D-Bus or Varlink, also once new sockets appear there.
   It hides the directories of `job_hidden_socket_dirs` (by default `/run/libvirt` and
   `/run/cups`, whose daemons check the caller's groups in the user database) behind empty
   tmpfs the same way, creating missing ones first, each reached without following symlinks
   through directories that only root can change (the helper refuses to start if a directory
   on the way of an entry is not root's or is group- or world-writable);
   CRIU dumps these mounts and recreates them on restore (see
   [What a job can and cannot do](install.md#what-a-job-can-and-cannot-do)). Then it drops to
   the owner's
   uid, gid and groups with `no_new_privs`, opens the log
   (`/var/lib/gpueue/logs/<uid>/<id>.log`) and the working directory as the owner, and runs the
   command through `sh -c`. The environment is the client's, plus `CUDA_VISIBLE_DEVICES`,
   `CUDA_DEVICE_ORDER=PCI_BUS_ID` and the fork guard in `LD_PRELOAD`. Signals for the job (TERM,
   INT, HUP, QUIT, USR1, USR2, TSTP, ALRM, WINCH and the other non-fault signals) go to the
   unit's main process, the outer shim (or, after a restore, the init), and are forwarded to the
   job's process group; STOP, CONT and KILL go to every process of the unit. The helper refuses
   any other signal, which would reach the root shim instead of the job. The init reads the
   signals it forwards from a signalfd and keeps them blocked, but starts the command (with
   `posix_spawn`) with no signal blocked and every signal at its default action, as a login
   shell or a systemd service would, so a program that the shell execs (`exec python3 train.py`)
   gets SIGTERM and SIGINT too. gpueue 0.1.2 and earlier passed the blocked signals on (see
   [Upgrading](install.md#upgrading)).
6. **Checkpoint.** The helper first refuses every dump when the configured `criu` was missing,
   could not be run or was older than 4.2 when it started (it runs `criu --version` once at
   start-up; an output without a version is only warned about): such a dump would fail and end
   the job. Then it checks free space (for RSS and VRAM, deleted files saved in the
   image and the contents of the job's `/tmp` and `/dev/shm`), the mounts and the job's sockets.
   FUSE and network mounts of the job's namespace that the host no longer has, whose `statfs`
   fails with a connection error or does not return within 5 s (asked in a child process), or
   that refuse root (`lstat` fails with `EACCES`: a FUSE mount without `allow_other`) are
   dead, and so is any mount that CRIU could only bind again from a host path below a directory
   that a job's owner could change (see [Restores and paths](#restores-and-paths)): a job with an open file, its working or root directory, its executable, a mapping or a
   bound Unix socket on one is refused with a reason that names the mount. For any other job the
   helper passes them to CRIU as external mounts with keys of its own
   (`--external mnt[<mount point>]:gpueue-detached-<n>`, mounts below them with `--skip-mnt`) and
   records them in `helper-mounts.json`, with whether each mount point is a file (a bind mount
   of a file or socket) and whether it lies in a tmpfs of the job's own (whose contents, mount
   points included, CRIU saves in the image), so that one dead filesystem does not keep every job
   from being checkpointed (see
   [Network and FUSE filesystems that stop answering](install.md#network-and-fuse-filesystems-that-stop-answering)).
   A job with processes in a PID or user namespace of their own (`unshare`, `bwrap`, a rootless
   container runtime), found among the descendants of the namespace's init, is refused: CRIU
   cannot dump nested PID and user namespaces.
   Every lookup of the job's files in these checks runs in a child process with a deadline, and
   the checks of a `checkpoint` request give up after 45 s (a stopping helper waits for them no
   longer). If a check fails, it refuses the
   dump before touching the job, which keeps running, and the `checkpoint` request fails with the
   reason (a refusal found only when the dump is due is reported with the outcome instead, and the
   daemon warns the owner). CRIU's `post-dump` action script (`gpueue-helper dump-hook`) runs the
   same checks of the job's files again while CRIU still has the dumped tasks stopped; if they find
   something to refuse (the job opened it after the first checks), or do not finish in time, the
   script fails, CRIU abandons the dump and resumes the job, and the checkpoint is reported as
   refused with the reason. In manual mode it then runs
   `cuda-checkpoint --action lock` and `checkpoint` on each CUDA process, followed by
   `criu dump` of the namespace's init with `--tcp-close` and the image directory under
   `/var/lib/gpueue/checkpoints/<id>/`. In plugin mode it runs only `criu dump`, with CRIU's
   default plugin directory, and CRIU's `cuda_plugin.so` calls cuda-checkpoint (in plugin mode a
   CUDA job is refused when the plugin is missing). Before the dump it writes what a restore needs
   besides CRIU's images (the owner record, the job's mounts and files, a draft `meta.json`);
   right after a successful dump it flushes the images to disk (CRIU does not fsync them, and the
   processes are gone) and writes a `DUMPED` marker, then the external mounts from CRIU's log,
   the final `meta.json` (including the mode) and the `COMPLETE` marker last. A successful dump
   stops the process, so from then on the image is never deleted: if finishing it fails, the
   helper retries, keeps the image and reports the checkpoint as not finalized (the daemon counts
   the job as checkpointed); it finalizes the image before restoring it or when it starts again,
   also after a crash between the dump and the marker, even if systemd still reports the dumped
   unit as active. If the dump fails, CRIU
   leaves the process running and the daemon then terminates it. A failed dump with an error
   code the daemon does not know (from a newer helper) counts as refused: the job keeps running. The helper moves CRIU's
   `dump.log` to `/var/lib/gpueue/checkpoints/failed-logs/` and removes the image directory.
   A **paused** job is resumed (SIGCONT) before the dump, because cuda-checkpoint hangs on a
   stopped process, so it runs for a moment during the dump. The daemon records in the
   checkpoint record that the job was paused and asks the helper to restore it stopped (see
   step 7), so the restored job runs nothing until `gpueue start`. When the dump is refused, the
   daemon pauses it again (SIGSTOP). `pause` is refused while a dump is in progress.
7. **Restore.** The helper claims the job (a delete of its checkpoint is refused from then on)
   and checks, off its request loop and with every file system lookup in a child process with a
   deadline (so that an NFS server that is down cannot hold it up), the GPU model, driver, free
   VRAM and mounts: the paths CRIU
   binds must exist, the filesystems they were on at the dump (recorded in `helper-mounts.json`)
   must be mounted at the same place with the same type (automount points are triggered), and
   the files and working directories the job had open must exist. A mount left out at the dump
   is bound from the host's mount at the same place if a filesystem of the same type is mounted
   there again and answers (for a bind of a local filesystem's host path left out because the
   path seemed redirectable, if the path is on a filesystem of the same type again), and
   otherwise from an empty, root-owned directory of mode 0
   (`/var/lib/gpueue/checkpoints/detached-mount`), or for a mount of a file or socket an empty,
   root-owned file of mode 0 (`detached-mount-file` there); its mount point must exist on the
   host (a directory, or a file), unless it lies in a tmpfs of the job's own. It starts a
   new unit `gpueue-job-<id>-r<n>.service` that runs `criu restore --root` on a bind mount of `/`
   (`/run/gpueue-helper/criu-root`), with the device access of the GPUs of the restore. In manual
   mode it then runs cuda-checkpoint `restore` and
   `unlock` on the CUDA processes. In plugin mode CRIU's plugin does this before `criu restore`
   returns. Each image is restored in the mode it was dumped in, whatever `cuda_checkpoint_mode`
   says now. The job keeps its id and log, and the image is deleted afterwards (should that
   fail, when the job exits or is checkpointed again, or when the helper starts again; an image
   the job was restored from never refuses its next dump).
   A job that was paused at the dump is restored **stopped**, as `gpueue pause` leaves it: the
   restore hook (see [Restores and paths](#restores-and-paths)) sends SIGSTOP to its processes
   while CRIU still holds them, after CRIU's plugin has restored their CUDA state, so they enter
   the stop before they run a single instruction. (CRIU's own `--leave-stopped` stops them before
   the plugin runs, which then fails.) In manual mode the helper then runs cuda-checkpoint with
   only the CUDA restore threads of the processes running, as CRIU's plugin does: it attaches to
   these threads with `ptrace`, lets them run, holds back any signal that reaches them (it is sent
   to the process again afterwards, so it stays pending), and stops and detaches them again; the
   rest of the job stays stopped throughout. It attaches only once the processes are fully
   stopped and only to threads of those processes in the job's cgroup (checked once attached),
   never blocks waiting for a thread, and kills the job if a thread does not stop again within a
   few seconds. Should any thread of the job still run after the restore, the restore fails: the
   job is killed and the image kept. The helper reports the job as restored stopped and the
   daemon sends it no signal. A helper that predates this resumes the job anyway, and the
   daemon then stops it (SIGSTOP).
   CRIU re-opens the job's files as root by their dump-time paths, so the restore is guarded
   against an owner who changes those paths after the dump (see
   [Restores and paths](#restores-and-paths)).

## Restores and paths

CRIU runs as root and reopens, creates or binds everything of a restored job by its dump-time
path. After the dump the owner can change what such a path leads to, for example replace a
directory of theirs with a symlink to `/etc`; without precautions the restored job would then
hold a root file opened with the dump-time mode. The helper therefore:

- **at the dump** refuses a job whose namespace has a mount whose root or mount point is not
  UTF-8 or contains a control character (the kernel escapes only space, tab, newline and
  backslash in mountinfo, and CRIU 4.2 splits its fields at any ASCII whitespace and unescapes
  only space, tab and backslash, so it would dump such a mount at another path), refuses a job
  with Unix sockets bound on the host's filesystems (CRIU binds,
  unlinks and chowns those paths as root on restore; sockets below the job's own `/tmp` are
  fine, and a socket bound to a relative name counts as bound on the host, since `/proc` does
  not tell in which directory). The kernel keeps a socket's name as the job gave it to
  `bind(2)` and CRIU binds that very name, so a name only counts as the job's own if it is
  absolute and normalized (no `.`, `..` or empty components), not below `/proc`, below one of
  the private tmpfs mounts, and if the directory it is in is reached from that mount, by
  descriptor from the job's root, without symlinks and without crossing into another mount
  (`openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_XDEV)`). It also refuses a job
  with Unix sockets listening on abstract names (CRIU sets them up as root, so a client asking
  who listens, with `SO_PEERCRED`, would be told root; non-listening abstract sockets are fine),
  except libcuda's: every process that initialises CUDA listens on
  `\0cuda-uvmfd-<inode of its pid namespace>-<its pid there>\0` (`SOCK_SEQPACKET`; libcuda in
  another process of the same pid namespace connects to it by that name, sends its credentials
  with `SCM_CREDENTIALS` and gets the listener's `/dev/nvidia-uvm` descriptor; libcuda never calls
  `getsockopt`, so `SO_PEERCRED` misleads nobody), and cuda-checkpoint keeps it while it closes
  the `/dev/nvidia*` descriptors. Such a listener passes if one process holding it has libcuda
  mapped and the name has exactly that process's pid in its pid namespace (both the scan before
  the dump and the post-dump hook's). The inode is not compared: a restored process keeps the
  listener named after the pid namespace of its first run, and is in a new one. A job could
  name a socket of its own like that, but
  only a libcuda client of that pid namespace would ever connect to it, and those do not look
  at the listener's credentials. The restored name keeps the dump-time inode, which the kernel
  may give to another pid namespace later: while the restored job runs, a process there with
  the same pid cannot bind its own listener, and libcuda's `cuInit` then fails with
  `CUDA_ERROR_OPERATING_SYSTEM` (and a restore fails if such a process holds the name). Anyone
  on the host can take such a name already (abstract names have no permissions, and jobs share
  the host's network namespace), and gpueue 0.1.3 restored these listeners the same way,
  a job with inotify or fanotify descriptors (CRIU re-adds their watches as root, by paths that
  it follows symlinks in), and, in the post-dump hook, a dump with descriptors in flight in
  Unix socket messages (`SCM_RIGHTS` in `sk-queues.img`; CRIU re-opens them as root and hands
  them to the job, out of sight of the restore hook, so the hook fails the dump and the job
  keeps running). It refuses a job with internet sockets that have `SO_REUSEPORT`
  (CRIU creates sockets as root before it restores the job's credentials, so the restored socket
  would be root's and could join the `SO_REUSEPORT` group of a root service on the same port;
  confirmed with CRIU 4.2; the helper looks at the sockets through `pidfd_getfd`, and refuses
  the dump if it cannot), never passes `--link-remap` (with it CRIU links open files that have
  no name of their own back into their directories, as root, by path, while the dump runs):
  deleted files are saved in the image instead (CRIU's `--ghost-limit` is set to the free space
  of the image base, since the job may create a larger deleted file, such as a PyTorch DataLoader
  batch in `/dev/shm`, after the checks), and a job holding a deleted file that still has
  another hard link, or a file the NFS client renamed to `.nfs<hex>` because it was deleted while
  open, is refused, since CRIU could only link those back. A restore re-creates a deleted file as
  root with the owner and mode it had, so a job holding a deleted file with the setuid or setgid
  bit or file capabilities (a `security.capability` attribute) is refused too: it would get a
  privileged program back. Other deleted files, the owner's or not (a shared library replaced by
  a package upgrade while the job runs, say), are saved: their contents are what the job could
  already read. It records the groups the job runs with. All these checks of the job's
  descriptors are made before the dump and again by the `post-dump` script while CRIU has the
  job stopped, which vetoes the dump (CRIU resumes the job, and the checkpoint is reported as
  refused with every reason found) if the job has opened such a descriptor meanwhile;
- **before the restore** refuses it if the owner left one of those groups, if the image has a
  socket bound on a host filesystem (by the same rules for its name), a listening socket with
  an abstract name (but libcuda's, if `pstree.img` has the pid in its name and the image has
  libcuda mapped), inotify or fanotify descriptors, descriptors in flight (`sk-queues.img`),
  or an internet socket bound to a port that another socket on the host has too (any address
  and family, same protocol; read from `/proc/net/{tcp,udp,udplite}{,6}`): CRIU 4.2 sets
  `SO_REUSEADDR` and `SO_REUSEPORT` on every internet socket it restores, as root, before it
  binds it, so it could join the group of a root service that took the port since the dump.
  The restore hook checks that again once CRIU has bound the sockets, before the job runs (a
  socket of someone else on the port of one of the job's sockets kills the restored job). If a path in the image (`files.img`) can no longer be
  reached by the owner without a symlink on the way (checked as the owner, with
  `openat2(RESOLVE_NO_SYMLINKS)`);
- **for the mounts CRIU binds from the host** (the external mounts of the job's namespace, and
  the host's filesystem in place of a mount left out at the dump): CRIU looks each source up by
  its path, as root, following symlinks on the way (`open_tree(AT_SYMLINK_NOFOLLOW)` only
  protects the last component), and the restore hook checks only the files the tasks hold, not
  their mounts. So every directory on the way to such a source must be one that no job's owner
  can change (owned by root or by a system account below `job_min_uid` other than the daemon's
  user, not group- or world-writable unless sticky, not on a FUSE or network filesystem, whose
  server or another client decides what a lookup there returns), walked without following
  symlinks, and the source itself must not be a symlink (CRIU follows it) nor, in a sticky
  directory such as `/tmp`, an entry of anyone else (who may rename it there) or missing (anyone
  may create it there); no job's owner can then redirect it before CRIU binds it. This assumes
  that no job's owner can act as a system account below `job_min_uid` other than through gpueue
  (which never runs a job as one): an owner who can run code as such an account by other means
  (a `sudo` rule, a service they control) could change its directories, so keep `job_min_uid`
  above every account that users can act as. No job runs as
  such a system account, so its directories are as safe as root's: systemd-resolved's
  `/run/systemd/resolve` (owned by `systemd-resolve`), from which the shim binds resolved's
  socket and `resolv.conf` files into every job, passes. (gpueue 0.1.2 accepted only root's
  directories, left these binds out of every dump and then refused every restore.)
  Otherwise the owner of, say, an `allow_other` FUSE mount in their home could rename a
  directory on the way after the checks and leave a symlink in its place, and the restored job
  would get another host directory bound there, such as one below another user's home it could
  not reach. At the dump such a mount counts as dead (a job that uses it is refused, any other
  job is checkpointed without it); a mount left out is restored as the empty, inaccessible
  directory unless its host path passes the check at the restore; and a restore whose recorded
  external sources fail it is refused;
- **before the restored job runs**, checks what CRIU actually opened: CRIU runs
  `gpueue-helper verify-restore-hook` as its `pre-resume` action script (`--action-script`),
  when the restored tasks are complete but still stopped under its control; the hook's child
  process (`gpueue-helper verify-access`) pins every open file, working and root
  directory, executable and file mapping of the tasks through `/proc`, enters the job's
  mount namespace, drops to the owner, and requires that the owner may access each of them the
  way it is open (`faccessat2` on the pinned file) and reaches it by its path, without
  symlinks, as the same inode. It also requires that every thread of every task runs as the
  owner, with groups the owner is still in, without capabilities, with an empty capability
  bounding set and with `no_new_privs`, and that the namespace init is not dumpable (it holds
  files the shim opened as root, so its owner must not be able to ptrace it; CRIU restores the
  flag it had at the dump). Files on the job's own mounts (`/proc`, the
  private tmpfs) come from the image and are skipped. A file the job held at the dump only
  needs its path to lead to that very file: a job may hold a file in a way its owner could not
  open it again (created read-only, or on an sshfs mount that shows files as root's), and
  getting that same file back gives it nothing new. The files held are listed before the
  dump and again by `gpueue-helper dump-hook`, CRIU's `post-dump` action script, which runs
  while the dumped tasks are still stopped under CRIU's control (so a file the job opened
  just before CRIU stopped it counts too). It is that file if it has the device and
  inode recorded at the dump, or (only after a remount, when the device number changed, or on a
  FUSE or network filesystem, whose inode numbers are not stable: sshfs renumbers them) the owner,
  group, mode, size, modification and change times, and the birth time where the filesystem
  reports one, recorded right after the dump; the change time cannot be set by
  users, and renaming or linking another file there changes it (see
  [Filesystems and mounts](install.md#filesystems-and-mounts)). The helper looks these
  attributes up in a child process with a deadline, so that a filesystem that does not answer
  cannot hold it up (a file not looked up in time is recorded without them, and only its device
  and inode identify it). If the hook finds a problem, it kills the tasks while they are
  still stopped (CRIU itself ignores a failed `pre-resume` script), so they never run; the
  restore fails with the reason and the image is kept. If the check passes and the job was
  paused at the dump, the hook stops the tasks (SIGSTOP) instead of letting them run. If the hook did not run at all, the
  helper kills the restored job as soon as `criu restore` returns.

The last check looks at the objects CRIU opened, so racing it by changing paths does not help.
What remains is the short window in which CRIU itself works on paths as root before that
check: re-creating a deleted file that was saved in the image (a file named
`<name>.cr.<id>.ghost` that exists only for a moment, with the owner and mode the deleted file
had; the dump made sure it has no setuid or setgid bit) or creating the
missing directories on its way. An owner who swaps a directory between the pre-check and that
moment can make such a transient file appear in another directory.

After a crash of the helper during a restore, its next start kills whatever the restore unit
runs (it was not checked, and in manual mode its CUDA state may not be restored) and reports the
restore as failed, also when the unit is already gone; the image is kept. If the helper cannot
record that a restore succeeded, it kills the restored job right away, reports the restore as
failed and keeps the image as well.

## Other privilege boundaries

- <a id="job-credentials"></a>The helper, not the daemon, applies the policy on who may run
  jobs, so a compromised daemon cannot get around the policy: it cannot make the helper run
  anything as root, join a group the owner is not in, run as a user the policy refuses, or run
  as a uid outside the range of regular users. What the helper cannot check is who submitted a
  job: it takes the uid, working directory, command and environment from the daemon, so a
  compromised daemon can start any command as any user that passes the policy (and, through the
  signal, checkpoint and restore requests, act on those users' jobs). The authorization of
  clients (who may add, see or change which job) is the daemon's, and so is the job
  environment, which the daemon stores: members of `gpueue-admin` can read every user's job
  environment (including secrets passed in it) through the CLI. A design in which the helper
  verifies the submitter itself is not implemented yet (issue #107). For every spawn and restore it looks the owner up
  through NSS and refuses uid 0, gid 0, uids above `job_max_uid` (when set), 65534, 65535,
  4294967295 and systemd's dynamic-user range 61184-65519, groups the owner is not in, users below `job_min_uid`
  (default `UID_MIN` of `/etc/login.defs`), users missing from a non-empty `job_allowed_users`,
  and a job group listed in `job_denied_groups` (root-equivalent groups such as `docker`, `disk`
  or `sudo`, looked up in `/etc/group`), with an error of code `policy_denied` whose message
  (`gpueue refuses to run jobs as <user>: <reason>`) becomes the job's spawn failure or restore
  error. It computes the job's supplementary groups itself (what `initgroups` would set, without
  denied groups and gid 0) and passes them to the shim; the helper's journal records the groups it
  left out. A restore is refused when the checkpointed processes had a denied
  group, and the check after the restore allows only the owner's groups minus the denied ones. See
  [Who jobs may run as](install.md#who-jobs-may-run-as).
- The helper binds its socket in `/run/gpueue`, which the daemon user owns, through a private
  root-owned staging directory that it uses only by file descriptor. It serves one daemon session
  at a time (a new one replaces the old), at most a few connections waiting for their Hello, and
  bounds the queues and requests in flight of a session.
- The shim works on the job's mount namespace as root before it drops privileges. It moves and detaches mounts (including
  the volatile ones below `/run` and `/media` and other containers' mounts) only through file descriptors, reached once
  through directories that only root can change and without following symlinks, so the owner
  cannot redirect a mount operation onto a system directory. A mount below a directory that
  someone else can change is left alone. The supplementary groups come from the helper (see
  above), so the shim does not look them up again.
- At start-up the helper looks up programs given by name (`criu`, `cuda-checkpoint`,
  `nvidia-smi`) once in its `PATH`, requires the file found, like a program given by an
  absolute path, to belong to root and to be reached only through directories that only root can
  change, and uses that absolute path from then on. A program it did not find stays unavailable
  until the helper is restarted; the helper names it in its Hello, together with the paths of the
  programs it found, and the daemon passes that on to `gpueue doctor` (which reports a program
  installed, removed or made replaceable since). Without cuda-checkpoint, or when a process maps an NVIDIA device but
  `cuda-checkpoint --get-state` cannot tell its state, the dump of a GPU job is refused rather
  than left to CRIU, which cannot dump the device mappings.
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
  `<data_dir>/criu-no-plugins`, passed to `criu -L`, and `<data_dir>/lib`, which holds the fork
  guard and the helper copies) unless they and their parents belong to root.

<a id="helper-binary-copies"></a>**Helper binary copies.** A job's process tree contains the helper
(the namespace init is `gpueue-helper shim-init`), so every checkpoint image maps the helper's
executable, and CRIU restores it only if the file at that path has the same size and build ID.
Package upgrades, `install.sh` and `gpueue update` replace the installed binary in place, which
made jobs dumped before an upgrade unrestorable and jobs running across one undumpable (CRIU
cannot dump a deleted executable of that size). At start-up the helper therefore copies itself to
`<data_dir>/lib/gpueue-helper-<FNV-1a hash of the contents>` (mode 0755, written to a temporary file
and renamed into place, then checked like the other root-owned paths), and job units run that
copy. The job record and the record kept with the image (`helper-job.json`) name it
(`shim_exe`), so a restore puts the tree back on the binary it was dumped with, and a refused
restore says so when the copy is missing. Before a restore the helper also checks that the binary
is a regular file owned by root and writable by nobody else, and that a copy still has the contents
its name says; at start-up it checks its own binary and the directories above it like the other
root-owned paths. At each start the helper removes the copies that are
neither its own nor named by a job record or an image's record, nor mapped by an image
(`files.img`); if a checkpoint cannot be read, nothing is removed. On NixOS the helper runs from
the Nix store, whose paths do not change; it makes no copy, and the Nix GC roots
(`/nix/var/nix/gcroots/gpueue/job-<id>`) keep the old store path instead. If the copy cannot be
made, the helper logs a warning and job units run the installed binary. With SELinux, a file
created in `/var/lib` gets the type `var_lib_t`, which systemd (`init_t`) may not execute (the job
unit fails with status 203/EXEC). So the helper copies the `security.selinux` attribute of its own
binary (`bin_t` for `/usr/bin/gpueue-helper`; systemd started the helper from it, so it may run
it) onto the temporary file before the rename, relabels the existing copies at each start, and
checks the label of a copy before a job unit or a restore uses it, which repairs a relabel such
as `restorecon -R /var/lib`. Without SELinux the binary has no such attribute and nothing is
labelled; a failure to label is logged. No file-context rule is installed: `semanage` is not
installed everywhere, and the helper's own labelling also covers `install.sh` and
`gpueue service install`. The fork guard library in the same directory keeps `var_lib_t`: jobs
run unconfined (`unconfined_service_t`), which may map it.

## Shutdown ordering

- **OS shutdown or reboot.** systemd stops units in the reverse order of starting.
  `gpueue.service` runs `After=gpueue-helper.service`, so the daemon stops first. Stopping the daemon
  never stops jobs. `gpueue-helper.service` runs after the local and remote filesystems, D-Bus,
  `nvidia-persistenced` and `nvidia-fabricmanager`, so those are still available while it stops.
  On SIGTERM the helper asks systemd whether the whole system is stopping. If so, it refuses new
  work, dumps every running job one at a time (except `--no-checkpoint` jobs), keeps extending
  its stop timeout up to the end of the running dump's deadline (plus its rollback), and writes
  `shutdown-manifest.json`. The whole shutdown is capped by `JobTimeoutSec=` of the shutdown
  targets (`reboot.target` and the like), after which systemd forces the machine down; gpueue's
  drop-ins raise it from systemd's 30 minutes to 2 hours (see
  [Shutdown timeouts](install.md#shutdown-timeouts)). Every checkpoint and restore has a deadline that scales with the
  job's size; when it passes, the tool's whole process tree is killed and a checkpoint is rolled
  back (the job is thawed and its CUDA state restored and unlocked; in plugin mode, where the
  killed CRIU's plugin may have left the CUDA processes locked or checkpointed, the helper asks
  `cuda-checkpoint --get-state` and restores and unlocks them as needed). Paused jobs are dumped too
  (resumed first), and come back paused (stopped, without running) after `gpueue restore`. Job units are
  `Before=gpueue-helper.service`, so systemd stops them only after the helper has finished, and
  `After=remote-fs.target network.target`, so the ones still running then (`--no-checkpoint` jobs,
  failed or refused dumps) are stopped before remote filesystems are unmounted and the network
  goes down. On the
  next boot the daemon gets the results from the helper's replayed events and marks the jobs
  `Checkpointed`. Jobs whose dump failed or was refused were stopped by systemd. They are marked
  `Checkpoint failed` with the reason, and their owners are warned.
- **`gpueue shutdown`** (administrators) does not rely on unit ordering. The daemon checkpoints
  running jobs one at a time (or terminates them with `--no-checkpoint`), waits for them to settle,
  and exits. The helper keeps running. A job whose dump fails is terminated. A job whose dump the
  helper refuses is left running, because terminating it would lose the work. The daemon logs a
  warning and the owner is warned. Stop it with `gpueue kill` or `gpueue shutdown --no-checkpoint`
  if needed.
- **`systemctl stop/restart gpueue-helper` or `gpueue`** on its own leaves jobs running. A
  stopping daemon first puts the launches it has decided on but not sent to the helper yet back
  in line (otherwise the next daemon would find their jobs gone and mark them lost, failing their
  dependants), and gives the helper up to 10 seconds to answer the spawn and restore requests
  already sent.

## Priority and fair share

The order of queued jobs is Slurm's multifactor priority (the formula and settings are in
[docs/install.md](install.md#priority-and-fair-share)). The work is split like the rest of the
scheduler:

- `gpueue-core::priority` is pure: the factors, the formula, the owner's order (top, `-o`
  priority, id; applied by pooling the ages of a user's jobs that could launch now, i.e. group
  present and not paused, GPUs detected, dependencies done, and handing them out in that order), the
  classic fair-share
  factor `2^(-U/S)`, the decay, the billing of a job and the reset-period boundaries (computed
  from the local UTC offset the daemon passes in). `scheduler::plan_prioritized` orders the
  candidates with it and then applies the usual slot, pause and dependency rules unchanged, so a
  high-priority job that does not fit never blocks jobs behind it. `plan`/`plan_with` keep pueue's
  order (also used for `priority_type = "basic"`, where `priority::basic_order` lets a user's `-o` reorder only that user's own jobs). `scheduler::pending_priorities` (behind
  `gpueue prio`) uses the same candidate selection, so it lists what the plan considers.
- The storage keeps `jobs.nice`, `jobs.site_factor`, `jobs.top_seq`, `jobs.run_since`, `groups.priority`, the
  `user_shares` and `user_usage` tables and, in `meta`, when usage was last calculated and reset
  (schema version 6), and the local day on which the reset period of the last reset began. A
  reset is due when the current period began on a later day, so a change of the UTC offset
  (daylight saving time) does not make the same boundary due twice.
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
  `gpueue-core::authz`.

## State

| Data | Location | Owner |
| --- | --- | --- |
| jobs, groups, GPU slot limits, checkpoint records, pending warnings, fair-share shares and usage | `/var/lib/gpueue/daemon/state.sqlite` (WAL with `synchronous=FULL`, so an acknowledged helper event survives a power failure; schema versioned with `PRAGMA user_version`) | daemon |
| job records, event queue | `/var/lib/gpueue/helper/` | helper |
| job output | `/var/lib/gpueue/logs/<uid>/<id>.log` | the helper creates the directory (owned by the user, group `gpueue`, mode 2770). The shim writes the log as the owner. The daemon reads and truncates it without following symlinks and only if it is a regular file owned by the job's owner. |
| checkpoint images | `/var/lib/gpueue/checkpoints/<id>/` | helper (root only) |
| configuration | `/etc/gpueue/config.toml` | root |

Only the daemon writes the database. The CLI always goes through the daemon.

## Crate layout

| Crate | Contents |
| --- | --- |
| `crates/gpueue` | the `gpueue` binary: pueue-compatible CLI, `gpueue daemon`, `service`, `doctor`, `update` (the only crate with an HTTPS client) |
| `crates/gpueue-core` | domain model, job lifecycle, scheduling decisions, authorization, SQLite storage (no other I/O) |
| `crates/gpueue-protocol` | wire types: CLI ⇄ daemon (HTTP API and client), daemon ⇄ helper (NDJSON messages) |
| `crates/gpueue-config` | the `/etc/gpueue/config.toml` schema, shared by the daemon, the helper (with root-ownership checks) and `doctor` |
| `crates/gpueue-daemon` | the daemon: axum API, scheduler loop, helper client, GPU detection (NVML) |
| `crates/gpueue-checkpoint` | CRIU and cuda-checkpoint command plans, execution with timeouts and rollback, image layout |
| `crates/gpueue-helper` | the `gpueue-helper` binary: socket server, job engine, systemd launcher, shim, mount analysis, fork guard (`forkguard/forkguard.c`, embedded in the binary) |

The systemd units, sysusers.d and tmpfiles.d snippets and the default configuration live in
[`packaging/`](../packaging) and are embedded in the `gpueue` binary for `gpueue service install`.
The AUR package (`packaging/aur/gpueue-bin`) installs the same files unchanged.
