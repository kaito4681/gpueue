"""CPU job of the e2e test that holds files deleted for good, as ordinary
workloads do, and prints a counter like the shell COUNTER (0, 1, 2, ...):

- a multiprocessing semaphore (POSIX, created and unlinked at once under
  the job's own /dev/shm),
- a temporary file on the job's own /tmp, larger than CRIU's default ghost
  limit (1 MiB), deleted while open,
- a file in the working directory (a host filesystem), deleted while open.

Each step checks that the deleted files still have their contents (also
after a checkpoint and restore) and exits with 3 if one does not.
"""

import hashlib
import multiprocessing
import os
import sys
import tempfile
import time

sem = multiprocessing.get_context("fork").Semaphore(1)

private = tempfile.TemporaryFile(dir="/tmp")
private.write(os.urandom(3 << 20))
private.flush()

host_name = f"e2e-deleted-{os.getpid()}-{time.time_ns()}"
host = open(host_name, "w+b")
host.write(b"held by the job\n" * 64)
host.flush()
os.unlink(host_name)


def digest(f):
    f.seek(0)
    return hashlib.sha256(f.read()).hexdigest()


want = (digest(private), digest(host))
i = 0
while True:
    with sem:
        if (digest(private), digest(host)) != want:
            print("the deleted files lost their contents", file=sys.stderr, flush=True)
            sys.exit(3)
    print(i, flush=True)
    i += 1
    time.sleep(0.2)
