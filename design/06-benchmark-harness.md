# 06 — Audio-quality benchmark & optimization harness

**Status:** built 2026-06-21. Lives in `crates/corti-bench` + `bench/`. Tunes corti along four
dimensions against a free, reproducible, speaker-labelled corpus (NPR **Planet Money**), and is
**rebootable**: when a new ASR/AEC model ships, re-run the same sweep to re-pick defaults.

## Why

We had been tuning transcription/diarization/AEC/memory by hand against a distracted human in the loop.
That doesn't converge. Planet Money gives us *known content* (public transcripts, named speakers) we can
play through the real macOS audio stack and score automatically, so an agent can sweep parameters, rank
them, and promote the winners to shipped defaults — no human judgement per iteration.

The four dimensions:
1. **Transcription accuracy** — WER vs the episode transcript.
2. **Diarization accuracy** — speaker-attributed cpWER on the opt-in far-end-diarization path.
3. **Memory** — peak RSS of a transcription job.
4. **Echo cancellation** — dB-ERLE + near-end preservation.

## Architecture — two tiers, one Mac, serial execution

It is **one** Mac: one mic/speaker, one CPU, tight disk, and peak-RSS only means anything in isolation.
So **every experiment runs serially through an in-process `flock`** (`corti-bench` acquires it itself — no
"forgot to wrap it" footgun) plus a hard refuse-to-run if the Corti tray is up (it fights for the mic and
shares on-disk writers). The agent "fleet" parallelizes *search, scoring, and code changes* — never the runs.

- **File tier (no hardware — the search engine).** Pristine NPR clips + a synthetic double-talk fixture,
  fully ground-truthed. Transcribing a WAV needs no macOS permission, so this tier is never blocked on
  capture setup. The bulk of the sweep happens here.
- **Acoustic tier (hardware-locked — validation).** Play the podcast through the speakers, `corti-bench`
  captures the real mic + system-tap, and we validate the file-tier winners under real ADC + room echo.
  Needs a one-time audio-capture (TCC) grant via a signed `.app` (see `bench/bundle-capture.sh`).

### Capture topology (one capture → many signals)

corti builds **one** CoreAudio aggregate device, so ch0 (mic, "me") and ch1 (system-tap, "them") are
sample-aligned by construction and written as a 2-track 32-bit-float WAV.

```
            ┌─ ch1 (tap)  = pristine digital podcast        ── transcribe → WER (pristine)
 podcast ──▶│
 (speakers) └─ ch0 (mic)  = same podcast, acoustically      ── transcribe → WER (acoustic, robustness)
                            re-recorded through the room     ── AEC(ch0 vs ch1) → dB-ERLE
```

Because **both channels carry the same source**, AEC nulls the podcast out of the mic — so transcription
scores the *raw* channels and AEC scores the *pair*. Headline WER/diarization come from the **file tier**
(clean, reproducible); the acoustic mic transcription is a **secondary robustness signal** (it is far-end
leakage, not an independent near-end), and **near-end preservation** comes from the **synthetic** fixture
(which has a clean ground-truth near-end). This is why we don't ask the user to talk.

## Components

| Path | What |
|---|---|
| `crates/corti-bench` | the harness CLI: `process` (AEC→transcribe→JSON+timing), `aec` (offline AEC→cleaned WAV + channel dumps), `capture` (serial, tray-guarded 2-track capture). In-binary flock. AWS is **not** linked. |
| `bench/scoring/` | `normalize.py` (frozen WER normalizer), `wer.py`, `cpwer.py` (`meeteval`, timestamp-free), `erle.py`, `align_reference.py` |
| `bench/fixtures/` | NPR manifest + parsed references (`<id>.reference.txt`, `<id>.turns.json`) + `fetch.sh`; `synthetic/` double-talk fixture + `build_synthetic.py`; `room_ir/` |
| `bench/acoustic/room_ir.py` | sine-sweep generator + deconvolver for the real room IR |
| `bench/sweep.py` | serial sweep runner: (config × fixture) → `/usr/bin/time -l` (peak RSS) → score → JSONL (resumable) |
| `bench/configs/` | sweep specs; `bench/results/` | committed results + Pareto summary |
| `bench/bundle-capture.sh` | wraps `corti-bench capture` in a signed `.app` for the TCC grant |

### Knob surface (all default to the shipping value → byte-identical until flipped)

| Knob | Where | Flag (`corti-bench`) | Env (app) | Default |
|---|---|---|---|---|
| VAD threshold | `LocalConfig.vad_threshold` → `engine::build_vad` | `--vad-threshold` | (add `CORTI_LOCAL_*`) | 0.5 |
| VAD min-silence | `LocalConfig.vad_min_silence` | `--vad-min-silence` | | 0.25 |
| ASR decoding / beam / blank | `LocalConfig.asr_*` → `build_recognizer` | `--asr-decoding/--asr-beam/--asr-blank-penalty` | | greedy / — / — |
| diarize far-end | `LocalConfig.diarize_far_end` | `--diarize` | `CORTI_LOCAL_DIARIZE` | false |
| diarize threshold / clusters / min-dur | `LocalConfig.diarize_*` → `build_diarizer` | `--diarize-threshold/--diarize-num-clusters/--diarize-min-on/--diarize-min-off` | `CORTI_LOCAL_DIARIZE_THRESHOLD` | 0.5 / −1 / 0.3 / 0.5 |
| embedding model | `LocalConfig.embedding_model` | `--embedding` | `CORTI_LOCAL_EMBEDDING` | titanet |
| threads | `LocalConfig.num_threads` | `--threads` | `CORTI_LOCAL_THREADS` | 4 |
| AEC filter_len / mu / eps / power-smoothing / DTR / suppress / max-lag | `AecConfig.*` | `--filter-len … --max-lag-ms` | `CORTI_AEC_*` | 4096 / 0.3 / 1e-6 / 0.9 / 2.0 / 2.5 / 10ms |

