# ADR 0009 — Chunked/live transcription: a pull-based sync core, batch refactored onto it

- **Status:** Accepted (2026-07-07; this branch)
- **References:** ADR 0003 (local ASR / sherpa-onnx), ADR 0007 (streaming AEC; #74 — bounded
  capture tee + `StreamingAec.push()` live consumer), ADR 0008 (superseded by ADR 0013's bounded UI);
  guardrail 6 (pluggable `Transcriber`), guardrail 9 (HAL callbacks hand work off via a channel, never block).
- **Amended by:** ADR 0012 — reusable `checkpoint()` boundaries force the current VAD tail final, reset
  bounded state, and preserve call-relative timestamps; ADR 0013 observes those same closed regions in the UI.

## Context

The local backend was structurally batch-only: `Transcriber::transcribe(&Path)` read a finished WAV, VAD-
chunked each channel, decoded every region, and materialized words only at the end
(`crates/corti-transcribe-local/src/engine.rs`). Feeding audio incrementally — for a live transcript
(ADR 0008), or to overlap decode with capture — had no entry point, and the capture writer
(`corti-coreaudio` `run_writer`) was the only place downmixed PCM existed in memory yet it only ever reached
`hound`. We need a chunked transcription surface without adding a runtime to the engine (guardrail 9 keeps the
audio path sync) and without regressing the batch path.

## Decision

1. **A pull-based, synchronous core: `LiveTranscriber`.** `push(&[f32], sample_rate)` resamples to 16 kHz
   (continuously across pushes), feeds one stateful Silero VAD in 512-sample windows, and **decodes each
   completed region inside that push** with the resident `OfflineRecognizer`; `poll_words()` non-blockingly
   drains queued words; `finish()` flushes VAD + resampler and returns the tail. No async, no callback, no
   channel in the core — the caller drives the cadence. Timestamps stay call-relative because one VAD sees the
   whole contiguous 16 kHz stream and `SpeechSegment::start()` is already the absolute sample index.

2. **Batch is refactored onto the live core.** `engine::transcribe_channel` now builds a `LiveTranscriber`,
   pushes the whole channel once, and calls `finish()` — the same window sequence the old `.chunks(512)` loop
   produced, so batch is byte-identical (verified: chunked-vs-whole equivalence on real speech). One decode
   path, not two. The recognizer is shared across both channels via `Arc`.

3. **The tee is lossy-bounded by design.** `run_writer` gains an optional `CaptureTee`: it `try_send`s
   ~4096-frame downmixed `(mic, tap)` chunks and **never blocks** — on a full/hung-up channel the chunk is
   dropped and counted (`RecordingHandle::tee_dropped_chunks`). Live transcription is throwaway (ADR 0008);
   the on-disk WAV is the source of truth and is untouched, so dropping a live chunk is acceptable and a
   blocking tee (which could stall the writer and corrupt the recording) is not. With no tee attached the
   writer path is byte-identical.

4. **The async `Stream` adapter is feature-gated (`stream`).** A `futures_core::Stream<Item = Vec<Word>>`
   wrapper runs the sync `LiveTranscriber` on a dedicated std thread and bridges it with a tokio mpsc. No
   tokio/futures in the default build; the engine stays runtime-free (guardrail 9). Sync core, async only at
   the edge.

## Consequences

- **One engine path.** Batch and live share `LiveTranscriber`; a decode bug or VAD tweak is fixed once. The
  batch refactor changes no observable behavior or tests.
- **#74 and the app consumers shipped later.** `corti-tap --live` proved tee → `StreamingAec` →
  `LiveTranscriber`; #87/#103 added crash-safe app filing, and ADR 0013 added its bounded timestamped observer.
- **Closed regions, not unstable partials.** A live word appears once its VAD region closes. ADR 0012 made
  zero-drop committed live windows canonical (batch remains fallback); ADR 0013 deliberately reuses those
  words instead of adding ADR 0008's proposed trailing pseudo-partial re-decode.
- **Workspace deps.** `futures-core` is new, pulled only by `corti-transcribe-local`'s `stream` feature.
  `tokio` was already a workspace dep (`corti-tap`'s default `inbox`/AWS); the `stream` feature reuses it. Both
  are pinned in the root `Cargo.toml`. `corti-tap --live` pulls neither — it wires the sync core over std
  channels (it enables `corti-transcribe-local`/`corti-aec` without their async features).
