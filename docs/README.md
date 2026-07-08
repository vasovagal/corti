# corti internals guide

Current-state reference for how corti actually works, crate by crate, with `file.rs:line`
anchors into the shipped code. This is the "as-built" companion to the "as-designed"
`design/` tree — where the two disagree, this guide wins.

## Contract

- **Current state, not history.** Each page describes what the code does today, not what was
  planned. Roadmap/rationale lives elsewhere (below).
- **Verified against a version.** Every page opens with a `Verified against vX.Y.Z` line. The
  failure mode for internals docs is undated drift, so treat any page whose version predates the
  code you're reading as suspect and re-verify the anchors.
- **Decisions live in `design/adr/`** — the append-only "why" log (ADR NNNN). This guide cites
  ADRs but does not restate them.
- **Hard-won traps live in `design/LESSONS.md`** (LESSONS §N) and the invariants in
  `design/guardrails.md` (guardrail N). Cross-referenced here, not copied.

## Pages

| Page | Scope |
|------|-------|
| [architecture.md](architecture.md) | The whole-pipeline mechanistic graph: every thread, channel, ring, and ownership chain, with the two common misconceptions corrected. Start here. |
| [audio-pipeline.md](audio-pipeline.md) | Data plane: mic-in-use detection, aggregate-device + process-tap capture, the `io_proc` → ring → writer path, and offline file-to-file AEC. |
| [transcription.md](transcription.md) | The `Transcriber` trait, local (Parakeet/sherpa-onnx) and AWS backends, the queue + durable jobs, live inbox filing (#87, the `State:` line contract), and filing to vagus. |
| [app.md](app.md) | Tauri app surface: tray, windows, commands, threads. |
| [streaming.md](streaming.md) | The chunked `LiveTranscriber` API + tokio `Stream` adapter, the capture tee, and the in-app live-filing consumer. |
