# Installing and operating puzuz

This guide is for the administrator of the machine. Regular users need nothing beyond the `puzuz`
command on their `PATH`.

- [Requirements](#requirements)
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
| OS | Linux with systemd as init | Ubuntu (Docker image with systemd as PID 1), x86_64 |
| systemd | 240 or newer (`puzuz doctor` checks this) | 259 |
| Kernel | `CONFIG_CHECKPOINT_RESTORE=y` (`/proc/sys/kernel/ns_last_pid` must exist), PID and mount namespaces, 5.5 or newer recommended (`clone3` with `set_tid`, used by CRIU to restore PIDs), 5.2 or newer for `open_tree`/`move_mount` (used to clean up hidden mounts) | 7.0 |
| CRIU | 4.2 or newer | 4.2 (distribution package), 4.2.1 |
| NVIDIA driver (GPU jobs) | a driver whose `cuda-checkpoint` supports `--action lock/checkpoint/restore/unlock` and `--timeout`: r570 or newer (cuda-checkpoint itself supports r550, but only with `--toggle`, which puzuz does not use) | 595 |
| cuda-checkpoint (GPU jobs) | the binary from NVIDIA, with the same major version as the driver | 595 |
| nvidia-smi (GPU jobs) | the helper uses it to find CUDA processes and GPU memory | from the driver |
| Rust (building only) | 1.88 or newer, plus a C compiler (`cc`) for the helper's fork guard library | 1.98.1 |

Without an NVIDIA driver puzuz still runs CPU-only jobs, and checkpoints them as well.

### CRIU

Install the distribution's package (for example `sudo apt install criu`) or build it from
<https://criu.org/Installation>. In the default **manual** mode
(`cuda_checkpoint_mode = "manual"`) the helper calls cuda-checkpoint itself and runs CRIU without
plugins. The distribution package works for this, even though it ships without CRIU's CUDA plugin.

The **plugin** mode (`cuda_checkpoint_mode = "plugin"`) lets CRIU's CUDA plugin (`cuda_plugin.so`,
CRIU 4.0 or newer, usually built from source) drive cuda-checkpoint. It is optional and has not
been tested with puzuz yet.

### cuda-checkpoint

NVIDIA distributes cuda-checkpoint as a prebuilt binary in
<https://github.com/NVIDIA/cuda-checkpoint>, under `bin/x86_64_Linux/cuda-checkpoint`. Put it
where root's `PATH` finds it, owned by root and not writable by others:

```sh
sudo install -o root -g root -m 0755 cuda-checkpoint /usr/local/bin/cuda-checkpoint
```

You can put it somewhere else and set `cuda_checkpoint_path` in `/etc/puzuz/config.toml`. Absolute
paths of `criu`, `cuda-checkpoint` and `nvidia-smi` must be owned by root, because the helper runs
them as root.

## Building

Both binaries are needed: `puzuz` (CLI and daemon) and `puzuz-helper` (the root helper). A plain
`cargo build` only builds `puzuz`, so name both packages:

```sh
cargo build --release -p puzuz -p puzuz-helper
# -> target/release/puzuz, target/release/puzuz-helper
```

Without a local Rust toolchain you can build in the project's Docker image (Rust 1.98.1 on Debian
bookworm, so the binaries need glibc 2.36 or newer):

```sh
scripts/test-in-docker.sh build --release -p puzuz -p puzuz-helper
# -> target/docker/release/puzuz, target/docker/release/puzuz-helper
```

## Installing

```sh
sudo target/release/puzuz service install --bin-dir /usr/local/bin
```

The units run the binaries from the directory the `puzuz` binary is in, but only if that is
`/usr/bin` or `/usr/local/bin`. Otherwise pass `--bin-dir <DIR>` to copy `puzuz` and
`puzuz-helper` there. `puzuz-helper` must sit next to `puzuz`. `--no-start` enables the services
without starting or restarting them.

`puzuz service install` is safe to run again (for example after an upgrade). It does the
following:

1. Copies `puzuz` and `puzuz-helper` into `--bin-dir`, if given (mode 0755, replaced
   atomically).
