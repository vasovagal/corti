//! Recording orchestration: drive the [`corti_coreaudio`] capture engine for the lifetime of a meeting and
//! write the result as a **2-track WAV** — channel 0 = the mic ("me"), channel 1 = the far-end tap
//! ("them") — into the recordings cache (`~/Library/Caches/corti/recordings/`, never a vault; guardrail 5).
//!
//! Keeping the two ends as separate tracks is what gives downstream code free "me vs. them" diarization and
//! lets [`corti-aec`](../corti_aec) cancel speaker bleed offline from time-aligned signals. The raw tracks
//! are always preserved; AEC produces an additional cleaned track and never overwrites these.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

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

/// Read the raw 2-track recording, cancel speaker bleed on ch0 (mic) using ch1 (far-end tap) as the
/// reference, and write the cleaned 2-track WAV (ch0 = cleaned mic, ch1 = **raw** tap, preserved). Returns
/// the clean path.
///
/// Errors if the input isn't 2-channel (e.g. a tap-only/listen-only recording has nothing to cancel) — the
/// caller is expected to fall back to the raw recording. The raw file is only read, never modified, so the
/// originals are always preserved (design/04-corti-aec.md invariant).
pub fn write_clean_wav(raw_2track_wav: &Path) -> Result<PathBuf> {
    let mut reader = hound::WavReader::open(raw_2track_wav)
        .with_context(|| format!("opening {} for AEC", raw_2track_wav.display()))?;
    let spec = reader.spec();
    if spec.channels != 2 {
        anyhow::bail!(
            "expected a 2-channel recording for AEC, got {} channel(s)",
            spec.channels
        );
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

    let clean = corti_aec::cancel(
        &mic,
        &tap,
        spec.sample_rate,
        &corti_aec::AecConfig::default(),
    );

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
        w.write_sample(clean.get(i).copied().unwrap_or(0.0))?; // ch0 = cleaned mic
        w.write_sample(tap.get(i).copied().unwrap_or(0.0))?; // ch1 = raw tap (preserved)
    }
    w.finalize()?;
    Ok(out)
}

#[cfg(target_os = "macos")]
pub use platform::{Recorder, write_two_track_wav};

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use corti_coreaudio::{CaptureSession, CapturedAudio, TapTarget};

    /// An in-progress recording. Built from the owning app's PID (per-app tap, falling back to a global tap
    /// inside the engine if the PID can't be resolved); call [`finish`] to stop and write the WAV.
    ///
    /// [`finish`]: Recorder::finish
    pub struct Recorder {
        session: CaptureSession,
        out: PathBuf,
    }

    impl Recorder {
        /// Start recording the given app (`pid = None` ⇒ global tap of all system audio) to a fresh file in
        /// the recordings cache. Returns the recorder and the output path.
        pub fn start(app: &corti_core::OwningApp, pid: Option<i32>) -> Result<Self> {
            let dir = recordings_dir()?;
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            let stem = recording_stem(app, chrono::Local::now());
            let out = dir.join(format!("{stem}.wav"));
            let target = match pid {
                Some(pid) => TapTarget::Process(pid),
                None => TapTarget::Global,
            };
            let session = CaptureSession::start(target)
                .with_context(|| format!("starting capture for {}", app.name))?;
            Ok(Self { session, out })
        }

        /// The path the recording will be written to.
        pub fn output_path(&self) -> &Path {
            &self.out
        }

        /// Stop capture and write the 2-track WAV. Returns the written path. Errors if no audio was
        /// delivered (e.g. the TCC audio-capture permission is missing).
        pub fn finish(self) -> Result<PathBuf> {
            let audio = self.session.stop();
            if audio.callbacks == 0 {
                anyhow::bail!(
                    "no audio captured (IO proc never fired) — likely the macOS audio-capture permission \
                     is not granted to corti"
                );
            }
            write_two_track_wav(&self.out, &audio)?;
            Ok(self.out)
        }
    }

    /// Write captured audio as a 2-channel float WAV: ch0 = mic (mono), ch1 = far-end tap (mono).
    pub fn write_two_track_wav(path: &Path, audio: &CapturedAudio) -> Result<()> {
        let mic = audio.mic_mono();
        let tap = audio.tap_mono();
        let frames = mic.len().max(tap.len());
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: audio.sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec)
            .with_context(|| format!("creating {}", path.display()))?;
        for i in 0..frames {
            w.write_sample(mic.get(i).copied().unwrap_or(0.0))?; // ch0 = me
            w.write_sample(tap.get(i).copied().unwrap_or(0.0))?; // ch1 = them
        }
        w.finalize()?;
        Ok(())
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
    fn write_clean_wav_preserves_tap_and_layout() {
        let dir = std::env::temp_dir().join("corti-clean-wav-test");
        std::fs::create_dir_all(&dir).unwrap();
        let raw = dir.join("rec.wav");

        // Synthetic 2-track float WAV: ch0 = mic, ch1 = tap. A few hundred frames so the (single-block)
        // AEC has something to chew on; exact values don't matter — we assert structure + tap preservation.
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let frames = 512usize;
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

        let clean_path = write_clean_wav(&raw).unwrap();
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
}
