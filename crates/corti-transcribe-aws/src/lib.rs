//! AWS Transcribe (batch) backend for corti.
//!
//! Stages the recording in S3, runs a `StartTranscriptionJob`, polls to completion, fetches the result
//! JSON, and parses it into a [`DiarizedTranscript`]. A 2-channel mic+tap WAV (ch0 = me / ch1 = them) is
//! transcribed with **channel identification** so each channel maps deterministically to a speaker; a
//! 1-channel tap-only ("webinar") WAV is transcribed as a plain single-speaker job (everything is the
//! far-end "Them") since AWS only accepts channel identification for multi-channel audio. See
//! `design/02-corti-transcribe.md`.
//!
//! The [`Transcriber`] contract is synchronous, so [`AwsTranscriber::transcribe`] drives the async AWS SDK
//! on a private current-thread tokio runtime. The caller (the Tauri app) builds the `SdkConfig` via the
//! standard credential chain and passes it to [`AwsTranscriber::new`]; this crate never reads the
//! environment or resolves credentials itself.

use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use aws_sdk_transcribe::types::{
    LanguageCode, Media, MediaFormat, Settings, TranscriptionJobStatus,
};
use corti_core::{DiarizedTranscript, RecordingMeta};
use corti_transcribe::Transcriber;

mod parse;
mod wav;

/// Tunables for the AWS Transcribe backend. Construct via [`AwsOptions::new`] (sets the required bucket)
/// and override fields as needed; [`Default`] is provided for ergonomic struct-update syntax.
#[derive(Debug, Clone)]
pub struct AwsOptions {
    /// S3 bucket used to stage the audio and receive the transcript JSON. **Required.**
    pub bucket: String,
    /// Key prefix for staged objects (e.g. `corti/`).
    pub key_prefix: String,
    /// BCP-47 language code, e.g. `en-US`.
    pub language: String,
    /// Delete the staged `.wav` and `.json` from S3 once transcription completes.
    pub delete_after: bool,
    /// How often to poll job status.
    pub poll_interval: Duration,
    /// Give up (and error) if the job hasn't finished within this long.
    pub max_wait: Duration,
    /// Stable Transcribe job name (the caller's recording id). When set, the AWS job and its staged S3
    /// objects use this name verbatim (sanitized), so a crash mid-transcribe re-attaches to the *same* job
    /// on resume — `start` tolerates the resulting `ConflictException` and falls through to polling, instead
    /// of paying to submit a fresh job. When `None`, a unique-per-attempt name is minted (the original
    /// behavior), which is what you want for one-shot transcribes with no durable queue behind them.
    pub job_name: Option<String>,
}

impl AwsOptions {
    /// Options for `bucket`, with sensible defaults for everything else.
    pub fn new(bucket: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            ..Default::default()
        }
    }
}

impl Default for AwsOptions {
    fn default() -> Self {
        Self {
            bucket: String::new(),
            key_prefix: "corti/".to_string(),
            language: "en-US".to_string(),
            delete_after: true,
            poll_interval: Duration::from_secs(5),
            max_wait: Duration::from_secs(30 * 60),
            job_name: None,
        }
    }
}

/// AWS Transcribe batch backend. Build with [`AwsTranscriber::new`] from a caller-provided `SdkConfig`.
pub struct AwsTranscriber {
    s3: aws_sdk_s3::Client,
    transcribe: aws_sdk_transcribe::Client,
    opts: AwsOptions,
}

impl AwsTranscriber {
    /// Build the S3 + Transcribe clients from a caller-provided `SdkConfig` (the app runs the credential
    /// chain and handles/logs any failures building it).
    pub fn new(sdk_config: &aws_config::SdkConfig, opts: AwsOptions) -> Self {
        Self {
            s3: aws_sdk_s3::Client::new(sdk_config),
            transcribe: aws_sdk_transcribe::Client::new(sdk_config),
            opts,
        }
    }

