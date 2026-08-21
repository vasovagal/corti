# Benchmark results — 2026-06-21

Corpus: NPR Planet Money (dough `nx-s1-5844617`, trees `nx-s1-5856509`, hackers `nx-s1-5859441`).
Baseline backend: local Parakeet-TDT-0.6B-v3-int8 + Silero VAD + pyannote, CPU, Apple-Silicon. AWS out of scope. ADR 0011's later Metal comparison is recorded separately below.
Method: `design/06-benchmark-harness.md`. Raw data: `bench/results/*.jsonl`. Regenerate tables with
`bench/analyze.py {asr,aec,diar} <jsonl>`.

## Baseline (shipping defaults, normalized WER on clean 3-min clips)

| episode | WER norm | WER raw | peak RSS |
|---|---|---|---|
| dough | 0.085 | 0.172 | ~1150 MB |
| trees | 0.062 | 0.147 | ~990 MB |
| hackers | 0.141 | 0.289 | ~1160 MB |

Believable Parakeet numbers (hackers is harder — security jargon). Peak RSS dominated by model load, not
knob-sensitive on the non-diarize path.

## 0. Later runtime comparison — transcribe.cpp / Metal (2026-08-04, ADR 0011)

Same Parakeet-TDT-0.6B-v3 model and 300 s dough excerpt, shipping VAD settings, no diarization. The GGML arm
uses the official Q8_0 GGUF through transcribe.cpp upstream revision `553f1099…`; three alternating
post-warm release processes per engine, each including model load:

| engine | mean ASR wall | speedup | mean peak RSS | normalized WER |
|---|---:|---:|---:|---:|
| sherpa ONNX / CPU | 24.605 s | 1.00× | 1,543 MB | 0.304791 |
| transcribe.cpp GGML / Metal | **6.016 s** | **4.09×** | **1,246 MB (−19.25%)** | **0.304791** |

The stored 5-minute reference is imperfectly aligned, so use the equal WER as a relative parity result, not
an absolute accuracy claim. Hypotheses differ by 14/887 normalized words (1.58%). The first-ever Metal
shader build took 10.3 s; the cached library loaded in 11 ms on the next process. Raw rows:
`transcribe_cpp_round1.jsonl`. Outcome: ship a selectable accelerated engine, retain sherpa as the
upgrade-safe default until a real live-checkpoint call soak (ADR 0011).

## 0b. Whole-corpus engine comparison (2026-08-21, issue #118 stage 2)

Round 0 above is one 300 s excerpt. This round re-runs the same two engines over **all three episodes at
full length** — 5,568.7 s (92.8 min) of audio — plus the 5-minute excerpts, two alternating post-warm
release processes each. Spec `bench/configs/engine_round2.json`, raw rows `engine_round2.jsonl`.

Full-episode references are the untrimmed NPR transcripts while the mp3s carry inserted ads, so the ~0.18–0.21
absolute WER is inflated by that mismatch. Both engines eat the identical penalty, so the **delta** is the
result; the level is not a product-quality claim.

| fixture | audio | sherpa WER | ggml WER | ΔWER | sherpa ASR | ggml ASR | speedup | sherpa RSS | ggml RSS | ΔRSS |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| dough (full) | 1851 s | 0.1926 | **0.1796** | −0.0130 | 149.8 s | **39.0 s** | 3.84× | 1473 MB | **1291 MB** | −12.4% |
| trees (full) | 1722 s | 0.1810 | **0.1719** | −0.0091 | 145.7 s | **35.1 s** | 4.15× | 1334 MB | **1162 MB** | −12.9% |
| hackers (full) | 1995 s | 0.2149 | **0.2004** | −0.0146 | 165.8 s | **42.3 s** | 3.92× | 1336 MB | **1261 MB** | −5.6% |
| dough (5 min) | 300 s | 0.2926 | **0.2875** | −0.0051 | 25.5 s | **7.0 s** | 3.62× | 1302 MB | **1105 MB** | −15.1% |
| trees (5 min) | 300 s | n/a | n/a | n/a | 25.9 s | **6.5 s** | 4.02× | 1171 MB | **1153 MB** | −1.5% |
| hackers (5 min) | 300 s | n/a | n/a | n/a | 26.5 s | **6.8 s** | 3.89× | 1260 MB | **1157 MB** | −8.2% |

