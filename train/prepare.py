"""Streams DNS Challenge archives (tar.bz2) into flat int16 pools without
unpacking them: <out>/<name>.pcm plus <name>.tsv (offset, length, metrics, file).

    uv run python prepare.py D:/discostu-data/archives D:/discostu-data/pool

Metrics per file, used later to pick clean speech:
  snr   loud frames (p95) over quiet frames (p10), dB
  hf    9-12 kHz energy relative to 0.3-4 kHz in the loudest frames, dB
        (upsampled narrowband audio has next to nothing up there)
  clip  fraction of samples at full scale
"""

import io
import sys
import tarfile
from multiprocessing import Pool
from pathlib import Path

import numpy as np
import soundfile as sf
from scipy.signal import resample_poly

SR = 48_000


def metrics(x: np.ndarray) -> tuple[float, float, float]:
    n = 480
    f = x[: len(x) // n * n].reshape(-1, n)
    db = 10 * np.log10(np.mean(f**2, axis=1) + 1e-12)
    snr = float(np.percentile(db, 95) - np.percentile(db, 10))
    loud = f[db >= np.percentile(db, 80)]
    if len(loud) == 0:
        return snr, -99.0, 0.0
    spec = np.abs(np.fft.rfft(loud * np.hanning(n), axis=1)) ** 2
    hz = np.fft.rfftfreq(n, 1 / SR)
    hi = spec[:, (hz >= 9000) & (hz < 12000)].sum()
    lo = spec[:, (hz >= 300) & (hz < 4000)].sum()
    hf = float(10 * np.log10(hi / (lo + 1e-12) + 1e-12))
    clip = float(np.mean(np.abs(x) > 0.999))
    return snr, hf, clip


def load(data: bytes) -> np.ndarray | None:
    try:
        x, sr = sf.read(io.BytesIO(data), dtype="float32", always_2d=True)
    except Exception:
        return None
    x = x[:, 0]
    if sr == 44_100:
        x = resample_poly(x, 160, 147).astype(np.float32)
    elif sr != SR:
        return None
    return x


def run(job):
    archive, out = job
    name = archive.name.split(".")[2] if archive.name.count(".") > 3 else archive.stem
    pcm_path = out / f"{name}.pcm"
    if (out / f"{name}.tsv").exists():
        return f"{name}: already done"
    offset, kept, seen = 0, 0, 0
    with open(pcm_path, "wb") as pcm, open(out / f"{name}.tsv.part", "w", encoding="utf-8") as idx:
        with tarfile.open(archive, "r|bz2") as tar:
            for m in tar:
                if not m.isfile() or not m.name.lower().endswith(".wav"):
                    continue
                seen += 1
                x = load(tar.extractfile(m).read())
                if x is None or len(x) < SR:
                    continue
                peak = np.max(np.abs(x))
                if peak < 1e-4:
                    continue
                snr, hf, clip = metrics(x)
                x = x * (0.95 / peak)
                pcm.write((x * 32767).astype(np.int16).tobytes())
                idx.write(f"{offset}\t{len(x)}\t{snr:.1f}\t{hf:.1f}\t{clip:.5f}\t{m.name}\n")
                offset += len(x)
                kept += 1
                if kept % 2000 == 0:
                    print(f"{name}: {kept} files, {offset / SR / 3600:.1f} h", flush=True)
    (out / f"{name}.tsv.part").rename(out / f"{name}.tsv")
    return f"{name}: {kept}/{seen} files, {offset / SR / 3600:.1f} h"


if __name__ == "__main__":
    src, out = Path(sys.argv[1]), Path(sys.argv[2])
    out.mkdir(parents=True, exist_ok=True)
    only = sys.argv[3:]
    jobs = [(a, out) for a in sorted(src.glob("*.tar.bz2"))
            if "impulse" not in a.name and (not only or any(o in a.name for o in only))]
    with Pool(len(jobs)) as p:
        for r in p.imap_unordered(run, jobs):
            print(r, flush=True)
