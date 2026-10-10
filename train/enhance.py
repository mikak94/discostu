"""Runs a WAV through the streaming suppressor exactly as the app would.

    uv run python enhance.py runs/main/best.pt in.wav out.wav [amount]
    uv run python enhance.py runs/main/best.pt --mix clean.wav noise.wav snr_db out_dir
"""

import sys
from pathlib import Path

import numpy as np
import soundfile as sf
from scipy.signal import resample_poly

import dsp
from export import Step, load


def read(path):
    x, sr = sf.read(path, dtype="float32", always_2d=True)
    x = x.mean(axis=1)
    if sr != dsp.SR:
        x = resample_poly(x, dsp.SR, sr).astype(np.float32)
    return x


def enhance(ckpt, x, amount=1.0):
    net, ck = load(ckpt)
    return dsp.run_stream(x, Step(net.state_dict(), ck["hidden"]), amount)


def main():
    a = sys.argv[1:]
    if a[1] == "--mix":
        ckpt, clean, noise, snr, out = a[0], read(a[2]), read(a[3]), float(a[4]), Path(a[5])
        out.mkdir(parents=True, exist_ok=True)
        noise = np.resize(noise, len(clean))
        noise *= np.sqrt(np.mean(clean**2) / np.mean(noise**2) / 10 ** (snr / 10))
        x = clean + noise
        y = enhance(ckpt, x)
        n = len(y)
        for name, sig in [("noisy", x[:n]), ("clean", clean[:n]), ("enhanced", y)]:
            sf.write(out / f"{name}.wav", sig, dsp.SR)

        def snr_of(sig):
            return 10 * np.log10(np.sum(clean[:n] ** 2) / np.sum((sig - clean[:n]) ** 2))

        print(f"SNR in {snr_of(x[:n]):.1f} dB -> out {snr_of(y):.1f} dB")
        return
    ckpt, inp, out = a[0], a[1], a[2]
    amount = float(a[3]) if len(a) > 3 else 1.0
    x = read(inp)
    y = enhance(ckpt, x, amount)
    sf.write(out, y, dsp.SR)
    print(f"{inp}: {len(x) / dsp.SR:.1f} s, output/input energy {10 * np.log10(np.sum(y**2) / np.sum(x[: len(y)] ** 2)):.1f} dB")


if __name__ == "__main__":
    main()