2. Writes these files from the packaging templates built into the binary. Each file starts with
   an "Installed by `puzuz service install`" marker line and is overwritten on the next install:
   - `/etc/systemd/system/puzuz-helper.service`, the root helper (from
     [packaging/systemd/puzuz-helper.service](../packaging/systemd/puzuz-helper.service)),
   - `/etc/systemd/system/puzuz.service`, the daemon, which runs as user `puzuz` with a strict
     sandbox (from [packaging/systemd/puzuz.service](../packaging/systemd/puzuz.service)),
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
   `nvidia-smi` works, it appends a `[[gpus]]` entry for each GPU it lists.
6. Runs `systemctl daemon-reload`. If the units or the binaries changed, it runs
   `systemctl try-restart` on both services. Then it runs
   `systemctl enable --now puzuz-helper.service puzuz.service`, waits up to 10 seconds for the
   daemon to connect to the helper, and prints the `puzuz doctor` report.

Unit files owned by a distribution package (`/usr/lib/systemd/...`) are never touched. Customize
the units with drop-ins (`sudo systemctl edit puzuz-helper.service`). Edits to the files in
`/etc/systemd/system` are lost on the next install.

The helper's `TimeoutStopSec=30min` caps how long it may spend checkpointing all jobs when the
system shuts down. The helper also asks systemd for more time before each job (on systemd versions
that support it). A dump writes roughly RSS + VRAM per job, and jobs are dumped one after another.
In tests, 7 GB of VRAM took about 11 s. If your jobs are large, raise the timeout in a drop-in.

## Verifying

```sh
puzuz doctor           # every check, with a `fix:` line for each problem; exits non-zero on FAIL
puzuz service status   # both units, and whether the daemon is reachable and connected to the helper
```

`puzuz doctor` checks the configuration file and its ownership, `criu` (version 4.2 or newer), the
NVIDIA driver, `cuda-checkpoint` (manual mode support, version matching the driver), `nvidia-smi`
against the configured GPUs, the kernel, the systemd version, both units, the helper binary's
ownership, both sockets, the daemon and helper versions and their connection, the checkpoint
directory (exists, not tmpfs, free space) and the total checkpoint size, and whether you are an
administrator. It does not run `criu check`. Run `sudo criu check` yourself to see what CRIU
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
The same rule applies to the directories it writes to: `data_dir`, `helper_state_dir`,
`logs_dir`, `checkpoints_dir` and all their parents must be owned by root. Keep the layout that
tmpfiles.d creates. The daemon stores its database in the `daemon/` subdirectory because
`data_dir` itself must belong to root.

