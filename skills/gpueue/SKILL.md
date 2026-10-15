---
name: gpueue
description: Run, monitor and manage long-running or GPU jobs on a shared Linux machine through the gpueue job queue (a pueue-compatible CLI with GPU slots, fair-share priority and CRIU checkpoint/restore). Use when the machine has `gpueue` installed and the user wants to queue a training run or other long command, pick or wait for a free GPU, follow a job's log, wait for jobs to finish, or pause, kill, restart, checkpoint or restore their jobs.
license: MIT
compatibility: Requires a Linux machine where an administrator installed gpueue (the `gpueue` CLI and its system daemon at /run/gpueue/gpueue.sock). Regular users install nothing.
---

# Using gpueue as a regular user

gpueue is a job queue for one Linux machine shared by several users. A system-wide daemon
schedules everybody's jobs, assigns GPUs, runs each job as its owner in its own systemd unit,
and can checkpoint running jobs to disk (CPU, memory and GPU state) and restore them later.
The CLI mirrors pueue 4.0 (`pueue` → `gpueue`), with "job" instead of "task".

Use gpueue instead of `nohup`, `tmux`, `&` or `screen` for anything that should outlive the agent
session, needs a GPU, or competes with other users for the machine.

## Safety rules for agents

- Act only on jobs the user owns. Always pass explicit job ids to `kill`, `pause`, `remove`,
  `restart`, `checkpoint`, `restore` and `clean`-like actions. Without ids, `kill`, `pause` and
  `start` act on all of the user's jobs in the default group; `reset` kills and removes all of
  the user's jobs. Never run `reset` unless the user explicitly asks for it.
- Never use `sudo` with gpueue, and do not run administrator commands (`shutdown`, `parallel`,
  `group add/remove/priority`, `gpu parallel`, `share set`, `site`, `service`, `uninstall`,
  `update`, `daemon`). If the user is not an administrator they fail with "permission denied";
  if they are, they affect everybody. Ask the user instead.
- Do not busy-poll `gpueue status` in a loop. Block with `gpueue wait <id>` or stream with
  `gpueue follow <id>`; check `status`/`log` once when you need a snapshot.
- Jobs have no stdin (it is `/dev/null`) and `gpueue send` always fails. Make commands
  non-interactive (`--yes`, `yes |`, config files).
- Other users' commands, logs, labels and environments are hidden from you; do not try to read
  them some other way.
- `remove` of a `Checkpointed` job deletes its checkpoint image, which may hold hours of
  progress. Confirm with the user first.

## 1. Check that gpueue is available

```sh
command -v gpueue && gpueue status   # all jobs of all users; others' commands are hidden
gpueue gpu                          # GPUs, free/used slots, and which jobs hold them
gpueue group                        # groups (separate queues) and their parallel limits
```

`gpueue: error: the daemon is not running (cannot connect to the daemon socket ...)` means the
daemon is down or gpueue is not installed: tell the user and suggest they ask an administrator
(`gpueue doctor` prints a diagnosis anyone can read). Do not try to start a daemon yourself.

## 2. Add a job

The command runs through `sh -c` as the user, **in the current directory, with the environment
of the `gpueue add` invocation** (activate the virtualenv / export variables first, or use
`-w DIR`). Quote the whole command in single quotes and put `--` before it.

```sh
gpueue add -p -l resnet -- 'python train.py --epochs 100'            # CPU-only; -p prints only the id
gpueue add -p --gpu 0 -l resnet -- 'python train.py'                  # on GPU 0
gpueue add -p --gpu 0,1 -w ~/proj -- 'torchrun --nproc-per-node 2 train.py'
gpueue add -p -g big -- './long.sh'                                   # into group "big"
gpueue add -p --after 3 4 -- 'python eval.py'                         # after jobs 3 and 4 succeed
gpueue add -p -s -- 'make bench'                                      # stashed; start later with `gpueue enqueue <id>`
gpueue add -p -d 3h -- './nightly.sh'                                 # enqueue in 3 hours
```

Key flags (all verified against `gpueue add --help`):

| Flag | Meaning |
| --- | --- |
| `-p, --print-task-id` | print only the job id (use this in scripts and capture it) |
| `--gpu LIST` | GPUs by index (`0` or `0,1`, PCI bus order as in `nvidia-smi`). Starts only when **every** listed GPU has a free slot. Inside the job `CUDA_VISIBLE_DEVICES` lists them, so they appear as `cuda:0`, `cuda:1`, ... Without `--gpu` the job is CPU-only and cannot open any GPU. |
| `-w, --working-directory DIR` | working directory (default: current directory) |
| `-l, --label TEXT` | label shown in `status` |
| `-g, --group NAME` | queue group (default `default`, which starts with `parallel 1`) |
| `-a, --after ID...` | dependencies; takes several ids, so end the list with `--`. Only your own jobs. If one fails, this job fails (`dependency_failed`). |
| `-o, --priority N` | order among **your own** jobs only (higher first) |
| `--nice N` | lower the job's priority (non-negative for regular users) |
| `-s, --stashed` / `-d, --delay WHEN` | don't queue now / queue later (`3h`, `3600`, `wednesday 10:30pm`, ...) |
| `-i, --immediate` (`--follow`) | see below |
| `--no-checkpoint` | never checkpoint this job; on shutdown/reboot it is terminated |
| `-e, --escape` | escape shell characters (disables `&&`, redirections, ...) |

