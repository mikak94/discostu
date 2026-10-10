"""Signal processing shared by training and the app (app/src/audio/dsp/denoise.rs
mirrors this exactly; `export.py` writes test vectors that pin the two together).

Zero added latency: every 2.5 ms block the network looks at the last 1024 mic
samples (ending with the block itself) and predicts 32 band gains. Those become
a minimum-phase FIR filter that is applied to the block right away, crossfading
from the previous block's filter. Nothing waits for future samples.
"""

import math

import numpy as np
import torch

SR = 48_000
BLOCK = 120
WIN = 1024
NBIN = WIN // 2 + 1
# Analysis window: a long rise and a one-block fall, so the newest samples
# count fully but the spectrum still has 47 Hz resolution.
FALL = BLOCK
RISE = WIN - FALL
# Band centres in FFT bins (47 Hz each), ERB-spaced from DC to 20 kHz; the
# lowest bands are single bins.
CENTERS = [0, 1, 2, 3, 4, 5, 7, 9, 11, 13, 16, 19, 23, 27, 32, 38, 44, 52, 61, 71, 83, 97,
           113, 131, 152, 176, 205, 237, 275, 318, 369, 427]
NB = len(CENTERS)
# Fine low-frequency bins (pitch harmonics) fed to the network as they are.
FINE_LO, FINE_HI = 1, 49
NFEAT = NB + (FINE_HI - FINE_LO)
# Minimum-phase filter length (5.3 ms of impulse response, front-loaded).
TAPS = 256
# Gains are floored at -60 dB before the log.
MIN_GAIN = 1e-3
EPS = 1e-9


def window() -> np.ndarray:
    n = np.arange(WIN, dtype=np.float64)
    w = np.empty(WIN)
    r = n < RISE
    w[r] = 0.5 - 0.5 * np.cos(np.pi * (n[r] + 0.5) / RISE)
    m = n[~r] - RISE
    w[~r] = 0.5 + 0.5 * np.cos(np.pi * (m + 0.5) / FALL)
    return w.astype(np.float32)


def band_matrix() -> np.ndarray:
    """(NBIN, NB) triangular weights: band energy = power @ M, and
    per-bin gain = M-weighted interpolation of band gains (rows sum to 1)."""
    m = np.zeros((NBIN, NB), dtype=np.float32)
    for i in range(NB - 1):
        lo, hi = CENTERS[i], CENTERS[i + 1]
        for j in range(hi - lo):
            frac = j / (hi - lo)
            m[lo + j, i] += 1.0 - frac
            m[lo + j, i + 1] += frac
    m[CENTERS[-1]:, NB - 1] = 1.0
    return m


def frames(x: torch.Tensor) -> torch.Tensor:
    """(B, N) samples -> (B, N // BLOCK, WIN): the WIN samples ending with each
    block (zeros before the start)."""
    t = x.shape[-1] // BLOCK
    x = torch.nn.functional.pad(x[..., : t * BLOCK], (WIN - BLOCK, 0))
    return x.unfold(-1, WIN, BLOCK)


class Analysis(torch.nn.Module):
    def __init__(self):
        super().__init__()
        self.register_buffer("win", torch.from_numpy(window()))
        self.register_buffer("bands", torch.from_numpy(band_matrix()))

    def power(self, x: torch.Tensor) -> torch.Tensor:
        spec = torch.fft.rfft(frames(x) * self.win, dim=-1)
        return spec.real.square() + spec.imag.square()

    def band_energy(self, p: torch.Tensor) -> torch.Tensor:
        return p @ self.bands

    def features(self, p: torch.Tensor) -> torch.Tensor:
        """log10 band energies, then log10 fine low bins."""
        e = self.band_energy(p)
        return torch.cat([torch.log10(e + EPS), torch.log10(p[..., FINE_LO:FINE_HI] + EPS)], dim=-1)


def gains_to_bins(g: np.ndarray) -> np.ndarray:
    return band_matrix() @ g


