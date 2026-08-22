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

use std::collections::HashMap;
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
    SEGMENT_GAP, SpeakerTurn, Word, diarize_words, merge_by_time, words_to_segments,
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
/// Bounded in-memory canonical-row assembly used only for the optional final pass after every raw window is
/// already synced. Exceeding it disables that pass and leaves the existing raw note path unchanged.
const MAX_FINAL_TRANSCRIPT_BYTES: usize = 16 * 1024 * 1024;

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
            let trace = crate::offline_trace::live_session("mixed", "local");
            let dispatch = crate::offline_trace::Dispatch::capture();
            let thread = std::thread::Builder::new().name("corti-live".into()).spawn(
                move || -> LiveOutcome {
                    dispatch.with_default(|| {
                        let outcome = session_thread(
                            pending.rx,
                            verdict_rx,
                            meta,
                            sample_rate,
                            cfg,
                            pipe_tx,
                            publisher,
                            &trace,
                        );
                        match &outcome {
                            LiveOutcome::Filed { .. } => trace.ok(),
                            LiveOutcome::NoNote => trace.skipped(),
                            LiveOutcome::Fallback { .. } => trace.fallback(),
                        }
                        outcome
                    })
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
#[allow(clippy::too_many_arguments)] // explicit worker handoff keeps ownership and trace parent visible
fn session_thread(
    rx: Receiver<CaptureChunk>,
    verdict_rx: Receiver<Verdict>,
    meta: RecordingMeta,
    sample_rate: u32,
    cfg: AppConfig,
    pipe_tx: Sender<PipelineMsg>,
    publisher: StorePublisher,
    trace: &crate::offline_trace::Span,
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
        run_session(
            &rx,
            &verdict_rx,
            sample_rate,
            &cfg,
            &mut writer,
            &publisher,
            trace,
        )
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
    )
    .context("reserving the bounded live transcript window")?;
    Ok(SessionParts {
        engine,
        mic,
        them,
        // Streaming AEC on the mic, per config (skipped cleanly per-chunk when the mic side is empty).
        aec: cfg
            .aec_enabled
            .then(|| StreamingAec::new(sample_rate, cfg.aec_config())),
        window,
    })
}

/// Aggregate live spans are created once, then repeatedly entered only around active model/chunk work.
/// Blocking receives therefore accrue idle time rather than falsely inflating busy time.
struct SessionTrace {
    session: crate::offline_trace::Span,
    transcription: crate::offline_trace::Span,
    backend: crate::offline_trace::Span,
    decode: crate::offline_trace::Span,
    aec: crate::offline_trace::Span,
    consume: crate::offline_trace::Span,
}

#[cfg(feature = "local")]
impl SessionTrace {
    fn new(session: &crate::offline_trace::Span, cfg: &AppConfig) -> Self {
        let engine = crate::offline_trace::engine(&cfg.local_asr_engine);
        let transcription = crate::offline_trace::transcription(session, "local", engine);
        Self {
            session: session.clone(),
            backend: crate::offline_trace::transcription_backend(&transcription, "local", engine),
            decode: crate::offline_trace::transcription_decode(&transcription, "local", engine),
            aec: crate::offline_trace::transcription_aec(&transcription, "local", engine),
            consume: crate::offline_trace::live_consume(session, "mixed", "local"),
            transcription,
        }
    }
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
    trace: &crate::offline_trace::Span,
) -> Result<LiveOutcome> {
    let spans = SessionTrace::new(trace, cfg);
    if !cfg.aec_enabled {
        spans.aec.skipped();
    }
    let mut parts = spans.session.in_scope(|| {
        spans
            .transcription
            .in_scope(|| spans.backend.in_scope(|| build_parts(sample_rate, cfg)))
    });
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
                Some(&spans),
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

    let result = (|| -> Result<LiveOutcome> {
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
                    Some(&spans),
                )
            }
            Ok(Verdict::Discard) => {
                writer.discard()?;
                Ok(LiveOutcome::NoNote)
            }
            // Manager gone (app shutting down mid-call): leave whatever was written; don't flip.
            Err(_) => anyhow::bail!("live session received no finish/discard verdict"),
        }
    })();
    match &result {
        Ok(LiveOutcome::NoNote) => {
            spans.backend.skipped();
            spans.decode.skipped();
            if cfg.aec_enabled {
                spans.aec.skipped();
            }
            spans.transcription.skipped();
        }
        Ok(_) => {
            spans.backend.ok();
            spans.decode.ok();
            if cfg.aec_enabled {
                spans.aec.ok();
            }
            spans.transcription.ok();
        }
        Err(_) => {
            spans.backend.error(crate::offline_trace::ErrorCode::Other);
            spans
                .decode
                .error(crate::offline_trace::ErrorCode::DecodeFailed);
            if cfg.aec_enabled {
                spans.aec.error(crate::offline_trace::ErrorCode::Other);
            }
            spans
                .transcription
                .error(crate::offline_trace::ErrorCode::DecodeFailed);
        }
    }
    result
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
}

