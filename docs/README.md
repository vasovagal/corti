# corti internals guide

Use these pages to trace Corti's current capture, transcription and filing paths. Start with
[architecture](architecture.md), then choose the component below. Source symbols identify the
implementation; check the current tree before relying on an older checkout's description.

## Contract

- **Describe the current implementation.** Keep plans and rationale in `design/`.
- **Verify against source.** Code and workflows establish behavior; this guide does not override
  the binding [guardrails](../design/guardrails.md). Report disagreements instead of silently
  changing a contract.
- **Keep decisions in [ADRs](../design/adr/README.md).** Preserve their history and link to their
  rationale rather than copying it.
- **Keep operational traps in [LESSONS](../design/LESSONS.md).** Link to them where they affect a step.

## Pages

| Page | Scope |
|------|-------|
| [architecture.md](architecture.md) | Capture and filing threads, channels and ownership. Start here. |
| [audio-pipeline.md](audio-pipeline.md) | Detection, process-tap capture, audio transport, writer and echo cancellation. |
| [transcription.md](transcription.md) | Local and AWS backends, durable retries, live filing and the Vagus boundary. |
| [app.md](app.md) | Tauri tray, windows, commands and app threads. |
| [streaming.md](streaming.md) | `LiveTranscriber`, the optional async adapter, capture tee and live consumers. |
| [quality.md](quality.md) | Capture/decode losses, live boundaries, echo decisions and validation limits. |
| [local-transcript-evidence.md](local-transcript-evidence.md) | Optional local transcript evidence: retention, identities, privacy and memory bounds. |
