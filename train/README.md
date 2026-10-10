# Noise suppression training

Trains the network behind Settings → Noise suppression
(`app/src/audio/dsp/denoise.rs`). The app adds no latency. Each 2.5 ms block
goes through these steps:

1. The last 1024 mic samples (21 ms, ending with the block) are windowed with
   a long rise and a one-block fall, and transformed.
2. The network gets 80 features: log energies of 32 ERB-spaced bands
   (DC–20 kHz) plus the 48 lowest FFT bins (47 Hz–2.3 kHz, so it can see pitch
   harmonics). It runs one step: dense 128 → GRU 128 → GRU 128 → 32 sigmoid
   gains.
3. The gains (scaled in dB by the Strength slider) become a 256-tap
   minimum-phase FIR via the real cepstrum.
4. The block is filtered with it, crossfading from the previous block's
   filter.

`dsp.py` is the reference for all of it. `denoise.rs` mirrors it, and a test
checks they agree to within 50 dB SNR.

## Data

Everything comes from the Microsoft DNS Challenge 4 full-band (48 kHz) set:
https://github.com/microsoft/DNS-Challenge, blobs under
`https://dns4public.blob.core.windows.net/dns4archive/datasets_fullband/`.

| Use | Blob |
|---|---|
| Speech | `clean_fullband/…vctk_wav48_silence_trimmed_000` (English, studio) |
| Speech | `clean_fullband/…read_speech_024_4.67_NA` (English audiobooks, top quality scores) |
| Speech | `clean_fullband/…german_speech_016_4.43_NA`, `…spanish_speech_001_4.09_NA` |
| Speech | `clean_fullband/…emotional_speech_000_NA_NA` (CREMA-D; only its cleanest files) |
| Noise | `noise_fullband/…freesound_000`, `…freesound_001`, `…audioset_000` |
| Reverb | `datasets_fullband.impulse_responses_000` (simulated small/medium rooms) |

Speech files also have to pass our own checks (`prepare.py` metrics, rules in
`train.py`'s `SPEECH`): loud-to-quiet frame ratio and real energy above 9 kHz,
so upsampled narrowband audio doesn't teach the network that treble is noise.

Every batch is mixed fresh on the GPU:
- **Levels:** speech from −45 to −10 dBFS, SNR from −5 to 30 dB.
- **Room reverb:** 30% of clips.
- **Mic coloration:** random EQ curves.
- **Synthetic noise:** coloured hiss and 50/60 Hz hum.
- **Band-limited clips:** 12%.
- **Edge cases:** 8% of clips are clean speech, which must pass untouched, and
  5% are noise only.

## Running

```
uv sync                                    # Python 3.12 + PyTorch (CUDA 12.8)
uv run python prepare.py D:/discostu-data/archives D:/discostu-data/pool
uv run python train.py --steps 60000 --out runs/main
uv run python enhance.py runs/main/best.pt noisy.wav cleaned.wav
uv run python export.py runs/main/best.pt  # -> app/src/audio/dsp/denoise/
cargo test -p discostu --release denoise
```

`train.py` expects the pools and the unpacked impulse responses under
`D:/discostu-data` (`ROOT`).