Groups and GPU slot limits are not configured here. They are stored in the daemon's database and
changed with `puzuz group`, `puzuz parallel` and `puzuz gpu parallel`.

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
| `criu_dump_timeout_secs` | `1800` | helper | kill `criu dump` after this long |
| `criu_restore_timeout_secs` | `1800` | helper | kill `criu restore` after this long |
| `fork_guard` | `true` | helper | preload the fork guard into jobs (see the README's limitations) |
| `gpu_detection` | `"auto"` | daemon | `"auto"`: NVML, falling back to `[[gpus]]`. `"static"`: only `[[gpus]]`. |
| `nvml_library` | unset | daemon | NVML library path or name; by default `libnvidia-ml.so.1`, then `libnvidia-ml.so` |
| `[[gpus]]` | empty | daemon | static GPU list: `index` (required), `uuid`, `name`, `memory_total_bytes` (see [GPU detection](#gpu-detection)) |

`puzuz-helper serve` also accepts command-line flags (`puzuz-helper serve --help`). They override
the file. Prefer the file: `puzuz doctor` reports the file's values and warns when the unit's
`ExecStart=` passes flags.

## Administrators

Administrators are root and the members of `puzuz-admin` (primary or supplementary group). They
can act on every user's jobs and run the admin-only commands (`group add/remove`, `parallel`,
`gpu parallel`, group-wide `pause/start/kill -g`, `shutdown`, `reset` of all jobs).

```sh
sudo usermod -aG puzuz-admin alice
```

The daemon looks the group up on every request (through NSS), so the change applies at once,
without logging in again.

## GPU detection

The daemon detects GPUs once, when it starts. Restart `puzuz.service` after adding, removing or
replacing a GPU.

- With `gpu_detection = "auto"` (the default) it loads NVML (`libnvidia-ml.so.1`) at run time. If
  NVML cannot be loaded or initialized, it uses the `[[gpus]]` list from the configuration.
- With `gpu_detection = "static"` it always uses the `[[gpus]]` list.

`puzuz service install` writes the `[[gpus]]` list from `nvidia-smi` when it creates the
configuration file. Keep the list in sync with the hardware (`puzuz doctor` compares it with
`nvidia-smi`):

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

> The packaged `puzuz.service` runs the daemon with `PrivateDevices=yes`, which hides
> `/dev/nvidia*`. NVML detection inside this sandbox has not been tested, and the end-to-end
> tests use `gpu_detection = "static"`. Until it is, rely on the `[[gpus]]` list. If `puzuz gpu`
> shows the right GPUs, detection works.

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

## Upgrading

```sh
cargo build --release -p puzuz -p puzuz-helper
sudo target/release/puzuz service install --bin-dir /usr/local/bin
```

This replaces the binaries and units and restarts both services if anything changed. The
configuration file is kept.

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

```sh
sudo puzuz service uninstall            # stop, disable and remove the units and snippets
sudo puzuz service uninstall --purge    # also delete /etc/puzuz, /var/lib/puzuz, /run/puzuz, /run/puzuz-helper
```

- Only files that start with the install marker are removed. Others are reported and kept.
- Running jobs keep running in their own units. `uninstall` warns about them, and `--purge`
  refuses to run while any `puzuz-job-*` unit is active. Stop them first with
  `sudo systemctl stop 'puzuz-job-*'`, or checkpoint/kill them before uninstalling.
- `--purge` asks for confirmation (`-y/--yes` skips it). It does not descend into mount points
  (for example the bind mount of `/` below `/run/puzuz-helper`). Those are reported for you to
  unmount and delete by hand.
- The system user `puzuz`, the group `puzuz-admin` and the binaries are always kept.

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
| `criu` | not found, or too old | install CRIU 4.2 or newer, or set `criu_path` |
| `cuda-checkpoint` | not found, or lacks `--action lock/checkpoint/restore/unlock` | install the binary for a driver of r570 or newer into `/usr/local/bin` |
| `cuda-checkpoint` | version X but the driver is Y | use the cuda-checkpoint that matches the driver's major version |
| `gpus` | nvidia-smi lists N GPU(s), the configuration M | update `[[gpus]]` (see [GPU detection](#gpu-detection)) |
| `kernel` | lacks `/proc/sys/kernel/ns_last_pid` | use a kernel with `CONFIG_CHECKPOINT_RESTORE=y` |
| `puzuz.service`, `puzuz-helper.service` | not installed / not active | `sudo puzuz service install`, or `sudo systemctl start <unit>` and read `journalctl -u <unit>` |
| `helper binary` | not owned by root or writable by others | `sudo chown root:root <path>; sudo chmod 755 <path>`, or reinstall with `--bin-dir /usr/local/bin` |
| `helper` | the daemon is not connected to the helper | `journalctl -u puzuz-helper`. Often an ownership problem with the configuration or a data directory. |
| `daemon` | runs version X while this puzuz is Y | `sudo systemctl restart puzuz-helper.service puzuz.service` after upgrading, or update the CLI |
| `checkpoint dir` | on tmpfs, or little free space | move `checkpoints_dir` to a disk with room for RAM + VRAM of your jobs |
| `helper settings` | ExecStart= passes flags | move the settings into `config.toml` and drop the flags |

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
- `checkpoint is not compatible with this host`: a different GPU model or driver version, or a
  missing GPU. Restore with the original driver and GPU. In plugin mode the GPU UUIDs must match
  as well. In manual mode a different card of the same model is remapped.
- `not enough free GPU memory`: other jobs use the GPU. Wait, or free it.
- Files the process had open, or its executables and libraries, were changed or deleted (see
  `restore.log`). Restore them at the same paths.
