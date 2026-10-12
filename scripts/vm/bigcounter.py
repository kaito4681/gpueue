"""A counter (0, 1, 2, ... every 0.2 s) that holds N MiB of random memory,
so that dumping it takes a while. Usage: bigcounter.py MIB"""

import os
import sys
import time

ballast = os.urandom(int(sys.argv[1]) << 20)
i = 0
while True:
    print(i, flush=True)
    i += 1
    time.sleep(0.2)
