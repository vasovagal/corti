//! A captured recording and where it is in the capture → transcribe → file pipeline.

use std::path::PathBuf;

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::app::OwningApp;

/// The friendly app name a manual "webinar" (tap-only, listen-only) capture is recorded under. The tray
/// toggle sets `OwningApp { bundle_id: None, name: WEBINAR_NAME }`, which is the signal [`RecordingMode`]
/// derives from — no new persisted state. Kept here (not in the app) so both the producer and the
/// mode-deriving consumer agree on the exact string.
pub const WEBINAR_NAME: &str = "Webinar";

/// How a recording was captured. Derived from signals already on [`RecordingMeta`] (no new persisted
/// state — see issue #28): the manual webinar path records under [`WEBINAR_NAME`] with no bundle id, while
/// every detector-triggered (mic-in-use) capture and every two-way tap carries a real owning app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingMode {
    /// A two-way call: the detector saw the mic go live for a real app (mic + tap captured).
    Call,
    /// A listen-only "webinar": the manual tap-only path (no mic opened).
    Webinar,
}

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
    /// The capture mode, derived from existing signals (no new persisted state; see issue #28). A manual
    /// webinar is recorded under [`WEBINAR_NAME`] with no bundle id; everything else (detector calls, taps
    /// attributed to a real app, even "Unknown app") is a two-way [`Call`](RecordingMode::Call).
    pub fn mode(&self) -> RecordingMode {
        if self.owning_app.bundle_id.is_none() && self.owning_app.name == WEBINAR_NAME {
            RecordingMode::Webinar
        } else {
            RecordingMode::Call
        }
    }

    /// A default note title from the owning app and start time, styled by [`mode`](RecordingMeta::mode):
    /// a call reads `Zoom call — 2026-05-29 14:05`; a listen-only capture reads `Webinar — 2026-05-29 14:05`
    /// (no " call" suffix). An attributed-but-unknown app keeps its name without the suffix too.
    pub fn note_title(&self) -> String {
        let suffix = match self.mode() {
            // A real, recognized conferencing app reads best as "<App> call"; anonymous/unknown captures
            // (no bundle id) drop the suffix so we never write "Unknown app call" / "Webinar call".
            RecordingMode::Call if self.owning_app.bundle_id.is_some() => " call",
            RecordingMode::Call | RecordingMode::Webinar => "",
        };
        format!(
            "{}{suffix} — {}",
            self.owning_app.name,
            self.started_at.format("%Y-%m-%d %H:%M")
        )
    }

    /// The `--source` value for `vagus add-note`: app name plus start time. The app-name half is run through
    /// [`yaml_safe`] because `vagus` writes `--source` verbatim and unquoted into the note's YAML frontmatter,
    /// and the name can be an arbitrary macOS bundle id or the user's `corti-tap --label`.
    pub fn source(&self) -> String {
        format!(
            "{} · {}",
            yaml_safe(&self.owning_app.name),
            self.started_at.format("%Y-%m-%d %H:%M")
        )
    }
}

/// Make `name` safe to embed in `vagus`'s single-line YAML `source:` frontmatter. vagus writes `--source`
/// verbatim and unquoted, so a value with a newline, a `": "` mapping marker, a leading YAML indicator
/// (`#`, `&`, `*`, `!`, …), a `" #"` comment, or a quote would corrupt the note's frontmatter (invalid YAML
/// the vault indexer then skips or mis-parses). The recording-time half of `source()` is known-safe
/// (`HH:MM`'s colon isn't followed by a space), so only the app-name half needs this.
fn yaml_safe(name: &str) -> String {
    // Collapse all whitespace (incl. newlines/tabs) to single spaces — this alone defeats newline injection.
    let collapsed = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::with_capacity(collapsed.len());
    let mut chars = collapsed.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // `": "` (or a trailing colon) opens a nested mapping in a plain scalar — neutralize to a dash.
            ':' if chars.peek().is_none_or(|n| *n == ' ') => out.push('-'),
            // `" #"` starts a YAML comment; drop the marker so the rest of the name survives.
            '#' if out.ends_with(' ') => {}
            // A bare quote/backtick can flip the value into a quoted scalar; drop it.
            '"' | '\'' | '`' => {}
            other => out.push(other),
        }
    }
    // Strip any leading YAML indicator so the value is always parsed as a plain scalar.
    let cleaned = out
        .trim()
        .trim_start_matches(|c: char| "#&*!|>%@,-?:[]{}".contains(c))
        .trim();
    if cleaned.is_empty() {
        "Unknown app".to_string()
    } else {
        cleaned.to_string()
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
    /// Transcription backend is working (e.g. AWS job running, or local Parakeet/ONNX in progress).
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
    fn mode_is_call_for_attributed_recordings() {
        // A detected call (real bundle id).
        assert_eq!(sample().mode(), RecordingMode::Call);
        // Attribution failed but it's still a mic-triggered call, not a webinar.
        let mut unknown = sample();
        unknown.owning_app = OwningApp::unknown();
        assert_eq!(unknown.mode(), RecordingMode::Call);
    }

    #[test]
    fn mode_is_webinar_for_the_tap_only_path() {
        let mut m = sample();
        m.owning_app = OwningApp {
            bundle_id: None,
            name: WEBINAR_NAME.to_string(),
        };
        assert_eq!(m.mode(), RecordingMode::Webinar);
        // A webinar's title drops the " call" suffix and keeps the WEBINAR_NAME.
        assert_eq!(m.note_title(), "Webinar — 2026-05-29 14:05");
        assert_eq!(m.source(), "Webinar · 2026-05-29 14:05");
    }

    #[test]
    fn source_is_yaml_frontmatter_safe() {
        let at = Local.with_ymd_and_hms(2026, 5, 29, 14, 5, 0).unwrap();
        let src = |name: &str| {
            RecordingMeta {
                started_at: at,
                ended_at: None,
                owning_app: OwningApp {
                    bundle_id: None,
                    name: name.to_string(),
                },
                audio_path: PathBuf::from("/tmp/x.wav"),
            }
            .source()
        };
        // A `": "` mapping marker (reachable via `corti-tap --label`) is neutralized, not left to break YAML.
        assert_eq!(src("My Call: part 2"), "My Call- part 2 · 2026-05-29 14:05");
        // A newline can't inject a second frontmatter line.
        assert_eq!(
            src("evil\ninjected: true"),
            "evil injected- true · 2026-05-29 14:05"
        );
        // Leading indicators and quotes are stripped so the value stays a plain scalar.
        assert_eq!(src("#tag"), "tag · 2026-05-29 14:05");
        assert_eq!(src("\"quoted\""), "quoted · 2026-05-29 14:05");
        // An all-hostile name degrades to a placeholder rather than an empty value.
        assert_eq!(src("###"), "Unknown app · 2026-05-29 14:05");
    }

    #[test]
    fn terminal_states() {
        assert!(JobStatus::Done.is_terminal());
        assert!(JobStatus::Failed.is_terminal());
        assert!(!JobStatus::Recording.is_terminal());
        assert!(!JobStatus::Transcribing.is_terminal());
    }
}
