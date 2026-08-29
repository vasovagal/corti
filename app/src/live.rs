#![cfg_attr(not(feature = "local"), allow(dead_code))]

//! Crash-safe live inbox filing (issues #87/#103, ADRs 0010/0012): transcribe a detector recording
//! **while it records**, then diarize and durably append bounded rolling windows to the vagus note. A
//! crash can lose the active configured window, never every previously committed window.
//!
//! ## Shape
//! [`AppLiveHook`] implements `corti_detect::LiveHook`: at recording start it checks eligibility
//! (config `live_filing`, local backend, models on disk) and, if eligible, hands the detector a bounded
//! lossy [`CaptureTee`]; once capture is running it spawns ONE `corti-live` std thread for the recording.
//! That thread drains tee chunks → [`StreamingAec::push`] on the mic → two `LiveTranscriber`s. Every
//! `live_buffer_minutes` (one minute by default), it checkpoints both VADs, optionally diarizes only that
//! bounded far-end PCM window, merges the speakers, appends once, and calls `sync_all`. Independent audio/
//! text caps force an earlier commit. The note is created lazily on the first non-empty window; its file +
//! parent directory are synced before the path is published. The thread never blocks the capture writer
//! (the fixed tee drops + counts when full) and is panic-contained — failure preserves prior chunks and
//! degrades to the same-inode batch fallback.
//!
//! ## Finish / discard
//! The tee sender is dropped when the recorder stops, which ends the chunk loop; the thread then waits
//! for an explicit recording-specific verdict so a finish and discard are never confused. The detector's
//! `LiveHook` delivers that verdict immediately, before its later pipeline message:
//! - finish freezes the tee's canonical dropped-chunk count, moves the handle into an ID-keyed collection,
//!   and lets the thread flush both transcribers. The pipeline later [`LiveManager::collect`]s that exact ID;
//! - discard transfers the handle to a manager-owned non-blocking reaper that removes any partial note,
//!   including one returned after a contained panic/failure. Discard remains inside the one-model gate; a
//!   reaper spawn failure transfers joining to the pipeline without dropping the handle/reporter.
//!
//! A zero-drop finish flips the state line and reports [`LiveOutcome::Filed`]. Any drop still flushes tails
//! but leaves `State: transcribing` and reports [`LiveOutcome::Fallback`], so the lossless WAV batch pass
//! rewrites the same note. Completed finish/discard handles are reaped before the gate opens; a new model
//! session never overlaps an older one that is still flushing or draining after discard.
//!
//! Each window is merged by timestamp before its one append. Far-end `Them N` identities are window-local
//! and may be renumbered at a boundary; stable cross-window embeddings are deliberately outside ADR 0012.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use corti_aec::StreamingAec;
use corti_capture::{CaptureChunk, CaptureTee};
use corti_core::{DiarizedTranscript, RecordingMeta, Speaker, TranscriptSegment};
use corti_transcribe::segment::{
    CleanupConfig, CleanupStats, EchoCandidate, SEGMENT_GAP, SpeakerTurn, Word, cleanup,
    cleanup_with_evidence, diarize_words, merge_by_time, split_regions, words_to_segments,
};
use tracing::{info, warn};

use crate::config::{AppConfig, BackendChoice};
use crate::live_view::LiveTranscriptStore;
use crate::pipeline::PipelineMsg;
use crate::settings::SharedConfig;

/// Bounded tee backlog in chunks (~4096 frames ≈ 85 ms each at 48 kHz, so ≈ 175 s of slack, ≤ ~64 MiB).
/// The larger fixed queue absorbs one bounded 10-minute diarization pass on the measured local backend plus
/// model/decode bursts. It never scales with call length; on a slower machine it still drops + counts rather
/// than growing or blocking the capture writer.
const TEE_BACKLOG: usize = 2048;

/// Absolute cap for the one source-rate far-end window retained for optional diarization. The configured
/// interval normally fires first (one minute at 48 kHz is ~11 MiB for this one `f32` channel); this guard
/// makes unusual sample rates or hand-edited config unable to turn the rolling window into O(call length).
const MAX_DIARIZATION_AUDIO_BYTES: usize = 128 * 1024 * 1024;
/// Independent cap for recognized words awaiting the next durable append. ASR output per minute is tiny in
/// practice, but a hard cap makes the memory contract hold even for malformed/model-pathological output.
const MAX_BUFFERED_TRANSCRIPT_BYTES: usize = 1024 * 1024;

/// Hard cap on the per-block AEC statistics a window retains as cleanup evidence (#149 phase 3b). The
/// working set is one window plus `echo_window_seconds` of lookback — at the default 8192-tap hop / 48 kHz
/// that is ≈360 blocks for a 1-minute window — so this only ever binds when a hand-edited
/// `echo_window_seconds` is enormous. 8192 blocks is ≈23 minutes and ≈256 KiB; the oldest go first, exactly
/// like the canceller's own ring.
const MAX_WINDOW_AEC_BLOCKS: usize = 8192;

/// Bounded in-memory canonical-row assembly used only for the optional final pass after every raw window is
/// already synced. Exceeding it disables that pass and leaves the existing raw note path unchanged.
const MAX_FINAL_TRANSCRIPT_BYTES: usize = 16 * 1024 * 1024;
/// Live preview must not inherit the conservative five-second offline opening. This bounded 100 ms warm-up
/// still gives the adaptive filter an opening window while making first audio available promptly.
const LIVE_AEC_LOOKAHEAD_SECONDS: f32 = 0.1;

/// A mic region longer than this (seconds) publishes immediately: residual echo the AEC could not remove is
/// decoded as a clipped fragment of what the far end just said, never as a sustained utterance (#149
/// phase 2, #107). Nothing longer is ever delayed, so a real sentence has zero added latency.
const LIVE_HOLD_MAX_REGION_SECONDS: f64 = 2.0;
/// …and neither is a region carrying more content tokens than this, whatever its duration.
const LIVE_HOLD_MAX_CONTENT_TOKENS: usize = 3;
/// Hard caps on the early-drop state, so a pathological VAD cannot make either collection grow with call
/// length. Overflow releases the oldest held region early and forgets the oldest echo source: both weaken
/// the rule for one region, neither loses a word or reorders one.
const MAX_HELD_MIC_REGIONS: usize = 64;
const MAX_LIVE_ECHO_SOURCES: usize = 256;

fn new_live_aec(sample_rate: u32, config: corti_aec::AecConfig) -> StreamingAec {
    StreamingAec::new_with_lookahead_seconds(sample_rate, config, LIVE_AEC_LOOKAHEAD_SECONDS)
}

/// How a live session ended, collected by the pipeline worker at `Process` time.
#[derive(Debug)]
pub enum LiveOutcome {
    /// The note is fully written and its state line flipped — the job can go straight to `Done`.
    Filed { note_path: PathBuf },
    /// The session ran but never produced a segment, so no note was created — run the batch path.
    NoNote,
    /// The live result is not canonical (decode failure, panic, or a lossy tee). Batch transcription must
    /// rewrite `note_path` in place when one exists, never create a second note.
    Fallback {
        reason: String,
        note_path: Option<PathBuf>,
    },
}

impl LiveOutcome {
    /// Any note the batch/discard path must take ownership of.
    fn note_path(&self) -> Option<&PathBuf> {
        match self {
            Self::Filed { note_path } => Some(note_path),
            Self::Fallback { note_path, .. } => note_path.as_ref(),
            Self::NoNote => None,
        }
    }
}

/// Capture-quality facts frozen when the recorder closes its tee. The detector delivers this verdict
/// immediately, so a later pipeline backlog cannot make the live thread guess whether its transcript was
/// lossless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FinishQuality {
    dropped_chunks: u64,
}

/// The finish/discard decision the session thread waits for after the tee disconnects.
enum Verdict {
    Finish(FinishQuality),
    Discard,
}

/// Owns live-session lifetimes. There is at most one model-backed capture actively consuming chunks;
/// finished recordings move into an ID-keyed collection until the serial pipeline collects that exact
/// result. Completed threads are collapsed to small [`LiveOutcome`] values, so their model sessions are
/// released even while an older pipeline job blocks collection.
pub struct LiveManager {
    inner: Mutex<Inner>,
    transcript: LiveTranscriptStore,
    hosted: Option<crate::postprocess_app::HostedHandle>,
}

#[derive(Default)]
struct Inner {
    /// Tee receiver stashed between `LiveHook::attach` and `LiveHook::started`.
    pending: Option<Pending>,
    /// Generation token held while the explicit microphone test owns the one resident local model slot.
    test_reservation: Option<u64>,
    /// The one session that may still consume capture chunks.
    active: Option<Active>,
    /// Finish verdict delivered, awaiting collection by recording ID.
    awaiting: HashMap<String, AwaitingFinish>,
    /// Discarded sessions remain manager-owned until their model thread and cleanup reaper finish. Keeping
    /// them in the same gate prevents a second local model from loading while discard drains its tee.
    discarding: HashMap<String, Discarding>,
}

struct Pending {
    rx: Receiver<CaptureChunk>,
    dropped: Arc<AtomicU64>,
}

struct Active {
    id: String,
    verdict_tx: Sender<Verdict>,
    handle: JoinHandle<LiveOutcome>,
    dropped: Arc<AtomicU64>,
    discard_reporter: Option<DiscardReporter>,
}

struct DiscardReporter {
    meta: RecordingMeta,
    pipe_tx: Sender<PipelineMsg>,
}

struct DiscardWork {
    id: String,
    handle: JoinHandle<LiveOutcome>,
    reporter: Option<DiscardReporter>,
}

enum Discarding {
    /// Per-discard cleanup thread successfully spawned.
    Reaper(JoinHandle<()>),
    /// Reaper spawn failed; the original live handle/reporter stay owned until the pipeline or manager
    /// performs the same cleanup.
    Inline(DiscardWork),
    /// The pipeline moved an inline fallback out to join. Keep the model gate closed until it returns.
    Collecting,
}

enum AwaitingFinish {
    /// The live thread is flushing its transcriber tails.
    Running(JoinHandle<LiveOutcome>),
    /// A collector moved the handle out to join without holding the manager mutex. Keep the single-model
    /// gate closed until that join actually returns.
    Collecting,
    /// The thread has been joined and its model/session state released.
    Ready(LiveOutcome),
}

impl Inner {
    /// Join only threads already known to be finished. `JoinHandle::is_finished` keeps this non-blocking
    /// on the detector thread; storing the small outcome allows a newer recording to start before the
    /// serial pipeline gets around to collecting the older ID.
    fn reap_completed(&mut self) {
        let finished: Vec<String> = self
            .awaiting
            .iter()
            .filter_map(|(id, session)| match session {
                AwaitingFinish::Running(handle) if handle.is_finished() => Some(id.clone()),
                AwaitingFinish::Running(_)
                | AwaitingFinish::Collecting
                | AwaitingFinish::Ready(_) => None,
            })
            .collect();
        for id in finished {
            let Some(AwaitingFinish::Running(handle)) = self.awaiting.remove(&id) else {
                continue;
            };
            self.awaiting
                .insert(id, AwaitingFinish::Ready(join_live_thread(handle)));
        }

        let discarded: Vec<String> = self
            .discarding
            .iter()
            .filter_map(|(id, session)| match session {
                Discarding::Reaper(handle) if handle.is_finished() => Some(id.clone()),
                Discarding::Inline(work) if work.handle.is_finished() => Some(id.clone()),
                Discarding::Reaper(_) | Discarding::Inline(_) | Discarding::Collecting => None,
            })
            .collect();
        for id in discarded {
            match self.discarding.remove(&id) {
                Some(Discarding::Reaper(handle)) => {
                    if handle.join().is_err() {
                        warn!(target: "corti::live", job_id = %id, "discard reaper panicked");
                    }
                }
                Some(Discarding::Inline(work)) => reap_discard_work(work),
                Some(Discarding::Collecting) | None => {}
            }
        }
    }

    fn has_finishing_thread(&self) -> bool {
        self.awaiting.values().any(|session| {
            matches!(
                session,
                AwaitingFinish::Running(_) | AwaitingFinish::Collecting
            )
        }) || !self.discarding.is_empty()
    }
}

impl Default for LiveManager {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveManager {
    pub fn new() -> Self {
        Self::with_transcript(LiveTranscriptStore::detached())
    }

