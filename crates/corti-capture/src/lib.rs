//! Recording orchestration: drive the [`corti_coreaudio`] capture engine for the lifetime of a meeting and
//! write the result as a **lossless 2-track WAV** — channel 0 = the mic ("me"), channel 1 = the far-end tap
//! ("them"), 32-bit float — into the recordings cache (`~/Library/Caches/corti/recordings/`, never a vault;
//! guardrail 5).
//!
//! Keeping the two ends as separate tracks is what gives downstream code free "me vs. them" diarization and
//! lets [`corti-aec`](../corti_aec) cancel speaker bleed from time-aligned signals. Under ADR 0007
//! (streaming-AEC-first) the mic is echo-cancelled against the mono tap; since #74 that happens **in the
//! capture writer thread**, block by block, so normal output is already clean; filter failure is durably
//! marked and fails open rather than losing the call. [`write_clean_wav`] remains the file-to-file pass for foreign audio, legacy rows, and a
//! wholly-raw writer setup fallback (`corti --input`, `corti-bench`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Re-exported so callers configuring an in-flight capture filter depend only on `corti-capture`.
pub use corti_aec::AecConfig;

pub const CAPTURE_PROCESSING_SCHEMA_VERSION: u32 = 1;
/// Maximum production FDAF hop accepted from app config or a durable fallback record.
pub const MAX_CAPTURE_AEC_FILTER_LEN: usize = 32 * 1024;

/// Durable description of how the retained recording's mic track was produced. The app stores this as JSON
/// on the queue row, making retries and upgrades distinguishable without changing WAV names or contents.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CaptureProcessing {
    pub schema_version: u32,
    pub aec: CaptureAecState,
}

/// AEC disposition plus the exact immutable settings captured when recording started.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CaptureAecState {
    /// A two-track recording intentionally made with AEC disabled.
    Disabled,
    /// A tap-only recording has no mic track to clean.
    NotApplicable,
    /// Every retained mic frame is filter output.
    Applied {
        config: AecConfig,
        lookahead_seconds: f32,
    },
    /// Filter setup/processing failed before any clean frame was written; the file is wholly raw and a
    /// whole-file fallback remains safe.
    RawFallback {
        config: AecConfig,
        lookahead_seconds: f32,
    },
    /// Some filter output was already written before fail-open. A second whole-file pass would risk
    /// double-processing the prefix, so retain and report the mixed disposition.
    Degraded {
        config: AecConfig,
        lookahead_seconds: f32,
    },
}

impl CaptureProcessing {
    pub fn disabled() -> Self {
        Self {
            schema_version: CAPTURE_PROCESSING_SCHEMA_VERSION,
            aec: CaptureAecState::Disabled,
        }
    }

    pub fn not_applicable() -> Self {
        Self {
            schema_version: CAPTURE_PROCESSING_SCHEMA_VERSION,
            aec: CaptureAecState::NotApplicable,
        }
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.schema_version == CAPTURE_PROCESSING_SCHEMA_VERSION,
            "unsupported capture-processing schema version {}",
            self.schema_version
        );
        let settings = match &self.aec {
            CaptureAecState::Applied {
                config,
                lookahead_seconds,
            }
            | CaptureAecState::RawFallback {
                config,
                lookahead_seconds,
            }
            | CaptureAecState::Degraded {
                config,
                lookahead_seconds,
            } => Some((config, *lookahead_seconds)),
            CaptureAecState::Disabled | CaptureAecState::NotApplicable => None,
        };
        if let Some((config, lookahead_seconds)) = settings {
            anyhow::ensure!(
                (1..=MAX_CAPTURE_AEC_FILTER_LEN).contains(&config.filter_len),
                "capture AEC filter_len is outside the production bound"
            );
            anyhow::ensure!(
                lookahead_seconds.is_finite() && (0.0..=30.0).contains(&lookahead_seconds),
                "capture AEC lookahead is invalid"
            );
            anyhow::ensure!(
                [
                    config.mu,
                    config.eps,
                    config.power_smoothing,
                    config.double_talk_ratio,
                    config.suppress_residual,
                    config.max_lag_ms,
                ]
                .into_iter()
                .all(f32::is_finite),
                "capture AEC config contains a non-finite value"
            );
        }
        Ok(())
    }
}

fn bounded_capture_aec_config(mut config: AecConfig) -> AecConfig {
    let defaults = AecConfig::default();
    config.filter_len = config.filter_len.clamp(1, MAX_CAPTURE_AEC_FILTER_LEN);
    if !config.mu.is_finite() {
        config.mu = defaults.mu;
    }
    if !config.eps.is_finite() {
        config.eps = defaults.eps;
    }
    if !config.power_smoothing.is_finite() {
        config.power_smoothing = defaults.power_smoothing;
    }
    if !config.double_talk_ratio.is_finite() {
        config.double_talk_ratio = defaults.double_talk_ratio;
    }
    if !config.suppress_residual.is_finite() {
        config.suppress_residual = defaults.suppress_residual;
    }
    if !config.max_lag_ms.is_finite() {
        config.max_lag_ms = defaults.max_lag_ms;
    }
    config
}

/// A finalized recording plus the processing identity the queue must persist before transcription.
#[derive(Debug, Clone, PartialEq)]
pub struct FinishedRecording {
    pub path: PathBuf,
    pub processing: CaptureProcessing,
}

/// Whole-file AEC adapter push size (frames). The WAV is decoded in memory here, but bounded pushes keep
/// `StreamingAec`'s steady-state staging independent of call length (#97).
const AEC_PUSH_FRAMES: usize = 16 * 1024;

/// Drive `StreamingAec` over the mic/tap blocks a capture writer thread hands it (#74).
///
/// This is the in-flight half of ADR 0007: identical DSP to [`write_clean_wav`], just fed from the ring
/// drain instead of a finished file. Its memory is the filter's own state plus one block — nothing here is
/// sized by call length.
pub struct StreamingAecFilter {
    aec: corti_aec::StreamingAec,
    /// Where the per-block echo record goes, when anyone asked for it. `None` is the pre-#149 filter: the
    /// canceller still fills its own bounded ring and simply drops the overflow.
    stats: Option<AecStatsCollector>,
}

impl StreamingAecFilter {
    pub fn new(sample_rate: u32, cfg: AecConfig) -> Self {
        Self {
            aec: corti_aec::StreamingAec::new(sample_rate, bounded_capture_aec_config(cfg)),
            stats: None,
        }
    }

    pub fn new_with_lookahead(sample_rate: u32, cfg: AecConfig, lookahead_seconds: f32) -> Self {
        Self::new_with_lookahead_and_stats(sample_rate, cfg, lookahead_seconds, None)
    }

