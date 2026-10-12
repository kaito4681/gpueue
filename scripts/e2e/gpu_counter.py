"""GPU job of the e2e test: keeps a counter in GPU memory.

Prints "<step> <value on the GPU>"; both must stay equal across a
checkpoint/restore (the GPU memory was saved and restored).
"""

import time

import torch

x = torch.zeros(1, device="cuda")
step = 0
while True:
    x += 1
    step += 1
    print(step, int(x.item()), flush=True)
    time.sleep(0.2)
