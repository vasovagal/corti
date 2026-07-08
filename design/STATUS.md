# corti — status

Shipped through **v0.8.0**: the full menu-bar pipeline — mic-in-use detection → CoreAudio process-tap
capture (2-track WAV) → offline streaming AEC (`StreamingAec`, ADR 0007) → transcription (AWS batch or
local offline Parakeet-TDT via sherpa-onnx, runtime-selectable) → filed vagus note. Plus the Tauri UI
surface (Settings, diagnostics Console, live stats, Ethics/Voiceprint guide, Recording Queue), the `corti`
CLI (`corti --list`), `corti-tap`, and the `corti-bench` audio-quality harness. Durability landed in #85
via `corti-jobs`: durable transcribe/file retry with backoff, startup recovery of orphaned jobs, and an
hourly retention sweep — job-level, not a full resume of a recording crashed mid-first-attempt (ADR 0007).

- **Current-state internals:** see [`../docs/`](../docs/).
- **Decisions:** see [`adr/`](adr/).
- **Landed since v0.8.0:** `#85` (`feat/opus-independent-stack`) — durable background jobs (`corti-jobs`)
  + retry/retention + a Recording Queue window (`app/src/queue_ui.rs`, `app/ui/src/Queue.tsx`).
