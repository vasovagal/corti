//! Bounded, event-driven state for the timestamped Live Transcript window (issue #105).
//!
//! The crash-safe Vagus note remains the durable transcript authority. This store is deliberately
//! transient: one active/recent session, a hard-capped deque for open-late snapshots, and small Tauri
//! delta events for an already-open window. Publishing is always update-then-emit and never runs while
//! the store mutex is held.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use corti_core::{Speaker, TranscriptSegment};
use corti_transcribe::segment::{SEGMENT_GAP, Word, words_to_segments};
use serde::Serialize;
use tauri::{Emitter, State};

/// Event listened to by the Live Transcript webview.
pub(crate) const LIVE_TRANSCRIPT_EVENT: &str = "live-transcript-changed";
/// UI history is independent of call length. Text plus conservative per-row overhead stays under ~1 MiB.
const MAX_RETAINED_BYTES: usize = 1024 * 1024;
/// A second independent bound protects against pathological streams of tiny rows.
const MAX_RETAINED_LINES: usize = 2_000;
const LINE_OVERHEAD_BYTES: usize = 64;
const MAX_SPEAKER_BYTES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LiveTranscriptMode {
    Idle,
    Call,
    Test,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(not(feature = "local"), allow(dead_code))]
pub(crate) enum LiveTranscriptStatus {
    Idle,
    Loading,
    Listening,
    Stopping,
    Complete,
    Unavailable,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct LiveTranscriptLine {
    /// Process-monotonic row id. It does not reset between sessions, so stale events are unambiguous.
    pub seq: u64,
    pub speaker: String,
    pub start_sec: f64,
    pub end_sec: f64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LiveTranscriptSnapshot {
    pub revision: u64,
    pub session_id: Option<String>,
    pub mode: LiveTranscriptMode,
    pub status: LiveTranscriptStatus,
    pub title: String,
    pub detail: Option<String>,
    pub active: bool,
    pub evicted_lines: u64,
    pub retained_from_seq: u64,
    pub lines: Vec<LiveTranscriptLine>,
}

/// Small delta event. The UI subscribes before taking a snapshot, then applies only increasing revisions.
/// `reset` clears rows for a new session; `retained_from_seq` trims rows evicted by either hard cap.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct LiveTranscriptEvent {
    pub revision: u64,
    pub session_id: Option<String>,
    pub mode: LiveTranscriptMode,
    pub status: LiveTranscriptStatus,
    pub title: String,
    pub detail: Option<String>,
    pub active: bool,
    pub evicted_lines: u64,
    pub retained_from_seq: u64,
    pub reset: bool,
    pub line: Option<LiveTranscriptLine>,
}

type Notify = Arc<dyn Fn(LiveTranscriptEvent) + Send + Sync>;

/// Cloneable managed state shared by the detector/live worker, test worker, and Tauri commands.
#[derive(Clone)]
pub(crate) struct LiveTranscriptStore {
    inner: Arc<Mutex<Inner>>,
    /// Serializes update → emit without holding `inner`, so two producer threads cannot deliver revisions
    /// out of order even though Tauri notification itself runs after the state mutation.
    publish: Arc<Mutex<()>>,
    notify: Notify,
}

struct Inner {
    revision: u64,
    next_seq: u64,
    session_id: Option<String>,
    mode: LiveTranscriptMode,
    status: LiveTranscriptStatus,
    title: String,
    detail: Option<String>,
    active: bool,
    lines: VecDeque<LiveTranscriptLine>,
    retained_bytes: usize,
    evicted_lines: u64,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            revision: 0,
            next_seq: 1,
            session_id: None,
            mode: LiveTranscriptMode::Idle,
            status: LiveTranscriptStatus::Idle,
            title: "Live transcript".to_string(),
            detail: Some("Start a microphone test or join a call.".to_string()),
            active: false,
            lines: VecDeque::new(),
            retained_bytes: 0,
            evicted_lines: 0,
        }
    }
}

impl Inner {
    fn retained_from_seq(&self) -> u64 {
        self.lines.front().map_or(self.next_seq, |line| line.seq)
    }

