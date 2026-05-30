# 02 — corti-transcribe (+ aws, + whisper)

Turns a recorded 2-track WAV into a `DiarizedTranscript` (defined in `corti-core`, which also renders it to
Markdown via `to_markdown()`). Backends are **feature-flavored** behind one trait so the rest of the
pipeline is backend-agnostic (guardrail 6).

## The trait (corti-transcribe)
```rust
pub trait Transcriber {
    /// Transcribe the recording at `audio` into a diarized, timestamped transcript.
    /// Synchronous/blocking by design: corti transcribes *after* the call, off the UI thread, so a
    /// blocking call keeps the trait dependency-free (no async-trait/tokio in the contract). The Tauri app
    /// runs it on a background task; AWS polling happens inside the impl.
    fn transcribe(&self, audio: &Path, meta: &RecordingMeta) -> Result<DiarizedTranscript>;
}
```
The 2-track layout (ch0 = me, ch1 = them) is the diarization prior: even with a backend that can't diarize,
mapping ch0→`Speaker::Me` and ch1→`Speaker::Other` gives a usable transcript.

## corti-transcribe-aws (feature `aws`, the default backend)
Batch flow (per the validated plan):
1. Upload the (optionally 16 kHz mono downmixed) audio to S3 — `aws-sdk-s3`.
2. `StartTranscriptionJob` with `Settings::builder().show_speaker_labels(true).max_speaker_labels(N)` —
   `aws-sdk-transcribe`. Diarization + word timestamps built in.
3. Poll job status; on completion fetch the result JSON from `TranscriptFileUri`.
4. Join `results.items[]` (word + `start_time`/`end_time`) to `results.speaker_labels.segments[]`
   (`spk_0`…) → `Vec<TranscriptSegment>`. Map the speaker that aligns with ch0 energy to `Speaker::Me`.

Deps to add when implementing: `aws-config`, `aws-sdk-s3`, `aws-sdk-transcribe` (~1.106), `tokio` (the SDK is
async; run a private runtime inside the blocking `transcribe`). Needs creds/region/bucket config — surface
via env or a config struct. **Cost + privacy:** audio leaves the device; default conservatively and document
it. S3 lifecycle: delete uploaded media after the job (or set a bucket TTL).

## corti-transcribe-whisper (feature `whisper`, offline flavor)
Local `whisper-rs`. Fully offline — matches vagus's ethos and avoids per-minute cost. Weak/no diarization,
so lean on the 2-track split (transcribe ch0 and ch1 separately, then interleave segments by time). Model
file cached under `~/Library/Caches/corti/models/`.

## Feature wiring (in the app)
`default = ["aws"]`; `whisper` opt-in. The app picks an impl at startup based on enabled features / config.

## Depends on
`corti-core` (DiarizedTranscript, RecordingMeta, Speaker, TranscriptSegment). The renderer is already in
core; backends only produce the struct.
