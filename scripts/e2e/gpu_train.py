"""GPU training job of the e2e test with DataLoader worker processes.

Uses the default multiprocessing start method of the platform ("fork" on
Linux before Python 3.14), as a user who does not think about checkpoints
would. Deterministic: each sample depends only on its index, so the loss
and the parameter checksum of every step match a run without a
checkpoint.

Usage: gpu_train.py <steps> <workers> [pin] [persistent] [epochs]
  pin, persistent: DataLoader(pin_memory=True / persistent_workers=True)
  epochs: epochs of 10 batches instead of one endless epoch, so new workers
          are forked from the (restored) CUDA process every 10 steps unless
          they are persistent
Prints "step=<n> loss=<..> csum=<..> worker=<id>:<pid>" per step and
"done" at the end. The worker pid shows that the restored workers keep
producing batches.
"""

import os
import sys
import time

import torch
import torch.nn as nn


class Samples(torch.utils.data.Dataset):
    def __init__(self, length):
        self.length = length

    def __len__(self):
        return self.length

    def __getitem__(self, i):
        g = torch.Generator().manual_seed(i)
        w = torch.utils.data.get_worker_info()
        return (
            torch.randn(256, generator=g),
            int(torch.randint(0, 10, (1,), generator=g)),
            w.id if w else -1,
            os.getpid(),
        )


def main():
    steps = int(sys.argv[1])
    workers = int(sys.argv[2])
    flags = set(sys.argv[3:])
    torch.manual_seed(0)
    dev = "cuda"
    model = nn.Sequential(nn.Linear(256, 512), nn.ReLU(), nn.Linear(512, 10)).to(dev)
    opt = torch.optim.SGD(model.parameters(), lr=1e-2)
    loss_f = nn.CrossEntropyLoss()
    loader = torch.utils.data.DataLoader(
        Samples(320 if "epochs" in flags else 10**9),
        batch_size=32,
        num_workers=workers,
        pin_memory="pin" in flags,
        persistent_workers="persistent" in flags,
    )
    start_method = torch.multiprocessing.get_start_method()
    preload = os.environ.get("LD_PRELOAD", "")
    print(
        f"python={sys.version.split()[0]} torch={torch.__version__} start_method={start_method} preload={preload}",
        flush=True,
    )
    step = 0
    while step < steps:
        step = train_epoch(loader, model, opt, loss_f, step, steps)
    print("done", flush=True)


def train_epoch(loader, model, opt, loss_f, step, steps):
    dev = "cuda"
    for x, y, wid, wpid in loader:
        step += 1
        x, y = x.to(dev, non_blocking=True), y.to(dev, non_blocking=True)
        loss = loss_f(model(x), y)
        opt.zero_grad()
        loss.backward()
        opt.step()
        csum = sum(float(p.detach().double().sum()) for p in model.parameters())
        print(f"step={step} loss={loss.item():.8f} csum={csum:.8f} worker={int(wid[0])}:{int(wpid[0])}", flush=True)
        if step == steps:
            break
        time.sleep(0.1)
    return step


if __name__ == "__main__":
    main()