    fn snapshot(&self) -> LiveTranscriptSnapshot {
        LiveTranscriptSnapshot {
            revision: self.revision,
            session_id: self.session_id.clone(),
            mode: self.mode,
            status: self.status,
            title: self.title.clone(),
            detail: self.detail.clone(),
            active: self.active,
            evicted_lines: self.evicted_lines,
            retained_from_seq: self.retained_from_seq(),
            lines: self.lines.iter().cloned().collect(),
        }
    }

    fn event(&self, reset: bool, line: Option<LiveTranscriptLine>) -> LiveTranscriptEvent {
        LiveTranscriptEvent {
            revision: self.revision,
            session_id: self.session_id.clone(),
            mode: self.mode,
            status: self.status,
            title: self.title.clone(),
            detail: self.detail.clone(),
            active: self.active,
            evicted_lines: self.evicted_lines,
            retained_from_seq: self.retained_from_seq(),
            reset,
            line,
        }
    }
}

impl LiveTranscriptStore {
    pub(crate) fn for_app(app: tauri::AppHandle) -> Self {
        Self::with_notifier(Arc::new(move |event| {
            let _ = app.emit(LIVE_TRANSCRIPT_EVENT, event);
        }))
    }

    /// A no-event store for model-free unit tests and managers constructed outside the Tauri app.
    pub(crate) fn detached() -> Self {
        Self::with_notifier(Arc::new(|_| {}))
    }

