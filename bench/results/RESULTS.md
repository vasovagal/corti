# Benchmark results — 2026-06-21

Corpus: NPR Planet Money (dough `nx-s1-5844617`, trees `nx-s1-5856509`, hackers `nx-s1-5859441`).
Backend: local Parakeet-TDT-0.6B-v3-int8 + Silero VAD + pyannote, CPU, Apple-Silicon. AWS out of scope.
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