    /// [`new_with_lookahead`](Self::new_with_lookahead) that also drains the canceller's per-block record
    /// into `stats` (#149 phase 3b). The DSP is untouched: draining a ring cannot change a sample.
    pub fn new_with_lookahead_and_stats(
        sample_rate: u32,
        cfg: AecConfig,
        lookahead_seconds: f32,
        stats: Option<AecStatsCollector>,
    ) -> Self {
        Self {
            aec: corti_aec::StreamingAec::new_with_lookahead_seconds(
                sample_rate,
                bounded_capture_aec_config(cfg),
                lookahead_seconds,
            ),
            stats,
        }
    }
}

#[cfg(target_os = "macos")]
impl corti_coreaudio::CaptureFilter for StreamingAecFilter {
    fn max_output_lag_samples(&self) -> usize {
        self.aec.max_output_lag_samples()
    }

    fn push(&mut self, mic: &[f32], far: &[f32]) -> Vec<f32> {
        let out = self.aec.push(mic, far);
        if self.stats.is_some() {
            // Drain per push so a call longer than the canceller's own ring loses nothing here.
            let blocks = self.aec.block_stats();
            if let Some(collector) = &self.stats {
                collector.record(blocks);
            }
        }
        out
    }

    fn finish(self: Box<Self>) -> Vec<f32> {
        let Self { aec, stats } = *self;
        match stats {
            Some(collector) => {
                // `finish` consumes the canceller, so this is the only way to reach the blocks the flush
                // itself records — and the locked delay.
                let fin = aec.finish_with_stats();
                collector.finish(fin.stats, fin.stats_dropped, fin.delay_samples);
                fin.audio
            }
            None => aec.finish(),
        }
    }
}

/// Where recordings are cached. Outside any vault, prunable. Override with `$CORTI_RECORDINGS_DIR`.
pub fn recordings_dir() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("CORTI_RECORDINGS_DIR") {
        return Ok(PathBuf::from(d));
    }
    let cache = dirs::cache_dir().context("cannot resolve cache dir")?;
    Ok(cache.join("corti/recordings"))
}

/// A filename stem for a recording starting now, owned by `app`: `YYYYMMDD-HHMMSS-<app-slug>`.
pub fn recording_stem(app: &corti_core::OwningApp, now: chrono::DateTime<chrono::Local>) -> String {
    let slug: String = app
        .name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    let slug = if slug.is_empty() { "app".into() } else { slug };
    format!("{}-{slug}", now.format("%Y%m%d-%H%M%S"))
}

/// The cleaned-recording sibling of a raw recording: `<dir>/<stem>.wav` → `<dir>/<stem>-clean.wav`.
/// Shared by [`write_clean_wav`] and the pipeline's prune step so both agree on the path.
pub fn clean_wav_path(raw: &Path) -> PathBuf {
    let stem = raw
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "recording".to_string());
    raw.with_file_name(format!("{stem}-clean.wav"))
}

/// Schema version of the `-aec-stats.json` sidecar. Bump on any incompatible field change.
pub const AEC_STATS_SCHEMA_VERSION: u32 = 1;

/// Whether the file-to-file AEC pass also writes its per-block diagnostic sidecar. Opt-in: `corti-bench`
/// and the app's offline fallback turn it on; `write_clean_wav`/`write_clean_wav_with_lookahead` do not.
/// The app's ordinary in-flight path has its own seam — [`AecStatsCollector`], drained at
/// [`Recorder::stop_capture`] — and writes the same file to the same place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AecStatsSidecar {
    /// Do not write `-aec-stats.json`.
    Off,
    /// Write `-aec-stats.json` next to the `-clean.wav`.
    Write,
}

/// The `-aec-stats.json` sidecar: everything needed to interpret one file-to-file AEC pass without the
/// audio — the settings, the delay the filter locked, and the per-block echo record (#107 diagnostics).
///
/// `blocks[k].t_start_secs` is on the **cleaned** timeline, which for this pass is also the raw input's
/// timeline: the pass emits one sample per input sample, so a time in this file indexes `-clean.wav`, the
/// raw 2-track input, and the transcript alike. See [`corti_aec::BlockStats`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AecStatsFile {
    pub schema_version: u32,
    /// File name of the raw 2-track input this pass read.
    pub source: String,
    pub sample_rate: u32,
    /// Mic frames in the input (== frames in `-clean.wav`).
    pub frames: usize,
    pub lookahead_seconds: f32,
    pub config: AecConfig,
    /// Acoustic mic↔far delay locked at the end of warm-up.
    pub delay_samples: usize,
    pub delay_ms: f32,
    /// Block hop = `config.filter_len`; `blocks[k]` covers `[t_start_secs, t_start_secs + block_hop_secs)`.
    pub block_hop_samples: usize,
    pub block_hop_secs: f64,
    /// Blocks the bounded ring dropped before the pass could drain them (0 for a healthy run).
    pub stats_dropped: u64,
    pub blocks: Vec<corti_aec::BlockStats>,
}

/// Per-block records the in-flight collector retains before evicting the oldest. 32 768 blocks is ≈93
/// minutes at the default 8192-tap hop / 48 kHz and ≈1 MiB — a constant, so the writer thread stays free of
/// anything sized by call length. Overflow is counted into the sidecar's `stats_dropped`, and the hole is
/// always at the *old* end, where the cleanup pass falls back to its text rules.
pub const MAX_CAPTURE_BLOCK_STATS: usize = 32 * 1024;

/// Bounded hand-off for the writer-thread filter's per-block echo record (#149 phase 3b).
///
/// [`CaptureFilter`] returns only audio and the writer thread should not do file I/O it can avoid, so the
/// filter records here and the thread that *stops* the capture — the one that learns from
/// [`CaptureFilterDisposition`] whether the filter's output actually reached the WAV — decides whether the
/// record describes the retained audio and is worth persisting.
#[derive(Clone, Default)]
pub struct AecStatsCollector {
    inner: std::sync::Arc<std::sync::Mutex<CollectedAecStats>>,
}

/// What the collector holds. `delay_samples` is only known once the filter finishes.
#[derive(Default)]
struct CollectedAecStats {
    blocks: Vec<corti_aec::BlockStats>,
    dropped: u64,
    delay_samples: usize,
    finished: bool,
}

