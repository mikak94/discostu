"""Trains the zero-latency noise suppressor.

    uv run python train.py --steps 40000

Clean speech and noise come from the pools `prepare.py` builds; every batch is
mixed fresh on the GPU (levels, SNR, room reverb, mic EQ, synthetic hiss and
hum, band limits). The network learns per-band gains: sqrt(clean / noisy)
energy in the same analysis window the app uses.
"""

import argparse
import math
import time
from pathlib import Path

import numpy as np
import torch
import torch.nn.functional as F
from torch import nn

import dsp

ROOT = Path("D:/discostu-data")
SEG = 4 * dsp.SR  # samples per training example
RIR_LEN = dsp.SR // 2
SPEECH = {
    # pool name -> (min snr, min hf) : what counts as clean
    "vctk_wav48_silence_trimmed_000": (25.0, -60.0),
    "read_speech_024_4": (30.0, -60.0),
    "german_speech_016_4": (32.0, -60.0),
    "spanish_speech_001_4": (35.0, -60.0),
    "emotional_speech_000_NA_NA": (32.0, -60.0),
}
NOISE = ["freesound_000", "freesound_001", "audioset_000"]


class Pool:
    def __init__(self, names, rule=None, val=False):
        self.data, self.files = [], []
        for name in names:
            tsv = next((p for p in (ROOT / "pool").glob(f"{name}*.tsv")), None)
            if tsv is None:
                print(f"  (no pool for {name})")
                continue
            pcm = np.memmap(tsv.with_suffix(".pcm"), dtype=np.int16, mode="r")
            rows = np.loadtxt(tsv, delimiter="\t", usecols=(0, 1, 2, 3), ndmin=2)
            keep = np.ones(len(rows), bool)
            if rule is not None:
                snr, hf = rule[name]
                keep &= (rows[:, 2] >= snr) & (rows[:, 3] >= hf)
            # Every 40th file is held out for validation.
            held = (np.arange(len(rows)) % 40) == 0
            keep &= held if val else ~held
            d = len(self.data)
            self.data.append(pcm)
            self.files += [(d, int(o), int(n)) for o, n, *_ in rows[keep]]
            print(f"  {tsv.stem}: {keep.sum()}/{len(rows)} files, {rows[keep, 1].sum() / dsp.SR / 3600:.1f} h")
        lens = np.array([n for *_, n in self.files], dtype=np.float64)
        self.p = lens / lens.sum()

    def pick(self, rng):
        return self.files[rng.choice(len(self.files), p=self.p)]

    def read(self, f, start, n):
        d, o, _ = f
        return self.data[d][o + start : o + start + n].astype(np.float32) / 32768.0


class Mixes(torch.utils.data.IterableDataset):
    """Raw material for one example: speech (utterances with pauses), noise
    (one or two clips), an impulse response or none."""

    def __init__(self, val=False, seed=0):
        self.val, self.seed = val, seed

    def __iter__(self):
        info = torch.utils.data.get_worker_info()
        wid = info.id if info else 0
        rng = np.random.default_rng(self.seed + 1000 * wid + (7 if self.val else 0))
        speech = Pool(list(SPEECH), SPEECH, self.val)
        noise = Pool(NOISE, None, self.val)
        rirs = sorted((ROOT / "rir").rglob("*.wav"))
        rirs = [r for r in rirs if "largeroom" not in str(r)]
        import soundfile as sf

        while True:
            s = np.zeros(SEG, np.float32)
            pos = int(rng.uniform(0, 0.8) * dsp.SR) if rng.random() < 0.5 else 0
            while pos < SEG - dsp.SR // 4:
                f = speech.pick(rng)
                n = min(f[2], SEG - pos)
                start = rng.integers(0, f[2] - n + 1)
                s[pos : pos + n] = speech.read(f, start, n) * 10 ** (rng.uniform(-4, 4) / 20)
                pos += n + int(rng.uniform(0.05, 0.6) * dsp.SR)
            nz = np.zeros(SEG, np.float32)
            for k in range(1 if rng.random() < 0.7 else 2):
                pos = 0
                layer = np.zeros(SEG, np.float32)
                while pos < SEG:
                    f = noise.pick(rng)
                    n = min(f[2], SEG - pos)
                    start = rng.integers(0, f[2] - n + 1)
                    layer[pos : pos + n] = noise.read(f, start, n)
                    pos += n
                layer /= np.sqrt(np.mean(layer**2)) + 1e-6
                nz += layer * 10 ** (rng.uniform(-10, 0) / 20 if k else 0)
            rir = np.zeros(RIR_LEN, np.float32)
            if rng.random() < 0.3 and rirs:
                h, _ = sf.read(rirs[rng.integers(len(rirs))], dtype="float32", always_2d=True)
                h = h[:RIR_LEN, 0]
                rir[: len(h)] = h
            else:
                rir[0] = 1.0
            yield s, nz, rir