    pub(crate) fn with_transcript(transcript: LiveTranscriptStore) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            transcript,
            hosted: None,
        }
    }

    pub(crate) fn with_transcript_and_hosted(
        transcript: LiveTranscriptStore,
        hosted: crate::postprocess_app::HostedHandle,
    ) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            transcript,
            hosted: Some(hosted),
        }
    }

    /// Reserve the same one-model gate detector calls use for a microphone test. A generation token makes
    /// stale test cleanup unable to release a newer reservation.
    pub(crate) fn reserve_test(&self, generation: u64) -> bool {
        let mut inner = self.inner.lock().unwrap();
        inner.reap_completed();
        if inner.pending.is_some()
            || inner.active.is_some()
            || inner.has_finishing_thread()
            || inner.test_reservation.is_some()
        {
            return false;
        }
        inner.test_reservation = Some(generation);
        true
    }

    pub(crate) fn release_test(&self, generation: u64) {
        let mut inner = self.inner.lock().unwrap();
        if inner.test_reservation == Some(generation) {
            inner.test_reservation = None;
        }
    }

    /// Reserve the one active-model slot for the tee being attached. A previous finished outcome does not
    /// block a new call, but a previous thread still flushing does: two large local model sessions must not
    /// overlap. Returns false without replacing any existing session.
    fn stash_pending(&self, rx: Receiver<CaptureChunk>, dropped: Arc<AtomicU64>) -> bool {
        let mut inner = self.inner.lock().unwrap();
        inner.reap_completed();
        if inner.pending.is_some()
            || inner.active.is_some()
            || inner.has_finishing_thread()
            || inner.test_reservation.is_some()
        {
            return false;
        }
        inner.pending = Some(Pending { rx, dropped });
        true
    }

    fn take_pending(&self) -> Option<Pending> {
        self.inner.lock().unwrap().pending.take()
    }

    /// Spawn the per-recording `corti-live` thread. No-op in a build without the local backend
    /// (eligibility already said no, so this is never reached at runtime there).
    fn spawn(
        &self,
        meta: RecordingMeta,
        sample_rate: u32,
        cfg: AppConfig,
        pending: Pending,
        pipe_tx: Sender<PipelineMsg>,
    ) {
        #[cfg(feature = "local")]
        {
            let id = corti_queue::job_id(&meta);
            self.transcript.begin_call(&id, &meta.owning_app.name);
            let (verdict_tx, verdict_rx) = std::sync::mpsc::channel::<Verdict>();
            let dropped = pending.dropped.clone();
            let discard_reporter = DiscardReporter {
                meta: meta.clone(),
                pipe_tx: pipe_tx.clone(),
            };
            let publisher = StorePublisher {
                store: self.transcript.clone(),
                id: id.clone(),
                hosted: self.hosted.clone(),
            };
            let thread = std::thread::Builder::new().name("corti-live".into()).spawn(
                move || -> LiveOutcome {
                    session_thread(
                        pending.rx,
                        verdict_rx,
                        meta,
                        sample_rate,
                        cfg,
                        pipe_tx,
                        publisher,
                    )
                },
            );
            match thread {
                Ok(handle) => {
                    let active = Active {
                        id: id.clone(),
                        verdict_tx,
                        handle,
                        dropped,
                        discard_reporter: Some(discard_reporter),
                    };
                    let rejected = {
                        let mut inner = self.inner.lock().unwrap();
                        inner.reap_completed();
                        if inner.active.is_none()
                            && !inner.has_finishing_thread()
                            && inner.test_reservation.is_none()
                        {
                            inner.active = Some(active);
                            None
                        } else {
                            Some(active)
                        }
                    };
                    if let Some(active) = rejected {
                        // `stash_pending` reserved this slot on the same detector thread, so this is only
                        // defensive. Never replace/drop an older recording's handle or outcome.
                        warn!(target: "corti::live", job_id = %id, "live-session slot changed before spawn — using batch path");
                        let mut inner = self.inner.lock().unwrap();
                        inner.reap_completed();
                        Self::park_discard(&mut inner, active);
                    }
                }
                Err(e) => {
                    self.transcript.set_error(
                        &id,
                        "Could not start the live transcript; Corti will transcribe after the call.",
                    );
                    warn!(target: "corti::live", error = %e, "could not spawn the live transcription thread — batch path will run");
                }
            }
        }
        #[cfg(not(feature = "local"))]
        {
            let _ = (meta, sample_rate, cfg, pending, pipe_tx);
        }
    }

    fn park_discard(inner: &mut Inner, active: Active) {
        let id = active.id;
        info!(target: "corti::live", job_id = %id, "discarding live session");
        let _ = active.verdict_tx.send(Verdict::Discard);
        let previous = inner.discarding.insert(
            id.clone(),
            spawn_discard_reaper(DiscardWork {
                id,
                handle: active.handle,
                reporter: active.discard_reporter,
            }),
        );
        debug_assert!(previous.is_none(), "recording ids must be unique");
    }

    /// Deliver a recording-specific finish verdict without joining. Called by the detector immediately
    /// after capture closes the tee and before it emits `RecordingFinished`; the serial pipeline later
    /// calls [`collect`](Self::collect) for this exact ID.
    pub fn finish(&self, id: &str) {
        self.transcript
            .set_stopping(id, "Finishing the last speech region…");
        let mut inner = self.inner.lock().unwrap();
        inner.reap_completed();
        let Some(active) = inner.active.take_if(|active| active.id == id) else {
            return;
        };
        let dropped_chunks = active.dropped.load(Ordering::Relaxed);
        if dropped_chunks > 0 {
            warn!(
                target: "corti::live",
                job_id = %id,
                dropped_chunks,
                "live tee dropped chunks — finishing tails but requiring lossless batch fallback"
            );
        }
        let _ = active
            .verdict_tx
            .send(Verdict::Finish(FinishQuality { dropped_chunks }));
        let replaced = inner
            .awaiting
            .insert(id.to_string(), AwaitingFinish::Running(active.handle));
        debug_assert!(replaced.is_none(), "recording ids must be unique");
        inner.reap_completed();
    }

    /// Collect the finished live result for exactly `id`. This is the only blocking join path and runs on
    /// the pipeline worker; a newer active recording remains untouched. `None` means this recording did not
    /// have an eligible live session (or its finish verdict was never delivered).
    pub fn collect(&self, id: &str) -> Option<LiveOutcome> {
        enum Collection {
            Join(JoinHandle<LiveOutcome>),
            Ready(LiveOutcome),
        }

        let collection = {
            let mut inner = self.inner.lock().unwrap();
            inner.reap_completed();
            match inner.awaiting.get(id)? {
                AwaitingFinish::Running(_) => {
                    let previous = inner
                        .awaiting
                        .insert(id.to_string(), AwaitingFinish::Collecting);
                    let Some(AwaitingFinish::Running(handle)) = previous else {
                        unreachable!("entry changed while the manager mutex was held")
                    };
                    Collection::Join(handle)
                }
                AwaitingFinish::Collecting => return None,
                AwaitingFinish::Ready(_) => {
                    let Some(AwaitingFinish::Ready(outcome)) = inner.awaiting.remove(id) else {
                        unreachable!("entry changed while the manager mutex was held")
                    };
                    Collection::Ready(outcome)
                }
            }
        };

        Some(match collection {
            Collection::Ready(outcome) => outcome,
            Collection::Join(handle) => {
                let outcome = join_live_thread(handle);
                let removed = self.inner.lock().unwrap().awaiting.remove(id);
                debug_assert!(matches!(removed, Some(AwaitingFinish::Collecting)));
                outcome
            }
        })
    }

    /// Deliver a discard verdict and transfer the handle to manager-owned cleanup. Normal discard uses a
    /// tiny reaper thread; if spawning it fails, the original handle/reporter remain in `Inline` for the
    /// pipeline. Either state keeps the single-model gate closed without blocking the detector callback.
    pub fn discard(&self, id: &str) {
        self.transcript
            .set_stopping(id, "The short recording is being discarded…");
        let mut inner = self.inner.lock().unwrap();
        inner.reap_completed();
        let Some(active) = inner.active.take_if(|active| active.id == id) else {
            return;
        };
        Self::park_discard(&mut inner, active);
    }

    /// Pipeline fallback for the rare per-discard reaper spawn failure. Joining may block here (never on the
    /// detector callback), while a `Collecting` sentinel keeps the model gate closed.
    pub(crate) fn reap_unspawned_discard(&self, id: &str) {
        let work = {
            let mut inner = self.inner.lock().unwrap();
            inner.reap_completed();
            let Some(Discarding::Inline(work)) = inner.discarding.remove(id) else {
                return;
            };
            inner
                .discarding
                .insert(id.to_string(), Discarding::Collecting);
            work
        };
        reap_discard_work(work);
        let removed = self.inner.lock().unwrap().discarding.remove(id);
        debug_assert!(matches!(removed, Some(Discarding::Collecting)));
    }

    /// Whether a live note for `id` still belongs to a session that will be collected. Includes a
    /// finish-delivered session (not just the currently active capture), because its final flush may create
    /// the first note while `Process` is waiting behind another pipeline job. Discarded sessions are false.
    pub fn accepts_note(&self, id: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        inner.reap_completed();
        inner.active.as_ref().is_some_and(|active| active.id == id)
            || inner.awaiting.contains_key(id)
    }
}

fn join_live_thread(handle: JoinHandle<LiveOutcome>) -> LiveOutcome {
    handle.join().unwrap_or_else(|_| LiveOutcome::Fallback {
        reason: "live transcription thread panicked".to_string(),
        note_path: None,
    })
}

fn reap_discard_work(work: DiscardWork) {
    let DiscardWork {
        id,
        handle,
        reporter,
    } = work;
    let outcome = join_live_thread(handle);
    if let Some(path) = outcome.note_path().cloned() {
        match std::fs::remove_file(&path) {
            Ok(()) => info!(
                target: "corti::live",
                job_id = %id,
                note_path = %path.display(),
                "deleted partial live note while reaping discarded session"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                warn!(
                    target: "corti::live",
                    job_id = %id,
                    note_path = %path.display(),
                    error = %e,
                    "could not delete partial live note while reaping discarded session"
                );
                if let Some(reporter) = reporter
                    && reporter
                        .pipe_tx
                        .send(PipelineMsg::LiveDiscardCleanup {
                            meta: reporter.meta,
                            note_path: path,
                            error: e.to_string(),
                        })
                        .is_err()
                {
                    warn!(
                        target: "corti::live",
                        job_id = %id,
                        "pipeline unavailable; failed discard path could not be persisted"
                    );
                }
            }
        }
    }
}

fn spawn_discard_reaper(work: DiscardWork) -> Discarding {
    spawn_discard_reaper_with(work, |run| {
        std::thread::Builder::new()
            .name("corti-live-reap".into())
            .spawn(run)
    })
}

fn spawn_discard_reaper_with(
    work: DiscardWork,
    launch: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<JoinHandle<()>>,
) -> Discarding {
    // `Builder::spawn` consumes and drops its closure on failure. Keep the actual ownership in a shared
    // slot so the detector can recover it instead of detaching the live handle and losing its note path.
    let log_id = work.id.clone();
    let slot = Arc::new(Mutex::new(Some(work)));
    let worker_slot = slot.clone();
    match launch(Box::new(move || {
        if let Some(work) = worker_slot.lock().unwrap().take() {
            reap_discard_work(work);
        }
    })) {
        Ok(handle) => Discarding::Reaper(handle),
        Err(error) => {
            warn!(
                target: "corti::live",
                job_id = %log_id,
                error = %error,
                "could not spawn discarded-session reaper; pipeline retains cleanup ownership"
            );
            let work = slot
                .lock()
                .unwrap()
                .take()
                .expect("failed spawn must return the unstarted discard work");
            if let Some(reporter) = work.reporter.as_ref()
                && reporter
                    .pipe_tx
                    .send(PipelineMsg::LiveDiscardReap {
                        id: work.id.clone(),
                    })
                    .is_err()
            {
                warn!(
                    target: "corti::live",
                    job_id = %work.id,
                    "pipeline unavailable; manager will retain failed reaper ownership"
                );
            }
            Discarding::Inline(work)
        }
    }
}

/// The app-side factory the detector consults at every recording start (`corti_detect::LiveHook`).
pub struct AppLiveHook {
    manager: Arc<LiveManager>,
    config: SharedConfig,
    pipe_tx: Sender<PipelineMsg>,
}

impl AppLiveHook {
    pub fn new(
        manager: Arc<LiveManager>,
        config: SharedConfig,
        pipe_tx: Sender<PipelineMsg>,
    ) -> Self {
        Self {
            manager,
            config,
            pipe_tx,
        }
    }
}

impl corti_detect::LiveHook for AppLiveHook {
    fn attach(&self, app: &corti_core::OwningApp) -> Option<CaptureTee> {
        let cfg = self.config.lock().unwrap().clone();
        if let Err(reason) = live_eligible(&cfg) {
            info!(target: "corti::live", app = %app.name, reason, "live filing skipped — batch path will run");
            return None;
        }
        // Models must already be on disk (cheap file-existence checks; no engine load here — `attach`
        // runs on the detect worker and must not delay Recorder::start).
        #[cfg(feature = "local")]
        if let Err(e) = discover_models(&cfg) {
            info!(
                target: "corti::live",
                app = %app.name,
                error = %format!("{e:#}"),
                "live filing skipped — local models unavailable; batch path will run"
            );
            return None;
        }
        let (tx, rx) = sync_channel::<CaptureChunk>(TEE_BACKLOG);
        let tee = CaptureTee::new(tx);
        if !self.manager.stash_pending(rx, tee.dropped_counter()) {
            info!(
                target: "corti::live",
                app = %app.name,
                "live filing skipped — the previous model session is still finishing"
            );
            return None;
        }
        Some(tee)
    }

    /// Read straight from the shared runtime config, so toggling AEC in Settings takes effect on the next
    /// recording without any reload plumbing. Independent of `attach`: in-flight AEC (#74) applies to every
    /// recording, live-filed or batch.
    fn aec_config(&self) -> Option<corti_capture::AecConfig> {
        let cfg = self.config.lock().unwrap();
        cfg.aec_enabled.then(|| cfg.aec_config())
    }

    fn started(&self, meta: &RecordingMeta, sample_rate: u32) {
        let Some(pending) = self.manager.take_pending() else {
            warn!(target: "corti::live", "live hook started() without a pending tee — batch path will run");
            return;
        };
        let cfg = self.config.lock().unwrap().clone();
        self.manager.spawn(
            meta.clone(),
            sample_rate,
            cfg,
            pending,
            self.pipe_tx.clone(),
        );
    }

    fn finished(&self, meta: &RecordingMeta) {
        self.manager.finish(&corti_queue::job_id(meta));
    }

    fn discarded(&self, meta: &RecordingMeta) {
        self.manager.discard(&corti_queue::job_id(meta));
    }

    fn aborted(&self) {
        self.manager.take_pending();
    }

    fn failed(&self, meta: &RecordingMeta) {
        // Capture could not finish, so no `RecordingFinished`/`Process` will follow. Deliver the discard
        // verdict here on the detector worker before asking the serial pipeline to close any queue row.
        let id = corti_queue::job_id(meta);
        self.manager.discard(&id);
        let _ = self.pipe_tx.send(PipelineMsg::LiveDiscarded { id });
    }
}

/// Pure config-level eligibility for live filing (the on-disk models check happens after). Returning
/// `Err(reason)` means the recording silently takes today's batch path.
fn live_eligible(cfg: &AppConfig) -> Result<(), &'static str> {
    if !cfg.live_filing {
        return Err("live_filing is off");
    }
    if cfg.transcribe_backend != BackendChoice::Local {
        return Err("transcribe backend is not local");
    }
    if !cfg!(feature = "local") {
        return Err("local backend not compiled into this build");
    }
    Ok(())
}

/// Cheap file-existence validation of exactly the selected local ASR/VAD/diarization artifacts (no engine
/// load). In particular, GGML uses its GGUF and does not require the legacy Parakeet ONNX files.
#[cfg(feature = "local")]
fn discover_models(cfg: &AppConfig) -> Result<()> {
    use corti_transcribe_local::{LocalConfig, LocalTranscriber};

    LocalTranscriber::new(LocalConfig {
        model_dir: cfg.local_model_dir.clone(),
        diarize_far_end: cfg.local_diarize_far_end,
        embedding_model: cfg.local_embedding_model.clone(),
        asr_engine: cfg.local_asr_engine.clone(),
        ggml_model: cfg.local_ggml_model.clone(),
        ..LocalConfig::default()
    })
    .validate_models()
}

// ----- The per-recording session thread -----

/// Thread body: run the session with panics contained; any failure carries the partial note path so
/// the caller can persist (fallback) or delete (discard) it.
#[cfg(feature = "local")]
fn session_thread(
    rx: Receiver<CaptureChunk>,
    verdict_rx: Receiver<Verdict>,
    meta: RecordingMeta,
    sample_rate: u32,
    cfg: AppConfig,
    pipe_tx: Sender<PipelineMsg>,
    publisher: StorePublisher,
) -> LiveOutcome {
    // Hosted session setup is off capture/ASR and fails closed. Raw publication and filing do not depend on
    // this reply; the bounded row handoff below simply remains unavailable for this recording.
    if let Some(hosted) = publisher.hosted.as_ref() {
        let _ = hosted.begin_live_session(&publisher.id);
    }
    let provenance =
        crate::provenance::from_config(&cfg, corti_vagus::provenance::GenerationMode::Live);
    let mut writer =
        NoteWriter::with_provenance(VagusFiler, meta.clone(), Some(pipe_tx), provenance);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_session(&rx, &verdict_rx, sample_rate, &cfg, &mut writer, &publisher)
    }));
    let outcome = match result {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(e)) => LiveOutcome::Fallback {
            reason: format!("{e:#}"),
            note_path: writer.path().cloned(),
        },
        Err(_) => LiveOutcome::Fallback {
            reason: "live transcription panicked".to_string(),
            note_path: writer.path().cloned(),
        },
    };
    match &outcome {
        LiveOutcome::Filed { .. } => publisher.complete("Call complete — transcript filed."),
        LiveOutcome::NoNote => publisher.complete("Call complete — no note was filed."),
        LiveOutcome::Fallback { reason, .. } => publisher.error(format!(
            "Live transcript stopped ({reason}); Corti will rebuild it from the recording."
        )),
    }
    if let Some(hosted) = publisher.hosted.as_ref() {
        // Final places the same FIFO fence when enabled. When it is off, flush the ASR tail here so the
        // priority EndSession command cannot clear the ledger before the last nonblocking row arrives.
        let _ = hosted.flush_finalized_rows();
        let _ = hosted.end_live_session(&publisher.id);
    }
    outcome
}