impl AecStatsCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock, tolerating a poisoned mutex: a panicking writer thread must not also cost the diagnostics of
    /// the *next* call, and nothing here can leave the data structurally invalid.
    fn lock(&self) -> std::sync::MutexGuard<'_, CollectedAecStats> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Append one `block_stats()` drain, evicting the oldest past [`MAX_CAPTURE_BLOCK_STATS`].
    fn record(&self, blocks: Vec<corti_aec::BlockStats>) {
        let mut guard = self.lock();
        guard.blocks.extend(blocks);
        if guard.blocks.len() > MAX_CAPTURE_BLOCK_STATS {
            let excess = guard.blocks.len() - MAX_CAPTURE_BLOCK_STATS;
            guard.blocks.drain(..excess);
            guard.dropped = guard.dropped.saturating_add(excess as u64);
        }
    }

    /// The filter's last word: its trailing blocks, its own ring's drop count, and the locked delay.
    fn finish(&self, blocks: Vec<corti_aec::BlockStats>, dropped: u64, delay_samples: usize) {
        self.record(blocks);
        let mut guard = self.lock();
        guard.dropped = guard.dropped.saturating_add(dropped);
        guard.delay_samples = delay_samples;
        guard.finished = true;
    }

    /// Take the record, leaving the collector empty. `None` until the filter has finished — a filter that
    /// never ran, or that panicked before its flush, has nothing trustworthy to say.
    fn take(&self) -> Option<(Vec<corti_aec::BlockStats>, u64, usize)> {
        let mut guard = self.lock();
        if !guard.finished {
            return None;
        }
        Some((
            std::mem::take(&mut guard.blocks),
            guard.dropped,
            guard.delay_samples,
        ))
    }
}

impl std::fmt::Debug for AecStatsCollector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let guard = self.lock();
        f.debug_struct("AecStatsCollector")
            .field("blocks", &guard.blocks.len())
            .field("dropped", &guard.dropped)
            .field("finished", &guard.finished)
            .finish()
    }
}

/// Serialize one [`AecStatsFile`] to [`aec_stats_path`] and log it. The single writer for both producers:
/// the file-to-file pass and the capture writer's in-flight filter.
fn persist_aec_stats(raw_2track_wav: &Path, record: &AecStatsFile) -> Result<PathBuf> {
    let stats_path = aec_stats_path(raw_2track_wav);
    let json = serde_json::to_vec(record).context("serializing AEC block statistics")?;
    std::fs::write(&stats_path, &json)
        .with_context(|| format!("writing {}", stats_path.display()))?;
    tracing::info!(
        target: "corti::capture",
        path = %stats_path.display(),
        blocks = record.blocks.len(),
        dropped = record.stats_dropped,
        delay_samples = record.delay_samples as u64,
        "wrote AEC block statistics"
    );
    Ok(stats_path)
}

/// The AEC statistics sidecar of a raw recording: `<dir>/<stem>.wav` → `<dir>/<stem>-aec-stats.json`.
/// Sibling of [`clean_wav_path`], written only when the pass is asked for [`AecStatsSidecar::Write`].
pub fn aec_stats_path(raw: &Path) -> PathBuf {
    let stem = raw
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "recording".to_string());
    raw.with_file_name(format!("{stem}-aec-stats.json"))
}

/// The **file-to-file** AEC pass: read a 2-track WAV, cancel speaker bleed on ch0 (mic) using ch1 (mono
/// far-end tap) as the echo reference, and write the cleaned 2-track sibling (ch0 = cleaned mic, ch1 = mono
/// tap "them"). Returns the clean path wrapped in `Some`.
///
/// Since #74 ordinary app recordings are cleaned in the capture writer. This path remains for foreign
/// audio, marker-less pre-upgrade rows, a wholly-raw writer setup fallback, and `corti-bench` scoring. It
/// decodes the whole file into memory, so it is bounded by the input, not the capture path.
///
/// Returns `Ok(None)` for a **1-channel** input: a tap-only / listen-only ("webinar") recording has no mic
/// track, so there is nothing to cancel — the caller transcribes the tap directly. This is an expected
/// outcome, not an error. Any other channel count (0, or 3+) is a genuine error.
///
/// The input file is only read here, not modified.
pub fn write_clean_wav(
    raw_2track_wav: &Path,
    aec_cfg: &corti_aec::AecConfig,
) -> Result<Option<PathBuf>> {
    write_clean_wav_with_lookahead(
        raw_2track_wav,
        aec_cfg,
        corti_aec::configured_lookahead_seconds(),
    )
}

/// [`write_clean_wav`] with an immutable lookahead captured earlier (for a writer-construction fallback or
/// durable retry). This prevents a later environment change from altering the recovery pass.
pub fn write_clean_wav_with_lookahead(
    raw_2track_wav: &Path,
    aec_cfg: &corti_aec::AecConfig,
    lookahead_seconds: f32,
) -> Result<Option<PathBuf>> {
    write_clean_wav_with_options(
        raw_2track_wav,
        aec_cfg,
        lookahead_seconds,
        AecStatsSidecar::Off,
    )
}