`trees`/`hackers` at 5 min score `n/a`: their `.5min.reference.txt` are placeholder stubs (the untimed NPR
transcript cannot be sliced to a [120 s, 420 s) window), so a WER against them is meaningless. Their timing
and RSS are still valid. Only `dough` has a real aligned 5-minute reference.

Corpus totals over the three full episodes: mean WER **0.1840 ggml vs 0.1961 sherpa (−1.22 pp)**, total ASR
**116.4 s vs 461.3 s (3.96×**, 47.8× vs 12.1× realtime), mean peak RSS **1238 MB vs 1381 MB (−10.3%)**.

GGML is not merely at parity — it is slightly *more* accurate on every episode. Scoring the two hypotheses
against **each other** (frozen normalizer, sherpa as reference) puts the disagreement at **3.63%** of words
(657 edits / 18,114 words) across all six fixtures — a sharper parity signal than either engine's WER against
an ad-contaminated reference.

Corpus caveat: NPR re-encoded all three mp3s between the round-0 run and this one (new SHA-256s, durations
shorter by 3–29 s — dynamic ad insertion). `manifest.json` was regenerated to match the audio actually
measured here. Round 0's numbers are therefore not directly comparable to this round's, though both engines
within this round saw byte-identical input.

**This did not unlock a default flip.** See ADR 0011's addendum: the remaining gate is a real detector-call
live soak, which this file-tier batch benchmark cannot exercise.

## 1. Transcription — `vad_min_silence` is the win

Round 1 (12 configs × 3 clips, mean ΔWER vs baseline):

| config | WER norm | ΔWER |
|---|---|---|
| **vad_min_silence=0.5** | **0.073** | **−0.023** |
| vad_threshold=0.3 | 0.086 | −0.010 |
| blank_penalty=1.0 | 0.092 | −0.005 |
| baseline (sil=0.25) | 0.096 | — |
| beam4 | 0.120 | +0.024 (worse) |
| vad_min_silence=0.1 | 0.127 | +0.031 (worse) |

`vad_min_silence` 0.25 → 0.5 cuts WER ~2.3 points: a longer trailing-silence stops the VAD from splitting
within-utterance pauses, which were causing word-boundary errors at chunk seams. **Beam search is *worse*
than greedy** for Parakeet-TDT (greedy stays the default). `blank_penalty=-1` errors (sherpa rejects it).
`threads` only moves speed/memory (threads=8 ≈ 10 s vs threads=2 ≈ 25 s, identical WER).

Rounds 2–3 swept the min-silence axis (mean WER over the 3 clips), and it improves **monotonically** then
plateaus — the mechanism is "give Parakeet longer chunks, up to the 20 s cap":

| vad_min_silence | 0.25 (base) | 0.5 | 0.75 | 1.0 | 1.25 | 1.5 | 2.0 |
|---|---|---|---|---|---|---|---|
| WER norm | 0.096 | 0.073 | 0.067 | 0.062 | **0.060** | 0.060 | 0.062 |

Optimum ≈ 1.25–1.5 (**−38 % relative** vs baseline), flat there, slightly regressing by 2.0. The best
multi-knob combo (`vad_min_silence=0.5 + vad_threshold=0.3 + blank_penalty=1`) reached 0.061 — no better than
single-knob `min_silence=1.25`, and not worth 3 coupled knobs. **Chosen default: `vad_min_silence = 1.0`** —
a clean, robust value capturing essentially all the gain (0.062) without over-fitting to three clips.
`vad_threshold=0.3` is a smaller independent win (−0.010) left as an optional follow-up.

## 2. Echo cancellation — `filter_len` is the win (not `max_lag_ms`)

AEC sweep on the synthetic double-talk fixture rebuilt with the **real captured room IR** (24 ms delay +
~400 ms reverb — which dropped the default ERLE from 20.8 dB synthetic-IR to 5.0 dB, i.e. the real room is
much harder):

| config | echo-only ERLE | double-talk ERLE |
|---|---|---|
| filter_len=16384 | 14.4 dB | 5.9 dB |
| filter_len=8192 | 8.6 dB | **6.2 dB** |
| baseline (4096) | 5.0 dB | 4.6 dB |
| no-suppress | 2.6 dB | 2.1 dB (worst) |