    fn with_notifier(notify: Notify) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            publish: Arc::new(Mutex::new(())),
            notify,
        }
    }

    pub(crate) fn snapshot(&self) -> LiveTranscriptSnapshot {
        self.inner.lock().unwrap().snapshot()
    }

    #[cfg_attr(not(feature = "local"), allow(dead_code))]
    pub(crate) fn begin_call(&self, id: &str, title: &str) {
        self.begin(
            id,
            LiveTranscriptMode::Call,
            title,
            LiveTranscriptStatus::Loading,
            Some("Loading the local transcription engine…".to_string()),
        );
    }

    pub(crate) fn begin_test(&self, id: &str) {
        self.begin(
            id,
            LiveTranscriptMode::Test,
            "Microphone transcription test",
            LiveTranscriptStatus::Loading,
            Some("Loading the local transcription engine…".to_string()),
        );
    }

    pub(crate) fn show_test_error(&self, detail: impl Into<String>) {
        let id = "microphone-test-unavailable";
        self.begin_test(id);
        self.set_error(id, detail);
    }

    /// Ensure every detected call has a readable state, even when no local live tee could be attached.
    /// A matching eligible session already started by `LiveHook::started` wins and is left untouched.
    pub(crate) fn ensure_unavailable_call(&self, id: &str, title: &str, detail: String) {
        if self
            .inner
            .lock()
            .unwrap()
            .session_id
            .as_deref()
            .is_some_and(|active| active == id)
        {
            return;
        }
        self.begin(
            id,
            LiveTranscriptMode::Call,
            title,
            LiveTranscriptStatus::Unavailable,
            Some(detail),
        );
    }

    fn begin(
        &self,
        id: &str,
        mode: LiveTranscriptMode,
        title: &str,
        status: LiveTranscriptStatus,
        detail: Option<String>,
    ) {
        let _publish = self.publish.lock().unwrap();
        let event = {
            let mut inner = self.inner.lock().unwrap();
            inner.revision = inner.revision.saturating_add(1);
            inner.session_id = Some(id.to_string());
            inner.mode = mode;
            inner.status = status;
            inner.title = title.to_string();
            inner.detail = detail;
            inner.active = matches!(
                status,
                LiveTranscriptStatus::Loading | LiveTranscriptStatus::Listening
            );
            inner.lines.clear();
            inner.retained_bytes = 0;
            inner.evicted_lines = 0;
            inner.event(true, None)
        };
        (self.notify)(event);
    }

    #[cfg_attr(not(feature = "local"), allow(dead_code))]
    pub(crate) fn set_listening(&self, id: &str, detail: impl Into<String>) {
        self.set_status(
            id,
            LiveTranscriptStatus::Listening,
            true,
            Some(detail.into()),
        );
    }

    pub(crate) fn set_stopping(&self, id: &str, detail: impl Into<String>) {
        self.set_status(
            id,
            LiveTranscriptStatus::Stopping,
            true,
            Some(detail.into()),
        );
    }

    pub(crate) fn set_complete(&self, id: &str, detail: impl Into<String>) {
        self.set_status(
            id,
            LiveTranscriptStatus::Complete,
            false,
            Some(detail.into()),
        );
    }

    pub(crate) fn set_error(&self, id: &str, detail: impl Into<String>) {
        self.set_status(id, LiveTranscriptStatus::Error, false, Some(detail.into()));
    }

    fn set_status(
        &self,
        id: &str,
        status: LiveTranscriptStatus,
        active: bool,
        detail: Option<String>,
    ) {
        let _publish = self.publish.lock().unwrap();
        let event = {
            let mut inner = self.inner.lock().unwrap();
            if inner.session_id.as_deref() != Some(id) {
                return;
            }
            inner.revision = inner.revision.saturating_add(1);
            inner.status = status;
            inner.active = active;
            inner.detail = detail;
            inner.event(false, None)
        };
        (self.notify)(event);
    }

    /// Convert one closed VAD region's words into human-sized rows and publish them. The same words continue
    /// into the durable rolling writer; this observer neither consumes nor modifies that path.
    pub(crate) fn append_words(&self, id: &str, speaker: Speaker, words: &[Word]) {
        if words.is_empty() {
            return;
        }
        self.append_segments(id, words_to_segments(words, speaker, SEGMENT_GAP));
    }

    fn append_segments(&self, id: &str, segments: Vec<TranscriptSegment>) {
        let _publish = self.publish.lock().unwrap();
        let events = {
            let mut inner = self.inner.lock().unwrap();
            if inner.session_id.as_deref() != Some(id) {
                return;
            }
            let mut events = Vec::with_capacity(segments.len());
            for segment in segments {
                let text = cap_utf8(segment.text.trim(), MAX_RETAINED_BYTES / 2);
                if text.is_empty() {
                    continue;
                }
                let start_sec = finite_nonnegative(segment.start);
                let end_sec = finite_nonnegative(segment.end).max(start_sec);
                let displayed_speaker = segment.speaker.display();
                let speaker = if displayed_speaker.is_empty() || displayed_speaker == "Them" {
                    "Them 1".to_string()
                } else {
                    cap_utf8(displayed_speaker, MAX_SPEAKER_BYTES)
                };
                let line = LiveTranscriptLine {
                    seq: inner.next_seq,
                    speaker,
                    start_sec,
                    end_sec,
                    text,
                };
                inner.next_seq = inner.next_seq.saturating_add(1);
                inner.retained_bytes = inner.retained_bytes.saturating_add(line_cost(&line));
                inner.lines.push_back(line.clone());
                while inner.lines.len() > MAX_RETAINED_LINES
                    || inner.retained_bytes > MAX_RETAINED_BYTES
                {
                    let Some(evicted) = inner.lines.pop_front() else {
                        break;
                    };
                    inner.retained_bytes = inner.retained_bytes.saturating_sub(line_cost(&evicted));
                    inner.evicted_lines = inner.evicted_lines.saturating_add(1);
                }
                inner.revision = inner.revision.saturating_add(1);
                events.push(inner.event(false, Some(line)));
            }
            events
        };
        for event in events {
            (self.notify)(event);
        }
    }
}

fn finite_nonnegative(value: f64) -> f64 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

fn line_cost(line: &LiveTranscriptLine) -> usize {
    LINE_OVERHEAD_BYTES
        .saturating_add(line.speaker.len())
        .saturating_add(line.text.len())
}

fn cap_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

