"""GPU job of the helper-systemd test (GPUEUE_HELPER_GPU=1).

Holds <vram_gb> of GPU memory filled with a known pattern and trains a tiny
linear model on the GPU with a fixed, deterministic schedule. Each step
prints

    <step> <counter on the GPU> <pattern intact> <loss>

The checker recomputes the loss of every step on the CPU (the model is
small enough to be exact in float64), so a job that was resumed after a
failed or successful checkpoint must keep printing consecutive steps, a
GPU counter equal to the step, an intact pattern and the expected loss.

Usage: gpu_job.py <vram_gb>
       gpu_job.py --check <log>   (exit 1 unless the log is consistent)
"""

import sys
import time

import torch

DIM = 64


def data(step, device):
    # Deterministic inputs and targets for a step.
    g = torch.Generator(device="cpu").manual_seed(step)
    x = torch.randn(32, DIM, generator=g, dtype=torch.float64)
    y = x @ torch.linspace(-1, 1, DIM, dtype=torch.float64)
    return x.to(device), y.to(device)


def train(steps, device, emit):
    w = torch.zeros(DIM, dtype=torch.float64, device=device)
    for step in range(1, steps + 1):
        x, y = data(step, device)
        err = x @ w - y
        loss = (err * err).mean()
        w -= 0.05 * (2 * x.T @ err / len(y))
        if not emit(step, float(loss.item())):
            return


def check(path):
    rows = [line.split() for line in open(path) if line.strip()]
    rows = [r for r in rows if len(r) == 4 and r[0].isdigit()]
    assert rows, "no steps"
    expected = []
    train(len(rows), "cpu", lambda s, loss: expected.append(loss) or True)
    for i, (step, counter, intact, loss) in enumerate(rows, start=1):
        assert int(step) == i, f"step {step} at line {i}: gap or repeat"
        assert int(counter) == i, f"GPU counter {counter} at step {i}"
        assert intact == "True", f"GPU memory changed at step {i}"
        assert abs(float(loss) - expected[i - 1]) <= 1e-9 * max(1.0, expected[i - 1]), (
            f"loss {loss} at step {i}, expected {expected[i - 1]}"
        )
    print(f"{path}: {len(rows)} consistent steps")


def main():
    if sys.argv[1] == "--check":
        check(sys.argv[2])
        return
    vram_gb = float(sys.argv[1])
    n = int(vram_gb * (1 << 30)) // 4
    block = torch.arange(n, dtype=torch.int32, device="cuda")
    stride = max(1, n // 1_000_000)
    want = int(block[::stride].to(torch.int64).sum().item())
    counter = torch.zeros(1, dtype=torch.int64, device="cuda")

    def emit(step, loss):
        counter.add_(1)
        intact = int(block[::stride].to(torch.int64).sum().item()) == want
        print(step, int(counter.item()), intact, repr(loss), flush=True)
        time.sleep(0.2)
        return True

    train(10**9, "cuda", emit)


if __name__ == "__main__":
    main()