/// Everything a running session owns besides the writer, so an engine/consume error can be parked
/// while the parts (and the verdict logic) stay in one place.
#[cfg(feature = "local")]
struct SessionParts {
    engine: corti_transcribe_local::LiveEngine,
    mic: corti_transcribe_local::LiveTranscriber,
    them: corti_transcribe_local::LiveTranscriber,
    aec: Option<StreamingAec>,
    window: TranscriptWindow,
}

/// Build the engine + per-channel state on the live thread — chunks buffer in the bounded tee
/// meanwhile.
#[cfg(feature = "local")]
fn build_parts(sample_rate: u32, cfg: &AppConfig) -> Result<SessionParts> {
    use corti_transcribe_local::{LocalConfig, LocalTranscriber};

    let local_cfg = LocalConfig {
        model_dir: cfg.local_model_dir.clone(),
        num_threads: cfg.local_threads,
        diarize_far_end: cfg.local_diarize_far_end,
        embedding_model: cfg.local_embedding_model.clone(),
        diarize_threshold: cfg.local_diarize_threshold,
        asr_engine: cfg.local_asr_engine.clone(),
        ggml_model: cfg.local_ggml_model.clone(),
        ..LocalConfig::default()
    };
    let engine = LocalTranscriber::new(local_cfg)
        .live_engine()
        .context("loading the local live engine")?;
    let mic = engine.channel().context("building the mic transcriber")?;
    let them = engine.channel().context("building the tap transcriber")?;
    let window = TranscriptWindow::new(
        sample_rate,
        cfg.live_buffer_minutes,
        engine.diarizes_far_end(),
        cfg.cleanup_config(),
    )
    .context("reserving the bounded live transcript window")?;
    Ok(SessionParts {
        engine,
        mic,
        them,
        // Streaming AEC on the mic, per config (skipped cleanly per-chunk when the mic side is empty).
        aec: cfg
            .aec_enabled
            .then(|| new_live_aec(sample_rate, cfg.aec_config())),
        window,
    })
}

/// Load the engine and consume tee chunks, then act on the finish/discard verdict. Any error is
/// **held until the verdict arrives** — the thread must outlive its recording so a Discard can still
/// delete the note (the manager's discard path is non-joining); the bounded tee keeps dropping
/// chunks meanwhile, so the capture writer is never blocked by a parked session.
#[cfg(feature = "local")]
fn run_session(
    rx: &Receiver<CaptureChunk>,
    verdict_rx: &Receiver<Verdict>,
    sample_rate: u32,
    cfg: &AppConfig,
    writer: &mut NoteWriter<VagusFiler>,
    publisher: &StorePublisher,
) -> Result<LiveOutcome> {
    let mut parts = build_parts(sample_rate, cfg);
    let consumed = match parts.as_mut() {
        Ok(p) => {
            publisher.listening();
            consume_chunks(
                rx,
                sample_rate,
                &mut p.aec,
                &mut p.mic,
                &mut p.them,
                &p.engine,
                &mut p.window,
                writer,
                publisher,
            )
        }
        // Engine failed to load: surface it to the reader now, but still wait for the recording-specific
        // verdict so discard/fallback ownership remains unchanged.
        Err(error) => {
            publisher.error(format!(
                "Live transcript unavailable ({error:#}); Corti will transcribe after the call."
            ));
            Ok(())
        }
    };
    if let Err(error) = &consumed {
        publisher.error(format!(
            "Live transcript stopped ({error:#}); Corti will transcribe after the call."
        ));
    }

    match verdict_rx.recv() {
        Ok(Verdict::Finish(quality)) => {
            let p = parts?;
            consumed?;
            finish_session(
                sample_rate,
                p.aec,
                p.mic,
                p.them,
                p.engine,
                p.window,
                quality,
                writer,
                publisher,
            )
        }
        Ok(Verdict::Discard) => {
            writer.discard()?;
            Ok(LiveOutcome::NoNote)
        }
        // Manager gone (app shutting down mid-call): leave whatever was written; don't flip.
        Err(_) => anyhow::bail!("live session received no finish/discard verdict"),
    }
}

/// Per-recording rolling state. Every collection is bounded by either the configured time window or an
/// independent byte cap; after a successful durable append, lengths reset to zero and allocations are reused.
struct TranscriptWindow {
    sample_rate: u32,
    frame_limit: u64,
    start_frame: u64,
    frames: u64,
    mic_words: Vec<Word>,
    them_words: Vec<Word>,
    tap_audio: Vec<f32>,
    buffered_text_bytes: usize,
    diarize_far_end: bool,
    /// Deterministic segment cleanup applied to each window before its one append (#149).
    cleanup: CleanupConfig,
    /// The tail of the **previous** window's appended segments — every one whose end is still inside
    /// `echo_window_seconds` of this window's start. They are read-only echo sources: an echo whose source
    /// landed just before the one-minute boundary is still caught. Never appended (they are already in the
    /// note) and never mutated. Bounded by one window's segment count, so the memory contract is unchanged.
    carry: Vec<TranscriptSegment>,
    /// Live early drop (#149 phase 2). Per-recording state like `carry`, not per-window: a mic region
    /// withheld just before a boundary is still judged against far-end regions that close after it.
    early_drop: EarlyDrop,
    /// The live canceller's per-block echo record covering this window plus `echo_window_seconds` of
    /// lookback (#149 phase 3b), drained from `StreamingAec` after every push and trimmed at every flush.
    /// `t_start_secs` is on the cleaned timeline — the same timeline these segments are timestamped on —
    /// so a block time and a segment time compare directly. Empty when AEC is off, which simply leaves the
    /// echo pass with its text rules.
    aec_blocks: Vec<corti_aec::BlockStats>,
}

impl TranscriptWindow {
    fn new(
        sample_rate: u32,
        minutes: u32,
        diarize_far_end: bool,
        cleanup: CleanupConfig,
    ) -> Result<Self> {
        let sample_rate = sample_rate.max(1);
        let configured = u64::from(sample_rate)
            .saturating_mul(u64::from(minutes.max(1)))
            .saturating_mul(60)
            .max(1);
        let audio_cap_frames = (MAX_DIARIZATION_AUDIO_BYTES / std::mem::size_of::<f32>()) as u64;
        let frame_limit = if diarize_far_end {
            configured.min(audio_cap_frames).max(1)
        } else {
            configured
        };
        // Exact fallible preallocation makes the advertised audio cap an allocation cap, not merely a
        // length cap; ordinary geometric Vec growth could otherwise reserve almost twice the active window.
        // Allocation pressure degrades to batch fallback instead of panicking the live thread.
        let mut tap_audio = Vec::new();
        if diarize_far_end {
            tap_audio
                .try_reserve_exact(frame_limit as usize)
                .context("reserving far-end diarization audio")?;
        }
        Ok(Self {
            sample_rate,
            frame_limit,
            start_frame: 0,
            frames: 0,
            mic_words: Vec::new(),
            them_words: Vec::new(),
            tap_audio,
            buffered_text_bytes: 0,
            diarize_far_end,
            early_drop: EarlyDrop::new(cleanup.clone()),
            cleanup,
            carry: Vec::new(),
            aec_blocks: Vec::new(),
        })
    }

    /// Seconds from call start at which the *next* window begins.
    fn next_start_sec(&self) -> f64 {
        self.start_frame.saturating_add(self.frames) as f64 / f64::from(self.sample_rate)
    }

    /// Retain the appended segments that are still inside the echo window as sources for the next flush.
    /// Called after every successful append (including an empty one, which correctly clears a stale carry).
    fn remember_carry(&mut self, appended: &[TranscriptSegment]) {
        let horizon = self.next_start_sec() - self.cleanup.echo_window_seconds;
        self.carry.clear();
        self.carry
            .extend(appended.iter().filter(|s| s.end >= horizon).cloned());
    }

    /// Take the blocks a `StreamingAec::block_stats()` drain produced. Ordered by `t_start_secs` because
    /// the drain is, and the two clocks (window frames, canceller blocks) advance together.
    fn push_aec_blocks(&mut self, blocks: Vec<corti_aec::BlockStats>) {
        self.aec_blocks.extend(blocks);
        if self.aec_blocks.len() > MAX_WINDOW_AEC_BLOCKS {
            let excess = self.aec_blocks.len() - MAX_WINDOW_AEC_BLOCKS;
            self.aec_blocks.drain(..excess);
        }
    }

    /// Drop the blocks that can no longer be evidence for anything: older than `echo_window_seconds`
    /// before the window that is about to start. Called from `clear_after_flush`, so the retained set is
    /// bounded by the same time window that bounds the carry.
    fn trim_aec_blocks(&mut self) {
        let horizon = self.start_sec() - self.cleanup.echo_window_seconds;
        self.aec_blocks.retain(|b| b.t_start_secs >= horizon);
    }

    fn remaining_frames(&self) -> usize {
        self.frame_limit.saturating_sub(self.frames).max(1) as usize
    }

    fn push_audio(&mut self, tap: &[f32], frames: usize) {
        if self.diarize_far_end {
            self.tap_audio.extend_from_slice(tap);
        }
        self.frames = self.frames.saturating_add(frames as u64);
    }

    fn push_mic_words(&mut self, words: Vec<Word>) {
        self.buffered_text_bytes = self
            .buffered_text_bytes
            .saturating_add(buffered_word_bytes(&words));
        self.mic_words.extend(words);
    }

    fn push_them_words(&mut self, words: Vec<Word>) {
        self.buffered_text_bytes = self
            .buffered_text_bytes
            .saturating_add(buffered_word_bytes(&words));
        self.them_words.extend(words);
    }

    fn due(&self) -> bool {
        self.frames >= self.frame_limit || self.buffered_text_bytes >= MAX_BUFFERED_TRANSCRIPT_BYTES
    }

    fn start_sec(&self) -> f64 {
        self.start_frame as f64 / f64::from(self.sample_rate)
    }

    fn clear_after_flush(&mut self) {
        self.start_frame = self.start_frame.saturating_add(self.frames);
        self.frames = 0;
        self.mic_words.clear();
        self.them_words.clear();
        self.tap_audio.clear();
        self.buffered_text_bytes = 0;
        self.trim_aec_blocks();
    }

    #[cfg(test)]
    fn retained_bytes(&self) -> usize {
        self.tap_audio.capacity() * std::mem::size_of::<f32>() + self.buffered_text_bytes
    }
}

fn buffered_word_bytes(words: &[Word]) -> usize {
    words.iter().fold(0usize, |total, word| {
        total
            .saturating_add(std::mem::size_of::<Word>())
            .saturating_add(word.text.len())
    })
}

/// One decoded mic region withheld from the publisher until the far-end channel can be asked about it.
struct HeldMicRegion {
    /// The region's words, untouched — a late publication carries the original timestamps.
    words: Vec<Word>,
    candidate: EchoCandidate,
    /// Call time (seconds) at which the hold expires even if the far end never closes a region.
    deadline: f64,
}

/// What the live early drop did over one session. Logged once at the end, next to the per-window
/// [`CleanupStats`], so a reader who noticed a missing row can tell a drop from a decode failure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct EarlyDropStats {
    /// Regions withheld at least once (a held region is later either published or dropped).
    held: usize,
    released_published: usize,
    released_dropped: usize,
}

/// Live region-level early drop of far-end echo (#149 phase 2).
///
/// [`cleanup`] runs at the one-minute durability boundary, which is early enough for the note and far too
/// late for the Live Transcript window and the hosted Live lane: both see every closed VAD region as it
/// falls out of the decoder, ghosts included. Neither can be corrected afterwards — a published row is
/// immutable by construction. `LiveTranscriptStore` only ever appends a row or overlays `clean_text` on
/// one, the delta protocol the reader applies can add a row or reset the session but not remove a row, and
/// the hosted coordinator's watermark (rows, words, covered speech) only counts up. So the ghost has to be
/// stopped *before* it is published, and the only way to do that is to wait.
///
/// The wait is bounded and only short regions pay it. A mic region of at most
/// [`LIVE_HOLD_MAX_REGION_SECONDS`] or [`LIVE_HOLD_MAX_CONTENT_TOKENS`] is held; anything longer is a
/// sentence, not residual echo, and publishes immediately. A held region is released as soon as the far end
/// closes a region (which is what makes it judgeable) or `echo_window_seconds` of call time have passed
/// since it ended, whichever comes first — so the worst case added latency is `echo_window_seconds`, 6 s by
/// default, and the typical case is the far end drawing breath.
///
/// At release the phase-1 rule decides, through the same [`EchoCandidate`] the window pass uses: published
/// with its original words and timestamps, or dropped and counted. Only Them→Me is judged here; the
/// far-end channel is never delayed.
///
/// Publication is strictly FIFO. A long region arriving while something is held releases what is held
/// first, so a late row can never overtake an earlier mic row.
struct EarlyDrop {
    cfg: CleanupConfig,
    /// Recently closed far-end regions, oldest first, as read-only echo sources.
    sources: VecDeque<EchoCandidate>,
    /// Withheld mic regions, oldest first.
    held: VecDeque<HeldMicRegion>,
    stats: EarlyDropStats,
}

impl EarlyDrop {
    fn new(cfg: CleanupConfig) -> Self {
        Self {
            cfg,
            sources: VecDeque::new(),
            held: VecDeque::new(),
            stats: EarlyDropStats::default(),
        }
    }

    /// The pass is gated by the same `echo_drop` knob the window pass is: one rule, one switch.
    fn enabled(&self) -> bool {
        self.cfg.echo_drop
    }

    /// Nothing is waiting — the invariant that must hold before every durable append.
    fn is_idle(&self) -> bool {
        self.held.is_empty()
    }

    /// Residual echo is a clipped fragment; a region that is neither brief nor nearly wordless is speech.
    fn is_short(&self, candidate: &EchoCandidate) -> bool {
        candidate.end() - candidate.start() <= LIVE_HOLD_MAX_REGION_SECONDS
            || candidate.content_tokens() <= LIVE_HOLD_MAX_CONTENT_TOKENS
    }

    /// A mic poll produced words. Returns the word groups to publish now, in order.
    fn offer_mic(&mut self, words: Vec<Word>, now: f64) -> Vec<Vec<Word>> {
        if !self.enabled() || words.is_empty() {
            return vec![words];
        }
        let regions = split_regions(&words, SEGMENT_GAP);
        if regions.is_empty() {
            return vec![words];
        }
        let mut out = Vec::with_capacity(regions.len());
        for region in regions {
            let candidate = EchoCandidate::from_words(true, &region);
            if !self.is_short(&candidate) {
                // A sentence publishes now — but never ahead of a region that arrived before it.
                out.append(&mut self.drain_held());
                out.push(region);
                continue;
            }
            let deadline = candidate.end() + self.cfg.echo_window_seconds;
            self.held.push_back(HeldMicRegion {
                words: region,
                candidate,
                deadline,
            });
            self.stats.held += 1;
            // Everything popped here is older than what was just pushed, so order still holds.
            while self.held.len() > MAX_HELD_MIC_REGIONS {
                self.release_oldest(&mut out);
            }
        }
        self.prune_sources(now);
        out
    }

    /// A far-end poll produced words: they become echo sources, and a closed far-end region is exactly the
    /// event a held region was waiting for. Returns the mic word groups this released.
    fn offer_them(&mut self, words: &[Word], now: f64) -> Vec<Vec<Word>> {
        if !self.enabled() || words.is_empty() {
            return Vec::new();
        }
        for region in split_regions(words, SEGMENT_GAP) {
            self.sources
                .push_back(EchoCandidate::from_words(false, &region));
        }
        let released = self.drain_held();
        self.prune_sources(now);
        released
    }

