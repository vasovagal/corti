# ADR 0013 — Bounded live-transcript window and ephemeral microphone test

- **Status:** Accepted (2026-08-05; #105)
- **Supersedes:** ADR 0008's proposed recognizer/transport/buffer design
- **Amends:** ADR 0004 (tray-opened utility windows activate and focus), ADR 0012 (recognized live words gain a transient UI observer)
- **References:** ADR 0009, ADR 0010, ADR 0011, #74, #87, #103

## Context

The local detector path already transcribes both channels continuously and preserves call-relative word
timestamps across durable checkpoints. Until now its only mid-call reader was the rolling Vagus note, which
updates at the configured one-to-ten-minute durability interval. A user who wants to read what was just said
needs lower-latency, in-app feedback, and a way to verify microphone permission, model installation, and live
ASR without arranging a call.

ADR 0008 proposed a separate pseudo-partial recognizer, a call-length canonical in-memory transcript, and a
Tauri IPC channel. Subsequent work changed the premises: ADR 0009 delivered a real push-driven VAD/ASR core,
ADR 0010 made the live note canonical, and ADR 0012 made rolling words crash-safe and strictly bounded. A second
speculative recognizer would duplicate compute and could disagree with the transcript Corti actually files.
A call-length UI buffer would also violate ADR 0012's bounded-memory posture.

The existing utility-window opener has a separate usability defect: changing an Accessory app to Regular and
calling Tauri `set_focus` does not reliably activate Corti, so tray-opened windows can remain behind another
application.

## Decision

1. **Observe the existing live recognizer; do not add speculative decoding.** Whenever either
   `LiveTranscriber::poll_words`, `checkpoint`, or `finish` returns a closed speech region, the app converts
   those words with the existing `SEGMENT_GAP` grouping and publishes timestamped `Me` / `Them 1` rows. Word
   timestamps are seconds from call start and therefore survive ADR 0012 checkpoint resets. Far-end rows are
   `Them` in the immediate view; optional `Them N` diarization still runs at the durable-window boundary and
   remains authoritative in the filed note.
2. **Use one transient, bounded managed store.** `LiveTranscriptStore` retains at most 2,000 rows and about
   1 MiB including conservative row overhead. It evicts oldest rows and reports the eviction count. Every
   session and row has monotonic revision/sequence identity. The webview subscribes before requesting its
   open-late snapshot, ignores stale revisions, deduplicates row sequences, and trims to the server-provided
   retention floor. Tiny `live-transcript-changed` deltas update an open window; a slow reconciliation snapshot
   repairs a suspended/missed webview event. The crash-safe Vagus note remains the only durable transcript.
3. **Add one contextual tray action and singleton window.** During a detector call it reads **Read live
   transcript…** and only opens/focuses the existing stream. While idle it reads **Test microphone & live
   transcription…** and starts the explicit test before opening the same `?view=live` window. An active test
   becomes **Read microphone test transcript…**. Webinar capture disables the test action because webinar
   streaming remains batch-only. Rows show speaker, start/end timestamp (`MM:SS`, or hours), and text; automatic
   scroll follows only while the reader remains near the bottom.
4. **Make test mode microphone-only and non-persistent.** `MicrophoneCapture` opens the default input device
   directly and sends bounded mono `CaptureChunk { mic, tap: [] }` values through the existing wait-free
   ring/`try_send` discipline. It creates no process tap, aggregate, WAV, queue row, note, or retained
   transcript. The selected local Parakeet runtime and VAD are loaded before the microphone opens; one channel
   publishes `Me` rows and finalizes on explicit Stop.
5. **Suspend detection before opening Corti's test microphone.** The detector worker acknowledges `Pause` only
   when no real recording exists, clears pending debounce state, and ignores Corti's own mic edge until
   `Resume`. Test startup also excludes webinar/transcription activity, reserves `LiveManager`'s
   generation-owned one-model slot, and makes the pipeline defer due background jobs at a bounded 250 ms
   recheck cadence. Cleanup closes the microphone first, releases that exact generation, then
   resumes detection. A stale cleanup cannot release a newer test.
6. **Activate every tray-opened utility window.** On the AppKit main thread, the shared opener switches Corti
   to `Regular`, calls `NSApplication.activateIgnoringOtherApps(true)`, then unminimizes, shows, and focuses the
   webview. New windows are centered. The behavior applies to Settings, Queue, Ethics, Diagnostics, How Corti
   Works, and Live Transcript; closing the last one still returns Corti to `Accessory`.

## Consequences

- Live rows arrive at VAD closure/pause, not as unstable word-by-word hypotheses. There is no extra ASR pass,
  no second model, and no disagreement caused by speculative partial decoding.
- The UI can omit old rows on very long/dense calls, explicitly saying how many were evicted. The durable note
  is unaffected and continues to commit its independently bounded windows.
- Closing or reloading the window does not stop a call or microphone test. Reopening snapshots the retained
  rows and resumes deltas.
- Test mode validates the exact configured local ASR/VAD and default microphone while making a strong privacy
  promise: nothing from the test is filed or written to disk. It intentionally does not test system-audio
  capture, AEC, far-end ASR, or diarization.
- AWS calls and manual webinar captures still cannot stream because neither has the local detector tee. The
  window explains unavailability rather than silently starting another recording.
- Direct `NSApplication` activation is macOS-specific and justified by Corti's Apple-Silicon/macOS-only
  platform decision (ADR 0002).
