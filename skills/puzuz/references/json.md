# puzuz JSON output

`--json` (`-j`) prints puzuz's own schema (not pueue's state dump) on `status`, `log`, `gpu`,
`group`, `prio` and `share`. Timestamps are Unix epoch milliseconds; `owner` is a uid.

## `puzuz status --json`

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

- `state` is one of `queued` (`enqueued_at`), `stashed` (`enqueue_at` or null), `running`,
  `paused`, `checkpointing`, `checkpointed`, `restore_queued` (`enqueued_at`), `restoring`,
  `terminating` and `done`. The state's fields are flattened into the job object.
- `terminating` and `done` carry `result`: `success`, `failed` (with `exit_code`),
  `failed_to_spawn` (`reason`), `killed`, `dependency_failed`, `checkpoint_failed` (`reason`)
  or `lost`.
- `checkpoint_size_bytes`, `details`, `details.last_error`, `nice` and a group's `priority` are
  omitted when they do not apply (or are 0).
- Other users' jobs: `label` is null, `details` is absent and `reason` is `"<redacted>"`.
- `waiting` appears on a queued job that a non-administrator asked to launch right away but that
  waits for a resource: `{"reason": "gpu_slots", "gpus": [0]}`, `{"reason": "group_paused"}` or
  `{"reason": "unavailable"}`.

## Other commands

- `puzuz log --json [IDS]` → `{"<id>": {"job": <job>, "output": "..."}}` (last lines only unless
  `--full`; not streamed).
- `puzuz group --json` → `{"groups": [...]}`.
- `puzuz gpu --json` → `{"gpus": [{"index", "uuid", "name", "memory_total_bytes",
  "driver_version", "parallel", "used", "jobs": [{"id", "owner"}]}]}`; `"available": false`
  marks a GPU that disappeared.
- `puzuz prio --json` → `{"priority_type", "weights": {...}, "jobs": [{"id", "owner", "group",
  "priority", "factors": {...}, "weighted": {...}, "site", "nice"}]}`, jobs in scheduling order.
- `puzuz share --json` → `{"users": [{"uid", "shares", "norm_shares", "raw_usage",
  "effective_usage", "fairshare", "active"}], ...}`.

## jq recipes

```sh
# Result and exit code of job 7
puzuz status --json | jq '.jobs[] | select(.id == 7) | {state, result, exit_code}'

# My running jobs
puzuz status --json | jq --argjson me "$(id -u)" '[.jobs[] | select(.owner == $me and .state == "running") | .id]'

# GPUs with a free slot (parallel 0 means unlimited)
puzuz gpu --json | jq '[.gpus[] | select(.available != false and (.parallel == 0 or .used < .parallel)) | .index]'

# Full output of job 7 as plain text
puzuz log --json --full 7 | jq -r '."7".output'
```
