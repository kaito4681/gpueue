"""Checkpoint timing job: holds <vram_gb> of GPU memory and <ram_gb> of host
memory filled with a known pattern, and prints a counter with checksums so a
restored run can be checked.

Usage: job.py <vram_gb> <ram_gb>   (vram_gb = 0: CPU only, no CUDA)
"""

import sys
import time

import torch

GB = 1 << 30
vram_gb = float(sys.argv[1])
ram_gb = float(sys.argv[2])


def pattern(n_bytes, device):
    # int32 values 0, 1, 2, ... wrapping: not compressible to zero pages.
    n = int(n_bytes) // 4
    return torch.arange(n, dtype=torch.int32, device=device)


def checksum(t):
    # Sum of a strided sample (cheap on the CPU even for many GB).
    return int(t[:: max(1, t.numel() // 1_000_000)].to(torch.int64).sum().item())


host = pattern(ram_gb * GB, "cpu") if ram_gb > 0 else None
host_sum = checksum(host) if host is not None else 0
gpu = None
gpu_sum = 0
counter = None
if vram_gb > 0:
    gpu = pattern(vram_gb * GB, "cuda")
    gpu_sum = checksum(gpu)
    counter = torch.zeros(1, dtype=torch.int64, device="cuda")
print(f"ready host_sum={host_sum} gpu_sum={gpu_sum}", flush=True)

i = 0
while True:
    i += 1
    if counter is not None:
        counter += 1
        c = int(counter.item())
    else:
        c = i
    ok = (host is None or checksum(host) == host_sum) and (gpu is None or checksum(gpu) == gpu_sum)
    print(f"step {i} counter {c} ok {int(ok)}", flush=True)
    time.sleep(0.2)
