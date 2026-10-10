"""Exports a checkpoint for the app and pins Python and Rust together.

    uv run python export.py runs/main/best.pt

Writes app/src/audio/dsp/denoise/model.bin (weights, little-endian f32) and
vector.bin (a short input and the exact output `dsp.Stream` gives, which the
Rust test must reproduce).
"""

import struct
import sys
from pathlib import Path

import numpy as np
import torch

import dsp
from train import Net

OUT = Path(__file__).resolve().parent.parent / "app/src/audio/dsp/denoise"
MAGIC = b"DSNS"
VERSION = 1


def sigmoid(x):
    return 1.0 / (1.0 + np.exp(-x))


class Step:
    """One network step in numpy, written the way the Rust code does it."""

    def __init__(self, sd, hidden):
        self.w = {k: v.detach().cpu().numpy().astype(np.float32) for k, v in sd.items()}
        self.hidden = hidden
        self.h1 = np.zeros(hidden, np.float32)
        self.h2 = np.zeros(hidden, np.float32)

    def gru(self, p, x, h):
        w = self.w
        gi = w[f"{p}.weight_ih_l0"] @ x + w[f"{p}.bias_ih_l0"]
        gh = w[f"{p}.weight_hh_l0"] @ h + w[f"{p}.bias_hh_l0"]
        n = self.hidden
        r = sigmoid(gi[:n] + gh[:n])
        z = sigmoid(gi[n : 2 * n] + gh[n : 2 * n])
        c = np.tanh(gi[2 * n :] + r * gh[2 * n :])
        return ((1 - z) * c + z * h).astype(np.float32)

    def __call__(self, feat):
        w = self.w
        x = np.tanh(w["inp.weight"] @ ((feat - w["mean"]) / w["std"]) + w["inp.bias"])
        self.h1 = self.gru("gru1", x, self.h1)
        self.h2 = self.gru("gru2", self.h1, self.h2)
        return sigmoid(w["out.weight"] @ np.concatenate([self.h1, self.h2]) + w["out.bias"])


ORDER = ["mean", "std", "inp.weight", "inp.bias",
         "gru1.weight_ih_l0", "gru1.weight_hh_l0", "gru1.bias_ih_l0", "gru1.bias_hh_l0",
         "gru2.weight_ih_l0", "gru2.weight_hh_l0", "gru2.bias_ih_l0", "gru2.bias_hh_l0",
         "out.weight", "out.bias"]


def load(path):
    ck = torch.load(path, map_location="cpu")
    net = Net(ck["hidden"])
    net.load_state_dict(ck["net"])
    net.eval()
    return net, ck


def main():
    net, ck = load(sys.argv[1])
    sd = net.state_dict()
    hidden = ck["hidden"]

    # The numpy step must match the torch sequence forward.
    rng = np.random.default_rng(3)
    feats = (rng.standard_normal((200, dsp.NFEAT)) * net.std.numpy() + net.mean.numpy()).astype(np.float32)
    with torch.no_grad():
        ref = net(torch.from_numpy(feats)[None])[0][0].numpy()
    step = Step(sd, hidden)
    mine = np.stack([step(f) for f in feats])
    err = np.abs(ref - mine).max()
    print(f"numpy step vs torch: max diff {err:.2e}")
    assert err < 1e-4

    OUT.mkdir(parents=True, exist_ok=True)
    with open(OUT / "model.bin", "wb") as f:
        f.write(MAGIC)
        f.write(struct.pack("<5I", VERSION, hidden, dsp.NFEAT, dsp.NB, dsp.TAPS))
        for k in ORDER:
            f.write(sd[k].numpy().astype("<f4").tobytes())
    print(f"model.bin: {(OUT / 'model.bin').stat().st_size / 1024:.0f} KiB (step {ck['step']}, val {ck.get('val', 0):.4f})")

    # Test vector: 0.3 s of speech-like tones in noise, through the full stream.
    t = np.arange(int(0.3 * dsp.SR)) / dsp.SR
    x = 0.1 * np.sin(2 * np.pi * 180 * t) * (np.sin(2 * np.pi * 3 * t) > 0)
    x += 0.1 * np.sin(2 * np.pi * 360 * t + 1) * (np.sin(2 * np.pi * 3 * t) > 0)
    x += 0.02 * rng.standard_normal(len(t))
    x = x.astype(np.float32)[: len(t) // dsp.BLOCK * dsp.BLOCK]
    y = dsp.run_stream(x, Step(sd, hidden), amount=0.8)
    with open(OUT / "vector.bin", "wb") as f:
        f.write(struct.pack("<I", len(x)))
        f.write(x.astype("<f4").tobytes())
        f.write(y.astype("<f4").tobytes())
    print(f"vector.bin: {len(x)} samples, output/input energy {10 * np.log10((y**2).sum() / (x**2).sum()):.1f} dB")


if __name__ == "__main__":
    main()
