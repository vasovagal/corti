# 02 — corti-transcribe (+ aws, + local Parakeet)

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
**Implemented** via **channel identification** (not speaker labels): corti already captures ch0 = me,
ch1 = them as separate channels, so we let AWS transcribe each channel and map it deterministically —
`ch_0` → `Speaker::Me`, `ch_1` → `Speaker::Other("Them")` — no energy-alignment heuristic needed.
Batch flow (`crates/corti-transcribe-aws/src/lib.rs`):
1. For a durable stable name, probe `GetTranscriptionJob` first. A queued/in-progress/completed job is
   reattached without decoding or uploading the full call; a failed job is deleted for a fresh submission.
2. Only when no reusable job exists, re-encode the 2-track **float** WAV → **16-bit PCM** (`src/wav.rs`;
   AWS Transcribe rejects float WAV), preserving channels/rate, upload it, and call
   `StartTranscriptionJob` with channel identification plus our output bucket/key.
3. Poll until `Completed`/`Failed`; on success `GetObject` the result JSON from our key. A transient fetch
   or parse failure retains the job/output; a completed job whose key is confirmed missing is reset.
4. Parse `results.channel_labels.channels[].items[]` (word + `start_time`/`end_time`, punctuation glued),
   group each channel into segments on a >1.5 s pause, then merge both channels sorted by time
   (`src/parse.rs`, unit-tested). Unique one-shot calls attempt staged-object cleanup on every outcome; the
   durable app publishes exact ownership before upload, defers cleanup until its local transcript checkpoint
   is persisted, and keeps a separate cleanup job beyond terminal filing/transcription exhaustion.

**Config injection (not env):** the crate takes a caller-built `SdkConfig`
(`AwsTranscriber::new(&sdk_config, AwsOptions { bucket, .. })`); the Tauri app runs the standard
credential chain and logs failures. The sync `Transcriber::transcribe` drives the async SDK on a private
current-thread tokio runtime. (Alternative — speaker labels via `show_speaker_labels` + `max_speaker_labels`
for group calls with multiple remote voices — is left for later; channel-id is the default.)

### IAM permissions (the principal, not a service role)
We deliberately **do not** pass a `DataAccessRoleArn` to `StartTranscriptionJob`. When the role ARN is
omitted, Amazon Transcribe reaches S3 using the **permissions of the calling principal** (the IAM
user/role whose credentials the app resolved) — so there is no Transcribe service role, no bucket policy,
and no `transcribe.amazonaws.com` trust policy to manage for the same-account case. (A data-access role is
only needed for cross-account buckets; we'd add an optional `data_access_role_arn` to `AwsOptions` if that
ever comes up.)

Grant the **calling principal** the minimum below (input + output share one bucket under the `corti/`
prefix; `s3:DeleteObject` covers the `delete_after` cleanup; `s3:ListBucket` is required for Transcribe to
read the input object; Transcribe actions must be `Resource: "*"` because a job ARN doesn't exist until
after the call):
```json
{
  "Version": "2012-10-17",
  "Statement": [
    { "Sid": "CortiTranscribeJobs", "Effect": "Allow",
      "Action": ["transcribe:StartTranscriptionJob", "transcribe:GetTranscriptionJob",
                 "transcribe:DeleteTranscriptionJob"],
      "Resource": "*" },
    { "Sid": "CortiStagedObjects", "Effect": "Allow",
      "Action": ["s3:PutObject", "s3:GetObject", "s3:DeleteObject"],
      "Resource": "arn:aws:s3:::YOUR_BUCKET/corti/*" },
    { "Sid": "CortiListBucket", "Effect": "Allow",
      "Action": "s3:ListBucket",
      "Resource": "arn:aws:s3:::YOUR_BUCKET",
      "Condition": { "StringLike": { "s3:prefix": "corti/*" } } }
  ]
}
```
If `AwsOptions.key_prefix` is changed from the `corti/` default, update the two `corti/*` patterns to
match.

**Cost + privacy:** audio leaves the device; `delete_after` defaults on (staged `.wav` + `.json` removed
when the job completes). A bucket lifecycle TTL on the `corti/` prefix is a sensible backstop.

## corti-transcribe-local (feature `local`, offline flavor)
Fully offline, on-device, Apple-Silicon transcription, avoiding per-minute cost + PHI egress. The model is
**NVIDIA Parakeet-TDT-0.6B-v3**, a transducer far less hallucination-prone than Whisper. Its per-region ASR
runtime is selectable: int8 ONNX via official sherpa-onnx/CPU (compatibility default, ADR 0003), or Q8_0
GGUF via pinned transcribe.cpp/GGML on Metal (standard builds, ADR 0011). Both feed the same shared pipeline.

Pipeline (`crates/corti-transcribe-local/`): read the 2-track float WAV (`audio.rs`) → per channel,
resample to 16 kHz and run sherpa's **Silero VAD** to chunk into speech regions (also sidesteps Parakeet's
~30 s offline clip limit) → dispatch each region through `Asr::{Sherpa,Ggml}` → map engine results into
shared timestamped words. ch0 (mic) → `Speaker::Me`; ch1 (system tap) → `Speaker::Other("Them")` by default. Far-end
speaker splitting (`Them 1/2/…` via pyannote-segmentation-3.0 + a speaker-embedding model, ONNX) is **opt-in**
(`CORTI_LOCAL_DIARIZE=1`) and **off by default**; when off, the segmentation + embedding models aren't
required. The embedding stage is **runtime-selectable** among three English (VoxCeleb-trained) models — NeMo
TitaNet-Large (default), WeSpeaker ResNet34-LM, 3D-Speaker CAM++ — chosen in Settings → Transcription or via
`CORTI_LOCAL_EMBEDDING` (the old zh-cn model was removed; sherpa-onnx auto-detects each model's framework from
its ONNX metadata, so all three share one diarizer). Tune `CORTI_LOCAL_DIARIZE_THRESHOLD` (default 0.5) to
curb over-clustering (issue #18). All shaping (pause-split grouping, speaker merge,
diarization attribution) is the shared `corti_transcribe::segment` module — the same helpers the AWS parser
uses. Models cache under `~/Library/Caches/corti/models/`; Settings downloads the selected ASR artifact +
shared models with pinned SHA-256 verification. A missing selected artifact fails clearly. The M1 Pro
five-minute excerpt measured GGML/Metal at 4.09× sherpa's speed, 19% lower peak RSS, and equal normalized
WER (ADR 0011).

Out of scope here: far-end diarization quality (#18, #14, #15), replacing sherpa's VAD/diarization with
transcribe.cpp, and flipping the default engine before a real-call GGML live-checkpoint soak.

## Feature wiring (in the app)
Standard builds use `default = ["aws", "local", "local-ggml"]`: both cloud/local backends and both local
ASR runtimes compile in. Settings chooses `CORTI_TRANSCRIBE_BACKEND = aws | local` and, for local,
`CORTI_LOCAL_ASR_ENGINE = sherpa | ggml`; sherpa remains the config default for existing-model
compatibility. A minimal `--no-default-features --features local` build omits transcribe.cpp/Metal and the UI
disables that choice. Config reload applies between recordings.

## Depends on
`corti-core` (DiarizedTranscript, RecordingMeta, Speaker, TranscriptSegment) and
`corti-transcribe::segment` (shared word→segment helpers). The renderer is already in core; backends only
produce the struct.
