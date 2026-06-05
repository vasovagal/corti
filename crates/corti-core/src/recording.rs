//! A captured recording and where it is in the capture → transcribe → file pipeline.

use std::path::PathBuf;

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::app::OwningApp;

/// Metadata for one recording, captured by the detector and carried through the pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingMeta {
    /// When the mic became active (recording start).
    pub started_at: DateTime<Local>,
    /// When the mic was released (recording end). `None` while still recording.
    pub ended_at: Option<DateTime<Local>>,
    /// Best-effort attribution of the app that held the mic.
    pub owning_app: OwningApp,
    /// Local cache path of the captured audio (outside any vault).
    pub audio_path: PathBuf,
}

impl RecordingMeta {
    /// A default note title from the owning app and start time, e.g. `Zoom call — 2026-05-29 14:05`.
    /// When the app has no bundle id (manual/force-tap recordings), "call" is omitted:
    /// `System audio — 2026-05-29 14:05`.
    pub fn note_title(&self) -> String {
        let suffix = if self.owning_app.bundle_id.is_some() {
            " call"
        } else {
            ""
        };
        format!(
            "{}{suffix} — {}",
            self.owning_app.name,
            self.started_at.format("%Y-%m-%d %H:%M")
        )
    }

    /// The `--source` value for `vagus add-note`: app name plus start time.
    pub fn source(&self) -> String {
        format!(
            "{} · {}",
            self.owning_app.name,
            self.started_at.format("%Y-%m-%d %H:%M")
        )
    }
}

/// Where a recording is in the pipeline. The terminal states are `Done` and `Failed`; everything else is
/// resumable after a crash (see `corti-queue`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// Audio capture in progress.
    Recording,
    /// Capture finished; audio on disk, not yet handed to a transcription backend.
    PendingTranscription,
    /// Transcription backend is working (e.g. AWS job running, or local whisper in progress).
    Transcribing,
    /// Transcript ready; about to be filed into vagus.
    PendingNote,
    /// Note filed in vagus; pipeline complete.
    Done,
    /// Pipeline failed; see the accompanying error.
    Failed,
}

impl JobStatus {
    /// Terminal states need no further work and are skipped on crash-recovery startup.
    pub fn is_terminal(self) -> bool {
        matches!(self, JobStatus::Done | JobStatus::Failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::OwningApp;
    use chrono::TimeZone;

    fn sample() -> RecordingMeta {
        RecordingMeta {
            started_at: Local.with_ymd_and_hms(2026, 5, 29, 14, 5, 0).unwrap(),
            ended_at: None,
            owning_app: OwningApp::from_bundle_id("us.zoom.xos"),
            audio_path: PathBuf::from("/tmp/rec.wav"),
        }
    }

    #[test]
    fn title_and_source_render_app_and_time() {
        let m = sample();
        assert_eq!(m.note_title(), "Zoom call — 2026-05-29 14:05");
        assert_eq!(m.source(), "Zoom · 2026-05-29 14:05");
    }

    #[test]
    fn title_omits_call_when_no_bundle_id() {
        let mut m = sample();
        m.owning_app = OwningApp::unknown();
        assert_eq!(m.note_title(), "Unknown app — 2026-05-29 14:05");
    }

    #[test]
    fn terminal_states() {
        assert!(JobStatus::Done.is_terminal());
        assert!(JobStatus::Failed.is_terminal());
        assert!(!JobStatus::Recording.is_terminal());
        assert!(!JobStatus::Transcribing.is_terminal());
    }
}