## Ground truth (the three NPR problems we solved)

NPR transcripts are speaker-labelled but have **no timestamps**, and the mp3s carry **dynamically-inserted
ads** that aren't in the transcript. So:

1. **Parsing** (`bench/fixtures/parse_transcript.py`): isolate `<div class="transcript storytext">`, strip
   sponsor/copyright/disclaimer + bracketed cues, split on inline ALL-CAPS speaker labels, merge
   full-name→last-name aliases (stop-list for the sentence-final-CAPS-glue trap). → `reference.txt` + `turns.json`.
2. **Untimed → can't slice a clip's reference by time.** `align_reference.py` transcribes a clip once with a
   baseline config, anchors on the largest matching block against the full reference word-sequence, and emits
   the contiguous reference span (reports `coverage` = matched/hyp; >0.9 = clean window, <0.85 = ads/montage).
3. **Window choice matters.** Intro/montage/ad windows score badly (e.g. 31% WER on a names-montage open);
   curated clean-dialogue windows score ~6–14%. Sweep clips are chosen for high coverage.

Fetch with `curl -A 'Mozilla/5.0'` (WebFetch times out on npr.org). Raw mp3/html/wav are git-ignored
(copyright + disk); references + scripts are committed so anyone can re-derive via `bench/fixtures/fetch.sh`.

## Synthetic double-talk AEC fixture

`bench/fixtures/synthetic/build_synthetic.py` (AEC-Challenge recipe): near-end = macOS `say` of a known
script (`near.reference.txt` is the exact ground-truth text); far-end = an uncorrelated NPR segment;
`mic = near + (tanh(drive·far) ⊛ room_ir)·atten + noise`. The loudspeaker `tanh` nonlinearity stops us
over-fitting `suppress_residual` to a linear echo path. `--ir <wav>` swaps in the **real captured room IR**
(`bench/acoustic/room_ir.py`) so the fixture matches the actual room — which made it far harder and more
honest (default ERLE dropped from 20.8 dB synthetic-IR to ~5 dB real-IR). Outputs `doubletalk.wav`
(near+echo, ground-truth near) and `echoonly.wav` (pure ERLE target).

## Metrics

| Dimension | Metric | How |
|---|---|---|
| Transcription | WER normalized + raw (`jiwer`) | `wer.py` on the aligned clean clip |
| Diarization | cpWER + speaker-count error (`meeteval`, timestamp-free) | `cpwer.py` on the `--diarize` far-end path |
| Echo | dB-ERLE (echo-only) + near-end preservation (seg-SNR/corr) | `erle.py` on `aec --dump-channels` output |
| Memory | peak RSS | `/usr/bin/time -l` on the release binary |

Combined into a Pareto front (accuracy vs peak-RSS) → one recommended default, not four independent optima.

## How to run it (the reboot recipe)

```sh
cd <worktree>; export PATH=/opt/homebrew/bin:$PATH
# 0. models once: crates/corti-transcribe-local/fetch-models.sh
# 1. build:   cargo build -p corti-bench --release   # binary: target/aarch64-apple-darwin/release/corti-bench
# 2. fixtures: bench/fixtures/fetch.sh                # NPR mp3 + parsed references + clean sweep clips
#    synthetic: bench/fixtures/synthetic/build_synthetic.py [--ir bench/fixtures/synthetic/room_ir/real_ir.wav]
# 3. file-tier sweeps (serial, autonomous):
bench/.venv/bin/python bench/sweep.py --spec bench/configs/asr_round1.json --out bench/results/asr_round1.jsonl
bench/.venv/bin/python bench/sweep.py --spec bench/configs/aec_round1.json --out bench/results/aec_round1.jsonl
# 4. acoustic tier (hardware; quit the Corti tray first):
bench/bundle-capture.sh 6 /tmp/cap.wav            # one-time TCC grant, then re-run
#    room IR:  room_ir.py gen → afplay sweep during `open …app… capture` → room_ir.py deconv
#    acoustic ERLE/robustness: play a clip during capture, then `corti-bench aec`/`process` the result
# 5. analyze bench/results/*.jsonl → Pareto → flip defaults (LocalConfig/AecConfig Default impls).
```

A single `corti-bench process --input clip.wav` with no flags reproduces today's pipeline byte-for-byte;
that is the regression anchor.

## Re-tuning when a new model ships

The harness is model-agnostic; only the ASR/diarization model files change. To reboot: drop the new model
into the model dir, re-run the sweeps above, re-derive the clean-clip references (a new baseline transcription
→ `align_reference.py`), rebuild the synthetic fixture with the current room IR, and re-pick defaults from the
fresh `bench/results/`. The fixtures, scorers, and knob surface are unchanged.

## Results

This run's numbers, winning config, and the Pareto front: `bench/results/RESULTS.md`.
