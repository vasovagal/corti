# corti — status

Current tree (latest tag **v0.14.0**): the full menu-bar pipeline — mic-in-use detection → CoreAudio process-tap
capture + in-flight writer-thread AEC (`StreamingAec`, ADR 0007/#74) → cleaned 2-track WAV →
transcription (AWS batch or local offline Parakeet-TDT via sherpa/CPU or transcribe.cpp/Metal,
runtime-selectable) → filed vagus note. Capture disposition/config is durable in `queue.db`, so legacy
rows and raw setup fallback still take the safe file pass. Plus the Tauri UI
surface (Settings, diagnostics Console, live stats, Ethics/Voiceprint guide, Recording Queue), the `corti`
CLI (`corti --list`), `corti-tap`, and the `corti-bench` audio-quality harness. Durability landed in #85
via `corti-jobs`: durable transcribe/file retry with backoff, startup recovery of orphaned jobs, and an
hourly retention sweep — job-level, not a full resume of a recording crashed mid-first-attempt (ADR 0007).

- **Current-state internals:** see [`../docs/`](../docs/).
- **Decisions:** see [`adr/`](adr/).
- **v0.11.0:** crash-safe bounded live note commits (#103) and selectable transcribe.cpp/Metal Parakeet
  inference (#92), while sherpa remains the compatibility default.
- **v0.12.0:** bounded timestamped Live Transcript window, ephemeral microphone/ASR test, and reliable
  foreground activation for every tray-opened utility window (#105, ADR 0013).
- **v0.13.0:** versioned, searchable transcript-generation provenance in each note's `corti`
  frontmatter—release, final live/batch path, model artifacts, and safe quality configuration—with
  checkpoint-stable retries, truthful live→batch fallback rewrites, and dedicated Vagus lexical/semantic
  metadata chunks (#110, ADR 0014; Vagus ADRs 0027/0028).
- **v0.14.0:** optional hosted-LLM cleanup for live and final transcripts, raw/clean/diff views,
  configurable provider and language preferences, resumable encrypted request state, usage/cost accounting,
  and transcript-grounded questions. Direct transports cover OpenAI, Anthropic, Vertex AI, and Amazon
  Bedrock; experimental Codex broker support remains explicitly gated (#113, #115, ADR 0015).