    /// Call time advanced. Releases every hold whose deadline has passed; deadlines are non-decreasing, so
    /// the front of the queue is always the next to expire.
    fn due(&mut self, now: f64) -> Vec<Vec<Word>> {
        if !self.enabled() || self.held.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        while self.held.front().is_some_and(|h| h.deadline <= now) {
            self.release_oldest(&mut out);
        }
        self.prune_sources(now);
        out
    }

    /// Release everything, whatever the deadline: a durable append is about to render the note, or the
    /// session is over. Nothing is lost, only judged with the sources that exist by then.
    fn release_all(&mut self) -> Vec<Vec<Word>> {
        if !self.enabled() {
            return Vec::new();
        }
        self.drain_held()
    }

    fn drain_held(&mut self) -> Vec<Vec<Word>> {
        let mut out = Vec::with_capacity(self.held.len());
        while !self.held.is_empty() {
            self.release_oldest(&mut out);
        }
        out
    }

    /// Judge the oldest held region against the far-end ring and either append its words to `out` or count
    /// the drop.
    fn release_oldest(&mut self, out: &mut Vec<Vec<Word>>) {
        let Some(held) = self.held.pop_front() else {
            return;
        };
        let echo = self
            .sources
            .iter()
            .any(|source| held.candidate.is_echo_of(source, &self.cfg));
        if echo {
            self.stats.released_dropped += 1;
        } else {
            self.stats.released_published += 1;
            out.push(held.words);
        }
    }

    /// A far-end region can still source an echo up to `echo_window_seconds` after it ends, for any subject
    /// that has already been decoded — so sources are kept relative to the oldest thing still to be judged,
    /// not to the clock.
    fn prune_sources(&mut self, now: f64) {
        let floor = self.held.front().map_or(now, |h| h.candidate.start());
        let horizon = floor - self.cfg.echo_window_seconds;
        self.sources.retain(|source| source.end() >= horizon);
        while self.sources.len() > MAX_LIVE_ECHO_SOURCES {
            self.sources.pop_front();
        }
    }
}

/// Publish one decoded mic region, or withhold it until the far end has had its chance to prove it was an
/// echo (#149 phase 2). Publication and the note's buffer move together, so the reader and the note never
/// disagree about which regions existed.
fn publish_mic_words<P: TranscriptPublisher>(
    window: &mut TranscriptWindow,
    publisher: &P,
    words: Vec<Word>,
) {
    let now = window.next_start_sec();
    let released = window.early_drop.offer_mic(words, now);
    emit_mic_words(window, publisher, released);
}

/// Publish one decoded far-end region. The far end is never delayed; its closing is what releases held mic
/// regions, and it becomes an echo source for the ones still to come.
fn publish_them_words<P: TranscriptPublisher>(
    window: &mut TranscriptWindow,
    publisher: &P,
    words: Vec<Word>,
) {
    let now = window.next_start_sec();
    let released = window.early_drop.offer_them(&words, now);
    publisher.words(Speaker::Other("Them".to_string()), &words);
    window.push_them_words(words);
    emit_mic_words(window, publisher, released);
}

/// Call time advanced: a hold that no far-end region closed out still expires on its own.
fn release_due_mic_words<P: TranscriptPublisher>(window: &mut TranscriptWindow, publisher: &P) {
    let now = window.next_start_sec();
    let released = window.early_drop.due(now);
    emit_mic_words(window, publisher, released);
}

/// Release everything still held. Required before every [`flush_window`]: a held region is not in
/// `window.mic_words` yet, so leaving one behind would drop it from the note.
fn release_held_mic_words<P: TranscriptPublisher>(window: &mut TranscriptWindow, publisher: &P) {
    let released = window.early_drop.release_all();
    emit_mic_words(window, publisher, released);
}

fn emit_mic_words<P: TranscriptPublisher>(
    window: &mut TranscriptWindow,
    publisher: &P,
    released: Vec<Vec<Word>>,
) {
    for words in released {
        publisher.words(Speaker::Me, &words);
        window.push_mic_words(words);
    }
}

/// macOS drops Corti's TCC file-access grants whenever an ad-hoc-signed upgrade changes its code identity
/// (ADR 0006), so vault writes can start failing mid-call while capture keeps its freshly re-prompted mic
/// grant. Name the remedy: the raw `Operation not permitted` path is what reaches the tray, truncated.
fn degraded_detail(error: &anyhow::Error) -> String {
    let denied = error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
    });
    if denied {
        "Corti can't write to your vault — macOS revoked its file access, which happens after an update. \
         Re-grant Full Disk Access in System Settings › Privacy & Security, then restart Corti."
            .to_string()
    } else {
        format!("Live transcript paused ({error:#}); Corti will rebuild it after the call.")
    }
}