    /// The full async pipeline: re-encode → upload → start → poll → fetch → parse → clean up.
    async fn run(&self, audio: &Path) -> Result<DiarizedTranscript> {
        if self.opts.bucket.is_empty() {
            bail!("AwsOptions.bucket is empty — set the S3 bucket to stage audio in");
        }

        // 1. Re-encode the float WAV to 16-bit PCM (what AWS Transcribe accepts). The channel count decides
        //    whether we ask for channel identification: 2-track mic+tap ⇒ yes; 1-track tap-only ⇒ no
        //    (AWS rejects channel identification on mono audio).
        let (pcm_path, sample_rate, channels) = wav::to_pcm16_temp(audio)?;
        let multichannel = channels >= 2;

        // 2. Job name + object keys. A caller-supplied `job_name` (the recording id) makes the job and its
        //    staged S3 keys STABLE so a resumed `Transcribing` job re-attaches to the same AWS job; without
        //    one, a unique-per-attempt name avoids colliding with a still-retained AWS job whose staged
        //    output we may have already deleted. (See `resolve_job_name`.)
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let stem = audio.file_stem().map(|s| s.to_string_lossy().into_owned());
        let job = resolve_job_name(self.opts.job_name.as_deref(), stem.as_deref(), suffix);
        let in_key = format!("{}{job}.wav", self.opts.key_prefix);
        let out_key = format!("{}{job}.json", self.opts.key_prefix);

        // 3. Upload, then drop the local temp regardless of what follows.
        let upload = self.upload(&pcm_path, &in_key).await;
        let _ = std::fs::remove_file(&pcm_path);
        upload?;

        // 4. Start the job (idempotent on ConflictException).
        self.start_job(&job, &in_key, &out_key, sample_rate, multichannel)
            .await?;

        // 5. Poll, 6. fetch, 7. parse (channel-identified vs. single-speaker per `multichannel`).
        let result = self.await_result(&job, &out_key, multichannel).await;

        // 8. Best-effort cleanup of staged objects.
        if self.opts.delete_after {
            self.delete(&in_key).await;
            self.delete(&out_key).await;
        }

        result
    }

    async fn upload(&self, pcm_path: &Path, key: &str) -> Result<()> {
        let bytes = std::fs::metadata(pcm_path).map(|m| m.len()).unwrap_or(0);
        let body = aws_sdk_s3::primitives::ByteStream::from_path(pcm_path)
            .await
            .with_context(|| format!("reading {} for upload", pcm_path.display()))?;
        let started = std::time::Instant::now();
        self.s3
            .put_object()
            .bucket(&self.opts.bucket)
            .key(key)
            .body(body)
            .send()
            .await
            .with_context(|| format!("uploading s3://{}/{key}", self.opts.bucket))?;
        tracing::info!(
            target: "corti::transcribe::aws",
            bucket = %self.opts.bucket,
            key,
            bytes,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "uploaded audio to S3"
        );
        Ok(())
    }

    async fn start_job(
        &self,
        job: &str,
        in_key: &str,
        out_key: &str,
        sample_rate: u32,
        multichannel: bool,
    ) -> Result<()> {
        let media = Media::builder()
            .media_file_uri(format!("s3://{}/{in_key}", self.opts.bucket))
            .build();
        let mut req = self
            .transcribe
            .start_transcription_job()
            .transcription_job_name(job)
            .language_code(LanguageCode::from(self.opts.language.as_str()))
            .media_format(MediaFormat::Wav)
            .media_sample_rate_hertz(sample_rate as i32)
            .media(media)
            .output_bucket_name(&self.opts.bucket)
            .output_key(out_key);
        // Channel identification requires ≥ 2 channels; a tap-only (mono) recording is a plain job that AWS
        // returns as a single `results.items` stream (no `channel_labels`).
        if multichannel {
            req = req.settings(Settings::builder().channel_identification(true).build());
        }
        let started = req.send().await;

        if let Err(err) = started {
            // A pre-existing job with this name is fine — fall through to polling it.
            let is_conflict = err
                .as_service_error()
                .map(|e| e.is_conflict_exception())
                .unwrap_or(false);
            if !is_conflict {
                tracing::error!(
                    target: "corti::transcribe::aws",
                    job,
                    error = %err,
                    "StartTranscriptionJob failed"
                );
                return Err(err).context("StartTranscriptionJob failed");
            }
            tracing::info!(
                target: "corti::transcribe::aws",
                job,
                "transcription job already exists — re-attaching"
            );
        } else {
            tracing::info!(
                target: "corti::transcribe::aws",
                job,
                in_key,
                out_key,
                multichannel,
                "submitted transcription job"
            );
        }
        Ok(())
    }

