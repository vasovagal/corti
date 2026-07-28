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
    /// Delete the staged `.wav` and `.json` from S3. Stable failed jobs retain both for reattachment;
    /// unique one-shot jobs attempt cleanup on every outcome because their name cannot be rediscovered.
    /// The app's durable pipeline sets this to `false` and calls [`AwsTranscriber::cleanup_staged`] only
    /// after its local transcript checkpoint is durable.
    pub delete_after: bool,
    /// How often to poll job status.
    pub poll_interval: Duration,
    /// Give up (and error) if the job hasn't finished within this long.
    pub max_wait: Duration,
    /// Stable Transcribe job name (normally the caller's recording id). When set, the AWS job and staged S3
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StableJobState {
    Missing,
    Reusable,
    Failed,
}

fn classify_existing_job(status: Option<&TranscriptionJobStatus>) -> StableJobState {
    if status == Some(&TranscriptionJobStatus::Failed) {
        StableJobState::Failed
    } else {
        // Queued, in-progress, completed, absent status, and future status variants all name a real job.
        StableJobState::Reusable
    }
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

    /// Delete the stable job's staged input/output after a caller has durably persisted the transcript.
    /// Both deletes are attempted; any failure is returned after the other object has also been tried.
    pub fn cleanup_staged(&self, job_name: &str) -> Result<()> {
        if self.opts.bucket.is_empty() {
            bail!("AwsOptions.bucket is empty — set the S3 bucket to clean staged audio");
        }
        let job = sanitize(job_name);
        let (in_key, out_key) = object_keys(&self.opts.key_prefix, &job);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("building tokio runtime for AWS cleanup")?;
        rt.block_on(self.cleanup_objects(&in_key, &out_key))
    }

    /// The full async pipeline: attach-or-stage → poll → fetch → parse → clean up.
    async fn run(&self, audio: &Path) -> Result<DiarizedTranscript> {
        if self.opts.bucket.is_empty() {
            bail!("AwsOptions.bucket is empty — set the S3 bucket to stage audio in");
        }

        // A caller-supplied `job_name` makes the job and staged keys stable. Resolve it before touching the
        // full WAV so retries can probe and reattach without first re-encoding/re-uploading the whole call.
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let stem = audio.file_stem().map(|s| s.to_string_lossy().into_owned());
        let job = resolve_job_name(self.opts.job_name.as_deref(), stem.as_deref(), suffix);
        let (in_key, out_key) = object_keys(&self.opts.key_prefix, &job);

        let result = async {
            let mut state = if self.opts.job_name.is_some() {
                self.probe_stable_job(&job).await?
            } else {
                StableJobState::Missing
            };
            if state == StableJobState::Failed {
                // A terminally failed name can never become reusable. Delete it now and submit the same stable
                // name afresh in this attempt rather than spending one retry merely discovering the failure.
                self.delete_terminal_job(&job).await?;
                state = StableJobState::Missing;
            }

            let multichannel = if state == StableJobState::Reusable {
                let (_, channels) = wav::layout(audio)?;
                tracing::info!(
                    target: "corti::transcribe::aws",
                    job,
                    "transcription job already exists — re-attaching without upload"
                );
                channels >= 2
            } else {
                // AWS accepts 16-bit PCM. Only a genuinely missing job pays the full decode/re-encode/upload
                // cost; retries of queued/in-progress/completed jobs read the small WAV header above.
                let (pcm_path, sample_rate, channels) = wav::to_pcm16_temp(audio)?;
                let upload = self.upload(&pcm_path, &in_key).await;
                let _ = std::fs::remove_file(&pcm_path);
                upload?;
                let multichannel = channels >= 2;
                self.start_job(&job, &in_key, &out_key, sample_rate, multichannel)
                    .await?;
                multichannel
            };

            // Poll, fetch, parse (channel-identified vs. single-speaker per `multichannel`).
            self.await_result(&job, &out_key, multichannel).await
        }
        .await;

        // Stable jobs retain failed staging for reattachment. A fresh job has no durable owner or way to
        // rediscover its unique name, so every outcome after keys are minted attempts privacy cleanup.
        if should_cleanup_staged(
            self.opts.delete_after,
            self.opts.job_name.is_some(),
            result.is_ok(),
        ) && let Err(e) = self.cleanup_objects(&in_key, &out_key).await
        {
            tracing::warn!(
                target: "corti::transcribe::aws",
                job,
                error = %format!("{e:#}"),
                "failed to clean up staged S3 objects"
            );
        }

        result
    }

    async fn probe_stable_job(&self, job: &str) -> Result<StableJobState> {
        let response = self
            .transcribe
            .get_transcription_job()
            .transcription_job_name(job)
            .send()
            .await;
        match response {
            Ok(response) => Ok(classify_existing_job(
                response
                    .transcription_job()
                    .and_then(|job| job.transcription_job_status()),
            )),
            Err(err)
                if err
                    .as_service_error()
                    .is_some_and(|service| service.is_not_found_exception()) =>
            {
                Ok(StableJobState::Missing)
            }
            Err(err) => Err(err).context("probing stable Transcribe job"),
        }
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
                    // Only a stable durable name must be freed for retry. A fresh CLI/example attempt will
                    // mint another name and must not acquire an unnecessary DeleteTranscriptionJob grant.
                    if should_reset_terminal_job(self.opts.job_name.as_deref()) {
                        self.delete_terminal_job(job).await.with_context(|| {
                            format!(
                                "deleting terminal AWS job {job} before retry (failure: {reason})"
                            )
                        })?;
                    }
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

        // Completed: fetch the JSON we directed to our own bucket/key and parse it. A transient fetch
        // failure leaves the completed job/output untouched for cheap reattachment. A modeled NoSuchKey is
        // different: the output is confirmed unavailable (possible for pre-checkpoint legacy jobs), so
        // remove only the terminal job and let the next retry submit the same stable name afresh.
        let fetched = self
            .s3
            .get_object()
            .bucket(&self.opts.bucket)
            .key(out_key)
            .send()
            .await;
        let obj = match fetched {
            Ok(obj) => obj,
            Err(err)
                if err
                    .as_service_error()
                    .is_some_and(|service| service.is_no_such_key()) =>
            {
                if should_reset_terminal_job(self.opts.job_name.as_deref()) {
                    self.delete_terminal_job(job).await.with_context(|| {
                        format!("deleting completed AWS job {job} whose output is missing")
                    })?;
                }
                bail!(
                    "completed transcription output s3://{}/{out_key} is missing",
                    self.opts.bucket
                );
            }
            Err(err) => {
                tracing::error!(target: "corti::transcribe::aws", bucket = %self.opts.bucket, key = out_key, error = %err, "fetching transcript failed");
                return Err(err).with_context(|| {
                    format!("fetching transcript s3://{}/{out_key}", self.opts.bucket)
                });
            }
        };
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

    async fn delete_terminal_job(&self, job: &str) -> Result<()> {
        self.transcribe
            .delete_transcription_job()
            .transcription_job_name(job)
            .send()
            .await
            .context("DeleteTranscriptionJob failed")?;
        tracing::info!(
            target: "corti::transcribe::aws",
            job,
            "deleted terminal transcription job so retry can start fresh"
        );
        Ok(())
    }

    async fn cleanup_objects(&self, in_key: &str, out_key: &str) -> Result<()> {
        let input = self.delete(in_key).await;
        let output = self.delete(out_key).await;
        match (input, output) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(a), Ok(())) => Err(a),
            (Ok(()), Err(b)) => Err(b),
            (Err(a), Err(b)) => {
                anyhow::bail!("input cleanup failed: {a:#}; output cleanup failed: {b:#}")
            }
        }
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.s3
            .delete_object()
            .bucket(&self.opts.bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("deleting s3://{}/{key}", self.opts.bucket))?;
        tracing::info!(
            target: "corti::transcribe::aws",
            bucket = %self.opts.bucket,
            key,
            "deleted staged S3 object"
        );
        Ok(())
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
/// (normally the durable recording id) is used verbatim — sanitized — so a resumed job re-attaches to the same AWS
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

fn object_keys(prefix: &str, job: &str) -> (String, String) {
    (format!("{prefix}{job}.wav"), format!("{prefix}{job}.json"))
}

fn should_cleanup_staged(
    delete_after: bool,
    stable_job_name: bool,
    result_succeeded: bool,
) -> bool {
    delete_after && (result_succeeded || !stable_job_name)
}

fn should_reset_terminal_job(stable_job_name: Option<&str>) -> bool {
    stable_job_name.is_some()
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

    #[test]
    fn stable_job_probe_skips_staging_unless_missing_or_failed() {
        assert_eq!(
            classify_existing_job(Some(&TranscriptionJobStatus::Completed)),
            StableJobState::Reusable
        );
        assert_eq!(
            classify_existing_job(Some(&TranscriptionJobStatus::InProgress)),
            StableJobState::Reusable
        );
        assert_eq!(
            classify_existing_job(Some(&TranscriptionJobStatus::Failed)),
            StableJobState::Failed
        );
        assert_eq!(classify_existing_job(None), StableJobState::Reusable);
    }

    #[test]
    fn cleanup_policy_retains_only_failed_stable_attempts() {
        assert!(!should_cleanup_staged(true, true, false));
        assert!(should_cleanup_staged(true, false, false));
        assert!(!should_cleanup_staged(false, false, false));
        assert!(!should_cleanup_staged(false, false, true));
        assert!(should_cleanup_staged(true, true, true));
        assert!(should_cleanup_staged(true, false, true));
    }

    #[test]
    fn only_stable_attempts_delete_terminal_jobs_for_reuse() {
        assert!(should_reset_terminal_job(Some("recording")));
        assert!(!should_reset_terminal_job(None));
    }

    #[test]
    fn stable_job_names_address_stable_cleanup_objects() {
        assert_eq!(
            object_keys("corti/", "recording-1"),
            (
                "corti/recording-1.wav".to_string(),
                "corti/recording-1.json".to_string()
            )
        );
    }
}
