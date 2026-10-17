# Checkpoint and restore with gpueue

gpueue saves a running job's whole process tree (CPU memory, open files, GPU memory) with CRIU
and NVIDIA cuda-checkpoint, without changes to the program.

## When checkpoints happen

- `gpueue checkpoint <IDS>` (or `-g GROUP`, `-a` for all of your running jobs): save and stop the
  jobs now. Their GPU slots are freed. State: `Checkpointing` → `Checkpointed`.
- On an orderly OS shutdown or reboot, and on an administrator's `gpueue shutdown`, running jobs
  are checkpointed automatically, one at a time. `--no-checkpoint` jobs are terminated instead.
- Never on a power loss, kernel panic or hard reset, and there are no periodic checkpoints. Keep
  saving your own training checkpoints.

## Restoring

- `gpueue restore <IDS>` (or `-g GROUP`, `-a`) queues the jobs again (`Restore queued`); they
  resume once their group and GPUs have free slots. For non-administrators `restore -i` only
  moves them first among their own jobs.
- The job keeps its id; its output is appended to the same log. On success the image is deleted.
- A paused job can be checkpointed too. It is restored paused (still holding its GPU slots);
  resume it with `gpueue start <id>`.
- `gpueue restart <id>` on a `Checkpointed` job starts it over from the beginning as a new job and
  keeps the image. `gpueue remove <id>` discards the job **and deletes the image**.
- `gpueue start` does not resume a `Checkpointed` job; use `restore`.

## What can make a checkpoint or restore fail

A **refused** checkpoint leaves the job running and reports why (`gpueue checkpoint` prints the
reason; otherwise it is the job's last error in `gpueue status` / `gpueue log`). A **failed** dump
stops the job (SIGTERM, then SIGKILL after a grace period) and shows `Checkpoint failed` with the
reason. A refused restore keeps the image so it can be retried.

- **Network:** established TCP connections are closed at checkpoint; the program must reconnect
  after restore.
- **Unix sockets:** a socket bound to a path outside the job's own `/tmp` or `/dev/shm` (or to a
  relative name) makes the checkpoint refused. Bind to an absolute path below `/tmp`, or use an
  abstract name.
- **`SO_REUSEPORT`** on a TCP/UDP socket makes the checkpoint refused; turn it off or use
  `--no-checkpoint`.
- **Deleted files** the job still holds are saved in the image, except that a deleted file with
  another hard link, a file deleted on NFS (`.nfs<hex>`), or a deleted setuid/setgid program
  (or one with file capabilities) makes the checkpoint refused; restart such a job, or close the
  file before checkpointing. Libraries replaced by a package upgrade are fine.
- **Private `/tmp` and `/dev/shm`:** each job has its own empty tmpfs there (saved in the image).
  Exchange files through a persistent filesystem, not `/tmp`.
- **Same files:** do not change or delete the job's virtualenv, binaries, libraries, working
  directory or log before it is restored, do not replace a directory on the way with a symlink,
  and keep your group memberships.
- **Same mounts:** every filesystem the job saw when it started must be mounted at the same path
  again; files on a tmpfs such as `/scratch` do not survive a reboot.
- **Your own FUSE mounts** (sshfs, rclone, ...) in your home or another directory you own are
  left out of the checkpoint, and a job working in one is refused: mount it below a directory
  only root controls (ask the administrator), or use `--no-checkpoint`.
- **Nested namespaces:** a job that runs `unshare`, `bwrap` or a rootless container runtime
  (its own PID or user namespace) is refused; use `--no-checkpoint`.
- **Same GPU:** the GPU model, driver version and GPU index must match the time of the checkpoint.
- **Disk space:** an image takes roughly RSS + VRAM (+ the job's `/tmp`, `/dev/shm`).
- **PyTorch:** forked DataLoader workers are handled (a preloaded "fork guard"; Python 3.14+
  uses `forkserver` anyway). CUDA IPC between processes (`torch.multiprocessing` sharing CUDA
  tensors) cannot be checkpointed; multi-GPU NCCL jobs are untested. Very long CUDA kernels
  delay a checkpoint.
- **Static binaries** or processes created with raw `clone(2)` are not covered by the fork guard.

If a job cannot be checkpointed by design (a server with `SO_REUSEPORT`, CUDA IPC, ...), add it
with `--no-checkpoint` so a reboot terminates it cleanly instead of failing a dump.
