# ADR 0012 — Crash-safe bounded live-transcript commits

- **Status:** Accepted (2026-08-04; #103)
- **Amends:** ADR 0009 (the live transcriber gains reusable checkpoints), ADR 0010 (live note appends gain an explicit durability boundary)
- **References:** #68, #74, #87, #93, #96, ADR 0007

## Context

Live inbox filing (#87) made a partial transcript visible during a call, but it did not define a storage
durability boundary: `File::flush()` only flushed userspace state and did not ask macOS to commit dirty pages.
The live path also deliberately skipped optional far-end diarization. Enabling diarization therefore meant
waiting for the whole-call batch path, which materializes call-sized audio and can lose every in-memory word if
the app or OS fails before the post-ASR checkpoint.

The desired control is a simple crash-loss budget: "buffer X minutes, then write what has been transcribed and
diarized." That budget must also be a memory bound. No collection owned by a live session may grow with call
duration.

## Decision

1. **Commit a rolling transcript window.** `live_buffer_minutes` is persisted and exposed in Settings and as
   `CORTI_LIVE_BUFFER_MINUTES`. It defaults to one minute and is clamped to 1–10 minutes. Input chunks are split
   exactly at the active boundary.
2. **Checkpoint, do not reload, the live ASR.** `LiveTranscriber::checkpoint()` flushes the resampler/VAD tail,
   returns every pending word, resets the bounded VAD epoch, and remains usable. A cumulative 16 kHz sample base
   keeps timestamps relative to the full call across resets. Parakeet and VAD sessions remain resident once per
   recording.
3. **Diarize only the active far-end window.** When far-end diarization is enabled, `LiveEngine` loads one
   reusable diarizer. The app retains only the current window's tap PCM, diarizes it before rendering, merges it
   with the mic segments, and releases/reuses the window after a successful append. With diarization disabled,
   no tap PCM is retained by the transcript window.
4. **Bind memory independently of configuration.** The optional tap window has a 128 MiB hard cap and pending
   recognized text has a 1 MiB hard cap; either forces an early commit. The capture tee remains a fixed lossy
   queue (2,048 normal chunks, at most about 64 MiB at 48 kHz) and streaming AEC/VAD state is fixed-size. Model
   RSS is large but fixed for one recording and is released afterward. No transcript or PCM collection is
   cumulative over the call.
5. **Make every append a storage boundary.** A complete merged window is rendered once, appended once, and
   followed by `File::sync_all`. The initial note file and its parent directory are synced before its path is
   published. The final short window is synced before the same-inode `State: transcribed ` flip, and that flip
   is also synced. Body rewrites used by fallback are synced too.
6. **Keep existing quality ownership.** A zero-drop live result is canonical and skips whole-call batch ASR.
   Any capture-tee drop or live error preserves the already-synced partial note and routes the retained lossless
   recording through the existing same-inode fallback/retry path.

## Consequences

- After the first successful commit, an app or OS crash leaves a valid note containing every prior committed
  window. Normal uncommitted note loss is the configured interval; work already queued behind a slow decoder is
  additionally bounded by the fixed tee rather than by call length.
- One-minute default far-end PCM is about 11 MiB at 48 kHz. The largest supported configured window is about
  115 MiB; the hard cap fires earlier at unusual rates. RSS reaches a fixed high-water mark and then reuses it.
- Forced VAD boundaries trade a small amount of ASR context for a hard durability interval. Users can raise the
  interval (up to ten minutes) when context/diarization quality matters more than crash-loss latency.
- Far-end `Them N` clustering is performed independently per window. Numeric labels are meaningful within a
  window and may be renumbered at a boundary; stable cross-window identity would require persistent embedding
  matching and is outside this change.
- `sync_all` and diarization run on `corti-live`, never the CoreAudio callback/writer. If they are slower than
  the fixed tee slack, chunks are dropped and the existing quality gate chooses the lossless fallback rather
  than blocking capture or allocating without limit.
- Manual webinar captures still have no live hook and remain on the batch path. This ADR changes the existing
  detector-recording live path only.
