# Chunked / live transcription

_Verified against main + crash-safe rolling live commits (#87, #103)._

The local backend can transcribe audio **as it arrives**, in arbitrary-sized chunks, over the same resident
Parakeet engine the batch path uses — no second model, no async runtime in the engine. This page is the API
reference and the design stance. See [ADR 0009](../design/adr/0009-chunked-transcription-api.md) for the decision, and
`design/02-corti-transcribe.md` / [ADR 0003](../design/adr/0003-local-asr-sherpa-onnx.md) for the batch pipeline.

## The pull model — `LiveTranscriber`

`corti_transcribe_local::LiveTranscriber` (`crates/corti-transcribe-local/src/live.rs:69`) is a **synchronous,
pull-based** transcriber for one mono channel. The caller drives it:

```rust
// One recognizer, shared; one VAD per channel.
let engine = LocalTranscriber::new(cfg).live_engine()?;   // loads Parakeet + Silero once
let mut live = engine.channel()?;                         // a fresh VAD sharing the recognizer

live.push(&samples, 48_000);          // resample→VAD→decode closed regions→queue words
while let Some(words) = live.poll_words() {  /* words as they fall out */ }
let committed = live.checkpoint();    // force the open tail final; keep engine usable + absolute time
live.push(&more_samples, 48_000);     // next bounded durability epoch
let tail = live.finish();             // final flush; idempotently ends this channel
```

| method | signature | behavior |
|---|---|---|
| `push` | `push(&mut self, &[f32], sample_rate: u32)` (`live.rs:103`) | Resamples to 16 kHz (continuously across pushes; no-op at 16 kHz), feeds one Silero VAD in 512-sample windows. **Decode happens here** — every VAD region that *closes* during this push is decoded on the spot and its words queued. Cheap while a region is still open. No-op after `finish`. |
| `poll_words` | `poll_words(&mut self) -> Option<Vec<Word>>` | Non-blocking drain of queued words. `None` when empty. |
| `checkpoint` | `checkpoint(&mut self) -> Vec<Word>` | Flush the current resampler/VAD tail, return every un-polled word, reset the bounded VAD epoch, and remain usable. A cumulative sample base keeps timestamps call-relative across checkpoints. |
| `finish` | `finish(&mut self) -> Vec<Word>` | Flush the final trailing region, return all remaining words, and end this channel. Idempotent. |

Two knobs matter for the pull model:

- **Decode cost lands inside `push`.** A `push` that closes a long (up to the 20 s VAD cap) region pays that
  region's decode synchronously. There is no background thread in the core — if you need decode off your
  thread, use the `stream` adapter below.
- **Timestamps stay call-relative across checkpoints.** Within one epoch, `SpeechSegment::start()` is the
  VAD-relative sample index. `checkpoint()` advances a cumulative 16 kHz sample base before resetting the VAD;
  `drain_regions` adds that base, so every word remains seconds-from-call-start. Ordinary push boundaries are
  absorbed by a `WindowBuffer` carrying the sub-512 remainder.

`LiveEngine` is the resident engine: `LocalTranscriber::live_engine()` loads the recognizer + models once;
`LiveEngine::channel()` spawns a `LiveTranscriber` per channel — each with its own stateful VAD, all sharing the
one thread-safe recognizer via `Arc`. When `LocalConfig::diarize_far_end` is on, it also owns one reusable
speaker diarizer; `diarize_chunk` accepts one bounded source-rate window and returns call-relative turns.

## Batch runs on the live core

`engine::transcribe_channel` (`crates/corti-transcribe-local/src/engine.rs:185`) is now a thin wrapper: build a
`LiveTranscriber`, push the whole channel once, `finish()`. Feeding an entire channel in a single push produces
the **same VAD window sequence** the old `.chunks(512)` loop did (full windows, then the final partial, then
flush), so the batch transcript is byte-identical. Equivalence is checked directly — the gated
`live_equals_batch_over_chunking` test (`live.rs`, `#[ignore]`) asserts that pushing a real recording in
irregular boundary-straddling chunks yields exactly the same words as one whole-channel push. One decode path,
not two.

## The capture tee — bounded, lossy, counted

To transcribe **during** capture you need the downmixed PCM the writer thread already has. `run_writer`
(`crates/corti-coreaudio/src/capture.rs:634`) gains an optional `CaptureTee` (`capture.rs:88`):

- Chunks are `CaptureChunk { mic, tap }` (`capture.rs:76`) — mono mic ("me") + mono tap ("them"), same frame
  count, at the capture rate; `mic` is empty for a tap-only capture. ~`TEE_FRAMES_PER_CHUNK` (4096) frames per
  chunk (`capture.rs:70`).
- Delivery is `SyncSender::try_send` — **the writer never blocks** (`send_tee_chunk`, `capture.rs:728`). On a
  full or hung-up channel the chunk is **dropped and counted** (`RecordingHandle::tee_dropped_chunks`,
  `capture.rs:190`; live-readable via `CaptureTee::dropped_counter()`).
- **The on-disk WAV is untouched.** The tee is strictly additive; with no tee attached the writer path is
  byte-identical to before.

Contract: *the recording is the source of truth; the live stream is throwaway.* A blocking tee could stall the
writer and corrupt the recording, so dropping a live chunk is the correct trade — the dropped-chunk counter
tells you when the consumer fell behind.

Plumbed additively through `corti-capture`: `Recorder::start_with_tee` / `start_tap_only_with_tee`
(`crates/corti-capture/src/lib.rs:190`) plus `Recorder::sample_rate()` (`lib.rs:249`, needed to size a
resampler/AEC before `stop`); `CaptureChunk`/`CaptureTee` are re-exported (`lib.rs:139`) so callers depend only
on `corti-capture`. Existing `start`/`start_tap_only` call sites are unchanged.

```
IO proc ─push→ SPSC ring ─drain→ writer thread ─┬─ hound → WAV        (source of truth, unaffected)
                                                └─ try_send → CaptureTee → live consumer  (lossy, counted)
```

## The async edge — `stream` feature

Behind the `stream` cargo feature, `live_word_stream(LiveTranscriber)` (`live.rs:349`) returns a `LiveSink`
(push audio from any thread) + a `LiveWordStream` implementing `futures_core::Stream<Item = Vec<Word>>`
(`live.rs:323`). It runs the sync transcriber on a dedicated std thread and bridges words out over a tokio
mpsc. The sink mirrors the capture tee's **bounded-lossy** contract: `LiveSink::push` `try_send`s onto a
bounded queue (`AUDIO_BACKLOG`, `live.rs:294`) and, when the decoder falls behind, drops the chunk and counts
it (`LiveSink::dropped_chunks`) rather than growing unbounded. Dropping the sink flushes and ends the stream;
reaching `None` joins the worker thread (`live.rs:337`).

**Design stance: sync core, async at the edge (guardrail 9).** The engine has no runtime — the same reason the
capture HAL callbacks only ever hand work to a channel. `tokio`/`futures-core` are pulled **only** by
`corti-transcribe-local`'s `stream` feature — never its default build — and are pinned in the workspace root
`Cargo.toml`. `corti-tap --live` does *not* enable `stream`: it uses the sync core over std channels. (The
`tokio` pin is shared with `corti-tap`'s default `inbox` feature, which uses it for AWS.)

## First consumer — `corti-tap --live`

`corti-tap --live` (build with `--features live`) wires the whole path end-to-end (`run_live`,
`crates/corti-tap/src/main.rs:176`): bounded tee → optional `StreamingAec::push` on the mic (skipped under
`--no-mic`) → two `LiveTranscriber`s (mic → `Me`, tap → `Them`) → words to stdout, flushed per line. AEC
lookahead (`CORTI_AEC_LOOKAHEAD_SECS`, default 5 s) warms the filter, so the **first mic words are delayed** by
the lookahead — noted in `--help`. `--live` and `--inbox` are **mutually exclusive** (the parser bails,
`main.rs:36`): live prints a transcript, it does not file to vagus.

## In-app consumer — live inbox filing (#87, ADR 0010)

The app now drives the same path for detector recordings. `Detector::start_with_live_hook`
(`crates/corti-detect/src/platform.rs:70`) consults an app-supplied `LiveHook` (`platform.rs:37`) at every
recording start; `AppLiveHook` returns a bounded tee (`TEE_BACKLOG = 2048`, at most about 64 MiB / 175 s at
48 kHz) when `live_filing` is on, the backend is local, and all configured models are on disk — otherwise
`None` and the batch path runs unchanged. The fixed queue absorbs model/decode/rolling-diarization bursts but
never scales with call length; full still means drop + count, never block capture.

One `corti-live` thread continuously drives AEC + ASR. At `live_buffer_minutes` (default 1, range 1–10), it
forces both ASR tails final, optionally diarizes only that window's far-end PCM, merges by timestamp, renders
once, appends once, and `sync_all`s the note. The initial note + parent directory and the final state flip are
also synced. A 128 MiB far-audio cap or 1 MiB text cap forces an earlier commit. After success all window
lengths return to zero and their allocations are reused: memory reaches a fixed high-water mark instead of
following call duration. The detector delivers an ID-specific finish/discard verdict before its downstream
event; any tee drop quality-gates the result into lossless same-note fallback. Filing semantics are in
[transcription.md](transcription.md#live-inbox-filing-87); the write-authority amendment is
[ADR 0010](../design/adr/0010-live-inbox-filing.md).

## What this is *not* (yet)

- **Live quality trades context for durability.** A natural VAD region is still capped at 20 s; the configured
  durability boundary additionally forces an open region final. There is no trailing-window re-decode. When
  live filing succeeds, these committed windows **are** the filed note; batch runs only as fallback.
- **Far-end speaker numbers are window-local.** Optional diarization runs before every append, but `Them N`
  clustering may renumber at a boundary. Stable cross-window identity needs persistent embedding matching and
  is outside ADR 0012.
- **The ADR 0008 push-driven live-transcript window is still open.** #87 wired `StreamingAec::push` +
  `LiveTranscriber` into the app (closing #74's in-app gap for the filing path), but the in-process UI window
  and its Channel transport remain follow-up.