`max_lag_ms`, `mu`, `power_smoothing`, `double_talk_ratio` gave **nothing** — the filter *length* is what
covers the room's long impulse response. **Validated on the real acoustic capture** (60 s podcast echo,
the gold standard):

| filter_len | real ERLE |
|---|---|
| 4096 | 10.27 dB |
| 8192 | 10.61 dB |
| 12288 | 11.31 dB |
| 16384 | 10.36 dB (overfit) |

Honest caveat: the synthetic gains (5→14 dB) **don't fully transfer** — the real path has loudspeaker
nonlinearity / time-variance a linear filter can't model, so real improvement is ~+1 dB peaking at 12288.
Recommendation: **filter_len 4096 → 8192** (best synthetic double-talk balance + a real-world gain at
trivial cost; the residual suppressor must stay on).

## 3. Diarization (opt-in far-end path) — issue #18 quantified

Full dough episode (7 real speakers), `diarize_far_end=true`, cpWER + speaker-count error vs `turns.json`:

| config | hyp speakers (ref 7) | speaker-count error | cpWER | peak RSS |
|---|---|---|---|---|
| diarize_threshold=0.3 | 104 | +97 | uncomputable | 1723 MB |
| **base (threshold 0.5)** | **57** | **+50** | uncomputable | 1382 MB |
| diarize_threshold=0.7 | 29 | +22 | uncomputable | 1461 MB |
| **num_clusters=7 (known count)** | **7** | **0** | **0.443** | 1484 MB |

The default auto-clustering **catastrophically over-clusters** (8–15× too many speakers — `meeteval` even
refuses to score it). Raising the threshold helps but doesn't fix it (0.7 → still 29). **Only pinning the
known speaker count works** (cpWER 0.44 — high because podcast far-end audio, with music/ads/inserted clips,
is a *harder* diarization target than a real call's far-end). Outcome: **don't flip a threshold default** (no
value is good enough), keep diarization opt-in, escalate #18 with these numbers, and the newly-exposed
`diarize_num_clusters` lets users who know their participant count get correct diarization. Untested lever
left as a follow-up: the WeSpeaker/CampPlus embedding models (not fetched here) — also a memory win
(≈ +27 MB vs TitaNet's +100 MB).

## 4. Memory

Peak RSS ≈ 1.0–1.2 GB on the non-diarize path, set by the one-shot Parakeet model load, not the tuning
knobs. Levers: `threads` (minor) and `diarize_far_end` (major — loads pyannote-seg + a speaker-embedding
model; TitaNet ≈ +100 MB, WeSpeaker ≈ +27 MB). The recommended transcription winner (`vad_min_silence`)
costs no extra memory.

## Default changes (applied on `feat/bench-harness`, all tests green)

| knob | old | new | dimension | evidence |
|---|---|---|---|---|
| `LocalConfig.vad_min_silence` | 0.25 | **1.0** | transcription | −38 % WER (0.096 → 0.062) across 3 episodes |
| `AecConfig.filter_len` | 4096 | **8192** | echo | 2× synthetic ERLE, +0.3 dB real, best double-talk |
| diarization | — | _no change_ | diarization | auto-clustering needs work (#18); kept opt-in, count now pinnable |

**End-to-end verification:** a no-flag `corti-bench process` (i.e. the shipped pipeline with the new defaults)
now scores dough 0.065 / trees 0.053 / hackers 0.068 (was 0.085 / 0.062 / 0.141) — the −38 % win is baked
into the default, and `cargo test -p corti-aec` (12 dB ERLE gate + streaming↔offline parity) + `-p
corti-transcribe-local` stay green with the flipped defaults.

## Acoustic tier (real hardware)

Real 2-track capture verified (both channels live); room IR captured (24 ms delay incl. pipeline latency,
~400 ms reverb); real-capture ERLE 10.3 dB at the old default, 11.3 dB at filter_len 12288. Acoustic mic
transcription works (~12 % WER on a 60 s window) — a secondary robustness signal, not the headline (the mic
carries far-end leakage, not an independent near-end).

Each flip lands as its own commit + labelled GitHub issue; AEC parity tests (12 dB ERLE gate,
streaming↔offline) must stay green before flipping `filter_len`.
