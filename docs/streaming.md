# Chunked / live transcription

Use `LiveTranscriber` to decode one mono channel as chunks arrive. Batch and live callers share
the selected resident Parakeet engine: sherpa/ONNX on CPU or transcribe.cpp/GGML on Metal.
This page covers the pull API, capture transport, async adapter and app consumers. Decisions are in
[ADR 0009](../design/adr/0009-chunked-transcription-api.md),
[ADR 0011](../design/adr/0011-spike-transcribe-cpp-ggml-asr.md) and
[ADR 0012](../design/adr/0012-crash-safe-bounded-live-transcript.md).

## The pull model — `LiveTranscriber`

`corti_transcribe_local::LiveTranscriber` (`crates/corti-transcribe-local/src/live.rs`) is a **synchronous,
pull-based** transcriber for one mono channel. The caller drives it:

```rust
// One recognizer, shared; one VAD per channel.
let engine = LocalTranscriber::new(cfg).live_engine()?;   // loads selected Parakeet runtime + Silero once
let mut live = engine.channel()?;                         // a fresh VAD sharing the recognizer

live.push(&samples, 48_000);          // resample→VAD→decode closed regions→queue words
while let Some(words) = live.poll_words() {  /* words as they fall out */ }
let committed = live.checkpoint();    // force the open tail final; keep engine usable + absolute time
live.push(&more_samples, 48_000);     // next bounded durability epoch
let tail = live.finish();             // final flush; idempotently ends this channel
```

| method | signature | behavior |
|---|---|---|
| `push` | `push(&mut self, &[f32], sample_rate: u32)` (`live.rs`) | Resamples to 16 kHz (continuously across pushes; no-op at 16 kHz), feeds one Silero VAD in 512-sample windows. **Decode happens here** — every VAD region that *closes* during this push is decoded on the spot and its words queued. The configured maximum speech duration is 20 s; native settlement is not a hard cut at that duration. No-op after `finish`. |
| `poll_words` | `poll_words(&mut self) -> Option<Vec<Word>>` | Non-blocking drain of queued words. `None` when empty. |
| `checkpoint` | `checkpoint(&mut self) -> Vec<Word>` | Flush the current resampler/VAD tail, return every un-polled word, reset the bounded VAD epoch, and remain usable. A cumulative sample base keeps timestamps call-relative across checkpoints. |
| `finish` | `finish(&mut self) -> Vec<Word>` | Flush the final trailing region, return all remaining words, and end this channel. Idempotent. |

Two knobs matter for the pull model:

- **Decode cost lands inside `push`.** A `push` that closes a closed region pays that
  region's decode synchronously. There is no background thread in the core — if you need decode off your
  thread, use the `stream` adapter below.
- **Timestamps stay call-relative across checkpoints.** Within one epoch, `SpeechSegment::start()` is the
  VAD-relative sample index. `checkpoint()` advances a cumulative 16 kHz sample base before resetting the VAD;
  `drain_regions` adds that base, so every word remains seconds-from-call-start. Ordinary push boundaries are
  absorbed by a `WindowBuffer` carrying the sub-512 remainder.

`LiveEngine` is the resident engine: `LocalTranscriber::live_engine()` loads `Asr::Sherpa` or `Asr::Ggml`
plus shared models once; `LiveEngine::channel()` spawns a `LiveTranscriber` per channel — each with its own
stateful VAD, all sharing the one thread-safe ASR via `Arc`. A checkpoint never reloads either engine. When
`LocalConfig::diarize_far_end` is on, `LiveEngine` also owns one reusable sherpa diarizer;
`diarize_chunk` accepts one bounded source-rate window and returns call-relative turns.

## Batch runs on the live core

