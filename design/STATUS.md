# corti — status

Shipped through **v0.8.0**: the full menu-bar pipeline — mic-in-use detection → CoreAudio process-tap
capture (2-track WAV) → offline streaming AEC (`StreamingAec`, ADR 0007) → transcription (AWS batch or
local offline Parakeet-TDT via sherpa-onnx, runtime-selectable) → filed vagus note. Plus the Tauri UI
surface (Settings, diagnostics Console, live stats, Ethics/Voiceprint guide), the `corti` CLI
(`corti --list`), `corti-tap`, and the `corti-bench` audio-quality harness. Durability/crash-recovery is
**deferred** (ADR 0007): the queue is a session-spanning record, not a resumable store.

- **Current-state internals:** see [`../docs/`](../docs/).
- **Decisions:** see [`adr/`](adr/).
- **Unmerged work worth knowing:** `feat/opus-independent-stack` — durable background jobs
  (`corti-jobs`) + retry/retention + a Recording Queue window (`app/src/queue_ui.rs`, `app/ui/src/Queue.tsx`),
  cleanly based on v0.8.0.
