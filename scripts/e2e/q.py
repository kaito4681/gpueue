"""Query `puzuz status --json` (or `gpu --json`) read from stdin.

Usage: puzuz status --json | python3 q.py EXPR
EXPR is a Python expression over `d` (the parsed JSON) with these helpers:
  job(id)    the job object (KeyError if missing)
  st(id)     a compact state: "running", "done/success/0", "done/failed/3",
             "done/killed", "queued", "checkpointed", ...
  ids(state) ids of jobs whose st() starts with `state`
A str result is printed as-is, anything else as JSON.
"""

import json
import sys

d = json.load(sys.stdin)


def job(i):
    for j in d["jobs"]:
        if j["id"] == int(i):
            return j
    raise KeyError(f"no job {i}")


def st(i):
    s = job(i)["state"]
    name = s["state"]
    if "result" in s:
        r = s["result"]
        parts = [name, r["result"]]
        if "exit_code" in r:
            parts.append(str(r["exit_code"]))
        return "/".join(parts)
    return name


def ids(prefix):
    return [j["id"] for j in d["jobs"] if st(j["id"]).startswith(prefix)]


out = eval(sys.argv[1], {"d": d, "job": job, "st": st, "ids": ids})
print(out if isinstance(out, str) else json.dumps(out))
