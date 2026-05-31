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
**Implemented** via **channel identification** (not speaker labels): corti already captures ch0 = me,
ch1 = them as separate channels, so we let AWS transcribe each channel and map it deterministically —
`ch_0` → `Speaker::Me`, `ch_1` → `Speaker::Other("Them")` — no energy-alignment heuristic needed.
Batch flow (`crates/corti-transcribe-aws/src/lib.rs`):
1. Re-encode the 2-track **float** WAV → **16-bit PCM** (`src/wav.rs`; AWS Transcribe rejects float WAV),
   keeping both channels + sample rate. Upload to S3 — `aws-sdk-s3`.
2. `StartTranscriptionJob` with `Settings::builder().channel_identification(true)` and
   `output_bucket_name`/`output_key` pointing back at our bucket — `aws-sdk-transcribe`.
3. Poll `GetTranscriptionJob` until `Completed`/`Failed`; on success `GetObject` the result JSON we
   directed to our own key (stays inside `aws-sdk-s3`, no extra HTTP client).
4. Parse `results.channel_labels.channels[].items[]` (word + `start_time`/`end_time`, punctuation glued),
   group each channel into segments on a >1.5 s pause, then merge both channels sorted by time
   (`src/parse.rs`, unit-tested). Best-effort delete the staged `.wav` + `.json` when done.

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
      "Action": ["transcribe:StartTranscriptionJob", "transcribe:GetTranscriptionJob"],
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

## corti-transcribe-whisper (feature `whisper`, offline flavor)
Local `whisper-rs`. Fully offline — matches vagus's ethos and avoids per-minute cost. Weak/no diarization,
so lean on the 2-track split (transcribe ch0 and ch1 separately, then interleave segments by time). Model
file cached under `~/Library/Caches/corti/models/`.

## Feature wiring (in the app)
`default = ["aws"]`; `whisper` opt-in. The app picks an impl at startup based on enabled features / config.

## Depends on
`corti-core` (DiarizedTranscript, RecordingMeta, Speaker, TranscriptSegment). The renderer is already in
core; backends only produce the struct.
