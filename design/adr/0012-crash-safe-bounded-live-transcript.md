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
   keeps timestamps relative to the full call across resets. The selected Parakeet ASR (sherpa or transcribe.cpp) and VAD sessions remain resident once per
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

## Amendment: pause-aware live ownership (#155)

The configured `live_buffer_minutes`, its default, and the existing nominal audio-cap due point stay
unchanged. Once due, filing waits for a common source-frame ownership interval certified by both
established channels: no open or queued VAD region, all attempted regions behind the cut, and a
proved lower bound on every future region start. A never-present channel is neutral; a late channel
keeps its original source origin. Natural filing retires only the selected PCM prefix and leaves
VAD, resampler, AEC, cleanup carry, and their clocks running.

The maximum additional source ownership is the configured 20-second speech maximum plus the
actual minimum silence plus one second of slack: **22 seconds** at the unchanged one-second silence
setting. That speech maximum is not itself proof of a hard native split. If no common natural cut
is available at the deadline, a fixed source fence gates decoding before it can cross the cut.
Old polls are drained and filed or accounted as lost before acknowledgement permits suffix replay.
The existing text-cap escape remains immediate. A forced cut is recorded as `ForcedBoundary` /
`DegradedOnly`; it alone does not trigger batch transcription or make a recording partial.

Source ownership and filing latency are distinct. Real input beyond a forced fence may be needed
to settle native state. Checked reserve arithmetic includes the actual AEC output lag, 4096-frame
source admission, resampler packet/filter tails, and the native future-start guard. EOF finishes
the real AEC and resampler tails once. If real input still cannot settle a pending fence, only the
actually unfiled old and next source intervals become `WindowNotFiled` processing loss, and the
existing one-backend whole-file fallback rewrites the same note. A partial batch cannot certify
those intervals complete; no live words are spliced into that batch result.

After ownership is acknowledged, terminal processing of a fresh, pinned Silero detector admits
at most 5727 synthetic zero samples to the VAD alone. The locked detector's 575-sample remainder
and 5152-sample future-onset bound then prove that it has processed the real EOF. Padding does
not advance source time or enter retained PCM or ASR; emitted regions are clipped to the real
EOF before mapping and decoding. A processed VAD-negative suffix remains valid empty output.
Unknown detector provenance, insufficient native headroom, invalid mapping, or unresolved
ownership cannot grant this certificate. Legacy destructive checkpoints preserve uncertainty
about their unresolved suffix instead of silently discarding it. This is a processing guarantee,
not a guarantee that ASR recognizes every word; native word and critical-span gates still apply.

The optional tap allocation retains its **total 128 MiB limit**, including the nominal window,
deferral, and settlement reserve, reserved fallibly in one backing vector. The reserve component
also has its own 32 MiB bound. A configuration that leaves insufficient room fails live setup and
uses the existing batch route; it never shortens nominal ownership or enlarges the tap cap. This
affects unusually high-rate configurations whose nominal window already consumes that cap; the
ordinary 60-second and 600-second windows at 48 kHz still fit. Native retained
canonical PCM is independently limited to 128 MiB per channel; replay is bounded by the checked
settlement allowance. These are component limits, not a total-process RSS claim. Raw poll admission
reserves its retained text and child/context cost before inserting or cloning it against the
existing 1 MiB budget. Impossible capacity or omitted output produces truthful processing loss
instead of shrinking the nominal window or silently growing a queue.

Native signed-counter maintenance arms early, with enough headroom for the entire next ordinary
window and its settlement. It never makes a window due earlier or changes the selected source cut.
At a certified natural maintenance cut C, VAD resets only after old ownership is retired and replays
the retained, never-attempted suffix from the certified future-start bound U, while preserving C
and the source/resampler clocks. Exact PCM ownership does not prove inference equivalence after
reset. Natural, forced, and maintenance runs therefore share the same native word and critical-span
acceptance gates. The legacy public checkpoint/finish API remains available to existing callers.
