"""CPU job of the e2e test whose dump takes a while: touches <MiB> of
memory, then prints a counter like the shell COUNTER (0, 1, 2, ...).

Usage: mem_counter.py <MiB>
"""

import sys
import time

ballast = bytearray(int(sys.argv[1]) << 20)
for i in range(0, len(ballast), 4096):
    ballast[i] = 1
i = 0
while True:
    print(i, flush=True)
    i += 1
    time.sleep(0.2)