    async fn await_result(
        &self,
        job: &str,
        out_key: &str,
        multichannel: bool,
    ) -> Result<DiarizedTranscript> {
        let deadline = Instant::now() + self.opts.max_wait;
        let poll_started = Instant::now();
        loop {
            let resp = self
                .transcribe
                .get_transcription_job()
                .transcription_job_name(job)
                .send()
                .await
                .map_err(|e| {
                    tracing::error!(target: "corti::transcribe::aws", job, error = %e, "GetTranscriptionJob failed");
                    e
                })
                .context("GetTranscriptionJob failed")?;
            let tjob = resp
                .transcription_job()
                .context("GetTranscriptionJob returned no job")?;
            match tjob.transcription_job_status() {
                Some(TranscriptionJobStatus::Completed) => {
                    tracing::info!(
                        target: "corti::transcribe::aws",
                        job,
                        elapsed_ms = poll_started.elapsed().as_millis() as u64,
                        "transcription job completed"
                    );
                    break;
                }
                Some(TranscriptionJobStatus::Failed) => {
                    let reason = tjob.failure_reason().unwrap_or("unknown reason");
                    tracing::error!(target: "corti::transcribe::aws", job, reason, "transcription job failed");
                    bail!("transcription job failed: {reason}");
                }
                // Queued / InProgress / None / future variants: keep waiting.
                _ => {
                    if Instant::now() >= deadline {
                        tracing::error!(
                            target: "corti::transcribe::aws",
                            job,
                            max_wait_ms = self.opts.max_wait.as_millis() as u64,
                            "transcription job did not finish before deadline"
                        );
                        bail!(
                            "transcription job did not finish within {:?}",
                            self.opts.max_wait
                        );
                    }
                    tokio::time::sleep(self.opts.poll_interval).await;
                }
            }
        }

        // Completed: fetch the JSON we directed to our own bucket/key and parse it.
        let obj = self
            .s3
            .get_object()
            .bucket(&self.opts.bucket)
            .key(out_key)
            .send()
            .await
            .map_err(|e| {
                tracing::error!(target: "corti::transcribe::aws", bucket = %self.opts.bucket, key = out_key, error = %e, "fetching transcript failed");
                e
            })
            .with_context(|| format!("fetching transcript s3://{}/{out_key}", self.opts.bucket))?;
        let bytes = obj
            .body
            .collect()
            .await
            .context("reading transcript body")?
            .into_bytes();
        tracing::info!(
            target: "corti::transcribe::aws",
            bucket = %self.opts.bucket,
            key = out_key,
            bytes = bytes.len(),
            "fetched transcript JSON"
        );
        let text = String::from_utf8_lossy(&bytes);
        if multichannel {
            parse::parse_channel_transcript(&text)
        } else {
            parse::parse_single_channel_transcript(&text)
        }
    }

    async fn delete(&self, key: &str) {
        if let Err(e) = self
            .s3
            .delete_object()
            .bucket(&self.opts.bucket)
            .key(key)
            .send()
            .await
        {
            tracing::warn!(
                target: "corti::transcribe::aws",
                bucket = %self.opts.bucket,
                key,
                error = %e,
                "failed to clean up staged S3 object"
            );
        }
    }
}

impl Transcriber for AwsTranscriber {
    fn transcribe(&self, audio: &Path, _meta: &RecordingMeta) -> Result<DiarizedTranscript> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("building tokio runtime for AWS Transcribe")?;
        rt.block_on(self.run(audio))
    }
}

/// Resolve the Transcribe job name (which also names the staged S3 objects). A caller-supplied `job_name`
/// (the durable recording id) is used verbatim — sanitized — so a resumed job re-attaches to the same AWS
/// job and re-fetches the same output key. Otherwise a unique-per-attempt name is minted from the audio
/// stem plus a nanosecond suffix. AWS job names must match `0-9a-zA-Z._-`, hence [`sanitize`].
fn resolve_job_name(
    job_name: Option<&str>,
    audio_stem: Option<&str>,
    unique_suffix: u128,
) -> String {
    match job_name {
        Some(name) => sanitize(name),
        None => {
            let stem = audio_stem
                .map(sanitize)
                .unwrap_or_else(|| "corti-job".to_string());
            format!("{stem}-{unique_suffix:x}")
        }
    }
}

/// Keep only characters AWS allows in a job name (`0-9a-zA-Z._-`); replace the rest with `-`.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_disallowed_chars() {
        assert_eq!(sanitize("20260530-135756-slack"), "20260530-135756-slack");
        assert_eq!(sanitize("a b/c:d"), "a-b-c-d");
    }

    #[test]
    fn options_new_sets_bucket_and_defaults() {
        let o = AwsOptions::new("my-bucket");
        assert_eq!(o.bucket, "my-bucket");
        assert_eq!(o.key_prefix, "corti/");
        assert_eq!(o.language, "en-US");
        assert!(o.delete_after);
        // No stable job name by default — one-shot transcribes mint a unique name per attempt.
        assert!(o.job_name.is_none());
    }

    #[test]
    fn resolve_job_name_prefers_supplied_stable_name() {
        // A supplied recording id is used verbatim (sanitized), ignoring the stem + suffix, so a re-poll
        // re-attaches to the same AWS job and output key.
        assert_eq!(
            resolve_job_name(Some("20260530-135756-slack"), Some("ignored"), 0xdead_beef),
            "20260530-135756-slack"
        );
        assert_eq!(resolve_job_name(Some("a b/c:d"), None, 1), "a-b-c-d");
    }

    #[test]
    fn resolve_job_name_falls_back_to_unique_per_attempt() {
        assert_eq!(resolve_job_name(None, Some("rec"), 0x1f), "rec-1f");
        assert_eq!(resolve_job_name(None, Some("a b"), 0x10), "a-b-10");
        assert_eq!(resolve_job_name(None, None, 0xab), "corti-job-ab");
    }
}