def fft_conv(x, h):
    n = x.shape[-1] + h.shape[-1]
    n = 1 << (n - 1).bit_length()
    y = torch.fft.irfft(torch.fft.rfft(x, n) * torch.fft.rfft(h, n), n)
    return y[..., : x.shape[-1]]


def random_eq(x, g: torch.Generator, depth_db=6.0):
    """Smooth random magnitude curve over log frequency (mic coloration)."""
    b = x.shape[0]
    spec = torch.fft.rfft(x)
    nb = spec.shape[-1]
    knots = (torch.rand(b, 1, 10, device=x.device, generator=g) * 2 - 1) * depth_db
    fine = F.interpolate(knots, size=1000, mode="linear", align_corners=True)[:, 0]
    hz = torch.linspace(0, dsp.SR / 2, nb, device=x.device)
    pos = (torch.log2(hz.clamp(min=50) / 50) / math.log2(20000 / 50)).clamp(0, 1)
    curve = fine[:, (pos * 999).long()]
    return torch.fft.irfft(spec * 10 ** (curve / 20), x.shape[-1])


def lowpass(x, cutoff_hz):
    spec = torch.fft.rfft(x)
    hz = torch.linspace(0, dsp.SR / 2, spec.shape[-1], device=x.device)
    m = torch.sigmoid((cutoff_hz[:, None] - hz) / 150.0)
    return torch.fft.irfft(spec * m, x.shape[-1])


def synthetic(b, n, g, device):
    """Coloured hiss (random spectral slope) or mains hum with harmonics."""
    white = torch.randn(b, n, device=device, generator=g)
    spec = torch.fft.rfft(white)
    hz = torch.linspace(1, dsp.SR / 2, spec.shape[-1], device=device)
    slope = torch.rand(b, 1, device=device, generator=g) * 9 - 6  # dB/octave
    col = torch.fft.irfft(spec * 10 ** (slope * torch.log2(hz / 1000) / 20), n)
    t = torch.arange(n, device=device) / dsp.SR
    f0 = torch.where(torch.rand(b, 1, device=device, generator=g) < 0.5, 50.0, 60.0)
    hum = sum(torch.sin(2 * math.pi * f0 * k * t + k) / k ** torch.rand(b, 1, device=device, generator=g)
              for k in range(1, 12))
    pick = torch.rand(b, 1, device=device, generator=g) < 0.7
    out = torch.where(pick, col, hum)
    return out / (out.square().mean(-1, keepdim=True).sqrt() + 1e-6)