`engine::transcribe_resampled_channel` builds a `LiveTranscriber`, calls `push_resampled` and
`finish()`, and returns words, the decode report and completeness. The test-only
`transcribe_channel` wrapper uses `push` at 16 kHz and returns words.
The model-dependent `live_equals_batch_over_chunking` test compares one whole-channel push with
irregular chunks over the same recording. It is ignored in ordinary Rust test runs and requires
the native model gate. The app's bounded durability windows can change decoder context; they do
not promise batch-identical words. See [live boundaries](quality.md#live-boundaries-155).

## The capture tee — bounded, lossy, counted

The capture writer offers `CaptureTee` to callers that need audio before the WAV closes:

- `CaptureChunk` contains mono `mic`/`tap`, an immutable capture epoch, `retained_start_frame`,
  and frame-aligned `unavailable` flags. Chunks hold at most 4096 frames at the retained capture
  rate; mic is empty for tap-only capture. Empty availability means every frame is available.
  Epoch changes flush the preceding chunk. Dropped chunks do not compress retained offsets.
- `SyncSender::try_send` never blocks the writer. A full or disconnected receiver drops and
  counts the chunk. Read the counter through `CaptureTee::dropped_counter()` during capture
  or `RecordingHandle::tee_dropped_chunks` afterward.
- The tee is the raw downmix before writer AEC. A live consumer runs its own bounded filter and
  must reapply availability after filtering, including the delayed tail. Removing the tee does
  not change the on-disk filter policy.

The WAV remains recovery input; the tee can lose data. App quality accounting records those losses
and decides whether same-note batch recovery is needed. A drop count alone does not prove how much
source audio was lost.

`corti-capture` re-exports `CaptureChunk` and `CaptureTee`. Attach one with
`Recorder::start_with_tee` or `start_tap_only_with_tee`; `Recorder::sample_rate()` supplies the
retained rate before stop. Ordinary `start`/`start_tap_only` callers need no tee.

```
IO proc ─push→ SPSC ring ─drain→ writer thread ─┬─ StreamingAec → hound → WAV  (source of truth, #74)
                                                └─ try_send → CaptureTee → live consumer  (raw, lossy, counted)
```

## The async edge — `stream` feature

Behind the `stream` cargo feature, `live_word_stream(LiveTranscriber)` (`live.rs`) returns a `LiveSink`
(push audio from any thread) + a `LiveWordStream` implementing `futures_core::Stream<Item = Vec<Word>>`
(`live.rs`). It runs the sync transcriber on a dedicated std thread and bridges words out over a tokio
mpsc. The word-output channel is unbounded, so callers must drain it; the adapter does not
promise a fixed total output-memory bound. The sink mirrors the capture tee's **bounded-lossy** contract: `LiveSink::push` `try_send`s onto a
bounded queue (`AUDIO_BACKLOG`, `live.rs`) and, when the decoder falls behind, drops the chunk and counts
it (`LiveSink::dropped_chunks`) rather than growing unbounded. Dropping the sink flushes and ends the stream;
reaching `None` joins the worker thread (`live.rs`).

EOF means the worker has ended; it is not a completeness certificate. After EOF,
`LiveWordStream::completion()` exposes a bounded `LiveStreamCompletion`: its
`decode_report()` retains typed region outcomes and omission counts,
`decode_complete()` describes decoding of admitted input, `worker_failed()`
identifies a worker failure, and `dropped_chunks()` retains the existing
rejected-send count. That count does not measure source frames or buffered audio
abandoned by a failed worker; the independent worker-failure flag discloses that
failure. Metadata-only omissions do not turn successful decoding into failure.
An incomplete decode, worker failure, or known queue drop emits one fixed,
content-free terminal diagnostic. Surviving words keep their original order and
timestamps; no failure is represented by an invented word or an automatic retry.

Report spans and word times use the worker's admitted-input coordinates. Queue
drops are separately counted; those coordinates must not be treated as an exact
source timeline after a drop. A recording owner can bind known admitted input to
its existing capture epochs and shared quality record. The adapter does not
invent that source mapping. The constructor and `Stream<Item = Vec<Word>>`
remain unchanged; consumers that need completion information must inspect the
terminal record. The adapter never logs a panic payload; Rust's process panic
hook is a separate runtime facility and is not replaced.

**Design stance: sync core, async at the edge.** The engine has no runtime.
`tokio`/`futures-core` are pulled **only** by
`corti-transcribe-local`'s `stream` feature — never its default build — and are pinned in the workspace root
`Cargo.toml`. `corti-tap --live` does *not* enable `stream`: it uses the sync core over std channels. (The
`tokio` pin is shared with `corti-tap`'s default `inbox` feature, which uses it for AWS.)

## First consumer — `corti-tap --live`

`corti-tap --live` (in the default build) wires the whole path end-to-end (`run_live`,
`crates/corti-tap/src/main.rs`): bounded tee → optional `StreamingAec::push` on the mic (skipped under
`--no-mic`) → two `LiveTranscriber`s (mic → `Me`, tap → `Them`) → words to stdout, flushed per line. AEC
lookahead (`CORTI_AEC_LOOKAHEAD_SECS`, default 5 s) warms the filter, so the **first mic words are delayed** by
the lookahead — noted in `--help`. `--live` and `--inbox` are **mutually exclusive** (the parser bails,
`main.rs`): live prints a transcript, it does not file to vagus.

## In-app consumer — live inbox filing (#87, ADR 0010)

The app drives the same path for detector recordings. `Detector::start_with_live_hook`
(`crates/corti-detect/src/platform.rs`) consults an app-supplied `LiveHook` (`platform.rs`) at every
recording start; `AppLiveHook` returns a bounded tee (`TEE_BACKLOG = 2048`, at most about 72 MiB of PCM/availability plus <1 MiB of chunk metadata, or about
175 s of full chunks at 48 kHz) when `live_filing` is on, the backend is local, and the selected ASR + shared configured models are
on disk — GGML does not require the ONNX Parakeet set. Otherwise
`None` and the batch path runs unchanged. The fixed queue absorbs model/decode/rolling-diarization bursts but
never scales with call length; full still means drop + count, never block capture.

One `corti-live` thread drives AEC and ASR. App live AEC requests 0.1 s of lookahead, rounded to
whole filter blocks; this differs from the CLI/writer default. `live_buffer_minutes` selects a
nominal filing interval (default 1, range 1–10 minutes). The boundary policy waits for both
channels to certify a natural cut, then settles the selected source fence. A deadline or text
pressure forces a cut. See [quality.md](quality.md#live-boundaries-155) for the 22-second deferral,
reset behavior and known accuracy limits.

Each settled window optionally diarizes its bounded far-end PCM, merges by timestamp, applies
[segment cleanup](transcription.md#segment-cleanup--protect-words-retain-suspected-duplicates-149-161), renders once,
appends once and `sync_all`s the note. Initial note creation also syncs its parent directory;
the final state flip is synced. Far-audio retention is capped at 128 MiB, with a 32 MiB deferral
reserve inside that cap; native retention has a separate 128 MiB per-channel cap. Text is capped
at 1 MiB with settlement headroom. Unsupported configured windows fail setup rather than growing
with call length.

Cleanup preserves protected raw words and applies echo, fragment merge, backchannel, filler/stutter
and learned-lexicon rules after timestamp ordering. `strip_fillers` defaults on. The lexicon is
loaded once per session from `~/.local/share/corti/lexicon.json` through the `TextRule` seam.
`TranscriptWindow::carry` retains raw far-end decoder polls before diarization and cleanup, including
older sources still within the lookback across multiple windows. This read-only echo context is
never rendered or appended. It is capped at 256 sources and 1 MiB; omitted sources disable acoustic
deletion over their remaining lookback, with content-free omission counts.

`consume_chunks` drains AEC block statistics after each push. A window keeps its own statistics
plus echo lookback, capped at `MAX_WINDOW_AEC_BLOCKS` (8192). `finish_with_stats()` supplies the
final blocks; the locked delay and drop count are logged under `corti::live`. Missing acoustic
evidence, including AEC-off sessions, cannot authorize echo deletion. The same conservative rule
is available to batch cleanup through a valid AEC sidecar; live is not its only consumer.

A successful append clears window lengths and reuses allocations. The detector supplies an
ID-specific finish/discard verdict before the downstream event. Known processing loss triggers
same-note fallback while preserving captured quality facts. Filing authority is in
[ADR 0010](../design/adr/0010-live-inbox-filing.md) and
[transcription.md](transcription.md#live-inbox-filing-87).

### Early drop, before publication (#149 phase 2)

The Live Transcript store and hosted Live lane see closed decoder regions before the durable
append. Their published rows cannot be removed, so early echo decisions happen before publication.

`EarlyDrop` holds a mic region when it is at most 2 seconds **or** has at most 3 content tokens.
A region exceeding both limits publishes immediately. A hold ends when a far-end region closes
or `echo_window_seconds` passes after the mic region's end, whichever comes first. The default
maximum wait is 6 seconds; far-end rows are not held. Released rows retain their words and
call-relative timestamps, so the reader can place a late row correctly.

The shared `echo_assessment` first protects critical raw-text differences in matched sources.
Only a mic region with overlapping far-end speech and sufficient acoustic evidence can be
`DropAcoustic`. A text-only match is `SuspectedDuplicate` and stays. The reader and note receive
the same retained regions; optional evidence records withheld inputs without publishing them.

Durable append and session finish release all holds first. State is capped at 64 held regions
and 256 sources. Overflow releases the oldest hold or forgets the oldest source; it does not
discard words. Publication is FIFO, so an immediate mic region first releases earlier holds.
The `cleanup.echo_drop` setting gates the pass. Provenance records
`corti.configuration.segment_cleanup.live_early_drop`; the `corti::live` target records
`held`, `released_published` and `released_dropped` counts.

## In-app timestamped reader and microphone test (#105, ADR 0013)

The tray exposes one contextual action over the same live engine:

- During a detector recording, **Read live transcript…** opens a singleton `?view=live` window. Every closed
  mic/tap VAD region is grouped with the normal `SEGMENT_GAP` and published as a call-relative timestamp range
  plus `Me` / `Them 1` text. This happens before the one-minute default note boundary; no speculative second
  decode runs. Optional `Them N` diarization remains a durability-boundary operation, so immediate far-end rows
  deliberately say `Them`.
- Open-late readers take a `LiveTranscriptStore` snapshot, then apply monotonic revision/sequence events. The
  transient store is capped at 2,000 rows and about 1 MiB; it evicts oldest UI rows and reports that fact while
  leaving the durable note untouched. Closing the webview does not stop the stream.
- While idle, **Test microphone & live transcription…** loads the selected local ASR/VAD, pauses detector edges,
  and then opens the default microphone directly. `MicrophoneCapture` uses a fixed SPSC ring and bounded lossy
  tee but creates no process tap, aggregate, WAV, queue row, Vagus note, or retained transcript. Stop closes the
  microphone before releasing the generation-owned model slot and resuming detection.

See [ADR 0013](../design/adr/0013-live-transcript-window-and-microphone-test.md). AWS and manual webinar paths
remain batch-only and show an unavailable state rather than starting a second capture.

## What this is *not* (yet)

- **Live windows limit context.** VAD is configured for 20 s maximum speech,
  while the app uses native-certified natural cuts or an explicit forced boundary. There is no trailing-window re-decode. When
  live filing succeeds, these committed windows **are** the filed note; batch runs only as fallback. Segment
  cleanup inherits the same boundary for merging (see above).
- **Only the echo rule runs at the region.** Fragment merging and the backchannel rule still run only at the
  durability boundary, so the Live Transcript window (ADR 0013) and the hosted Live lane keep showing a
  "Yeah." and two halves of one sentence that the note will not. Echo is the one rule that runs early (see
  above), before a duplicate reaches those observers; only mic rows are eligible for echo deletion.
- **Far-end speaker numbers are window-local.** Optional diarization runs before every append, but `Them N`
  clustering may renumber at a boundary. Stable cross-window identity needs persistent embedding matching and
  is outside ADR 0012.
- **No unstable partial-word hypotheses.** ADR 0013 supersedes ADR 0008's proposed pseudo-partial recognizer.
  The reader updates when a VAD speech region closes, reusing exactly the words already headed to the durable
  path; it does not repeatedly re-decode a growing utterance.