/// Drain tee chunks until the sender (the capture writer) hangs up. Input chunks are split exactly at the
/// rolling boundary, so even a surprising producer chunk cannot push retained audio beyond the configured/
/// hard limit. AEC and ASR remain chunk-agnostic; only a full window forces their current tails final.
#[allow(clippy::too_many_arguments)] // the split keeps every piece testable without models
fn consume_chunks<C: LiveChannel, D: LiveDiarizer, F: NoteFiler, P: TranscriptPublisher>(
    rx: &Receiver<CaptureChunk>,
    sample_rate: u32,
    aec: &mut Option<StreamingAec>,
    mic: &mut C,
    them: &mut C,
    diarizer: &D,
    window: &mut TranscriptWindow,
    writer: &mut NoteWriter<F>,
    publisher: &P,
) -> Result<()> {
    let mut dropped_windows = 0u32;
    while let Ok(chunk) = rx.recv() {
        let chunk_frames = chunk.mic.len().max(chunk.tap.len());
        let mut offset = 0usize;
        while offset < chunk_frames {
            let take = window.remaining_frames().min(chunk_frames - offset);
            let end = offset + take;
            let mic_slice = if chunk.mic.len() == chunk_frames {
                &chunk.mic[offset..end]
            } else {
                &[]
            };
            let tap_slice = if chunk.tap.len() == chunk_frames {
                &chunk.tap[offset..end]
            } else {
                &[]
            };

            let clean = match aec.as_mut() {
                Some(aec) if !mic_slice.is_empty() && mic_slice.len() == tap_slice.len() => {
                    let cleaned = aec.push(mic_slice, tap_slice);
                    // --- #149 phase 3b: hand the window this push's per-block echo record. ---
                    window.push_aec_blocks(aec.block_stats());
                    // --- end phase 3b ---
                    cleaned
                }
                Some(_) => Vec::new(),
                None => mic_slice.to_vec(),
            };
            if !clean.is_empty() {
                mic.push(&clean, sample_rate);
            }
            if !tap_slice.is_empty() {
                them.push(tap_slice, sample_rate);
            }
            if let Some(words) = mic.poll_words() {
                publish_mic_words(window, publisher, words);
            }
            if let Some(words) = them.poll_words() {
                publish_them_words(window, publisher, words);
            }
            window.push_audio(tap_slice, take);
            // Call time advanced, so a hold no far-end region closed out can expire on its own (#149).
            release_due_mic_words(window, publisher);
            offset = end;

            // A failed checkpoint degrades the session instead of ending it: filing is best-effort and the
            // post-call batch pass rebuilds from the WAV, so retrying on the next window lets access
            // restored mid-call recover on its own.
            if window.due() {
                match checkpoint_and_flush(mic, them, diarizer, window, writer, publisher) {
                    Ok(()) if dropped_windows > 0 => {
                        info!(target: "corti::live", dropped_windows, "live filing recovered");
                        dropped_windows = 0;
                        publisher.listening();
                    }
                    Ok(()) => {}
                    Err(error) => {
                        dropped_windows += 1;
                        let detail = format!("{error:#}");
                        warn!(
                            target: "corti::live",
                            dropped_windows,
                            %detail,
                            "live checkpoint failed; dropping this window"
                        );
                        // Clearing bounds the window when the failure persists for the rest of the call;
                        // the incomplete mark keeps the batch rewrite from skipping the resulting gap.
                        window.clear_after_flush();
                        writer.mark_incomplete();
                        // Leading edge only — a permission failure recurs every minute and would
                        // otherwise rewrite the tray until the call ends.
                        if dropped_windows == 1 {
                            publisher.error(degraded_detail(&error));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn checkpoint_and_flush<C: LiveChannel, D: LiveDiarizer, F: NoteFiler, P: TranscriptPublisher>(
    mic: &mut C,
    them: &mut C,
    diarizer: &D,
    window: &mut TranscriptWindow,
    writer: &mut NoteWriter<F>,
    publisher: &P,
) -> Result<()> {
    let mic_words = mic.checkpoint();
    publish_mic_words(window, publisher, mic_words);
    let them_words = them.checkpoint();
    publish_them_words(window, publisher, them_words);
    // The forced far-end tail is the last echo source this window will get; nothing may stay held across
    // the append, because the note is rendered from the words buffered in the window.
    release_held_mic_words(window, publisher);
    flush_window(diarizer, window, writer)
}

/// Diarize and render one complete rolling window, then perform one OS-synced append. No state is cleared
/// until that durability boundary succeeds, so an error still carries the already-created note to fallback.
fn flush_window<D: LiveDiarizer, F: NoteFiler>(
    diarizer: &D,
    window: &mut TranscriptWindow,
    writer: &mut NoteWriter<F>,
) -> Result<()> {
    debug_assert!(
        window.early_drop.is_idle(),
        "every caller must release held mic regions before the window is rendered"
    );
    let mut segments = words_to_segments(&window.mic_words, Speaker::Me, SEGMENT_GAP);
    if !window.them_words.is_empty() {
        let turns = if window.diarize_far_end {
            diarizer
                .diarize_chunk(&window.tap_audio, window.sample_rate, window.start_sec())?
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        if window.diarize_far_end {
            segments.extend(diarize_words(
                &window.them_words,
                &turns,
                SEGMENT_GAP,
                "Them",
            ));
        } else {
            segments.extend(words_to_segments(
                &window.them_words,
                Speaker::Other("Them".to_string()),
                SEGMENT_GAP,
            ));
        }
    }
    let segments = merge_by_time(segments);
    // Cleanup runs on the merged window, with the previous window's tail as echo sources, before anything
    // is written. Fragment merges cannot cross the append boundary (a committed row is never rewritten);
    // echo lookback can, through `carry`.
    // With the live canceller running, its per-block record for this window is audio evidence the text
    // rules cannot reconstruct: a `Me` row whose mic span was measured as little more than the echo the
    // filter was already subtracting is a ghost regardless of its wording (#149 phase 3b).
    let (segments, stats) = if window.cleanup.is_noop() {
        (segments, CleanupStats::default())
    } else if window.aec_blocks.is_empty() {
        cleanup(segments, &window.cleanup, &window.carry)
    } else {
        let blocks = &window.aec_blocks;
        let evidence = |start: f64, end: f64| crate::transcribe::span_evidence(blocks, start, end);
        cleanup_with_evidence(segments, &window.cleanup, &window.carry, Some(&evidence))
    };
    if !segments.is_empty() {
        writer.append_segments(&segments)?;
        info!(
            target: "corti::live",
            start_sec = window.start_sec(),
            duration_sec = window.frames as f64 / f64::from(window.sample_rate),
            segments = segments.len(),
            echo_dropped_me = stats.echo_dropped_me,
            echo_dropped_them = stats.echo_dropped_them,
            echo_dropped_audio = stats.echo_dropped_audio,
            aec_blocks = window.aec_blocks.len(),
            merged = stats.merged,
            backchannels_dropped = stats.backchannels_dropped,
            buffered_audio_bytes = window.tap_audio.len() * std::mem::size_of::<f32>(),
            "durable live transcript chunk synced"
        );
    }
    window.remember_carry(&segments);
    window.clear_after_flush();
    Ok(())
}

/// Finish the AEC/transcriber tails, diarize + sync the final short window, then durably flip the state line.
/// A dropped tee leaves the note visibly `transcribing` for the canonical batch rewrite.
#[allow(clippy::too_many_arguments)]
fn finish_session<C: LiveChannel, D: LiveDiarizer, F: NoteFiler, P: TranscriptPublisher>(
    sample_rate: u32,
    mut aec: Option<StreamingAec>,
    mut mic: C,
    mut them: C,
    diarizer: D,
    mut window: TranscriptWindow,
    quality: FinishQuality,
    writer: &mut NoteWriter<F>,
    publisher: &P,
) -> Result<LiveOutcome> {
    if let Some(aec) = aec.take() {
        // `finish` consumes the filter, so the tail blocks — every block of a call shorter than the
        // lookahead — are only reachable through `finish_with_stats`. The final window's cleanup is the
        // one that needs them (#149 phase 3b).
        let fin = aec.finish_with_stats();
        info!(
            target: "corti::live",
            delay_samples = fin.delay_samples as u64,
            delay_ms = fin.delay_samples as f64 * 1000.0 / f64::from(sample_rate.max(1)),
            stats_dropped = fin.stats_dropped,
            tail_blocks = fin.stats.len(),
            "live AEC finished"
        );
        window.push_aec_blocks(fin.stats);
        if !fin.audio.is_empty() {
            mic.push(&fin.audio, sample_rate);
        }
    }
    if let Some(words) = mic.poll_words() {
        publish_mic_words(&mut window, publisher, words);
    }
    if let Some(words) = them.poll_words() {
        publish_them_words(&mut window, publisher, words);
    }
    let mic_words = mic.finish();
    publish_mic_words(&mut window, publisher, mic_words);
    let them_words = them.finish();
    publish_them_words(&mut window, publisher, them_words);
    // The session is over: every held region is judged now against everything the far end ever said in
    // range, and published or dropped. Nothing waits past the last append.
    release_held_mic_words(&mut window, publisher);
    let early_drop = window.early_drop.stats;
    flush_window(&diarizer, &mut window, writer)?;
    if early_drop != EarlyDropStats::default() {
        info!(
            target: "corti::live",
            held = early_drop.held,
            released_published = early_drop.released_published,
            released_dropped = early_drop.released_dropped,
            "live early drop of short mic echoes"
        );
    }

    match writer.path().cloned() {
        Some(note_path) if quality.dropped_chunks == 0 => {
            let mut applied_call_ids = Vec::new();
            if let (Some(hosted), Some(recording_id), Some(raw_transcript)) = (
                publisher.hosted(),
                publisher.recording_id(),
                writer.final_transcript(),
            ) {
                let settled = hosted.finalize(recording_id, raw_transcript, true);
                // A disabled default has no calls and preserves the exact historical flip-only behavior.
                // An attempted final (success or typed fallback) rewrites once while still transcribing so
                // provenance and the selected safe body become durable before publication.
                if settled.hosted_text_applied {
                    if hosted.mark_final_applied(&settled.call_ids).is_ok() {
                        if let Err(error) = writer.rewrite_settled_final(&settled) {
                            let _ = hosted.abandon_final_result(&settled.call_ids);
                            return Err(error);
                        }
                        applied_call_ids = settled.call_ids;
                    } else {
                        let _ = hosted.abandon_final_result(&settled.call_ids);
                    }
                } else if !settled.call_ids.is_empty() {
                    writer.rewrite_settled_final(&settled)?;
                }
            }
            corti_vagus::note::flip_state(&note_path).context("flipping the note's state line")?;
            if let Some(hosted) = publisher.hosted() {
                hosted
                    .mark_final_checkpointed(&applied_call_ids)
                    .map_err(|code| {
                        anyhow::anyhow!("acknowledging hosted Final note failed: {code}")
                    })?;
            }
            info!(
                target: "corti::live",
                note_path = %note_path.display(),
                "live note finalized — final chunk and state are synced"
            );
            Ok(LiveOutcome::Filed { note_path })
        }
        Some(note_path) => Ok(LiveOutcome::Fallback {
            reason: format!(
                "live capture tee dropped {} chunk(s)",
                quality.dropped_chunks
            ),
            note_path: Some(note_path),
        }),
        None => Ok(LiveOutcome::NoNote),
    }
}

// ----- Small seams so the loop and writer are testable without models or a vagus binary -----

trait TranscriptPublisher {
    fn words(&self, speaker: Speaker, words: &[Word]);

    /// Live filing hit a durable-write error. Default no-op so test publishers opt in.
    fn error(&self, _detail: String) {}

    /// Live filing is healthy and appending again.
    fn listening(&self) {}

    fn hosted(&self) -> Option<&crate::postprocess_app::HostedHandle> {
        None
    }

    fn recording_id(&self) -> Option<&str> {
        None
    }
}

struct StorePublisher {
    store: LiveTranscriptStore,
    id: String,
    hosted: Option<crate::postprocess_app::HostedHandle>,
}

impl StorePublisher {
    fn complete(&self, detail: impl Into<String>) {
        self.store.set_complete(&self.id, detail);
    }
}

impl TranscriptPublisher for StorePublisher {
    fn words(&self, speaker: Speaker, words: &[Word]) {
        // Mint/publish the raw rows first. Hosted fan-out is a bounded try_send afterward and can never
        // delay capture or ASR; saturation merely marks the optional final ledger incomplete.
        let rows = self.store.append_words(&self.id, speaker, words);
        if !rows.is_empty()
            && let Some(hosted) = self.hosted.as_ref()
        {
            let _ = hosted.try_observe_finalized_rows(&self.id, rows);
        }
    }

    /// The store is in-memory only — restarting the tray discards it, so the log is the copy that lasts.
    fn error(&self, detail: String) {
        warn!(target: "corti::live", recording_id = %self.id, %detail, "live transcript error");
        self.store.set_error(&self.id, detail);
    }

    fn listening(&self) {
        self.store.set_listening(
            &self.id,
            "Listening — lines appear when each speech region closes.",
        );
    }

    fn hosted(&self) -> Option<&crate::postprocess_app::HostedHandle> {
        self.hosted.as_ref()
    }

    fn recording_id(&self) -> Option<&str> {
        Some(&self.id)
    }
}

#[cfg(test)]
struct NoopPublisher;

#[cfg(test)]
impl TranscriptPublisher for NoopPublisher {
    fn words(&self, _speaker: Speaker, _words: &[Word]) {}
}

trait LiveChannel {
    fn push(&mut self, samples: &[f32], sample_rate: u32);
    fn poll_words(&mut self) -> Option<Vec<Word>>;
    fn checkpoint(&mut self) -> Vec<Word>;
    fn finish(&mut self) -> Vec<Word>;
}

#[cfg(feature = "local")]
impl LiveChannel for corti_transcribe_local::LiveTranscriber {
    fn push(&mut self, samples: &[f32], sample_rate: u32) {
        corti_transcribe_local::LiveTranscriber::push(self, samples, sample_rate);
    }
    fn poll_words(&mut self) -> Option<Vec<Word>> {
        corti_transcribe_local::LiveTranscriber::poll_words(self)
    }
    fn checkpoint(&mut self) -> Vec<Word> {
        corti_transcribe_local::LiveTranscriber::checkpoint(self)
    }
    fn finish(&mut self) -> Vec<Word> {
        corti_transcribe_local::LiveTranscriber::finish(self)
    }
}

trait LiveDiarizer {
    fn diarize_chunk(
        &self,
        samples: &[f32],
        sample_rate: u32,
        offset_sec: f64,
    ) -> Result<Option<Vec<SpeakerTurn>>>;
}

#[cfg(feature = "local")]
impl LiveDiarizer for corti_transcribe_local::LiveEngine {
    fn diarize_chunk(
        &self,
        samples: &[f32],
        sample_rate: u32,
        offset_sec: f64,
    ) -> Result<Option<Vec<SpeakerTurn>>> {
        corti_transcribe_local::LiveEngine::diarize_chunk(self, samples, sample_rate, offset_sec)
    }
}

/// How a note gets created — a seam so [`NoteWriter`] is testable against temp files. The production
/// impl shells out to `vagus add-note --print-path` (the ADR 0001 boundary).
trait NoteFiler {
    fn create_note(
        &self,
        title: &str,
        source: &str,
        body: &str,
        provenance: &corti_vagus::provenance::TranscriptProvenance,
    ) -> Result<PathBuf>;
}

/// Production filer: vagus is discovered lazily, at first-segment time — a missing binary is a
/// live-path error and the batch path (with its own re-discovery) takes over.
#[cfg(feature = "local")]
struct VagusFiler;

#[cfg(feature = "local")]
impl NoteFiler for VagusFiler {
    fn create_note(
        &self,
        title: &str,
        source: &str,
        body: &str,
        provenance: &corti_vagus::provenance::TranscriptProvenance,
    ) -> Result<PathBuf> {
        corti_vagus::Vagus::discover()?.add_note(title, source, body, provenance)
    }
}

/// Lazily creates the inbox note on the first non-empty rolling window and appends that window in one
/// OS-synced write. Reports the created path to the pipeline (`PipelineMsg::LiveNoteCreated`) only after
/// the initial note + directory entry are synced.
struct NoteWriter<F: NoteFiler> {
    filer: F,
    meta: RecordingMeta,
    provenance: corti_vagus::provenance::TranscriptProvenance,
    pipe_tx: Option<Sender<PipelineMsg>>,
    note: Option<PathBuf>,
    final_segments: Vec<TranscriptSegment>,
    final_transcript_bytes: usize,
    final_transcript_incomplete: bool,
}

impl<F: NoteFiler> NoteWriter<F> {
    #[cfg(test)]
    fn new(filer: F, meta: RecordingMeta, pipe_tx: Option<Sender<PipelineMsg>>) -> Self {
        Self::with_provenance(
            filer,
            meta,
            pipe_tx,
            corti_vagus::provenance::TranscriptProvenance::legacy_unknown(
                corti_vagus::provenance::GenerationMode::Live,
            ),
        )
    }

    fn with_provenance(
        filer: F,
        meta: RecordingMeta,
        pipe_tx: Option<Sender<PipelineMsg>>,
        provenance: corti_vagus::provenance::TranscriptProvenance,
    ) -> Self {
        Self {
            filer,
            meta,
            provenance,
            pipe_tx,
            note: None,
            final_segments: Vec::new(),
            final_transcript_bytes: 0,
            final_transcript_incomplete: false,
        }
    }

    /// Render and durably append one complete, already-merged transcript window. Empty/silent windows do
    /// not create a note. Rendering once avoids per-segment syscalls and establishes one clear crash boundary.
    fn append_segments(&mut self, segments: &[TranscriptSegment]) -> Result<()> {
        if segments.is_empty() {
            return Ok(());
        }
        if self.note.is_none() {
            self.create()?;
        }
        let chunk = DiarizedTranscript::new(segments.to_vec()).to_markdown();
        corti_vagus::note::append(self.note.as_ref().expect("just created"), &chunk)?;
        // Record canonical post-diarization rows only after the raw append is durable. This optional
        // bounded assembly can be dropped wholesale without weakening the note's crash safety.
        if !self.final_transcript_incomplete {
            let added = segments.iter().fold(0usize, |total, segment| {
                total
                    .saturating_add(segment.text.len())
                    .saturating_add(segment.speaker.display().len())
                    .saturating_add(std::mem::size_of::<TranscriptSegment>())
            });
            if self.final_transcript_bytes.saturating_add(added) > MAX_FINAL_TRANSCRIPT_BYTES {
                self.final_segments.clear();
                self.final_transcript_bytes = 0;
                self.final_transcript_incomplete = true;
            } else {
                self.final_transcript_bytes = self.final_transcript_bytes.saturating_add(added);
                self.final_segments.extend_from_slice(segments);
            }
        }
        Ok(())
    }

    /// Abandon the assembled final transcript: a dropped window leaves the note with a gap only the batch
    /// pass can fill, so it must not be published as canonical.
    fn mark_incomplete(&mut self) {
        self.final_segments.clear();
        self.final_transcript_bytes = 0;
        self.final_transcript_incomplete = true;
    }

    fn final_transcript(&self) -> Option<DiarizedTranscript> {
        (!self.final_transcript_incomplete)
            .then(|| DiarizedTranscript::new(self.final_segments.clone()))
    }

    fn rewrite_settled_final(
        &mut self,
        settled: &crate::postprocess_app::SettledFinalTranscript,
    ) -> Result<()> {
        let path = self.note.as_ref().context("live note is not owned")?;
        let mut provenance = self.provenance.clone();
        provenance
            .set_postprocess(settled.applied_postprocess.clone())
            .context("attaching live final postprocess provenance")?;
        let final_body = corti_vagus::recording_body(&self.meta, &settled.transcript);
        let in_progress_body = final_body.replacen(
            corti_vagus::note::STATE_TRANSCRIBED,
            corti_vagus::note::STATE_TRANSCRIBING,
            1,
        );
        corti_vagus::note::CurrentNote::from_returned_path(path.clone())?
            .rewrite_transcript(&in_progress_body, &provenance)
            .context("rewriting the live note before final state publication")?;
        self.provenance = provenance;
        Ok(())
    }

    #[cfg(test)]
    fn append_segment(&mut self, segment: &TranscriptSegment) -> Result<()> {
        self.append_segments(std::slice::from_ref(segment))
    }

    fn create(&mut self) -> Result<()> {
        let path = self
            .filer
            .create_note(
                &self.meta.note_title(),
                &self.meta.source(),
                &corti_vagus::live_initial_body(&self.meta),
                &self.provenance,
            )
            .context("creating the live inbox note")?;
        // Retain ownership before the fallible sync. If syncing fails, `session_thread` must still return
        // this path for fallback/recovery rather than forgetting a note vagus already created.
        self.note = Some(path.clone());
        corti_vagus::note::sync_created(&path)
            .context("syncing the new live inbox note and directory")?;
        info!(
            target: "corti::live",
            note_path = %path.display(),
            "live inbox note created and synced (State: transcribing)"
        );
        if let Some(tx) = &self.pipe_tx {
            let _ = tx.send(PipelineMsg::LiveNoteCreated {
                meta: self.meta.clone(),
                note_path: path.clone(),
            });
        }
        Ok(())
    }

    /// Delete the note (recording discarded). No-op when none was created. A failed unlink keeps the path
    /// owned by the writer so `session_thread` returns it in `LiveOutcome::Fallback` for reaper/pipeline
    /// cleanup instead of forgetting the only reference.
    fn discard(&mut self) -> Result<()> {
        let Some(path) = self.note.as_ref() else {
            return Ok(());
        };
        match std::fs::remove_file(path) {
            Ok(()) => {
                info!(
                    target: "corti::live",
                    note_path = %path.display(),
                    "deleted live note of a discarded recording"
                );
                self.note = None;
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.note = None;
                Ok(())
            }
            Err(e) => {
                Err(e).with_context(|| format!("deleting discarded live note {}", path.display()))
            }
        }
    }

    fn path(&self) -> Option<&PathBuf> {
        self.note.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use corti_core::OwningApp;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::path::Path;

    fn word(start: f64, end: f64, text: &str) -> Word {
        Word {
            start,
            end,
            text: text.to_string(),
        }
    }

    fn meta() -> RecordingMeta {
        RecordingMeta {
            started_at: chrono::Local::now(),
            ended_at: None,
            owning_app: OwningApp::from_bundle_id("us.zoom.xos"),
            audio_path: PathBuf::from("/tmp/rec.wav"),
        }
    }

    #[test]
    fn live_aec_emits_opening_audio_within_two_hundred_milliseconds() {
        let sample_rate = 48_000;
        let config = corti_aec::AecConfig::default();
        let expected_lookahead = corti_aec::lookahead_samples_for(
            sample_rate,
            config.filter_len,
            LIVE_AEC_LOOKAHEAD_SECONDS,
        );
        assert!(expected_lookahead <= sample_rate as usize / 5);
        let mut aec = new_live_aec(sample_rate, config);
        let mic = (0..480)
            .map(|sample| ((sample as f32) * 0.03).sin() * 0.2)
            .collect::<Vec<_>>();
        let far = vec![0.0; mic.len()];
        let mut supplied = 0usize;
        let emitted = loop {
            supplied += mic.len();
            let output = aec.push(&mic, &far);
            if !output.is_empty() {
                break output;
            }
            assert!(supplied <= expected_lookahead + mic.len());
        };
        assert!(supplied <= sample_rate as usize / 5);
        assert!(emitted.iter().any(|sample| sample.abs() > f32::EPSILON));
    }

    /// Scripted stand-in for `LiveTranscriber`: each `push` queues the next scripted word batch;
    /// `finish` returns the scripted tail.
    struct Scripted {
        on_push: VecDeque<Vec<Word>>,
        pending: Vec<Word>,
        tail: Vec<Word>,
        pushes: Vec<usize>,
    }

    impl Scripted {
        fn new(on_push: Vec<Vec<Word>>, tail: Vec<Word>) -> Self {
            Self {
                on_push: on_push.into(),
                pending: Vec::new(),
                tail,
                pushes: Vec::new(),
            }
        }
    }

    impl LiveChannel for Scripted {
        fn push(&mut self, samples: &[f32], _sample_rate: u32) {
            self.pushes.push(samples.len());
            if let Some(words) = self.on_push.pop_front() {
                self.pending.extend(words);
            }
        }
        fn poll_words(&mut self) -> Option<Vec<Word>> {
            if self.pending.is_empty() {
                None
            } else {
                Some(std::mem::take(&mut self.pending))
            }
        }
        fn checkpoint(&mut self) -> Vec<Word> {
            std::mem::take(&mut self.pending)
        }
        fn finish(&mut self) -> Vec<Word> {
            let mut words = std::mem::take(&mut self.pending);
            words.append(&mut self.tail);
            words
        }
    }

    #[test]
    fn injected_live_channel_can_publish_a_first_word_inside_the_audio_latency_bound() {
        let sample_rate = 48_000;
        let first = word(0.05, 0.09, "synthetic first word");
        let mut channel = Scripted::new(vec![vec![first.clone()]], Vec::new());
        let audio = vec![0.1; sample_rate as usize / 10];
        channel.push(&audio, sample_rate);
        let emitted = channel.poll_words().expect("scripted VAD emitted a word");
        assert_eq!(emitted, vec![first]);
        assert!(audio.len() <= sample_rate as usize / 5);
    }

    #[derive(Default)]
    struct NoDiarizer;

    #[derive(Default)]
    struct RecordingPublisher(RefCell<Vec<(String, Vec<Word>)>>);

    impl TranscriptPublisher for RecordingPublisher {
        fn words(&self, speaker: Speaker, words: &[Word]) {
            if !words.is_empty() {
                self.0
                    .borrow_mut()
                    .push((speaker.display().to_string(), words.to_vec()));
            }
        }
    }

    impl LiveDiarizer for NoDiarizer {
        fn diarize_chunk(
            &self,
            _samples: &[f32],
            _sample_rate: u32,
            _offset_sec: f64,
        ) -> Result<Option<Vec<SpeakerTurn>>> {
            Ok(None)
        }
    }

    struct ScriptedDiarizer {
        turns: Vec<SpeakerTurn>,
        calls: RefCell<Vec<(usize, u32, f64)>>,
    }

    impl LiveDiarizer for ScriptedDiarizer {
        fn diarize_chunk(
            &self,
            samples: &[f32],
            sample_rate: u32,
            offset_sec: f64,
        ) -> Result<Option<Vec<SpeakerTurn>>> {
            self.calls
                .borrow_mut()
                .push((samples.len(), sample_rate, offset_sec));
            Ok(Some(self.turns.clone()))
        }
    }

    /// Test filer: writes a vagus-shaped note (frontmatter + title + body) into a temp dir.
    struct TempFiler {
        dir: PathBuf,
    }

    impl TempFiler {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("corti-live-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }
        fn note(&self) -> PathBuf {
            self.dir.join("note.md")
        }
    }

    impl NoteFiler for TempFiler {
        fn create_note(
            &self,
            title: &str,
            source: &str,
            body: &str,
            provenance: &corti_vagus::provenance::TranscriptProvenance,
        ) -> Result<PathBuf> {
            let p = self.note();
            std::fs::write(
                &p,
                format!(
                    "---\ncreated: x\nstatus: inbox\nsource: {source}\n{}---\n\n# {title}\n\n{body}",
                    provenance.frontmatter_line()?
                ),
            )?;
            Ok(p)
        }
    }

    /// `TempFiler` that refuses to create the note for its first `remaining` calls — the production shape
    /// of a revoked vault grant, where the failure lands on creation rather than on the append.
    struct FlakyFiler {
        inner: TempFiler,
        remaining: RefCell<usize>,
    }

    impl FlakyFiler {
        fn new(name: &str, failures: usize) -> Self {
            Self {
                inner: TempFiler::new(name),
                remaining: RefCell::new(failures),
            }
        }
        fn note(&self) -> PathBuf {
            self.inner.note()
        }
    }

    impl NoteFiler for FlakyFiler {
        fn create_note(
            &self,
            title: &str,
            source: &str,
            body: &str,
            provenance: &corti_vagus::provenance::TranscriptProvenance,
        ) -> Result<PathBuf> {
            let mut remaining = self.remaining.borrow_mut();
            if *remaining > 0 {
                *remaining -= 1;
                anyhow::bail!("vault unavailable");
            }
            self.inner.create_note(title, source, body, provenance)
        }
    }

    /// Captures the status transitions `RecordingPublisher` ignores.
    #[derive(Default)]
    struct StatusPublisher {
        errors: RefCell<Vec<String>>,
        listening: RefCell<usize>,
    }

    impl TranscriptPublisher for StatusPublisher {
        fn words(&self, _speaker: Speaker, _words: &[Word]) {}
        fn error(&self, detail: String) {
            self.errors.borrow_mut().push(detail);
        }
        fn listening(&self) {
            *self.listening.borrow_mut() += 1;
        }
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }

    /// Lazy creation, exact appended strings, and delete-on-discard.
    #[test]
    fn note_writer_creates_lazily_appends_exact_lines_and_discards() {
        let filer = TempFiler::new("writer");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);

        assert!(!note.exists());
        writer.append_segments(&[]).unwrap();
        assert!(!note.exists(), "a silent window must stay lazy");

        writer
            .append_segment(&TranscriptSegment {
                speaker: Speaker::Me,
                start: 0.0,
                end: 1.0,
                text: "hello there".into(),
            })
            .unwrap();
        assert_eq!(writer.path(), Some(&note));
        let content = read(&note);
        assert!(
            content.contains(&format!(
                r#"corti: {{"schema":{}"#,
                corti_vagus::provenance::SCHEMA_VERSION
            )),
            "got: {content}"
        );
        assert!(content.contains(r#""mode":"live""#), "got: {content}");
        assert!(content.contains("State: transcribing\n\n"));
        assert!(content.contains("## Transcript\n\n"));
        assert!(content.ends_with("**[00:00] Me:** hello there\n\n"));

        writer
            .append_segment(&TranscriptSegment {
                speaker: Speaker::Other("Them".into()),
                start: 63.0,
                end: 64.0,
                text: "hi".into(),
            })
            .unwrap();
        assert!(read(&note).ends_with("**[00:00] Me:** hello there\n\n**[01:03] Them:** hi\n\n"));

        writer.discard().unwrap();
        assert!(!note.exists(), "discard must delete the note file");
        assert!(writer.path().is_none());
    }

    /// Simulate three hours without allocating three hours: the same one-minute allocation is reused 180
    /// times and retained bytes never exceed the configured window. This test exercises irregular producer
    /// chunks and exact boundary splitting, not just the arithmetic.
    #[test]
    fn rolling_window_memory_is_invariant_over_three_hours() {
        let sample_rate = 100u32;
        let mut window =
            TranscriptWindow::new(sample_rate, 1, true, CleanupConfig::default()).unwrap();
        let source = vec![0.0f32; 137];
        let mut remaining = u64::from(sample_rate) * 3 * 60 * 60;
        let mut flushes = 0usize;
        let mut max_retained = 0usize;

        while remaining > 0 {
            let take = window
                .remaining_frames()
                .min(source.len())
                .min(remaining as usize);
            window.push_audio(&source[..take], take);
            remaining -= take as u64;
            max_retained = max_retained.max(window.retained_bytes());
            if window.due() {
                assert_eq!(window.frames, u64::from(sample_rate) * 60);
                window.clear_after_flush();
                flushes += 1;
            }
        }

        assert_eq!(flushes, 180);
        assert_eq!(window.frames, 0);
        assert!(max_retained <= sample_rate as usize * 60 * std::mem::size_of::<f32>());
        assert_eq!(window.start_frame, u64::from(sample_rate) * 3 * 60 * 60);

        let high_rate = TranscriptWindow::new(192_000, 10, true, CleanupConfig::default()).unwrap();
        assert_eq!(
            high_rate.frame_limit as usize * std::mem::size_of::<f32>(),
            MAX_DIARIZATION_AUDIO_BYTES,
            "the absolute audio cap must override a large configured window"
        );
    }

    #[test]
    fn far_end_is_diarized_before_the_durable_chunk_is_written() {
        let filer = TempFiler::new("diarized-window");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);
        let diarizer = ScriptedDiarizer {
            turns: vec![SpeakerTurn {
                start: 60.0,
                end: 70.0,
                label: "Them 7".into(),
            }],
            calls: RefCell::new(Vec::new()),
        };
        let mut window = TranscriptWindow::new(10, 1, true, CleanupConfig::default()).unwrap();
        window.start_frame = 600; // second minute: proves the absolute offset is supplied
        window.push_audio(&vec![0.0; 100], 100);
        window.push_them_words(vec![word(62.0, 62.5, "owned action item")]);

        flush_window(&diarizer, &mut window, &mut writer).unwrap();

        assert_eq!(&*diarizer.calls.borrow(), &[(100, 10, 60.0)]);
        assert!(read(&note).contains("**[01:02] Them 7:** owned action item"));
        assert_eq!(window.frames, 0);
        assert!(window.tap_audio.is_empty());
    }

    #[test]
    fn closed_region_reaches_live_reader_before_the_durable_minute_boundary() {
        let filer = TempFiler::new("reader-before-commit");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);
        let mut mic = Scripted::new(vec![], vec![]);
        let mut them = Scripted::new(vec![vec![word(4.0, 5.0, "visible now")]], vec![]);
        let publisher = RecordingPublisher::default();
        let (tx, rx) = sync_channel(2);
        tx.send(CaptureChunk {
            mic: Vec::new(),
            tap: vec![0.0; 10],
        })
        .unwrap();
        drop(tx);

        consume_chunks(
            &rx,
            1,
            &mut None,
            &mut mic,
            &mut them,
            &NoDiarizer,
            &mut TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap(),
            &mut writer,
            &publisher,
        )
        .unwrap();

        let published = publisher.0.borrow();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].0, "Them");
        assert_eq!(published[0].1[0].text, "visible now");
        assert!(
            !note.exists(),
            "the one-minute durable window is not due yet"
        );
    }

    /// Empty-mic chunks never reach the mic channel; a full configured interval is written while the call
    /// is still live; the final short tail is synced before the state line flips.
    #[test]
    fn consume_and_finish_write_intervals_and_flip_state() {
        let filer = TempFiler::new("loop");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);
        let mut them = Scripted::new(
            vec![
                vec![word(0.0, 0.5, "hi"), word(0.6, 1.0, "Xavier")],
                vec![word(5.0, 5.5, "anyway")],
            ],
            vec![word(20.0, 20.5, "bye")],
        );
        let mut mic = Scripted::new(vec![], vec![word(10.0, 10.5, "thanks")]);
        let (tx, rx) = sync_channel::<CaptureChunk>(8);
        for _ in 0..2 {
            tx.send(CaptureChunk {
                mic: Vec::new(),
                tap: vec![0.0; 30],
            })
            .unwrap();
        }
        drop(tx);

        let mut aec = None;
        let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
        consume_chunks(
            &rx,
            1,
            &mut aec,
            &mut mic,
            &mut them,
            &NoDiarizer,
            &mut window,
            &mut writer,
            &NoopPublisher,
        )
        .unwrap();

        assert!(mic.pushes.is_empty());
        assert_eq!(them.pushes, vec![30, 30]);
        let mid_call = read(&note);
        assert!(mid_call.contains("State: transcribing\n"));
        assert!(mid_call.ends_with("**[00:00] Them:** hi Xavier\n\n**[00:05] Them:** anyway\n\n"));

        let outcome = finish_session(
            1,
            aec,
            mic,
            them,
            NoDiarizer,
            window,
            FinishQuality { dropped_chunks: 0 },
            &mut writer,
            &NoopPublisher,
        )
        .unwrap();
        let LiveOutcome::Filed { note_path } = outcome else {
            panic!("expected Filed");
        };
        assert_eq!(note_path, note);
        let final_content = read(&note);
        assert!(final_content.ends_with(
            "**[00:00] Them:** hi Xavier\n\n\
             **[00:05] Them:** anyway\n\n\
             **[00:10] Me:** thanks\n\n\
             **[00:20] Them:** bye\n\n"
        ));
        assert!(final_content.contains("State: transcribed \n"));
        assert!(!final_content.contains("State: transcribing"));
    }

    /// The live path applies the same cleanup the batch path does, before the append: an echo of far-end
    /// speech, a backchannel over it, and two fragments of one mic sentence all resolve inside one window,
    /// and an echo whose source landed in the *previous* window is still caught through `carry`.
    ///
    /// Synthetic script — this repo is public, so the shapes are reproduced with invented words.
    #[test]
    fn live_windows_drop_echoes_and_backchannels_and_merge_fragments() {
        let filer = TempFiler::new("cleanup");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);

        let mut them = Scripted::new(
            vec![
                vec![word(
                    2.0,
                    6.0,
                    "We should rotate the widget calibration before Friday.",
                )],
                vec![word(
                    57.0,
                    59.5,
                    "The gateway timeout hits the queue depth.",
                )],
            ],
            Vec::new(),
        );
        let mut mic = Scripted::new(
            vec![
                vec![
                    // A backchannel over the far end, and an echo of what it just said.
                    word(4.0, 4.3, "Yeah."),
                    word(8.0, 9.9, "Rotate the widget calibration."),
                ],
                vec![
                    // One sentence the VAD split at a breath (2.0 s > SEGMENT_GAP).
                    word(40.0, 41.0, "I will send the summary"),
                    word(43.0, 44.0, "after this call."),
                ],
                // Second window: an echo of the far-end turn that closed the first one.
                vec![
                    word(62.0, 63.0, "Gateway timeout queue depth."),
                    word(70.0, 71.0, "Sending the notes now."),
                ],
            ],
            Vec::new(),
        );

        // Four 30-frame chunks at 1 Hz ⇒ two full one-minute windows.
        let (tx, rx) = sync_channel::<CaptureChunk>(8);
        for _ in 0..4 {
            tx.send(CaptureChunk {
                mic: vec![0.0; 30],
                tap: vec![0.0; 30],
            })
            .unwrap();
        }
        drop(tx);

        let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
        consume_chunks(
            &rx,
            1,
            &mut None,
            &mut mic,
            &mut them,
            &NoDiarizer,
            &mut window,
            &mut writer,
            &NoopPublisher,
        )
        .unwrap();

        let content = read(&note);
        assert!(
            content.ends_with(
                "**[00:02] Them:** We should rotate the widget calibration before Friday.\n\n\
                 **[00:40] Me:** I will send the summary after this call.\n\n\
                 **[00:57] Them:** The gateway timeout hits the queue depth.\n\n\
                 **[01:10] Me:** Sending the notes now.\n\n"
            ),
            "unexpected note body:\n{content}"
        );
        assert!(!content.contains("Me:** Yeah."), "backchannel survived");
        assert!(
            !content.contains("Me:** Rotate the widget calibration."),
            "in-window echo survived"
        );
        assert!(
            !content.contains("Me:** Gateway timeout queue depth."),
            "cross-window echo survived — carry did not reach the next window"
        );
    }

    /// A short mic region decoded while the far end is still mid-region is withheld, judged against that
    /// region when it closes, and never reaches the reader at all (#149 phase 2).
    ///
    /// Synthetic script — this repo is public, so the shape is reproduced with invented words.
    #[test]
    fn a_short_mic_ghost_inside_a_far_end_region_is_never_published() {
        let filer = TempFiler::new("early-drop-ghost");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);

        // The far end speaks from 0.0 to 3.0, but its region only closes on the second chunk.
        let mut them = Scripted::new(
            vec![
                Vec::new(),
                vec![word(0.0, 3.0, "the settlement gateway rollout")],
            ],
            Vec::new(),
        );
        // Residual echo, decoded while that region is still open — nothing downstream can retract it.
        let mut mic = Scripted::new(
            vec![vec![word(1.0, 1.5, "Settlement gateway.")]],
            Vec::new(),
        );

        let (tx, rx) = sync_channel::<CaptureChunk>(4);
        for _ in 0..2 {
            tx.send(CaptureChunk {
                mic: vec![0.0; 5],
                tap: vec![0.0; 5],
            })
            .unwrap();
        }
        drop(tx);

        let publisher = RecordingPublisher::default();
        let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
        consume_chunks(
            &rx,
            1,
            &mut None,
            &mut mic,
            &mut them,
            &NoDiarizer,
            &mut window,
            &mut writer,
            &publisher,
        )
        .unwrap();

        assert_eq!(
            window.early_drop.stats,
            EarlyDropStats {
                held: 1,
                released_published: 0,
                released_dropped: 1,
            }
        );
        assert_eq!(
            publisher
                .0
                .borrow()
                .iter()
                .map(|(speaker, _)| speaker.as_str())
                .collect::<Vec<_>>(),
            vec!["Them"],
            "the ghost reached the reader"
        );

        finish_session(
            1,
            None,
            mic,
            them,
            NoDiarizer,
            window,
            FinishQuality { dropped_chunks: 0 },
            &mut writer,
            &publisher,
        )
        .unwrap();
        let content = read(&note);
        assert!(
            !content.contains("Me:**"),
            "unexpected note body:\n{content}"
        );
    }

    /// A short mic region that is *not* an echo is published late — after the far-end region whose closing
    /// released it — carrying its original words and timestamps. The reader orders rows by time, so a late
    /// row still lands in its place.
    #[test]
    fn a_short_genuine_answer_is_published_late_with_its_original_timestamps() {
        let filer = TempFiler::new("early-drop-answer");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);

        let mut them = Scripted::new(
            vec![
                vec![word(0.0, 1.0, "which region is the replica in?")],
                Vec::new(),
                Vec::new(),
                vec![word(4.0, 4.6, "Got it.")],
            ],
            Vec::new(),
        );
        let answer = word(1.4, 1.9, "Frankfurt.");
        let mut mic = Scripted::new(vec![Vec::new(), vec![answer.clone()]], Vec::new());

        let (tx, rx) = sync_channel::<CaptureChunk>(8);
        for _ in 0..5 {
            tx.send(CaptureChunk {
                mic: vec![0.0; 1],
                tap: vec![0.0; 1],
            })
            .unwrap();
        }
        drop(tx);

        let publisher = RecordingPublisher::default();
        let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
        consume_chunks(
            &rx,
            1,
            &mut None,
            &mut mic,
            &mut them,
            &NoDiarizer,
            &mut window,
            &mut writer,
            &publisher,
        )
        .unwrap();

        assert_eq!(
            window.early_drop.stats,
            EarlyDropStats {
                held: 1,
                released_published: 1,
                released_dropped: 0,
            }
        );
        let published = publisher.0.borrow();
        assert_eq!(
            published
                .iter()
                .map(|(speaker, words)| (speaker.as_str(), words[0].text.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("Them", "which region is the replica in?"),
                ("Them", "Got it."),
                ("Me", "Frankfurt."),
            ],
            "the answer was not held until the far end closed a region"
        );
        assert_eq!(published[2].1, vec![answer], "timestamps were rewritten");
        drop(published);

        finish_session(
            1,
            None,
            mic,
            them,
            NoDiarizer,
            window,
            FinishQuality { dropped_chunks: 0 },
            &mut writer,
            &publisher,
        )
        .unwrap();
        let content = read(&note);
        assert!(
            content.contains("Me:** Frankfurt."),
            "unexpected note body:\n{content}"
        );
    }

    /// A long mic region is a sentence, not residual echo: it publishes with no added latency. It still
    /// never overtakes a region held before it, and whatever is held when the session ends is released
    /// rather than lost.
    #[test]
    fn long_mic_regions_publish_immediately_and_holds_are_released_in_order_at_finish() {
        let filer = TempFiler::new("early-drop-order");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);

        let mut them = Scripted::new(Vec::new(), Vec::new());
        let opener = word(0.2, 0.7, "Okay sure.");
        let sentence = word(
            1.0,
            5.0,
            "We should move the settlement rollout to the next maintenance window.",
        );
        let trailing = word(6.0, 6.4, "Right.");
        let mut mic = Scripted::new(
            vec![
                vec![opener.clone()],
                vec![sentence.clone()],
                vec![trailing.clone()],
            ],
            Vec::new(),
        );

        let (tx, rx) = sync_channel::<CaptureChunk>(8);
        for _ in 0..3 {
            tx.send(CaptureChunk {
                mic: vec![0.0; 2],
                tap: vec![0.0; 2],
            })
            .unwrap();
        }
        drop(tx);

        let publisher = RecordingPublisher::default();
        let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
        consume_chunks(
            &rx,
            1,
            &mut None,
            &mut mic,
            &mut them,
            &NoDiarizer,
            &mut window,
            &mut writer,
            &publisher,
        )
        .unwrap();

        assert_eq!(
            publisher
                .0
                .borrow()
                .iter()
                .map(|(_, words)| words[0].text.clone())
                .collect::<Vec<_>>(),
            vec![opener.text.clone(), sentence.text.clone()],
            "the sentence overtook the region held before it"
        );
        assert_eq!(window.early_drop.stats.held, 2);

        finish_session(
            1,
            None,
            mic,
            them,
            NoDiarizer,
            window,
            FinishQuality { dropped_chunks: 0 },
            &mut writer,
            &publisher,
        )
        .unwrap();
        assert_eq!(
            publisher
                .0
                .borrow()
                .iter()
                .map(|(_, words)| words[0].text.clone())
                .collect::<Vec<_>>(),
            vec![opener.text, sentence.text, trailing.text],
            "a region still held at finish was lost"
        );
        let content = read(&note);
        for text in ["Okay sure.", "We should move the settlement", "Right."] {
            assert!(content.contains(text), "unexpected note body:\n{content}");
        }
    }

    /// The early-drop state never grows with call length. Overflowing either cap degrades the rule for one
    /// region — it does not lose a region or reorder one.
    #[test]
    fn early_drop_state_is_bounded_and_overflow_releases_rather_than_loses() {
        let mut early = EarlyDrop::new(CleanupConfig::default());
        let mut published = 0usize;
        for i in 0..(MAX_HELD_MIC_REGIONS + 20) {
            let start = i as f64 * 0.1;
            published += early
                .offer_mic(vec![word(start, start + 0.05, "ping")], start)
                .len();
        }
        assert_eq!(early.held.len(), MAX_HELD_MIC_REGIONS);
        assert_eq!(published, 20, "overflow lost the regions it evicted");
        published += early.release_all().len();
        assert!(early.is_idle());
        assert_eq!(published, MAX_HELD_MIC_REGIONS + 20);
        assert_eq!(early.stats.released_dropped, 0, "there was nothing to echo");

        // Far-end regions are normally pruned by time; the hard cap catches a pathological burst inside one
        // instant, which no horizon can trim.
        let mut early = EarlyDrop::new(CleanupConfig::default());
        for _ in 0..(MAX_LIVE_ECHO_SOURCES + 10) {
            early.offer_them(&[word(0.0, 0.5, "chatter")], 0.0);
        }
        assert_eq!(early.sources.len(), MAX_LIVE_ECHO_SOURCES);
    }

    /// With the echo pass switched off, the early drop is a passthrough: it holds nothing and counts
    /// nothing, and the live path behaves exactly as it did before #149 phase 2.
    #[test]
    fn early_drop_disabled_publishes_every_region_immediately() {
        let mut early = EarlyDrop::new(CleanupConfig {
            echo_drop: false,
            ..CleanupConfig::default()
        });
        let ghost = vec![word(1.0, 1.2, "Gateway.")];
        early.offer_them(&[word(0.0, 3.0, "the settlement gateway rollout")], 3.0);
        assert_eq!(early.offer_mic(ghost.clone(), 3.5), vec![ghost]);
        assert!(early.is_idle());
        assert_eq!(early.stats, EarlyDropStats::default());
    }

    /// A hold never survives a durable append: the note is rendered from the window's buffered words, so
    /// `checkpoint_and_flush` releases every held region before it flushes.
    #[test]
    fn a_hold_is_released_before_the_durable_append() {
        let filer = TempFiler::new("early-drop-boundary");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);

        let mut them = Scripted::new(Vec::new(), Vec::new());
        // Decoded at 58 s, so the hold would not expire on its own until 64.5 s — past this window's end.
        let mut mic = Scripted::new(
            vec![Vec::new(), vec![word(58.0, 58.5, "Frankfurt.")]],
            Vec::new(),
        );

        let (tx, rx) = sync_channel::<CaptureChunk>(4);
        for _ in 0..2 {
            tx.send(CaptureChunk {
                mic: vec![0.0; 30],
                tap: vec![0.0; 30],
            })
            .unwrap();
        }
        drop(tx);

        let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
        consume_chunks(
            &rx,
            1,
            &mut None,
            &mut mic,
            &mut them,
            &NoDiarizer,
            &mut window,
            &mut writer,
            &NoopPublisher,
        )
        .unwrap();

        assert!(window.early_drop.is_idle());
        let content = read(&note);
        assert!(
            content.contains("**[00:58] Me:** Frankfurt."),
            "the held region missed its window:\n{content}"
        );
    }

    /// One synthetic canceller block: `mic_energy` against `echo_estimate_energy` is the whole signal the
    /// audio rule reads (equal ⇒ 0 dB above the estimate ⇒ "this was echo").
    fn block(t_start_secs: f64, mic_energy: f32, double_talk: bool) -> corti_aec::BlockStats {
        corti_aec::BlockStats {
            t_start_secs,
            mic_energy,
            far_energy: 1.0,
            echo_estimate_energy: 1.0,
            error_energy: mic_energy,
            double_talk,
            suppressed: !double_talk,
        }
    }

    /// The per-block record a window retains is bounded twice over: by the echo lookback at every flush,
    /// and by a hard block cap that a hand-edited `echo_window_seconds` cannot defeat.
    #[test]
    fn window_aec_blocks_are_bounded_by_the_echo_lookback_and_a_hard_cap() {
        let filer = TempFiler::new("aec-blocks");
        let mut writer = NoteWriter::new(filer, meta(), None);
        let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();

        // One block every 0.5 s across the first (one-minute) window.
        window.push_aec_blocks(
            (0..120)
                .map(|k| block(f64::from(k) * 0.5, 1.0, false))
                .collect(),
        );
        assert_eq!(window.aec_blocks.len(), 120);

        window.frames = 60;
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();

        // The next window starts at 60 s and the echo window is 6 s, so only blocks at 54 s or later can
        // still be evidence for anything.
        assert_eq!(window.aec_blocks.len(), 12);
        assert_eq!(window.aec_blocks[0].t_start_secs, 54.0);

        window.push_aec_blocks(
            (0..MAX_WINDOW_AEC_BLOCKS + 50)
                .map(|k| block(60.0 + f64::from(k as u32) * 0.001, 1.0, false))
                .collect(),
        );
        assert_eq!(window.aec_blocks.len(), MAX_WINDOW_AEC_BLOCKS);
        assert!(
            window.aec_blocks[0].t_start_secs >= 60.0,
            "the cap evicts from the old end"
        );
    }

    /// The live flush hands the echo pass the canceller's own record for the window. A mic row that shares
    /// no vocabulary with the far end — nothing the text rules can act on — is dropped anyway when its
    /// span was measured as little more than the echo the filter was already subtracting, while a mic row
    /// over the same far-end speech with real energy behind it survives.
    ///
    /// Synthetic script and synthetic blocks — this repo is public.
    #[test]
    fn audio_evidence_drops_a_ghost_the_text_rules_would_keep() {
        let filer = TempFiler::new("audio-evidence");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);
        let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();

        window.push_them_words(vec![
            word(
                2.0,
                20.0,
                "The calibration jig arrives from Toronto on Tuesday.",
            ),
            word(38.0, 45.0, "Which desk is it going to?"),
        ]);
        window.push_mic_words(vec![
            // A ghost: no content token in common with the far end, so the text rules keep it.
            word(12.0, 12.6, "Gateway harness."),
            // Real near-end speech over the far end's second turn.
            word(40.0, 41.0, "Sending the notes now."),
        ]);
        // One block every 0.5 s. The mic is at the echo estimate through the ghost and 20 dB above it
        // everywhere else; the gate never fires, so the estimate is trustworthy throughout.
        window.push_aec_blocks(
            (0..120)
                .map(|k| {
                    let t = f64::from(k) * 0.5;
                    let ghost = (11.5..13.0).contains(&t);
                    block(t, if ghost { 1.0 } else { 100.0 }, false)
                })
                .collect(),
        );

        window.frames = 60;
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();

        let content = read(&note);
        assert!(
            !content.contains("Gateway harness."),
            "the audio rule did not fire on the ghost:\n{content}"
        );
        assert!(
            content.contains("**[00:40] Me:** Sending the notes now."),
            "real near-end speech over the far end was dropped:\n{content}"
        );
        assert!(content.contains("Which desk is it going to?"));
    }

    /// Without the record — AEC off, or a `--no-mic` capture — the same window keeps the ghost, because
    /// the text rules have nothing to go on. This is the control for the test above.
    #[test]
    fn the_same_window_keeps_the_ghost_when_no_blocks_were_recorded() {
        let filer = TempFiler::new("no-audio-evidence");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);
        let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();

        window.push_them_words(vec![word(
            2.0,
            20.0,
            "The calibration jig arrives from Toronto on Tuesday.",
        )]);
        window.push_mic_words(vec![word(12.0, 12.6, "Gateway harness.")]);
        window.frames = 60;
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();

        assert!(read(&note).contains("Gateway harness."));
    }

    /// `carry` holds only the previous window's tail, and only as echo sources: it is never appended a
    /// second time, and it is dropped once it falls outside the echo window.
    #[test]
    fn carry_holds_the_previous_windows_tail_and_nothing_else() {
        let filer = TempFiler::new("carry");
        let mut writer = NoteWriter::new(filer, meta(), None);
        let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();

        window.push_them_words(vec![
            word(1.0, 2.0, "An early turn nobody echoes."),
            word(57.0, 59.0, "The gateway timeout hits the queue depth."),
        ]);
        window.frames = 60;
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();

        // 60 s window, 6 s echo window ⇒ only segments ending at or after 54 s are carried.
        assert_eq!(window.carry.len(), 1);
        assert_eq!(window.carry[0].start, 57.0);

        // The carried segment is an echo source, not content: it is not re-appended by the next flush.
        window.push_mic_words(vec![word(62.0, 63.0, "Gateway timeout queue depth.")]);
        window.frames = 60;
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
        assert!(
            window.carry.is_empty(),
            "nothing survived the second window"
        );
    }

    /// A failed checkpoint degrades the session instead of ending it: the window is dropped, the note is
    /// marked for the batch rewrite, the reader is told once, and the next checkpoint recovers.
    #[test]
    fn failed_checkpoint_drops_its_window_and_recovers() {
        let filer = FlakyFiler::new("checkpoint-retry", 1);
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);
        let mut mic = Scripted::new(vec![], vec![]);
        let mut them = Scripted::new(
            vec![
                vec![word(0.0, 0.5, "dropped window")],
                vec![],
                vec![word(70.0, 70.5, "second window")],
                vec![],
            ],
            vec![],
        );
        let (tx, rx) = sync_channel::<CaptureChunk>(8);
        for _ in 0..4 {
            tx.send(CaptureChunk {
                mic: Vec::new(),
                tap: vec![0.0; 30],
            })
            .unwrap();
        }
        drop(tx);

        let publisher = StatusPublisher::default();
        let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
        consume_chunks(
            &rx,
            1,
            &mut None,
            &mut mic,
            &mut them,
            &NoDiarizer,
            &mut window,
            &mut writer,
            &publisher,
        )
        .expect("a failed checkpoint must not end the session");

        assert_eq!(publisher.errors.borrow().len(), 1, "leading edge only");
        assert_eq!(*publisher.listening.borrow(), 1, "recovered once");
        assert_eq!(
            window.frames, 0,
            "the failed window is cleared, not retained"
        );
        assert!(
            writer.final_transcript().is_none(),
            "a dropped window must force the batch rewrite"
        );
        let content = read(&note);
        assert!(content.contains("second window"));
        assert!(!content.contains("dropped window"));
    }

    /// A revoked vault grant must reach the tray as the remedy, not as a raw `Operation not permitted` path.
    #[test]
    fn degraded_detail_names_the_remedy_for_a_revoked_grant() {
        let denied = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            .context("opening newly-created note /Users/x/brain/00-Inbox/note.md");
        let detail = degraded_detail(&denied);
        assert!(detail.contains("Full Disk Access"), "{detail}");
        assert!(!detail.contains("Operation not permitted"), "{detail}");

        let other = anyhow::anyhow!("disk full");
        assert!(degraded_detail(&other).contains("disk full"));
    }

    /// A later write failure cannot truncate a prior synced chunk.
    #[test]
    fn failed_later_append_preserves_prior_chunk() {
        use std::os::unix::fs::PermissionsExt;

        let filer = TempFiler::new("append-failure");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);
        writer
            .append_segment(&TranscriptSegment {
                speaker: Speaker::Me,
                start: 0.0,
                end: 0.5,
                text: "durable prefix".into(),
            })
            .unwrap();
        let before = read(&note);
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o444)).unwrap();
        let result = writer.append_segment(&TranscriptSegment {
            speaker: Speaker::Me,
            start: 60.0,
            end: 60.5,
            text: "must not appear".into(),
        });
        std::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(result.is_err());
        assert_eq!(read(&note), before);
        assert_eq!(writer.path(), Some(&note), "fallback must retain ownership");
    }

    #[test]
    fn dropped_chunks_flush_tails_without_flipping_state() {
        let filer = TempFiler::new("dropped");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);
        writer
            .append_segment(&TranscriptSegment {
                speaker: Speaker::Other("Them".into()),
                start: 0.0,
                end: 0.5,
                text: "live prefix".into(),
            })
            .unwrap();

        let mic = Scripted::new(vec![], vec![word(2.0, 2.5, "flushed tail")]);
        let them = Scripted::new(vec![], vec![]);
        let outcome = finish_session(
            48_000,
            None,
            mic,
            them,
            NoDiarizer,
            TranscriptWindow::new(48_000, 1, false, CleanupConfig::default()).unwrap(),
            FinishQuality { dropped_chunks: 3 },
            &mut writer,
            &NoopPublisher,
        )
        .unwrap();

        let LiveOutcome::Fallback {
            reason,
            note_path: Some(note_path),
        } = outcome
        else {
            panic!("a dropped tee must require fallback");
        };
        assert_eq!(note_path, note);
        assert!(reason.contains("3 chunk"));
        let content = read(&note);
        assert!(content.contains("**[00:02] Me:** flushed tail"));
        assert!(content.contains("State: transcribing\n"));
        assert!(!content.contains("State: transcribed \n"));
    }

    #[test]
    fn silent_session_creates_no_note() {
        let filer = TempFiler::new("silent");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);
        let mut them = Scripted::new(vec![], vec![]);
        let mut mic = Scripted::new(vec![], vec![]);
        let (tx, rx) = sync_channel::<CaptureChunk>(2);
        tx.send(CaptureChunk {
            mic: Vec::new(),
            tap: vec![0.0; 10],
        })
        .unwrap();
        drop(tx);
        let mut aec = None;
        let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
        consume_chunks(
            &rx,
            1,
            &mut aec,
            &mut mic,
            &mut them,
            &NoDiarizer,
            &mut window,
            &mut writer,
            &NoopPublisher,
        )
        .unwrap();
        let outcome = finish_session(
            1,
            aec,
            mic,
            them,
            NoDiarizer,
            window,
            FinishQuality { dropped_chunks: 0 },
            &mut writer,
            &NoopPublisher,
        )
        .unwrap();
        assert!(matches!(outcome, LiveOutcome::NoNote));
        assert!(!note.exists());
    }

    fn install_active(
        manager: &LiveManager,
        id: &str,
        dropped_chunks: u64,
        run: impl FnOnce(Receiver<Verdict>) -> LiveOutcome + Send + 'static,
    ) {
        let (verdict_tx, verdict_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || run(verdict_rx));
        let replaced = manager.inner.lock().unwrap().active.replace(Active {
            id: id.to_string(),
            verdict_tx,
            handle,
            dropped: Arc::new(AtomicU64::new(dropped_chunks)),
            discard_reporter: None,
        });
        assert!(replaced.is_none());
    }

    /// Poll the same reservation gate `AppLiveHook::attach` uses. Success also proves every older finishing
    /// handle was reaped to a small outcome; clear the synthetic pending reservation before installing the
    /// next test session.
    fn wait_for_model_slot(manager: &LiveManager) {
        use std::time::{Duration, Instant};

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let (tx, rx) = sync_channel(1);
            let dropped = Arc::new(AtomicU64::new(0));
            if manager.stash_pending(rx, dropped) {
                drop(tx);
                manager.take_pending();
                return;
            }
            drop(tx);
            assert!(
                Instant::now() < deadline,
                "live model slot never became free"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn microphone_test_reservation_excludes_calls_and_uses_generation_ownership() {
        let manager = LiveManager::new();
        assert!(manager.reserve_test(7));
        assert!(!manager.reserve_test(8), "a second test cannot overlap");

        let (tx, rx) = sync_channel(1);
        assert!(!manager.stash_pending(rx, Arc::new(AtomicU64::new(0))));
        drop(tx);
        manager.release_test(8);
        let (tx, rx) = sync_channel(1);
        assert!(
            !manager.stash_pending(rx, Arc::new(AtomicU64::new(0))),
            "stale cleanup cannot release generation 7"
        );
        drop(tx);

        manager.release_test(7);
        let (tx, rx) = sync_channel(1);
        assert!(manager.stash_pending(rx, Arc::new(AtomicU64::new(0))));
        drop(tx);
        manager.take_pending();
    }

    /// Collection moves a running handle out before joining it. The Collecting sentinel must keep the
    /// single-model gate closed for that entire blocking join, not merely until `collect` takes the handle.
    #[test]
    fn collecting_join_keeps_single_model_gate_closed() {
        use std::time::{Duration, Instant};

        let manager = Arc::new(LiveManager::new());
        let (finishing_tx, finishing_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        install_active(&manager, "a", 0, move |verdicts| {
            assert!(matches!(verdicts.recv().unwrap(), Verdict::Finish(_)));
            finishing_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            LiveOutcome::NoNote
        });
        manager.finish("a");
        finishing_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let collecting_manager = manager.clone();
        let collector = std::thread::spawn(move || collecting_manager.collect("a"));
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if matches!(
                manager.inner.lock().unwrap().awaiting.get("a"),
                Some(AwaitingFinish::Collecting)
            ) {
                break;
            }
            assert!(Instant::now() < deadline, "collector never entered join");
            std::thread::sleep(Duration::from_millis(5));
        }

        let (blocked_tx, blocked_rx) = sync_channel(1);
        assert!(
            !manager.stash_pending(blocked_rx, Arc::new(AtomicU64::new(0))),
            "a second model must not start while collection is joining A"
        );
        drop(blocked_tx);
        release_tx.send(()).unwrap();
        assert!(matches!(
            collector.join().unwrap(),
            Some(LiveOutcome::NoNote)
        ));
        wait_for_model_slot(&manager);
    }

    /// Reproduce the serial-pipeline race: A receives its finish verdict but collection is blocked; after
    /// A's thread finishes, B starts. Collecting A by ID must neither finish nor remove B, and A is
    /// collectable exactly once.
    #[test]
    fn delayed_a_collection_does_not_disturb_active_b() {
        use std::time::Duration;

        let dir = TempFiler::new("delayed-a-b").dir;
        let note_a = dir.join("a.md");
        std::fs::write(&note_a, "A").unwrap();
        let manager = LiveManager::new();
        let (a_finishing_tx, a_finishing_rx) = std::sync::mpsc::channel();
        let (release_a_tx, release_a_rx) = std::sync::mpsc::channel();
        let note_for_a = note_a.clone();
        install_active(&manager, "a", 0, move |verdicts| {
            assert!(matches!(
                verdicts.recv().unwrap(),
                Verdict::Finish(FinishQuality { dropped_chunks: 0 })
            ));
            a_finishing_tx.send(()).unwrap();
            release_a_rx.recv().unwrap();
            LiveOutcome::Filed {
                note_path: note_for_a,
            }
        });

        manager.finish("a");
        a_finishing_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(manager.accepts_note("a"));

        // A is genuinely still flushing: the manager must refuse a second live model.
        let (blocked_tx, blocked_rx) = sync_channel(1);
        assert!(!manager.stash_pending(blocked_rx, Arc::new(AtomicU64::new(0))));
        drop(blocked_tx);

        // Once A has returned, the reservation reaps its handle to a Ready outcome, allowing B to start
        // even though A's serial pipeline Process remains blocked.
        release_a_tx.send(()).unwrap();
        wait_for_model_slot(&manager);
        install_active(&manager, "b", 0, |verdicts| {
            assert!(matches!(verdicts.recv().unwrap(), Verdict::Discard));
            LiveOutcome::NoNote
        });
        assert!(manager.accepts_note("b"));

        let Some(LiveOutcome::Filed { note_path }) = manager.collect("a") else {
            panic!("A's own filed outcome must survive until collection");
        };
        assert_eq!(note_path, note_a);
        assert!(
            manager.accepts_note("b"),
            "collecting A must leave B active"
        );
        assert!(manager.collect("a").is_none(), "A must be collected once");
        assert_eq!(read(&note_a), "A", "A must still have exactly its one note");
        manager.discard("b");
    }

    /// More than one finish can wait behind the serial pipeline. Outcomes are retained and collected by
    /// their own IDs rather than overwritten by whichever recording finished most recently.
    #[test]
    fn multiple_delayed_finishes_are_collected_by_id() {
        let manager = LiveManager::new();
        for id in ["a", "b", "c"] {
            let path = PathBuf::from(format!("/tmp/{id}.md"));
            install_active(&manager, id, 0, move |verdicts| {
                assert!(matches!(verdicts.recv().unwrap(), Verdict::Finish(_)));
                LiveOutcome::Filed { note_path: path }
            });
            manager.finish(id);
            wait_for_model_slot(&manager);
        }

        for id in ["b", "a", "c"] {
            let Some(LiveOutcome::Filed { note_path }) = manager.collect(id) else {
                panic!("missing outcome for {id}");
            };
            assert_eq!(note_path, PathBuf::from(format!("/tmp/{id}.md")));
        }
    }

    /// A live thread can fail before it consumes the later Discard verdict. The detached reaper must still
    /// join that completed handle and remove the partial note path carried by its fallback outcome.
    #[test]
    fn discard_reaper_removes_partial_note_from_failed_session() {
        use std::time::{Duration, Instant};

        let filer = TempFiler::new("discard-failed");
        let note = filer.note();
        std::fs::write(&note, "partial").unwrap();
        let manager = LiveManager::new();
        let failed_note = note.clone();
        install_active(&manager, "failed", 0, move |_verdicts| {
            LiveOutcome::Fallback {
                reason: "contained panic".to_string(),
                note_path: Some(failed_note),
            }
        });

        manager.discard("failed");
        assert!(!manager.accepts_note("failed"));
        let deadline = Instant::now() + Duration::from_secs(2);
        while note.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !note.exists(),
            "discard reaper left the partial note behind"
        );
    }

    #[test]
    fn discarded_session_keeps_single_model_gate_closed_until_reaped() {
        use std::time::Duration;

        let manager = LiveManager::new();
        let (draining_tx, draining_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        install_active(&manager, "discarded", 0, move |verdicts| {
            assert!(matches!(verdicts.recv().unwrap(), Verdict::Discard));
            draining_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            LiveOutcome::NoNote
        });

        manager.discard("discarded");
        draining_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let (blocked_tx, blocked_rx) = sync_channel(1);
        assert!(
            !manager.stash_pending(blocked_rx, Arc::new(AtomicU64::new(0))),
            "discard draining must remain inside the one-model gate"
        );
        drop(blocked_tx);

        release_tx.send(()).unwrap();
        wait_for_model_slot(&manager);
    }

    #[test]
    fn reaper_spawn_failure_retains_handle_and_reporter_for_pipeline_cleanup() {
        use std::time::Duration;

        let filer = TempFiler::new("discard-spawn-failure");
        let blocked = filer.dir.join("blocked.md");
        std::fs::create_dir(&blocked).unwrap();
        let outcome_path = blocked.clone();
        let handle = std::thread::spawn(move || LiveOutcome::Fallback {
            reason: "contained failure".to_string(),
            note_path: Some(outcome_path),
        });
        let (pipe_tx, pipe_rx) = std::sync::mpsc::channel();
        let state = spawn_discard_reaper_with(
            DiscardWork {
                id: "spawn-failed".to_string(),
                handle,
                reporter: Some(DiscardReporter {
                    meta: meta(),
                    pipe_tx,
                }),
            },
            |_| Err(std::io::Error::other("synthetic spawn exhaustion")),
        );
        assert!(matches!(state, Discarding::Inline(_)));
        let PipelineMsg::LiveDiscardReap { id } =
            pipe_rx.recv_timeout(Duration::from_secs(2)).unwrap()
        else {
            panic!("pipeline did not receive failed reaper ownership")
        };
        assert_eq!(id, "spawn-failed");

        let manager = LiveManager::new();
        manager
            .inner
            .lock()
            .unwrap()
            .discarding
            .insert("spawn-failed".to_string(), state);
        manager.reap_unspawned_discard("spawn-failed");
        let PipelineMsg::LiveDiscardCleanup { note_path, .. } =
            pipe_rx.recv_timeout(Duration::from_secs(2)).unwrap()
        else {
            panic!("pipeline did not inherit failed reaper cleanup")
        };
        assert_eq!(note_path, blocked);
        assert!(manager.inner.lock().unwrap().discarding.is_empty());
        std::fs::remove_dir(blocked).unwrap();
    }

    #[test]
    fn failed_discard_unlink_keeps_and_reports_the_path() {
        use std::time::Duration;

        let filer = TempFiler::new("discard-unlink-failure");
        let blocked = filer.dir.join("blocked.md");
        std::fs::create_dir(&blocked).unwrap(); // remove_file deterministically fails for a directory
        let mut writer = NoteWriter::new(filer, meta(), None);
        writer.note = Some(blocked.clone());
        assert!(writer.discard().is_err());
        assert_eq!(writer.path(), Some(&blocked));

        let (pipe_tx, pipe_rx) = std::sync::mpsc::channel();
        let outcome_path = blocked.clone();
        let handle = std::thread::spawn(move || LiveOutcome::Fallback {
            reason: "discard unlink failed".to_string(),
            note_path: Some(outcome_path),
        });
        let _reaper = spawn_discard_reaper(DiscardWork {
            id: "blocked".to_string(),
            handle,
            reporter: Some(DiscardReporter {
                meta: meta(),
                pipe_tx,
            }),
        });
        let PipelineMsg::LiveDiscardCleanup {
            note_path, error, ..
        } = pipe_rx.recv_timeout(Duration::from_secs(2)).unwrap()
        else {
            panic!("reaper did not hand the failed path to the pipeline");
        };
        assert_eq!(note_path, blocked);
        assert!(!error.is_empty());
        std::fs::remove_dir(blocked).unwrap();
    }

    /// The fallback decision: config-level eligibility.
    #[test]
    fn live_eligible_checks_flag_and_backend() {
        let mut cfg = AppConfig {
            live_filing: true,
            transcribe_backend: BackendChoice::Local,
            ..AppConfig::default()
        };
        if cfg!(feature = "local") {
            assert!(live_eligible(&cfg).is_ok());
        } else {
            assert!(live_eligible(&cfg).is_err());
        }

        cfg.live_filing = false;
        assert_eq!(live_eligible(&cfg), Err("live_filing is off"));

        cfg.live_filing = true;
        cfg.transcribe_backend = BackendChoice::Aws;
        assert_eq!(live_eligible(&cfg), Err("transcribe backend is not local"));
    }
}