/// [`write_clean_wav_with_lookahead`] that can also emit the [`AecStatsFile`] sidecar at
/// [`aec_stats_path`] (#107 diagnostics; `corti-bench` passes [`AecStatsSidecar::Write`]).
///
/// The sidecar is a pure by-product: the cleaned WAV is byte-identical either way. With the sidecar off
/// the canceller still records into its bounded ring and simply drops the overflow — a fixed ≈128 KiB,
/// never sized by call length.
pub fn write_clean_wav_with_options(
    raw_2track_wav: &Path,
    aec_cfg: &corti_aec::AecConfig,
    lookahead_seconds: f32,
    sidecar: AecStatsSidecar,
) -> Result<Option<PathBuf>> {
    let mut reader = hound::WavReader::open(raw_2track_wav)
        .with_context(|| format!("opening {} for AEC", raw_2track_wav.display()))?;
    let spec = reader.spec();
    match spec.channels {
        // Tap-only / listen-only ("webinar") recording: no mic track, nothing to cancel. Only the header
        // has been read at this point (never the samples), so the raw file is left untouched.
        1 => return Ok(None),
        2 => {}
        n => anyhow::bail!("expected a 1- or 2-channel recording for AEC, got {n} channel(s)"),
    }

    // Read interleaved samples as f32, tolerating Float and Int (mirrors corti-transcribe-aws::wav).
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<Result<_, _>>()
            .context("reading float samples")?,
        hound::SampleFormat::Int => {
            let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / max))
                .collect::<Result<_, _>>()
                .context("reading int samples")?
        }
    };
    let mic: Vec<f32> = samples.iter().step_by(2).copied().collect();
    let tap: Vec<f32> = samples.iter().skip(1).step_by(2).copied().collect();

    // Drive the streaming canceller with the env-tunable lookahead (ADR 0007 §1): the lookahead window
    // warms the filter and locks the mic↔far delay before the opening is emitted. This is the live path —
    // `cancel()` (full-length lookahead) is only the offline test/scoring shim.
    let mut aec = corti_aec::StreamingAec::new_with_lookahead_seconds(
        spec.sample_rate,
        aec_cfg.clone(),
        lookahead_seconds,
    );
    let mut clean = Vec::with_capacity(mic.len());
    let mut blocks: Vec<corti_aec::BlockStats> = Vec::new();
    let want_stats = sidecar == AecStatsSidecar::Write;
    for start in (0..mic.len()).step_by(AEC_PUSH_FRAMES) {
        let end = (start + AEC_PUSH_FRAMES).min(mic.len());
        clean.extend(aec.push(&mic[start..end], &tap[start..end]));
        // Drain per push so a call longer than the ring cannot lose blocks off the old end.
        if want_stats {
            blocks.extend(aec.block_stats());
        }
    }
    let fin = aec.finish_with_stats();
    clean.extend(fin.audio);
    clean.truncate(mic.len());
    if want_stats {
        blocks.extend(fin.stats);
    }

    let out = clean_wav_path(raw_2track_wav);
    let out_spec = hound::WavSpec {
        channels: 2,
        sample_rate: spec.sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(&out, out_spec)
        .with_context(|| format!("creating {}", out.display()))?;
    let frames = mic.len().max(tap.len());
    for i in 0..frames {
        w.write_sample(clean.get(i).copied().unwrap_or(0.0))?; // ch0 = cleaned mic ("me")
        w.write_sample(tap.get(i).copied().unwrap_or(0.0))?; // ch1 = mono tap ("them")
    }
    w.finalize()?;
    let bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    tracing::info!(
        target: "corti::capture",
        path = %out.display(),
        bytes,
        frames,
        "wrote AEC-cleaned WAV"
    );

    if want_stats {
        let hop = aec_cfg.filter_len.max(1);
        let sr = spec.sample_rate.max(1);
        let record = AecStatsFile {
            schema_version: AEC_STATS_SCHEMA_VERSION,
            source: raw_2track_wav
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            sample_rate: spec.sample_rate,
            frames: mic.len(),
            lookahead_seconds,
            config: aec_cfg.clone(),
            delay_samples: fin.delay_samples,
            delay_ms: fin.delay_samples as f32 * 1000.0 / sr as f32,
            block_hop_samples: hop,
            block_hop_secs: hop as f64 / sr as f64,
            stats_dropped: fin.stats_dropped,
            blocks,
        };
        persist_aec_stats(raw_2track_wav, &record)?;
    }

    Ok(Some(out))
}

#[cfg(target_os = "macos")]
pub use platform::{Recorder, RecordingOptions};

