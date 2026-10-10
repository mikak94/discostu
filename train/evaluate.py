"""Scores a checkpoint through the exact streaming path on held-out files.

    uv run python evaluate.py runs/main/best.pt [n]

The filters a stream picks from the noisy mix are replayed on the speech and
the noise separately (the filter is linear), so we see exactly:
  noise    how far the noise in the mix is turned down
  speech   how much the speech changes: band-energy distortion in dB over
           speech frames (0 = untouched), and its overall level change
  SNR      speech-to-noise energy after minus before (phase-blind)
"""

import sys

import numpy as np

import dsp
from export import Step, load
from train import NOISE, SPEECH, Pool

BANDS = dsp.band_matrix()


def band_db(x):
    n = len(x) // 512 - 1
    f = np.stack([x[i * 512 : i * 512 + 1024] * np.hanning(1024) for i in range(n)])
    p = np.abs(np.fft.rfft(f, axis=1)) ** 2
    return 10 * np.log10(p @ BANDS + 1e-10)


def distortion(s, s_out):
    a, b = band_db(s), band_db(s_out)
    frame = 10 * np.log10(np.sum(10 ** (a / 10), axis=1))
    on = frame > frame.max() - 35
    w = 10 ** (a[on] / 10)
    return float(np.sum(np.abs(a[on] - b[on]) * w) / np.sum(w))


def db(a, b):
    return 10 * np.log10(np.sum(a**2) / (np.sum(b**2) + 1e-20) + 1e-20)


def main():
    net, ck = load(sys.argv[1])
    n = int(sys.argv[2]) if len(sys.argv) > 2 else 12
    sd = net.state_dict()
    rng = np.random.default_rng(42)
    speech, noise = Pool(list(SPEECH), SPEECH, val=True), Pool(NOISE, None, val=True)
    seg = 6 * dsp.SR
    levels = (None, 0, 5, 10, 20)
    res = {l: {"noise": [], "pause": [], "dist": [], "level": [], "snr": []} for l in levels}
    for _ in range(n):
        f = speech.pick(rng)
        k = min(seg, f[2]) // dsp.BLOCK * dsp.BLOCK
        s = speech.read(f, rng.integers(0, f[2] - k + 1), k)
        s *= 10 ** (-25 / 20) / np.sqrt(np.mean(s**2))
        f = noise.pick(rng)
        z = np.resize(noise.read(f, rng.integers(0, max(1, f[2] - k + 1)), k), k)
        z /= np.sqrt(np.mean(z**2)) + 1e-9
        for l in levels:
            zl = z * 0 if l is None else z * np.sqrt(np.mean(s**2) / 10 ** (l / 10))
            filters = []
            dsp.run_stream(s + zl, Step(sd, ck["hidden"]), filters=filters)
            so, zo = dsp.apply_filters(s, filters), dsp.apply_filters(zl, filters)
            r = res[l]
            r["dist"].append(distortion(s, so))
            r["level"].append(db(so, s))
            if l is not None:
                r["noise"].append(db(zo, zl))
                fs = (s[: len(s) // 480 * 480].reshape(-1, 480) ** 2).mean(1)
                quiet = np.repeat(fs < fs.max() * 1e-4, 480)
                if quiet.sum() > dsp.SR // 10:
                    r["pause"].append(db(zo[: len(quiet)][quiet[: len(zo)]], zl[: len(quiet)][quiet[: len(zo)]]))
                r["snr"].append(db(so, zo) - db(s, zl))
    print(f"{sys.argv[1]} (step {ck['step']}), {n} clips, medians")
    print("  input      speech distortion  speech level  noise     in pauses  SNR gain")
    for l in levels:
        r = res[l]
        name = "clean" if l is None else f"{l:>2} dB SNR"
        noise_s = f"{np.median(r['noise']):6.1f} dB" if r["noise"] else "    -    "
        pause_s = f"{np.median(r['pause']):6.1f} dB" if r["pause"] else "    -    "
        snr_s = f"+{np.median(r['snr']):.1f} dB" if r["snr"] else "-"
        print(f"  {name:<10} {np.median(r['dist']):6.2f} dB          {np.median(r['level']):+5.1f} dB     {noise_s}  {pause_s}  {snr_s}")


if __name__ == "__main__":
    main()