impl TranscriptWindow {
    fn new(sample_rate: u32, minutes: u32, diarize_far_end: bool) -> Result<Self> {
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
        })
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
    trace: Option<&SessionTrace>,
) -> Result<()> {
    // Do not enter any span across this blocking receive. One aggregate span is repeatedly entered for
    // actual work, yielding useful busy/idle accounting without one record per audio chunk.
    while let Ok(chunk) = rx.recv() {
        let processed = match trace {
            Some(trace) => trace.session.in_scope(|| {
                trace.consume.in_scope(|| {
                    trace.transcription.in_scope(|| {
                        consume_chunk(
                            chunk,
                            sample_rate,
                            aec,
                            mic,
                            them,
                            diarizer,
                            window,
                            writer,
                            publisher,
                            Some(trace),
                        )
                    })
                })
            }),
            None => consume_chunk(
                chunk,
                sample_rate,
                aec,
                mic,
                them,
                diarizer,
                window,
                writer,
                publisher,
                None,
            ),
        };
        if let Err(error) = processed {
            if let Some(trace) = trace {
                trace.consume.error(crate::offline_trace::ErrorCode::Other);
            }
            return Err(error);
        }
    }
    if let Some(trace) = trace {
        trace.consume.ok();
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn consume_chunk<C: LiveChannel, D: LiveDiarizer, F: NoteFiler, P: TranscriptPublisher>(
    chunk: CaptureChunk,
    sample_rate: u32,
    aec: &mut Option<StreamingAec>,
    mic: &mut C,
    them: &mut C,
    diarizer: &D,
    window: &mut TranscriptWindow,
    writer: &mut NoteWriter<F>,
    publisher: &P,
    trace: Option<&SessionTrace>,
) -> Result<()> {
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
            Some(aec) if !mic_slice.is_empty() && mic_slice.len() == tap_slice.len() => match trace
            {
                Some(trace) => trace.aec.in_scope(|| aec.push(mic_slice, tap_slice)),
                None => aec.push(mic_slice, tap_slice),
            },
            Some(_) => Vec::new(),
            None => mic_slice.to_vec(),
        };
        let mut decode = || {
            if !clean.is_empty() {
                mic.push(&clean, sample_rate);
            }
            if !tap_slice.is_empty() {
                them.push(tap_slice, sample_rate);
            }
            if let Some(words) = mic.poll_words() {
                publisher.words(Speaker::Me, &words);
                window.push_mic_words(words);
            }
            if let Some(words) = them.poll_words() {
                publisher.words(Speaker::Other("Them".to_string()), &words);
                window.push_them_words(words);
            }
        };
        match trace {
            Some(trace) => trace.decode.in_scope(|| trace.backend.in_scope(decode)),
            None => decode(),
        }
        window.push_audio(tap_slice, take);
        offset = end;

        if window.due() {
            checkpoint_and_flush(mic, them, diarizer, window, writer, publisher, trace)?;
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
    trace: Option<&SessionTrace>,
) -> Result<()> {
    let mut checkpoint = || {
        let mic_words = mic.checkpoint();
        publisher.words(Speaker::Me, &mic_words);
        window.push_mic_words(mic_words);
        let them_words = them.checkpoint();
        publisher.words(Speaker::Other("Them".to_string()), &them_words);
        window.push_them_words(them_words);
    };
    match trace {
        Some(trace) => trace.decode.in_scope(checkpoint),
        None => checkpoint(),
    }
    flush_window(diarizer, window, writer, trace)
}

/// Diarize and render one complete rolling window, then perform one OS-synced append. No state is cleared
/// until that durability boundary succeeds, so an error still carries the already-created note to fallback.
fn flush_window<D: LiveDiarizer, F: NoteFiler>(
    diarizer: &D,
    window: &mut TranscriptWindow,
    writer: &mut NoteWriter<F>,
    trace: Option<&SessionTrace>,
) -> Result<()> {
    let window_trace = trace
        .map(|trace| crate::offline_trace::live_window_flush(&trace.session, "mixed", "local"));
    let result = match window_trace.as_ref() {
        Some(span) => span.in_scope(|| flush_window_inner(diarizer, window, writer, span)),
        None => flush_window_inner_without_trace(diarizer, window, writer),
    };
    if let Some(span) = window_trace.as_ref() {
        span.record_window_count(1);
        if result.is_ok() {
            span.ok();
        } else {
            span.error(crate::offline_trace::ErrorCode::Other);
        }
    }
    result
}

fn flush_window_inner<D: LiveDiarizer, F: NoteFiler>(
    diarizer: &D,
    window: &mut TranscriptWindow,
    writer: &mut NoteWriter<F>,
    trace: &crate::offline_trace::Span,
) -> Result<()> {
    flush_window_work(diarizer, window, writer, Some(trace))
}

fn flush_window_inner_without_trace<D: LiveDiarizer, F: NoteFiler>(
    diarizer: &D,
    window: &mut TranscriptWindow,
    writer: &mut NoteWriter<F>,
) -> Result<()> {
    flush_window_work(diarizer, window, writer, None)
}

fn flush_window_work<D: LiveDiarizer, F: NoteFiler>(
    diarizer: &D,
    window: &mut TranscriptWindow,
    writer: &mut NoteWriter<F>,
    trace: Option<&crate::offline_trace::Span>,
) -> Result<()> {
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
    if !segments.is_empty() {
        let synced = match trace {
            Some(parent) => {
                let sync = crate::offline_trace::live_note_sync(parent, "mixed", "local");
                let result = sync.in_scope(|| writer.append_segments(&segments));
                sync.record_item_count(segments.len());
                if result.is_ok() {
                    sync.ok();
                } else {
                    sync.error(crate::offline_trace::ErrorCode::Storage);
                }
                result
            }
            None => writer.append_segments(&segments),
        };
        synced?;
        info!(
            target: "corti::live",
            start_sec = window.start_sec(),
            duration_sec = window.frames as f64 / f64::from(window.sample_rate),
            segments = segments.len(),
            buffered_audio_bytes = window.tap_audio.len() * std::mem::size_of::<f32>(),
            "durable live transcript chunk synced"
        );
    }
    window.clear_after_flush();
    Ok(())
}

/// Finish the AEC/transcriber tails, diarize + sync the final short window, then durably flip the state line.
/// A dropped tee leaves the note visibly `transcribing` for the canonical batch rewrite.
#[allow(clippy::too_many_arguments)]
fn finish_session<C: LiveChannel, D: LiveDiarizer, F: NoteFiler, P: TranscriptPublisher>(
    sample_rate: u32,
    aec: Option<StreamingAec>,
    mic: C,
    them: C,
    diarizer: D,
    window: TranscriptWindow,
    quality: FinishQuality,
    writer: &mut NoteWriter<F>,
    publisher: &P,
    trace: Option<&SessionTrace>,
) -> Result<LiveOutcome> {
    match trace {
        Some(trace) => {
            let finish = crate::offline_trace::live_finish(&trace.session, "mixed", "local");
            let result = trace.session.in_scope(|| {
                finish.in_scope(|| {
                    finish_session_inner(
                        sample_rate,
                        aec,
                        mic,
                        them,
                        diarizer,
                        window,
                        quality,
                        writer,
                        publisher,
                        Some(trace),
                    )
                })
            });
            if result.is_ok() {
                finish.ok();
            } else {
                finish.error(crate::offline_trace::ErrorCode::Other);
            }
            result
        }
        None => finish_session_inner(
            sample_rate,
            aec,
            mic,
            them,
            diarizer,
            window,
            quality,
            writer,
            publisher,
            None,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_session_inner<C: LiveChannel, D: LiveDiarizer, F: NoteFiler, P: TranscriptPublisher>(
    sample_rate: u32,
    mut aec: Option<StreamingAec>,
    mut mic: C,
    mut them: C,
    diarizer: D,
    mut window: TranscriptWindow,
    quality: FinishQuality,
    writer: &mut NoteWriter<F>,
    publisher: &P,
    trace: Option<&SessionTrace>,
) -> Result<LiveOutcome> {
    if let Some(aec) = aec.take() {
        let tail = match trace {
            Some(trace) => trace.aec.in_scope(|| aec.finish()),
            None => aec.finish(),
        };
        if !tail.is_empty() {
            match trace {
                Some(trace) => trace.decode.in_scope(|| mic.push(&tail, sample_rate)),
                None => mic.push(&tail, sample_rate),
            }
        }
    }
    if let Some(words) = mic.poll_words() {
        publisher.words(Speaker::Me, &words);
        window.push_mic_words(words);
    }
    if let Some(words) = them.poll_words() {
        publisher.words(Speaker::Other("Them".to_string()), &words);
        window.push_them_words(words);
    }
    let mic_words = mic.finish();
    publisher.words(Speaker::Me, &mic_words);
    window.push_mic_words(mic_words);
    let them_words = them.finish();
    publisher.words(Speaker::Other("Them".to_string()), &them_words);
    window.push_them_words(them_words);
    flush_window(&diarizer, &mut window, writer, trace)?;

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
                let _ = hosted.mark_final_checkpointed(&applied_call_ids);
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
    fn listening(&self) {
        self.store.set_listening(
            &self.id,
            "Listening — lines appear when each speech region closes.",
        );
    }

    fn complete(&self, detail: impl Into<String>) {
        self.store.set_complete(&self.id, detail);
    }

    fn error(&self, detail: impl Into<String>) {
        self.store.set_error(&self.id, detail);
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
        let mut window = TranscriptWindow::new(sample_rate, 1, true).unwrap();
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

        let high_rate = TranscriptWindow::new(192_000, 10, true).unwrap();
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
        let mut window = TranscriptWindow::new(10, 1, true).unwrap();
        window.start_frame = 600; // second minute: proves the absolute offset is supplied
        window.push_audio(&vec![0.0; 100], 100);
        window.push_them_words(vec![word(62.0, 62.5, "owned action item")]);

        flush_window(&diarizer, &mut window, &mut writer, None).unwrap();

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
            &mut TranscriptWindow::new(1, 1, false).unwrap(),
            &mut writer,
            &publisher,
            None,
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
        let mut window = TranscriptWindow::new(1, 1, false).unwrap();
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
            None,
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
            None,
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
            TranscriptWindow::new(48_000, 1, false).unwrap(),
            FinishQuality { dropped_chunks: 3 },
            &mut writer,
            &NoopPublisher,
            None,
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
        let mut window = TranscriptWindow::new(1, 1, false).unwrap();
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
            None,
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
            None,
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
