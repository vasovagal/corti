# ADR 0007 — Streaming AEC-first pipeline: defer durability, calibration, and compression

- **Status:** Proposed (2026-06-18)
- **Supersedes (in part):** ADR 0005 (AEC stays offline/2-pass) — see Decision 1.

## Context

ADR 0005 chose an **offline, 2-pass** AEC fed from a streaming 16-bit/FLAC/Opus disk writer, surrounded by
durability (crash-recoverable `queue.db`), calibration (`--calibrate-aec`, `design/04a`), and a compression
plan (Opus, GH #63/#64). A subsequent review found a load-bearing flaw in the compression-first direction:
**compressing the tracks before AEC imposes a hard ERLE ceiling** — lossy codec noise on the reference and on
the echo-in-the-mic is *decorrelated*, so a linear adaptive filter cannot cancel below the codec's waveform
distortion floor (perceptual transparency ≠ waveform fidelity; see the FLAC-vs-Opus analysis).

Running AEC **in-flight on the lossless in-memory PCM** sidesteps that entirely: the filter sees bit-exact
audio and only the *clean output* is ever a candidate for compression. Capture already streams through a
lock-free SPSC ring → writer thread (ADR 0005 landed that) and is **3-channel** (mic 1ch + stereo tap 2ch),
currently downmixed to a 2-track file at write time.

We are deliberately narrowing scope to **get the streaming AEC right first**. Everything not on that critical
path is deferred.

## Decision

1. **AEC moves in-flight (streaming), replacing the offline 2-pass.** Block-adaptive, single forward pass with
   a **tunable lookahead window** that (a) lets the adaptive filter converge before the opening is emitted and
   (b) gives a stable mic↔far-end **delay/sync** estimate over a real span of audio. corti records-then-
   transcribes, so this lookahead latency is free. Default the window **conservatively large** — a more
   accurate cross-channel sync estimate is worth the RAM and the deferred opening. This supersedes ADR 0005's
   "keep AEC offline, 2-pass."

2. **Mono echo reference.** We are 3-channel at capture but do **not** take on stereo/multichannel AEC (and its
   non-uniqueness problem). The stereo tap is downmixed to a **mono** echo reference. `me = AEC(mic, mono_ref)`;
   `them = mono downmix of the tap` (no AEC needed — the tap is the clean digital far-end). True stereo
   far-end is a possible later extension, not now.

3. **Defer durability — it is OK to lose an in-flight recording for now.** Drop the durable crash-recoverable
   queue / `JobStatus` recovery / retention sweeps from the critical path. The pipeline runs in-process; a
   crash or quit mid-call loses **that** call. This trades guardrail 7 (crash-recoverable) for simplicity while
   the core loop is proven. Revisit before any real release. (Re-opens: #61 jobs, #65 retention, #66 queue UI,
   #71 size-cap, #60 timestamps — all parked until durability returns.)

4. **Drop AEC calibration (`--calibrate-aec`, `design/04a`).** The 144-combo sweep reads **persisted raw
   2-track recordings**, which the no-durability / no-raw-retention stance removes. Defaults ship as-is.
   Revisit when durability + raw retention come back.

5. **Skip compression (Opus #63/#64, FLAC) for now.** Get AEC right first; the clean output is the only thing
   that must land, and an **uncompressed transient intermediate is acceptable** while there is no retention to
   bound. Revisit compression — on the *clean output only*, never on the AEC input — once AEC is proven. The
   ERLE-ceiling reason for "never compress the AEC input" stands regardless.

6. **Ring + lookahead are tunable, defaulted bigger/safer** (see Consequences).

## Consequences

- **Resolves the ADR-0005 / #63–#64 contradiction**: no lossy audio ever feeds AEC, because AEC runs before
  any encode, on in-memory PCM.
- **Naturally fixes the AEC RAM crashes** (#67 `estimate_delay` O(n) FFT; #68 O(n) `d/x/out` buffers) and the
  whole-call-in-RAM OOM (#32): a block-streaming filter is bounded by `O(block + filter state + lookahead)`,
  independent of call length.
- **Landing state vs. end state.** The first PR runs `StreamingAec` (streaming, env-tunable lookahead via
  `CORTI_AEC_LOOKAHEAD_SECS`) **post-capture** over the recorded lossless 2-track — the canceller and its
  bound are real, but the samples are still read from a WAV the capture writer wrote. Relocating the
  `push()`-per-block call **into the capture writer thread** (so the raw 2-track never lands on disk and RAM
  is bounded end-to-end) is the tracked follow-up that actually closes #67/#68/#32; until then those stay
  open and the on-disk raw still exists.
- **Lost (intentionally, revisit-able):** re-tunability/calibration; crash recovery; the README "raw tracks are
  always preserved" invariant (update that copy); the retention/queue UI work.
- **Opening-seconds quality** now depends on the lookahead length, not a 2-pass re-filter — hence the
  conservative default and the tunability.
- **Disk:** with compression deferred, the clean intermediate is uncompressed. It is bounded only by being
  **transient** (deleted after transcription, since there is no retention). Long calls still spill a sizable
  transient file — acceptable short-term, and the concrete reason to revisit Opus on the clean output later.
- **Parked issues to re-open when durable:** #60, #61, #63, #64, #65, #66, #71, and `design/04a` calibration.
