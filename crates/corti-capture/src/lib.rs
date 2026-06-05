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

#[cfg(target_os = "macos")]
pub use platform::{Recorder, write_tap_only_wav, write_two_track_wav};

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

        /// Like [`start`], but **tap-only**: the microphone is never opened (no orange "mic in use" dot, no
        /// microphone TCC prompt) — only the system-audio tap runs. Pair with [`finish_tap_only`] to write
        /// the 1-channel WAV. This is the "webinar / listen-only" capture mode.
        ///
        /// [`start`]: Recorder::start
        /// [`finish_tap_only`]: Recorder::finish_tap_only
        pub fn start_tap_only(app: &corti_core::OwningApp, pid: Option<i32>) -> Result<Self> {
            let dir = recordings_dir()?;
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            let stem = recording_stem(app, chrono::Local::now());
            let out = dir.join(format!("{stem}.wav"));
            let target = match pid {
                Some(pid) => TapTarget::Process(pid),
                None => TapTarget::Global,
            };
            let session = CaptureSession::start_tap_only(target)
                .with_context(|| format!("starting tap-only capture for {}", app.name))?;
            Ok(Self { session, out })
        }

        /// The path the recording will be written to.
        pub fn output_path(&self) -> &Path {
            &self.out
        }

        /// Stop capture and write the 2-track WAV. Returns the written path. Errors if no audio was
        /// delivered (e.g. the TCC audio-capture permission is missing).
        pub fn finish(self) -> Result<PathBuf> {
            let (out, audio) = self.stop_capture()?;
            write_two_track_wav(&out, &audio)?;
            Ok(out)
        }

        /// Stop capture and write a 1-channel WAV containing only the tap (far-end / system audio). Pair
        /// with [`start_tap_only`] for a true no-mic recording; if paired with [`start`] the mic was opened
        /// as the clock leader and is simply discarded here.
        ///
        /// [`start`]: Recorder::start
        /// [`start_tap_only`]: Recorder::start_tap_only
        pub fn finish_tap_only(self) -> Result<PathBuf> {
            let (out, audio) = self.stop_capture()?;
            write_tap_only_wav(&out, &audio)?;
            Ok(out)
        }

        fn stop_capture(self) -> Result<(PathBuf, CapturedAudio)> {
            let audio = self.session.stop();
            if audio.callbacks == 0 {
                anyhow::bail!(
                    "no audio captured (IO proc never fired) — likely the macOS audio-capture permission \
                     is not granted to corti"
                );
            }
            Ok((self.out, audio))
        }
    }

    /// Write captured audio as a 1-channel float WAV: tap only (far-end / system audio, mono).
    pub fn write_tap_only_wav(path: &Path, audio: &CapturedAudio) -> Result<()> {
        let tap = audio.tap_mono();
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: audio.sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec)
            .with_context(|| format!("creating {}", path.display()))?;
        for &s in &tap {
            w.write_sample(s)?;
        }
        w.finalize()?;
        Ok(())
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
}
