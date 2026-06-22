# Synthetic double-talk AEC fixture

A ground-truthed acoustic-echo fixture for the corti bench AEC sweep. It lets the
sweep measure both **echo cancellation** (dB-ERLE) and **near-end preservation**
against a perfectly known clean near-end (the TTS script is the ground-truth
transcript).

Built after the Microsoft **AEC-Challenge** synthetic recipe: uncorrelated
near/far speech + a plausible room impulse response (IR) + a loudspeaker
nonlinearity.

**Everything is 48 kHz** (corti captures at 48 kHz; the AEC's `filter_len=4096`
≈ 85 ms is tuned for 48 kHz — do not rebuild at 16 kHz or the AEC knobs mistune).

## Regenerate

```sh
bench/.venv/bin/python bench/fixtures/synthetic/build_synthetic.py
# swap in a real captured room IR later:
bench/.venv/bin/python bench/fixtures/synthetic/build_synthetic.py --ir path/to/real_ir.wav
```

The generator is deterministic (`SEED=1234`). All WAVs are regenerable and
git-ignored; only the generator, the reference script, this README, and the IR
params are committed.

## Files

| File | Format | Role |
|------|--------|------|
| `near.wav` | 48k mono f32 | clean near-end (TTS, known text) |
| `far.wav` | 48k mono f32 | far-end reference (unrelated NPR segment) |
| `echo.wav` | 48k mono f32 | the echo component `conv(tanh(drive·far), ir)·atten` |
| `doubletalk.wav` | 48k **2-track** f32 | **ch0 = mic** (near+echo+noise), **ch1 = far** |
| `echoonly.wav` | 48k **2-track** f32 | **ch0 = mic_echo** (echo+small noise, near silent), **ch1 = far** |
| `room_ir/synthetic_ir.wav` | 48k mono f32 | the synthetic room IR (placeholder) |
| `near.reference.txt` | text | ground-truth near-end transcript |

**Channel order is corti's mic/tap order: ch0 = mic ("me"), ch1 = far ("them").**

## Recipe

1. **Near-end** — macOS `say -v Samantha` synthesizes the multi-sentence script
   in `near.reference.txt` (≈2.4 min, varied conversational text). Converted to
   48 kHz mono f32. This text is the ground-truth near-end transcript.
2. **Far-end** — a ~3-min segment of an unrelated NPR episode
   (`nx-s1-5856509.full.wav`, "trees"; `ffmpeg -ss 700 -t 180`), resampled to
   48 kHz mono f32. Content is different from the near-end (uncorrelated).
   Near/far are trimmed to the same (min) length.
3. **Room IR** — synthetic, plausible: unit **direct path** at a 4 ms bulk
   delay + a handful of decaying **early reflections** (random taps within
   ~30 ms) + an exponentially-decaying **late-reverb tail** (RT60 ≈ 200 ms),
   total length 250 ms, peak-normalized. **Placeholder — replace with a real
   captured room IR via `--ir`.**
4. **Double-talk mic** — loudspeaker nonlinearity then echo path:
   `echo = conv(tanh(drive·far), ir)·atten`; `mic = near + echo + noise`, with
   gaussian noise at 35 dB SNR vs near. `atten` is solved so the achieved ERL
   hits the target (a realistic speaker-bleed level).
5. **Echo-only mic** — near is silence: `mic_echo = echo + small_noise`
   (echo-only noise floor 45 dB below echo). The clean ERLE target: a perfect
   canceller drives ch0 → silence.

## Parameters

| Param | Value | Meaning |
|-------|-------|---------|
| sample rate | 48000 Hz | mono f32 components / 2-track mixes |
| duration | 142.8 s | min(near, far), 6 854 148 samples |
| seed | 1234 | deterministic build |
| far source | `nx-s1-5856509.full.wav` | `-ss 700 -t 180` |
| TTS voice | Samantha | macOS `say` |
| IR length | 250 ms | total |
| IR direct delay | 4 ms | bulk/direct path |
| IR early reflections | 8 taps within 30 ms | decaying |
| IR RT60 | 200 ms | late-reverb tail |
| drive (tanh) | 1.5 | loudspeaker nonlinearity |
| atten | 0.5370 | echo-path gain (solved for ERL) |
| target ERL | 3.0 dB | realistic speaker bleed |
| **achieved ERL** | **3.00 dB** | `10·log10(near_pow / echo_pow)` |
| mic noise SNR | 35 dB | gaussian, vs near (double-talk) |
| echo-only noise | 45 dB below echo | very low floor |
| channel order | ch0 = mic, ch1 = far | corti mic/tap |

## Verification (default AEC knobs)

`BIN=target/aarch64-apple-darwin/release/corti-bench`

**Echo-only ERLE** (pure cancellation, no near-end):
```sh
$BIN aec --input echoonly.wav --out /tmp/eo_clean.wav --dump-channels /tmp/eo
bench/.venv/bin/python bench/scoring/erle.py --mic /tmp/eo/mic.wav --far /tmp/eo/far.wav --out /tmp/eo/clean.wav
# -> erle_db ≈ 20.8 dB
```

**Double-talk** (ERLE + near-end preservation):
```sh
$BIN aec --input doubletalk.wav --out /tmp/dt_clean.wav --dump-channels /tmp/dt
bench/.venv/bin/python bench/scoring/erle.py --mic /tmp/dt/mic.wav --far /tmp/dt/far.wav --out /tmp/dt/clean.wav --near near.wav
# -> erle_db ≈ 12.7 dB, near_seg_snr_db ≈ 15.7 dB, near_correlation ≈ 0.974
```

**Near-end WER** (cleaned near survives AEC). `dt_clean.wav` is 2-track
(ch0 = cleaned near, ch1 = far); transcribe a mono WAV of just ch0 for a clean
near-WER:
```sh
# write mono ch0:
bench/.venv/bin/python -c "import soundfile as sf,numpy as np; d,sr=sf.read('/tmp/dt_clean.wav',always_2d=True); sf.write('/tmp/dt_clean_ch0.wav',d[:,0].astype(np.float32),sr,subtype='FLOAT')"
$BIN process --input /tmp/dt_clean_ch0.wav --out /tmp/dt_hyp.json
bench/.venv/bin/python bench/scoring/wer.py --ref near.reference.txt --hyp /tmp/dt_hyp.json
# -> wer_normalized ≈ 0.0125 (1.25%)
```

## Note on the IR

The synthetic IR is a **placeholder**. Capture a real room IR (e.g. exponential
sine sweep through your actual mic/speaker setup) and pass it via `--ir <wav>`
(48 kHz mono) to make the echo path acoustically realistic; the rest of the
recipe (nonlinearity, ERL targeting, noise, channel order) is unchanged.