/// Bounded, lossy live-tee of the downmixed capture stream (ADR 0009), plus the writer-thread filter seam
/// (#74); re-exported so callers depend only on `corti-capture`. See [`Recorder::start_with_tee`] and
/// [`RecordingOptions::with_aec`].
#[cfg(target_os = "macos")]
pub use corti_coreaudio::{
    CaptureChunk, CaptureFilter, CaptureTee, FILTER_FRAMES_PER_CHUNK, MicrophoneCapture,
    MicrophoneCaptureHandle,
};

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use corti_coreaudio::{
        CaptureFilter, CaptureFilterDisposition, CaptureOptions, CaptureSession, CaptureTee,
        OutputLayout, TapTarget,
    };

    #[derive(Debug, Clone, PartialEq)]
    struct RequestedAec {
        config: AecConfig,
        lookahead_seconds: f32,
    }

    /// Per-recording capture options: an optional live tee (ADR 0009) and optional in-flight AEC (#74).
    /// Both default to off, which is the pre-#74 behaviour byte for byte.
    #[derive(Default)]
    pub struct RecordingOptions {
        tee: Option<CaptureTee>,
        aec: Option<RequestedAec>,
    }

    impl RecordingOptions {
        /// Tee bounded, downmixed **raw** live chunks to `tee`. Never blocks capture, never affects the WAV.
        pub fn with_tee(mut self, tee: CaptureTee) -> Self {
            self.tee = Some(tee);
            self
        }

        /// Echo-cancel the mic in the writer thread, so the recording written is already clean.
        /// Ignored for a tap-only capture, which has no mic.
        pub fn with_aec(mut self, cfg: AecConfig) -> Self {
            self.aec = Some(RequestedAec {
                config: bounded_capture_aec_config(cfg),
                lookahead_seconds: corti_aec::configured_lookahead_seconds(),
            });
            self
        }

        /// Lower to the engine's options while retaining the immutable request for the durable completion
        /// record. `keep_aec = false` for layouts with no mic track. The third return is the drop-box the
        /// in-flight filter fills with its per-block echo record (#149 phase 3b), `None` when no filter runs.
        fn into_capture_options(
            self,
            keep_aec: bool,
        ) -> (
            CaptureOptions,
            Option<RequestedAec>,
            Option<AecStatsCollector>,
        ) {
            let mut out = CaptureOptions::default();
            if let Some(tee) = self.tee {
                out = out.with_tee(tee);
            }
            let requested = self.aec.filter(|_| keep_aec);
            let mut collector = None;
            if let Some(aec) = requested.clone() {
                let stats = AecStatsCollector::new();
                collector = Some(stats.clone());
                out = out.with_filter(Box::new(move |sample_rate| {
                    Box::new(StreamingAecFilter::new_with_lookahead_and_stats(
                        sample_rate,
                        aec.config,
                        aec.lookahead_seconds,
                        Some(stats),
                    )) as Box<dyn CaptureFilter>
                }));
            }
            (out, requested, collector)
        }
    }

    /// A fresh recording path in the recordings cache for `app`, creating the cache dir.
    fn new_recording_path(app: &corti_core::OwningApp) -> Result<PathBuf> {
        let dir = recordings_dir()?;
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let stem = recording_stem(app, chrono::Local::now());
        Ok(dir.join(format!("{stem}.wav")))
    }

    fn tap_target(pid: Option<i32>) -> TapTarget {
        match pid {
            Some(pid) => TapTarget::Process(pid),
            None => TapTarget::Global,
        }
    }

    /// An in-progress recording. Built from the owning app's PID (per-app tap, falling back to a global tap
    /// inside the engine if the PID can't be resolved). The session streams the chosen layout straight to
    /// disk; call [`finish`] to stop and finalize, or [`discard`] to stop and delete the partial file.
    ///
    /// [`finish`]: Recorder::finish
    /// [`discard`]: Recorder::discard
    pub struct Recorder {
        session: CaptureSession,
        out: PathBuf,
        requested_aec: Option<Box<RequestedAec>>,
        tap_only: bool,
        /// Filled by the writer-thread AEC filter; drained at stop into the `-aec-stats.json` sidecar.
        aec_stats: Option<AecStatsCollector>,
    }

    impl Recorder {
        /// Start recording the given app (`pid = None` ⇒ global tap of all system audio) to a fresh file in
        /// the recordings cache. Returns the recorder and the output path.
        pub fn start(app: &corti_core::OwningApp, pid: Option<i32>) -> Result<Self> {
            Self::start_with(app, pid, RecordingOptions::default())
        }

        /// Like [`start`], additionally teeing bounded, downmixed live chunks to `tee` (ADR 0009). The tee
        /// never blocks capture and never affects the on-disk 2-track WAV.
        ///
        /// [`start`]: Recorder::start
        pub fn start_with_tee(
            app: &corti_core::OwningApp,
            pid: Option<i32>,
            tee: CaptureTee,
        ) -> Result<Self> {
            Self::start_with(app, pid, RecordingOptions::default().with_tee(tee))
        }

        /// [`start`] with explicit [`RecordingOptions`] — the form that can request in-flight AEC (#74), so
        /// the 2-track written is already cleaned and the raw mic never lands on disk.
        ///
        /// [`start`]: Recorder::start
        pub fn start_with(
            app: &corti_core::OwningApp,
            pid: Option<i32>,
            options: RecordingOptions,
        ) -> Result<Self> {
            let out = new_recording_path(app)?;
            let (capture_options, requested_aec, aec_stats) = options.into_capture_options(true);
            let session = CaptureSession::start_recording_with_options(
                tap_target(pid),
                out.clone(),
                OutputLayout::TwoTrack,
                capture_options,
            )
            .with_context(|| format!("starting capture for {}", app.name))?;
            Ok(Self {
                session,
                out,
                requested_aec: requested_aec.map(Box::new),
                tap_only: false,
                aec_stats,
            })
        }

        /// Like [`start`], but **tap-only**: the microphone is never opened (no orange "mic in use" dot, no
        /// microphone TCC prompt) — only the system-audio tap runs, streamed as a 1-channel WAV. This is the
        /// "webinar / listen-only" capture mode.
        ///
        /// [`start`]: Recorder::start
        pub fn start_tap_only(app: &corti_core::OwningApp, pid: Option<i32>) -> Result<Self> {
            Self::start_tap_only_with(app, pid, RecordingOptions::default())
        }

        /// Like [`start_tap_only`], additionally teeing bounded, downmixed live chunks (the `mic` side is
        /// empty) to `tee` (ADR 0009).
        ///
        /// [`start_tap_only`]: Recorder::start_tap_only
        pub fn start_tap_only_with_tee(
            app: &corti_core::OwningApp,
            pid: Option<i32>,
            tee: CaptureTee,
        ) -> Result<Self> {
            Self::start_tap_only_with(app, pid, RecordingOptions::default().with_tee(tee))
        }

        /// [`start_tap_only`] with explicit [`RecordingOptions`]. Any requested AEC is dropped here: a
        /// tap-only capture has no mic and therefore no echo to cancel.
        ///
        /// [`start_tap_only`]: Recorder::start_tap_only
        pub fn start_tap_only_with(
            app: &corti_core::OwningApp,
            pid: Option<i32>,
            options: RecordingOptions,
        ) -> Result<Self> {
            let out = new_recording_path(app)?;
            let (capture_options, _ignored_aec, _ignored_stats) =
                options.into_capture_options(false);
            let session = CaptureSession::start_tap_only_recording_with_options(
                tap_target(pid),
                out.clone(),
                OutputLayout::TapOnlyMono,
                capture_options,
            )
            .with_context(|| format!("starting tap-only capture for {}", app.name))?;
            Ok(Self {
                session,
                out,
                requested_aec: None,
                tap_only: true,
                aec_stats: None,
            })
        }

        /// The path the recording will be written to.
        pub fn output_path(&self) -> &Path {
            &self.out
        }

        /// The capture sample rate (Hz). Available immediately after `start*` so a live tee consumer can build
        /// its resampler / AEC at the right rate.
        pub fn sample_rate(&self) -> u32 {
            self.session.sample_rate()
        }

        /// Stop capture and finalize the streamed WAV (2-track for [`start`], 1-track for
        /// [`start_tap_only`] — the layout was fixed at start). Returns the written path. Errors if no audio
        /// was delivered (e.g. the TCC audio-capture permission is missing).
        ///
        /// [`start`]: Recorder::start
        /// [`start_tap_only`]: Recorder::start_tap_only
        pub fn finish(self) -> Result<PathBuf> {
            Ok(self.finish_with_processing()?.path)
        }

        /// Finalize and return the durable capture-processing identity required by the app queue.
        pub fn finish_with_processing(self) -> Result<FinishedRecording> {
            self.stop_capture()
        }

        /// Alias of [`finish`] retained for callers that paired it with [`start_tap_only`]; the on-disk
        /// layout is fixed at start, so this finalizes whatever was being streamed.
        ///
        /// [`finish`]: Recorder::finish
        /// [`start_tap_only`]: Recorder::start_tap_only
        pub fn finish_tap_only(self) -> Result<PathBuf> {
            Ok(self.finish_tap_only_with_processing()?.path)
        }

        /// Tap-only counterpart to [`finish_with_processing`](Self::finish_with_processing).
        pub fn finish_tap_only_with_processing(self) -> Result<FinishedRecording> {
            self.stop_capture()
        }

        /// Stop capture and **delete** the streamed file — for a recording the user chose not to keep. The
        /// writer has already streamed a partial WAV to disk, so unlike the old buffer-then-write model this
        /// must remove it explicitly.
        pub fn discard(self) {
            let _ = self.session.stop(); // finalize the partial file (best-effort)
            let _ = std::fs::remove_file(&self.out);
        }

        fn stop_capture(self) -> Result<FinishedRecording> {
            let Self {
                session,
                out,
                requested_aec,
                tap_only,
                aec_stats,
            } = self;
            let handle = session.stop()?;
            if handle.callbacks == 0 {
                let _ = std::fs::remove_file(&out); // no file should exist, but be tidy
                anyhow::bail!(
                    "no audio captured (IO proc never fired) — likely the macOS audio-capture permission \
                     is not granted to corti"
                );
            }
            if handle.frames == 0 {
                let _ = std::fs::remove_file(&out);
                anyhow::bail!("IO proc fired but produced no audio frames (a format/layout issue)");
            }
            if handle.dropped_samples > 0 {
                tracing::warn!(
                    target: "corti::capture",
                    dropped_samples = handle.dropped_samples,
                    path = %out.display(),
                    "dropped samples during capture (disk too slow / ring overflow) — recording may have gaps"
                );
            }
            if handle.tee_dropped_chunks > 0 {
                tracing::warn!(
                    target: "corti::capture",
                    tee_dropped_chunks = handle.tee_dropped_chunks,
                    path = %out.display(),
                    "dropped live tee chunks (consumer fell behind) — the live transcript may have gaps"
                );
            }
            // The per-block echo record is only written when the filter's output *is* the retained mic:
            // `Applied`. A raw fallback is re-cleaned by the offline pass (which writes its own sidecar),
            // and a degraded recording is a cleaned prefix plus a raw remainder — the record would describe
            // audio the WAV does not contain, which is worse than no record at all (#149 phase 3b).
            if let (Some(collector), CaptureFilterDisposition::Applied) =
                (&aec_stats, handle.filter_disposition)
                && let Some(aec) = requested_aec.as_deref()
                && let Some((blocks, dropped, delay_samples)) = collector.take()
                && !blocks.is_empty()
            {
                let sr = handle.sample_rate.max(1);
                let hop = aec.config.filter_len.max(1);
                let record = AecStatsFile {
                    schema_version: AEC_STATS_SCHEMA_VERSION,
                    source: out
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    sample_rate: handle.sample_rate,
                    frames: handle.frames as usize,
                    lookahead_seconds: aec.lookahead_seconds,
                    config: aec.config.clone(),
                    delay_samples,
                    delay_ms: delay_samples as f32 * 1000.0 / sr as f32,
                    block_hop_samples: hop,
                    block_hop_secs: hop as f64 / sr as f64,
                    stats_dropped: dropped,
                    blocks,
                };
                // Best effort: a diagnostic sidecar must never cost a finished recording.
                if let Err(e) = persist_aec_stats(&out, &record) {
                    tracing::warn!(
                        target: "corti::capture",
                        error = %format!("{e:#}"),
                        "could not write the AEC statistics sidecar; transcription falls back to text-only cleanup"
                    );
                }
            }

            let processing = if tap_only {
                CaptureProcessing::not_applicable()
            } else {
                match (requested_aec, handle.filter_disposition) {
                    (None, _) => CaptureProcessing::disabled(),
                    (Some(aec), CaptureFilterDisposition::Applied) => CaptureProcessing {
                        schema_version: CAPTURE_PROCESSING_SCHEMA_VERSION,
                        aec: CaptureAecState::Applied {
                            config: aec.config,
                            lookahead_seconds: aec.lookahead_seconds,
                        },
                    },
                    (Some(aec), CaptureFilterDisposition::RawFallback)
                    | (Some(aec), CaptureFilterDisposition::NotRequested) => CaptureProcessing {
                        schema_version: CAPTURE_PROCESSING_SCHEMA_VERSION,
                        aec: CaptureAecState::RawFallback {
                            config: aec.config,
                            lookahead_seconds: aec.lookahead_seconds,
                        },
                    },
                    (Some(aec), CaptureFilterDisposition::Degraded) => CaptureProcessing {
                        schema_version: CAPTURE_PROCESSING_SCHEMA_VERSION,
                        aec: CaptureAecState::Degraded {
                            config: aec.config,
                            lookahead_seconds: aec.lookahead_seconds,
                        },
                    },
                }
            };
            let bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
            tracing::info!(
                target: "corti::capture",
                path = %out.display(),
                bytes,
                frames = handle.frames,
                aec = ?processing.aec,
                "finalized capture WAV"
            );
            Ok(FinishedRecording {
                path: out,
                processing,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use corti_core::OwningApp;

    #[test]
    fn stem_includes_date_and_app_slug() {
        let app = OwningApp::from_bundle_id("us.zoom.xos");
        let now = chrono::Local
            .with_ymd_and_hms(2026, 5, 30, 9, 5, 7)
            .unwrap();
        assert_eq!(recording_stem(&app, now), "20260530-090507-zoom");
    }

    #[test]
    fn stem_handles_unknown_app() {
        let app = OwningApp::unknown();
        let now = chrono::Local
            .with_ymd_and_hms(2026, 5, 30, 9, 5, 7)
            .unwrap();
        // "Unknown app" → "unknown-app"
        assert_eq!(recording_stem(&app, now), "20260530-090507-unknown-app");
    }

    #[test]
    fn recordings_dir_respects_override() {
        // SAFETY: single-threaded test; we set and read our own override.
        unsafe { std::env::set_var("CORTI_RECORDINGS_DIR", "/tmp/corti-test-recordings") };
        assert_eq!(
            recordings_dir().unwrap(),
            PathBuf::from("/tmp/corti-test-recordings")
        );
        unsafe { std::env::remove_var("CORTI_RECORDINGS_DIR") };
    }

    #[test]
    fn clean_wav_path_is_a_clean_sibling() {
        assert_eq!(
            clean_wav_path(Path::new(
                "/cache/corti/recordings/20260605-140500-zoom.wav"
            )),
            PathBuf::from("/cache/corti/recordings/20260605-140500-zoom-clean.wav")
        );
    }

    #[test]
    fn aec_stats_path_is_a_json_sibling() {
        assert_eq!(
            aec_stats_path(Path::new(
                "/cache/corti/recordings/20260605-140500-zoom.wav"
            )),
            PathBuf::from("/cache/corti/recordings/20260605-140500-zoom-aec-stats.json")
        );
    }

    #[test]
    fn aec_stats_sidecar_round_trips_and_covers_the_clean_timeline() {
        let dir = std::env::temp_dir().join("corti-aec-stats-sidecar-test");
        std::fs::create_dir_all(&dir).unwrap();
        let raw = dir.join("rec.wav");

        // 8 kHz + a 256-tap filter keeps the FFTs small; the length deliberately isn't a block multiple, so
        // `finish`'s zero-padded tail block is in the record too.
        let sample_rate = 8_000u32;
        let frames = AEC_PUSH_FRAMES + 257;
        let cfg = corti_aec::AecConfig {
            filter_len: 256,
            ..Default::default()
        };
        let tap: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
        let mic: Vec<f32> = (0..frames)
            .map(|i| {
                let near = (i as f32 * 0.011).sin() * 0.05;
                let echo = if i >= 24 { tap[i - 24] * 0.6 } else { 0.0 };
                near + echo
            })
            .collect();
        {
            let mut w = hound::WavWriter::create(
                &raw,
                hound::WavSpec {
                    channels: 2,
                    sample_rate,
                    bits_per_sample: 32,
                    sample_format: hound::SampleFormat::Float,
                },
            )
            .unwrap();
            for i in 0..frames {
                w.write_sample(mic[i]).unwrap();
                w.write_sample(tap[i]).unwrap();
            }
            w.finalize().unwrap();
        }

        // Off (the default the app path takes): no sidecar, and the cleaned audio is the baseline.
        let plain = write_clean_wav_with_lookahead(&raw, &cfg, 0.5)
            .unwrap()
            .unwrap();
        assert!(!aec_stats_path(&raw).exists(), "the sidecar must be opt-in");
        let read_clean = |p: &Path| -> Vec<f32> {
            let mut r = hound::WavReader::open(p).unwrap();
            let inter: Vec<f32> = r.samples::<f32>().collect::<Result<_, _>>().unwrap();
            inter.iter().step_by(2).copied().collect()
        };
        let baseline = read_clean(&plain);

        // Write: same audio, plus the sidecar.
        let out = write_clean_wav_with_options(&raw, &cfg, 0.5, AecStatsSidecar::Write)
            .unwrap()
            .unwrap();
        assert_eq!(
            baseline,
            read_clean(&out),
            "asking for statistics must not change the cleaned audio"
        );

        let stats_path = aec_stats_path(&raw);
        let bytes = std::fs::read(&stats_path).unwrap();
        let record: AecStatsFile = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(record.schema_version, AEC_STATS_SCHEMA_VERSION);
        assert_eq!(record.source, "rec.wav");
        assert_eq!(record.sample_rate, sample_rate);
        assert_eq!(record.frames, frames);
        assert_eq!(record.lookahead_seconds, 0.5);
        assert_eq!(record.config, cfg);
        assert_eq!(record.block_hop_samples, cfg.filter_len);
        assert_eq!(
            record.stats_dropped, 0,
            "draining per push must lose nothing"
        );

        // One record per emitted block, covering the whole cleaned file from 0.0 with no lookahead gap.
        assert_eq!(record.blocks.len(), frames.div_ceil(cfg.filter_len));
        assert_eq!(record.blocks[0].t_start_secs, 0.0);
        let last = record.blocks.last().unwrap().t_start_secs;
        let total = frames as f64 / sample_rate as f64;
        assert!(
            last < total && last + record.block_hop_secs >= total,
            "last block starts at {last:.3} s but the file is {total:.3} s long"
        );

        // The echo is real, so the record must show the filter estimating and removing it somewhere.
        assert!(
            record.blocks.iter().any(|b| b.echo_estimate_energy > 0.0),
            "no block estimated any echo"
        );
        let span = corti_aec::span_stats(&record.blocks, 0.0, total).unwrap();
        assert_eq!(span.blocks, record.blocks.len());
        assert!(span.mean_error_db < span.mean_mic_db, "no cancellation");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_clean_wav_preserves_tap_and_layout() {
        let dir = std::env::temp_dir().join("corti-clean-wav-test");
        std::fs::create_dir_all(&dir).unwrap();
        let raw = dir.join("rec.wav");

        // Synthetic 2-track float WAV: ch0 = mic, ch1 = tap. Cross the adapter's bounded-push boundary;
        // exact values don't matter — we assert structure + tap preservation across every slice.
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let frames = AEC_PUSH_FRAMES + 257;
        let tap_in: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.01).sin() * 0.3).collect();
        let mic_in: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.02).cos() * 0.2).collect();
        {
            let mut w = hound::WavWriter::create(&raw, spec).unwrap();
            for i in 0..frames {
                w.write_sample(mic_in[i]).unwrap(); // ch0 = me
                w.write_sample(tap_in[i]).unwrap(); // ch1 = them
            }
            w.finalize().unwrap();
        }

        let clean_path = write_clean_wav(&raw, &corti_aec::AecConfig::default())
            .unwrap()
            .expect("a 2-channel input produces a cleaned WAV");
        assert_eq!(clean_path, clean_wav_path(&raw));

        let mut r = hound::WavReader::open(&clean_path).unwrap();
        let got = r.spec();
        assert_eq!(got.channels, 2);
        assert_eq!(got.sample_rate, 48_000);
        assert_eq!(got.bits_per_sample, 32);
        assert_eq!(got.sample_format, hound::SampleFormat::Float);

        let out: Vec<f32> = r.samples::<f32>().collect::<Result<_, _>>().unwrap();
        assert_eq!(out.len(), frames * 2, "same frame count, 2 channels");
        // ch1 (the tap) must be bit-identical to the input tap — AEC never touches the far-end track.
        let tap_out: Vec<f32> = out.iter().skip(1).step_by(2).copied().collect();
        assert_eq!(tap_out, tap_in, "raw far-end tap must be preserved exactly");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_clean_wav_skips_tap_only_mono() {
        let dir = std::env::temp_dir().join("corti-clean-wav-taponly-test");
        std::fs::create_dir_all(&dir).unwrap();
        let raw = dir.join("rec.wav");

        // A 1-channel float WAV is a tap-only ("webinar"/listen-only) recording: no mic track, nothing to
        // cancel. `write_clean_wav` must report this as `Ok(None)` (an expected skip, not an error) and must
        // not create a `-clean.wav` sibling.
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        {
            let mut w = hound::WavWriter::create(&raw, spec).unwrap();
            for i in 0..256usize {
                w.write_sample((i as f32 * 0.01).sin() * 0.3).unwrap();
            }
            w.finalize().unwrap();
        }

        assert_eq!(
            write_clean_wav(&raw, &corti_aec::AecConfig::default()).unwrap(),
            None,
            "a tap-only (1-channel) recording skips AEC cleanly"
        );
        assert!(
            !clean_wav_path(&raw).exists(),
            "skipping AEC must not write a -clean.wav sibling"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The in-flight filter's collector sees every block, records the locked delay, and does not change a
    /// single emitted sample — the sidecar is a by-product, exactly as it is for the file-to-file pass.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_in_flight_filter_collects_blocks_without_changing_its_audio() {
        use corti_coreaudio::{CaptureFilter, FILTER_FRAMES_PER_CHUNK};

        let sample_rate = 8_000u32;
        let frames = 64 * 1024 + 97;
        let cfg = corti_aec::AecConfig {
            filter_len: 256,
            ..Default::default()
        };
        let tap: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
        let mic: Vec<f32> = (0..frames)
            .map(|i| {
                let near = (i as f32 * 0.011).sin() * 0.05;
                let echo = if i >= 24 { tap[i - 24] * 0.6 } else { 0.0 };
                near + echo
            })
            .collect();

        let drive = |mut filter: Box<dyn CaptureFilter>| -> Vec<f32> {
            let mut out = Vec::with_capacity(frames);
            for start in (0..frames).step_by(FILTER_FRAMES_PER_CHUNK) {
                let end = (start + FILTER_FRAMES_PER_CHUNK).min(frames);
                out.extend(filter.push(&mic[start..end], &tap[start..end]));
            }
            out.extend(filter.finish());
            out
        };

        let plain = drive(Box::new(StreamingAecFilter::new_with_lookahead(
            sample_rate,
            cfg.clone(),
            0.5,
        )));

        let collector = AecStatsCollector::new();
        assert!(collector.take().is_none(), "nothing before the filter runs");
        let collected = drive(Box::new(StreamingAecFilter::new_with_lookahead_and_stats(
            sample_rate,
            cfg.clone(),
            0.5,
            Some(collector.clone()),
        )));
        assert_eq!(plain, collected, "collecting must not change the audio");

        let (blocks, dropped, delay_samples) = collector.take().expect("the filter finished");
        assert_eq!(dropped, 0, "well under the collector cap");
        assert!(delay_samples > 0, "the warm-up locked a delay");
        // Emitted samples tile into `filter_len` blocks; the last one is short and zero-padded.
        assert_eq!(blocks.len(), frames.div_ceil(cfg.filter_len));
        assert_eq!(blocks[0].t_start_secs, 0.0);
        assert!(
            blocks
                .windows(2)
                .all(|w| w[1].t_start_secs > w[0].t_start_secs),
            "the record is sorted, which `span_stats` requires"
        );
        assert!(collector.take().unwrap().0.is_empty(), "take() drains");
    }

    /// The collector is bounded by a constant, not by call length: past the cap the oldest blocks go and
    /// the loss is counted into the sidecar's `stats_dropped` rather than silently papered over.
    #[test]
    fn the_collector_evicts_its_oldest_blocks_and_counts_them() {
        let collector = AecStatsCollector::new();
        let block = |t: f64| corti_aec::BlockStats {
            t_start_secs: t,
            mic_energy: 1.0,
            far_energy: 1.0,
            echo_estimate_energy: 1.0,
            error_energy: 1.0,
            double_talk: false,
            suppressed: true,
        };
        let overflow = 100usize;
        collector.record(
            (0..MAX_CAPTURE_BLOCK_STATS + overflow)
                .map(|k| block(k as f64))
                .collect(),
        );
        collector.finish(vec![block(1e6)], 7, 4_242);
        let (blocks, dropped, delay_samples) = collector.take().expect("finished");
        assert_eq!(blocks.len(), MAX_CAPTURE_BLOCK_STATS);
        // One more eviction when `finish` appends its trailing block.
        assert_eq!(dropped, overflow as u64 + 1 + 7);
        assert_eq!(delay_samples, 4_242);
        assert_eq!(
            blocks[0].t_start_secs,
            (overflow + 1) as f64,
            "the hole is at the old end"
        );
    }

    /// #74 acceptance: **ERLE parity**. The in-flight filter the capture writer drives in
    /// [`FILTER_FRAMES_PER_CHUNK`] blocks must cancel exactly as well as the post-capture
    /// [`write_clean_wav`] pass, which reads the whole file and pushes [`AEC_PUSH_FRAMES`] at a time. Both
    /// drive the same `StreamingAec`, so the chunk size must not change the output — asserted sample-exact
    /// here, with a real echo so a broken canceller can't pass by leaving the mic untouched.
    #[cfg(target_os = "macos")]
    #[test]
    fn in_flight_filter_matches_post_capture_pass() {
        use corti_coreaudio::{CaptureFilter, FILTER_FRAMES_PER_CHUNK};

        // 8 kHz keeps the 5 s lookahead (40k samples) and the run cheap; a short filter keeps the FFTs small.
        // The length is deliberately not a multiple of any block size in play (4096 in flight, 16384 batch,
        // filter_len 256), so `finish`'s zero-pad and overshoot-truncate actually run on both sides.
        let sample_rate = 8_000u32;
        let frames = 64 * 1024 + 97;
        let cfg = corti_aec::AecConfig {
            filter_len: 256,
            ..Default::default()
        };
        let tap: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
        // Mic = quiet near-end speech + a delayed, attenuated copy of the far end (the echo to cancel).
        let mic: Vec<f32> = (0..frames)
            .map(|i| {
                let near = (i as f32 * 0.011).sin() * 0.05;
                let echo = if i >= 24 { tap[i - 24] * 0.6 } else { 0.0 };
                near + echo
            })
            .collect();

        let dir = std::env::temp_dir().join("corti-aec-parity-test");
        std::fs::create_dir_all(&dir).unwrap();
        let raw = dir.join("rec.wav");
        {
            let mut w = hound::WavWriter::create(
                &raw,
                hound::WavSpec {
                    channels: 2,
                    sample_rate,
                    bits_per_sample: 32,
                    sample_format: hound::SampleFormat::Float,
                },
            )
            .unwrap();
            for i in 0..frames {
                w.write_sample(mic[i]).unwrap();
                w.write_sample(tap[i]).unwrap();
            }
            w.finalize().unwrap();
        }

        let clean_path = write_clean_wav(&raw, &cfg).unwrap().unwrap();
        let mut r = hound::WavReader::open(&clean_path).unwrap();
        let interleaved: Vec<f32> = r.samples::<f32>().collect::<Result<_, _>>().unwrap();
        let batch: Vec<f32> = interleaved.iter().step_by(2).copied().collect();

        // The writer thread's drive: fixed-size blocks, nothing sized by the recording length.
        let mut filter: Box<dyn CaptureFilter> =
            Box::new(StreamingAecFilter::new(sample_rate, cfg.clone()));
        let mut in_flight = Vec::with_capacity(frames);
        for start in (0..frames).step_by(FILTER_FRAMES_PER_CHUNK) {
            let end = (start + FILTER_FRAMES_PER_CHUNK).min(frames);
            in_flight.extend(filter.push(&mic[start..end], &tap[start..end]));
        }
        in_flight.extend(filter.finish());
        in_flight.truncate(frames);

        assert_eq!(in_flight.len(), batch.len(), "same frame count");
        assert_eq!(in_flight, batch, "chunk size must not change the output");

        let energy = |v: &[f32]| v.iter().map(|s| (s * s) as f64).sum::<f64>();
        let erle = 10.0 * (energy(&mic) / energy(&in_flight).max(f64::MIN_POSITIVE)).log10();
        assert!(erle > 6.0, "expected real cancellation, got {erle:.1} dB");

        std::fs::remove_dir_all(&dir).ok();
    }
}