def min_phase(g_bins: np.ndarray) -> np.ndarray:
    """Magnitude response on the NBIN grid -> TAPS-long minimum-phase FIR
    (real cepstrum folding), with a short fade at the end."""
    logm = np.log(np.maximum(g_bins, MIN_GAIN)).astype(np.float64)
    c = np.fft.irfft(logm, WIN)
    fold = np.zeros(WIN)
    fold[0] = c[0]
    fold[1 : WIN // 2] = 2.0 * c[1 : WIN // 2]
    fold[WIN // 2] = c[WIN // 2]
    h = np.fft.irfft(np.exp(np.fft.rfft(fold)), WIN)[:TAPS]
    return (h * fade()).astype(np.float32)


def fade() -> np.ndarray:
    f = np.ones(TAPS)
    n = TAPS // 4
    f[TAPS - n :] = 0.5 + 0.5 * np.cos(np.pi * (np.arange(n) + 0.5) / n)
    return f


class Stream:
    """Reference streaming processor (numpy), block by block, exactly as the
    app runs it. `predict(features) -> gains` is the network step."""

    def __init__(self, predict):
        self.predict = predict
        self.win = window()
        self.bands = band_matrix()
        self.hist = np.zeros(WIN + TAPS, dtype=np.float32)
        self.h = np.zeros(TAPS, dtype=np.float32)
        self.h[0] = 1.0

    def process(self, block: np.ndarray, amount: float = 1.0) -> np.ndarray:
        self.hist = np.concatenate([self.hist[BLOCK:], block.astype(np.float32)])
        spec = np.fft.rfft(self.hist[-WIN:] * self.win)
        p = (spec.real**2 + spec.imag**2).astype(np.float32)
        feat = np.concatenate([np.log10(p @ self.bands + EPS), np.log10(p[FINE_LO:FINE_HI] + EPS)])
        g = np.asarray(self.predict(feat.astype(np.float32)), dtype=np.float32)
        g = np.power(np.maximum(g, MIN_GAIN), amount)
        h_new = min_phase(self.bands @ g)
        seg = self.hist[-(BLOCK + TAPS - 1) :]
        old = np.convolve(seg, self.h, mode="valid")
        new = np.convolve(seg, h_new, mode="valid")
        r = (np.arange(BLOCK, dtype=np.float32) + 1.0) / BLOCK
        self.h = h_new
        if self.filters is not None:
            self.filters.append(h_new)
        return ((1.0 - r) * old + r * new).astype(np.float32)

    filters = None


def run_stream(x: np.ndarray, predict, amount: float = 1.0, filters: list | None = None) -> np.ndarray:
    """`filters`, if given, collects each block's FIR (for `apply_filters`)."""
    s = Stream(predict)
    s.filters = filters
    out = np.zeros(len(x) // BLOCK * BLOCK, dtype=np.float32)
    for i in range(0, len(out), BLOCK):
        out[i : i + BLOCK] = s.process(x[i : i + BLOCK], amount)
    return out


def apply_filters(x: np.ndarray, filters: list) -> np.ndarray:
    """Runs another signal through the same filter sequence a stream chose
    (it's linear: speech and noise can be followed separately)."""
    hist = np.zeros(TAPS - 1, dtype=np.float32)
    h = np.zeros(TAPS, dtype=np.float32)
    h[0] = 1.0
    r = (np.arange(BLOCK, dtype=np.float32) + 1.0) / BLOCK
    out = np.zeros(len(filters) * BLOCK, dtype=np.float32)
    for b, h_new in enumerate(filters):
        seg = np.concatenate([hist, x[b * BLOCK : (b + 1) * BLOCK]])
        out[b * BLOCK : (b + 1) * BLOCK] = (1 - r) * np.convolve(seg, h, "valid") + r * np.convolve(seg, h_new, "valid")
        hist, h = seg[BLOCK:], h_new
    return out


def check():
    """Sanity: unity gains give an exact passthrough; a min-phase filter's
    magnitude matches what was asked."""
    x = np.random.default_rng(0).standard_normal(BLOCK * 50).astype(np.float32) * 0.1
    y = run_stream(x, lambda f: np.ones(NB))
    assert np.max(np.abs(y - x[: len(y)])) < 1e-5, np.max(np.abs(y - x[: len(y)]))
    g = np.ones(NB)
    g[10:20] = 0.05
    h = min_phase(gains_to_bins(g))
    resp = np.abs(np.fft.rfft(h, WIN))
    want = gains_to_bins(g)
    err_db = 20 * np.log10(resp[30:300] / want[30:300])
    print("min-phase magnitude error (dB, 1.4-14 kHz): max", np.abs(err_db).max().round(2))
    centroid = (np.arange(TAPS) * h**2).sum() / (h**2).sum()
    print("energy centroid of filter:", round(centroid / SR * 1000, 3), "ms")


if __name__ == "__main__":
    check()