### Scheduling, priority and `-i`

- A job starts when its group has a free slot, its GPUs have free slots and its dependencies
  succeeded. Among waiting jobs, a Slurm-style multifactor priority decides: users who used
  less of the machine recently go first (fair share, decaying usage), plus waiting time, minus
  nice. You cannot jump ahead of other users.
- For non-administrators, `add -i`, `start <queued id>`, `restart -k` and `restore -i` do **not**
  start the job right away: the job is put first among the user's own jobs (like `gpueue top`)
  and still waits for slots. The CLI says "queued first among your jobs instead of being
  launched right away". This is expected, not an error.
- `gpueue prio [IDS]` shows why a pending job is where it is; `gpueue share` shows fair-share
  usage; `gpueue top <IDS>` and `gpueue nice <IDS> <N>` reorder or deprioritise your own jobs.

## 3. Watch, wait and read output

```sh
gpueue status                         # table: Id, Status, Owner, GPU, Label, Command, ...
gpueue status status=running          # query language; see `gpueue status --help`
gpueue log 7                          # last 15 lines of job 7 (and its last error)
gpueue log 7 -l 100                   # last 100 lines;  `--full` for everything
gpueue follow 7                       # stream output until the job ends (like tail -f)
gpueue wait 7                         # block until job 7 is done (success or failure)
gpueue wait 7 8 --status success -q   # exits non-zero if any of them did not succeed
```

- `wait` exits 0 when the jobs are done regardless of their result; use `--status success` when
  the exit status matters, or check the result afterwards with `gpueue status --json`.
- `wait` keeps waiting for queued, stashed, paused and `Checkpointed` jobs. A job checkpointed
  by a reboot only continues after `gpueue restore`, so `wait` on it will not return by itself.
- Run long `wait`/`follow` calls in the background or with a timeout if the agent's tool has
  one; they are cheap (event stream, no polling).
- For machine-readable output use `--json` on `status`, `log`, `gpu`, `group`, `prio` and
  `share`. See [references/json.md](references/json.md) for the schema and `jq` recipes.

## 4. Control jobs

```sh
gpueue pause 7          # SIGSTOP-style pause; keeps its GPU slots
gpueue start 7          # resume a paused job
gpueue kill 7           # terminate a running job (state: done, result killed)
gpueue kill -s INT 7    # send a signal instead (e.g. let the program save and exit)
gpueue restart 7        # re-run a finished (or Checkpointed) job as a new job
gpueue restart -i 7     # ...reusing the same id and overwriting its log
gpueue remove 7         # delete a finished/queued job from the list (kill running ones first)
gpueue clean -s         # remove your jobs that finished successfully
gpueue stash 7 / gpueue enqueue 7    # hold a queued job back / queue it again
gpueue switch 7 8       # swap two of your queued jobs
```

Not implemented yet (they fail): `edit`, `env`, `restart --edit`, `send`. To change a queued
job, `remove` it and `add` it again.

## 5. Checkpoint and restore

```sh
gpueue checkpoint 7     # save job 7 to disk and stop it; frees its GPUs. State: Checkpointed
gpueue restore 7        # queue it again; resumes from the saved state when slots are free
```

The job keeps its id and log. On an orderly OS shutdown/reboot gpueue checkpoints running jobs
automatically; after boot they are `Checkpointed` and the owner must `gpueue restore` them
(`gpueue restore -a` for all of the user's). Restores are never automatic.

Before relying on checkpoints, read [references/checkpoint.md](references/checkpoint.md): open
TCP connections are closed, `/tmp` is private to the job, the venv/binaries/working directory
must stay unchanged until restore, the GPU model/driver/index must match, and nothing is saved on
a power loss. Training code should still save its own checkpoints.

## 6. Common errors

| Message | Meaning / what to do |
| --- | --- |
| `the daemon is not running (cannot connect to the daemon socket ...)` | gpueue is down or not installed; tell the user (an admin runs `gpueue doctor`). |
| `permission denied` / `only administrators may ...` | the action needs admin rights or targets another user's job; don't retry with sudo. |
| `job N does not exist` | wrong id, or the job was removed/cleaned. |
| `job N cannot be <action> while it is <state>` | e.g. `remove` of a running job (kill first), `pause` during a checkpoint, `start` of a Checkpointed job (use `restore`). |
| `GPU N does not exist` | use an index listed by `gpueue gpu`. |
| `group "x" does not exist` | use `gpueue group`; only admins create groups. |
| `... queued first among your jobs instead of being launched right away` | normal for non-admin `-i`/`start`; the job waits for its turn. |
| job `Failed to spawn`: `gpueue refuses to run jobs as <user>: <reason>` | the account is not allowed to run jobs (system user, denied group such as `docker`/`sudo` as primary group); tell the user. |
| `Checkpoint failed` / checkpoint refused | see `gpueue log <id>` for the reason; see [references/checkpoint.md](references/checkpoint.md). |

`gpueue <command> --help` is authoritative for every flag.