def active_power(x):
    f = x[:, : x.shape[1] // 480 * 480].reshape(x.shape[0], -1, 480).square().mean(-1)
    on = f > f.amax(-1, keepdim=True) * 1e-3
    return (f * on).sum(-1) / on.sum(-1).clamp(min=1)


class Mixer(nn.Module):
    def __init__(self):
        super().__init__()
        self.ana = dsp.Analysis()

    @torch.no_grad()
    def forward(self, s, nz, rir, g):
        b, n, dev = s.shape[0], s.shape[1], s.device
        u = lambda lo, hi: lo + (hi - lo) * torch.rand(b, 1, device=dev, generator=g)
        s = fft_conv(s, rir)
        eq = torch.rand(b, 1, device=dev, generator=g) < 0.6
        s = torch.where(eq, random_eq(s, g), s)
        syn = torch.rand(b, 1, device=dev, generator=g) < 0.25
        nz = torch.where(syn, synthetic(b, n, g, dev), nz)
        nz = torch.where(torch.rand(b, 1, device=dev, generator=g) < 0.6, random_eq(nz, g, 8.0), nz)
        level = 10 ** (u(-45, -10) / 10)
        s = s * torch.sqrt(level / (active_power(s)[:, None] + 1e-12))
        snr = u(-5, 30)
        noise_p = level / 10 ** (snr / 10)
        nz = nz * torch.sqrt(noise_p / (nz.square().mean(-1, keepdim=True) + 1e-12))
        # Some clean (must pass untouched), some noise-only.
        nz = torch.where(torch.rand(b, 1, device=dev, generator=g) < 0.08, 0.0 * nz, nz)
        s = torch.where(torch.rand(b, 1, device=dev, generator=g) < 0.05, 0.0 * s, s)
        lp = torch.rand(b, device=dev, generator=g) < 0.12
        cutoff = torch.where(lp, 4000 + 12000 * torch.rand(b, device=dev, generator=g), torch.full((b,), 1e6, device=dev))
        s, nz = lowpass(s, cutoff), lowpass(nz, cutoff)
        x = s + nz
        ps, px = self.ana.power(s), self.ana.power(x)
        es, ex = self.ana.band_energy(ps), self.ana.band_energy(px)
        target = torch.sqrt(es / (ex + 1e-12)).clamp(0, 1)
        mask = (ex > 1e-8).float()
        return self.ana.features(px), target, mask, s, x


class Net(nn.Module):
    def __init__(self, hidden=128):
        super().__init__()
        self.register_buffer("mean", torch.zeros(dsp.NFEAT))
        self.register_buffer("std", torch.ones(dsp.NFEAT))
        self.inp = nn.Linear(dsp.NFEAT, hidden)
        self.gru1 = nn.GRU(hidden, hidden, batch_first=True)
        self.gru2 = nn.GRU(hidden, hidden, batch_first=True)
        self.out = nn.Linear(2 * hidden, dsp.NB)

    def forward(self, f, h1=None, h2=None):
        x = torch.tanh(self.inp((f - self.mean) / self.std))
        y1, h1 = self.gru1(x, h1)
        y2, h2 = self.gru2(y1, h2)
        return torch.sigmoid(self.out(torch.cat([y1, y2], -1))), h1, h2


def loss_fn(pred, target, mask):
    d = torch.sqrt(pred + 1e-8) - torch.sqrt(target + 1e-8)
    # Cutting speech (pred below target) hurts more than leaving some noise.
    w = torch.where(d < 0, 2.0, 1.0) * mask
    return ((d.square() + 10 * d.square().square()) * w).sum() / mask.sum().clamp(min=1)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--steps", type=int, default=40000)
    ap.add_argument("--batch", type=int, default=48)
    ap.add_argument("--lr", type=float, default=2e-3)
    ap.add_argument("--hidden", type=int, default=128)
    ap.add_argument("--out", default="runs/main")
    ap.add_argument("--resume", action="store_true")
    args = ap.parse_args()
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    dev = torch.device("cuda")
    torch.backends.cudnn.benchmark = True

    loader = torch.utils.data.DataLoader(Mixes(), batch_size=args.batch, num_workers=6,
                                         pin_memory=True, persistent_workers=True, prefetch_factor=4)
    val_loader = torch.utils.data.DataLoader(Mixes(val=True, seed=99), batch_size=args.batch, num_workers=1)
    g = torch.Generator(device=dev)
    g.manual_seed(1)
    mixer = Mixer().to(dev)
    net = Net(args.hidden).to(dev)
    opt = torch.optim.AdamW(net.parameters(), lr=args.lr, weight_decay=1e-4)
    sched = torch.optim.lr_scheduler.OneCycleLR(opt, args.lr, total_steps=args.steps, pct_start=0.03)
    step = 0

    it = iter(loader)
    if args.resume and (out / "last.pt").exists():
        ck = torch.load(out / "last.pt", map_location=dev)
        net.load_state_dict(ck["net"]); opt.load_state_dict(ck["opt"]); sched.load_state_dict(ck["sched"])
        step = ck["step"]
        print(f"resumed at {step}")
    else:
        # Feature normalisation from a few batches.
        fs = []
        for _ in range(8):
            s, nz, rir = (t.to(dev, non_blocking=True) for t in next(it))
            fs.append(mixer(s, nz, rir, g)[0].reshape(-1, dsp.NFEAT))
        fs = torch.cat(fs)
        net.mean.copy_(fs.mean(0)); net.std.copy_(fs.std(0).clamp(min=0.1))

    vit = iter(val_loader)
    val = [tuple(t.to(dev) for t in next(vit)) for _ in range(4)]
    gv = torch.Generator(device=dev)
    gv.manual_seed(5)
    val = [mixer(*v, gv)[:3] for v in val]
    del vit, val_loader

    t0, running = time.time(), 0.0
    best = float("inf")
    while step < args.steps:
        s, nz, rir = (t.to(dev, non_blocking=True) for t in next(it))
        feat, target, mask, *_ = mixer(s, nz, rir, g)
        pred, *_ = net(feat)
        loss = loss_fn(pred, target, mask)
        opt.zero_grad(set_to_none=True)
        loss.backward()
        nn.utils.clip_grad_norm_(net.parameters(), 1.0)
        opt.step(); sched.step()
        step += 1
        running += loss.item() if step % 50 == 0 else 0.0
        if step % 500 == 0:
            net.eval()
            with torch.no_grad():
                vl = sum(loss_fn(net(f)[0], t, m).item() for f, t, m in val) / len(val)
            net.train()
            print(f"step {step} train {running / 10:.4f} val {vl:.4f} lr {sched.get_last_lr()[0]:.2e} "
                  f"{(time.time() - t0) / 500 * 1000:.0f} ms/step", flush=True)
            running, t0 = 0.0, time.time()
            torch.save({"net": net.state_dict(), "opt": opt.state_dict(), "sched": sched.state_dict(),
                        "step": step, "hidden": args.hidden}, out / "last.pt")
            if vl < best:
                best = vl
                torch.save({"net": net.state_dict(), "hidden": args.hidden, "step": step, "val": vl}, out / "best.pt")


if __name__ == "__main__":
    main()
