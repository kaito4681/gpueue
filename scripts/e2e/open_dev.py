"""GPU isolation check of the e2e test: tries to open device nodes.

Prints "open <path>: ok" or "open <path>: <errno name>" for each argument
(read/write, like the CUDA driver opens them).
"""

import errno
import os
import sys

for path in sys.argv[1:]:
    try:
        os.close(os.open(path, os.O_RDWR | os.O_NOCTTY))
        result = "ok"
    except OSError as e:
        result = errno.errorcode.get(e.errno, str(e.errno))
    print(f"open {path}: {result}", flush=True)