#[tauri::command]
pub(crate) fn get_live_transcript(store: State<'_, LiveTranscriptStore>) -> LiveTranscriptSnapshot {
    store.snapshot()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(start: f64, end: f64, text: &str) -> Word {
        Word {
            start,
            end,
            text: text.to_string(),
        }
    }

    #[test]
    fn timestamps_and_speakers_are_serialized_from_closed_regions() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let store = LiveTranscriptStore::with_notifier(Arc::new(move |event| {
            sink.lock().unwrap().push(event);
        }));
        store.begin_call("call-a", "Zoom");
        store.set_listening("call-a", "Listening");
        store.append_words(
            "call-a",
            Speaker::Me,
            &[word(12.25, 12.5, "Hello"), word(12.6, 13.0, "there")],
        );
        store.append_words(
            "call-a",
            Speaker::Other("Them".to_string()),
            &[word(14.0, 14.5, "Hi")],
        );

        let snapshot = store.snapshot();
        assert_eq!(snapshot.lines.len(), 2);
        assert_eq!(snapshot.lines[0].speaker, "Me");
        assert_eq!(snapshot.lines[0].start_sec, 12.25);
        assert_eq!(snapshot.lines[0].end_sec, 13.0);
        assert_eq!(snapshot.lines[0].text, "Hello there");
        assert_eq!(snapshot.lines[1].speaker, "Them 1");
        assert!(
            events
                .lock()
                .unwrap()
                .windows(2)
                .all(|pair| pair[0].revision < pair[1].revision)
        );
    }

    #[test]
    fn detector_fallback_state_does_not_overwrite_an_attached_live_session() {
        let store = LiveTranscriptStore::detached();
        store.begin_call("call", "Zoom");
        store.ensure_unavailable_call("call", "Zoom", "generic fallback".to_string());
        let snapshot = store.snapshot();
        assert_eq!(snapshot.status, LiveTranscriptStatus::Loading);
        assert_ne!(snapshot.detail.as_deref(), Some("generic fallback"));
    }

    #[test]
    fn stale_session_updates_cannot_clobber_the_new_session() {
        let store = LiveTranscriptStore::detached();
        store.begin_call("old", "Old call");
        store.begin_test("new");
        store.set_error("old", "late failure");
        store.append_words("old", Speaker::Me, &[word(0.0, 1.0, "stale")]);

        let snapshot = store.snapshot();
        assert_eq!(snapshot.session_id.as_deref(), Some("new"));
        assert_eq!(snapshot.status, LiveTranscriptStatus::Loading);
        assert!(snapshot.lines.is_empty());
    }

    #[test]
    fn concurrent_producers_emit_revisions_in_store_order() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let store = LiveTranscriptStore::with_notifier(Arc::new(move |event| {
            sink.lock().unwrap().push(event.revision);
        }));
        store.begin_test("test");
        let left = store.clone();
        let right = store.clone();
        let a = std::thread::spawn(move || {
            for i in 0..100 {
                left.append_words("test", Speaker::Me, &[word(i as f64, i as f64 + 0.1, "a")]);
            }
        });
        let b = std::thread::spawn(move || {
            for i in 0..100 {
                right.append_words(
                    "test",
                    Speaker::Other("Them".to_string()),
                    &[word(i as f64, i as f64 + 0.1, "b")],
                );
            }
        });
        a.join().unwrap();
        b.join().unwrap();
        let revisions = events.lock().unwrap();
        assert!(revisions.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn line_count_is_bounded_and_eviction_is_visible() {
        let store = LiveTranscriptStore::detached();
        store.begin_test("test");
        for i in 0..=MAX_RETAINED_LINES {
            store.append_segments(
                "test",
                vec![TranscriptSegment {
                    speaker: Speaker::Me,
                    start: i as f64,
                    end: i as f64 + 0.5,
                    text: format!("line {i}"),
                }],
            );
        }
        let snapshot = store.snapshot();
        assert!(snapshot.lines.len() <= MAX_RETAINED_LINES);
        assert!(snapshot.evicted_lines > 0);
        assert_eq!(
            snapshot.retained_from_seq,
            snapshot.lines.first().unwrap().seq
        );
    }

    #[test]
    fn non_finite_timestamps_and_oversized_utf8_are_safe() {
        let store = LiveTranscriptStore::detached();
        store.begin_test("test");
        store.append_segments(
            "test",
            vec![TranscriptSegment {
                speaker: Speaker::Other("Them".to_string()),
                start: f64::NAN,
                end: f64::INFINITY,
                text: "é".repeat(MAX_RETAINED_BYTES),
            }],
        );
        let snapshot = store.snapshot();
        assert_eq!(snapshot.lines[0].start_sec, 0.0);
        assert_eq!(snapshot.lines[0].end_sec, 0.0);
        assert!(
            snapshot.lines[0]
                .text
                .is_char_boundary(snapshot.lines[0].text.len())
        );
        assert!(snapshot.lines[0].text.len() <= MAX_RETAINED_BYTES / 2 + 3);
    }
}
