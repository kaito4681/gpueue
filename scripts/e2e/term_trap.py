"""Job of the e2e test with a SIGTERM handler: prints "ready" once the
handler is installed, then sleeps; on SIGTERM it prints "got TERM" and
exits 0.

Usage: term_trap.py
"""

import signal
import sys
import time


def on_term(*_):
    print("got TERM", flush=True)
    sys.exit(0)


signal.signal(signal.SIGTERM, on_term)
print("ready", flush=True)
time.sleep(600)
