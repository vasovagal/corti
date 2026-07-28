#![cfg_attr(not(feature = "local"), allow(dead_code))]

//! Live inbox filing (issue #87, ADR 0010): transcribe a detector recording **while it records** and
//! append finalized segments to the vagus inbox note as they land, so `tail -f` on the note shows the
//! conversation arriving and the end-of-call batch spike disappears.
//!
//! ## Shape
//! [`AppLiveHook`] implements `corti_detect::LiveHook`: at recording start it checks eligibility
//! (config `live_filing`, local backend, models on disk) and, if eligible, hands the detector a bounded
//! lossy [`CaptureTee`]; once capture is running it spawns ONE `corti-live` std thread for the recording.
//! That thread drains tee chunks → [`StreamingAec::push`] on the mic (mirroring `corti-tap --live`'s
//! gating) → two `LiveTranscriber`s (mic → `Me`, tap → `Them`) → closed segments are appended to the
//! note, which is created lazily on the FIRST finalized segment (a too-short discarded recording almost
//! never creates one). The thread never blocks the capture writer (the tee already drops + counts when
//! the consumer falls behind) and is panic-contained — any failure degrades to the batch path.
//!
//! ## Finish / discard
//! The tee sender is dropped when the recorder stops, which ends the chunk loop; the thread then waits
//! for an explicit recording-specific verdict so a finish and discard are never confused. The detector's
//! [`LiveHook`] delivers that verdict immediately, before its later pipeline message:
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
//! Segments are appended in **finish order**, which may interleave `Me`/`Them` differently than the
//! batch path's `merge_by_time` — accepted by design; the segment *lines* are byte-identical to
//! `DiarizedTranscript::to_markdown`'s.

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
use corti_transcribe::segment::{SEGMENT_GAP, Word, merge_by_time};
use tracing::{info, warn};

use crate::config::{AppConfig, BackendChoice};
use crate::pipeline::PipelineMsg;
use crate::settings::SharedConfig;

/// Bounded tee backlog in chunks (~4096 frames ≈ 85 ms each at 48 kHz, so ≈ 22 s of slack, ≤ ~8 MB).
/// Sized to absorb the one-time model/engine load — which happens on the live thread, never the detect
/// worker — plus decode bursts, before the lossy tee starts dropping (drops are counted, not fatal).
const TEE_BACKLOG: usize = 256;

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
}

#[derive(Default)]
struct Inner {
    /// Tee receiver stashed between `LiveHook::attach` and `LiveHook::started`.
    pending: Option<Pending>,
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
        Self {
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Reserve the one active-model slot for the tee being attached. A previous finished outcome does not
    /// block a new call, but a previous thread still flushing does: two large local model sessions must not
    /// overlap. Returns false without replacing any existing session.
    fn stash_pending(&self, rx: Receiver<CaptureChunk>, dropped: Arc<AtomicU64>) -> bool {
        let mut inner = self.inner.lock().unwrap();
        inner.reap_completed();
        if inner.pending.is_some() || inner.active.is_some() || inner.has_finishing_thread() {
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
            let (verdict_tx, verdict_rx) = std::sync::mpsc::channel::<Verdict>();
            let dropped = pending.dropped.clone();
            let discard_reporter = DiscardReporter {
                meta: meta.clone(),
                pipe_tx: pipe_tx.clone(),
            };
            let thread = std::thread::Builder::new().name("corti-live".into()).spawn(
                move || -> LiveOutcome {
                    session_thread(pending.rx, verdict_rx, meta, sample_rate, cfg, pipe_tx)
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
                        if inner.active.is_none() && !inner.has_finishing_thread() {
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

/// Cheap file-existence validation of the local model cache (no engine load).
#[cfg(feature = "local")]
fn discover_models(cfg: &AppConfig) -> Result<()> {
    let dir = corti_transcribe_local::models::resolve_dir(cfg.local_model_dir.clone())?;
    corti_transcribe_local::models::discover(&dir, false, &cfg.local_embedding_model)?;
    Ok(())
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
) -> LiveOutcome {
    let mut writer = NoteWriter::new(VagusFiler, meta.clone(), Some(pipe_tx));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_session(&rx, &verdict_rx, sample_rate, &cfg, &mut writer)
    }));
    match result {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(e)) => LiveOutcome::Fallback {
            reason: format!("{e:#}"),
            note_path: writer.path().cloned(),
        },
        Err(_) => LiveOutcome::Fallback {
            reason: "live transcription panicked".to_string(),
            note_path: writer.path().cloned(),
        },
    }
}

/// Everything a running session owns besides the writer, so an engine/consume error can be parked
/// while the parts (and the verdict logic) stay in one place.
#[cfg(feature = "local")]
struct SessionParts {
    mic: corti_transcribe_local::LiveTranscriber,
    them: corti_transcribe_local::LiveTranscriber,
    aec: Option<StreamingAec>,
    mic_seg: Segmenter,
    them_seg: Segmenter,
}

/// Build the engine + per-channel state on the live thread — chunks buffer in the bounded tee
/// meanwhile.
#[cfg(feature = "local")]
fn build_parts(sample_rate: u32, cfg: &AppConfig) -> Result<SessionParts> {
    use corti_transcribe_local::{LocalConfig, LocalTranscriber};

    let local_cfg = LocalConfig {
        model_dir: cfg.local_model_dir.clone(),
        provider: cfg.local_provider.clone(),
        num_threads: cfg.local_threads,
        // Far-end diarization never runs live: the tap channel is a single `Them`, like the batch
        // default and `corti-tap --live`. Everything else stays at the shipping defaults.
        ..LocalConfig::default()
    };
    let engine = LocalTranscriber::new(local_cfg)
        .live_engine()
        .context("loading the local live engine")?;
    Ok(SessionParts {
        mic: engine.channel().context("building the mic transcriber")?,
        them: engine.channel().context("building the tap transcriber")?,
        // Streaming AEC on the mic, per config (skipped cleanly per-chunk when the mic side is empty).
        aec: cfg
            .aec_enabled
            .then(|| StreamingAec::new(sample_rate, cfg.aec_config())),
        mic_seg: Segmenter::new(Speaker::Me),
        them_seg: Segmenter::new(Speaker::Other("Them".to_string())),
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
) -> Result<LiveOutcome> {
    let mut parts = build_parts(sample_rate, cfg);
    let consumed = match parts.as_mut() {
        Ok(p) => consume_chunks(
            rx,
            sample_rate,
            &mut p.aec,
            &mut p.mic,
            &mut p.them,
            &mut p.mic_seg,
            &mut p.them_seg,
            writer,
        ),
        // Engine failed to load: fall through to the verdict wait; the error surfaces on Finish.
        Err(_) => Ok(()),
    };

    match verdict_rx.recv() {
        Ok(Verdict::Finish(quality)) => {
            let p = parts?;
            consumed?;
            finish_session(
                sample_rate,
                p.aec,
                p.mic,
                p.them,
                p.mic_seg,
                p.them_seg,
                quality,
                writer,
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

/// Drain tee chunks until the sender (the capture writer) hangs up. Mirrors `corti-tap --live`'s
/// per-chunk gating: the AEC/mic side keys on the actual chunk data, never on the capture mode, and
/// `StreamingAec::push` is only reached with equal-length mic/tap blocks.
#[allow(clippy::too_many_arguments)] // the split keeps every piece testable without models
fn consume_chunks<C: LiveChannel, F: NoteFiler>(
    rx: &Receiver<CaptureChunk>,
    sample_rate: u32,
    aec: &mut Option<StreamingAec>,
    mic: &mut C,
    them: &mut C,
    mic_seg: &mut Segmenter,
    them_seg: &mut Segmenter,
    writer: &mut NoteWriter<F>,
) -> Result<()> {
    while let Ok(chunk) = rx.recv() {
        let clean = match aec.as_mut() {
            Some(aec) if !chunk.mic.is_empty() && chunk.mic.len() == chunk.tap.len() => {
                aec.push(&chunk.mic, &chunk.tap) // cleaned mic (empty while the lookahead warms)
            }
            Some(_) => Vec::new(), // no usable mic data this chunk — skip so the length assert stays unreachable
            None => chunk.mic.clone(),
        };
        if !clean.is_empty() {
            mic.push(&clean, sample_rate);
        }
        if let Some(words) = mic.poll_words() {
            append_closed(mic_seg, &words, writer)?;
        }
        if !chunk.tap.is_empty() {
            them.push(&chunk.tap, sample_rate);
        }
        if let Some(words) = them.poll_words() {
            append_closed(them_seg, &words, writer)?;
        }
    }
    Ok(())
}

/// Feed a poll batch through the segmenter and append every segment it closed.
fn append_closed<F: NoteFiler>(
    seg: &mut Segmenter,
    words: &[Word],
    writer: &mut NoteWriter<F>,
) -> Result<()> {
    for segment in seg.push_words(words) {
        writer.append_segment(&segment)?;
    }
    Ok(())
}

/// Finish the AEC/transcriber tails and append remaining segments merged by start time. The state line is
/// flipped only when the detector's terminal quality verdict says the bounded tee was lossless; any drop
/// leaves the note visibly `transcribing` for the canonical batch rewrite.
#[allow(clippy::too_many_arguments)] // explicit channel/segment state keeps this model-free test seam small
fn finish_session<C: LiveChannel, F: NoteFiler>(
    sample_rate: u32,
    mut aec: Option<StreamingAec>,
    mut mic: C,
    mut them: C,
    mut mic_seg: Segmenter,
    mut them_seg: Segmenter,
    quality: FinishQuality,
    writer: &mut NoteWriter<F>,
) -> Result<LiveOutcome> {
    if let Some(aec) = aec.take() {
        let tail = aec.finish();
        if !tail.is_empty() {
            mic.push(&tail, sample_rate);
        }
    }
    let mut finals: Vec<TranscriptSegment> = Vec::new();
    let mic_words = mic.finish();
    finals.extend(mic_seg.push_words(&mic_words));
    finals.extend(mic_seg.take());
    let them_words = them.finish();
    finals.extend(them_seg.push_words(&them_words));
    finals.extend(them_seg.take());
    for segment in merge_by_time(finals) {
        writer.append_segment(&segment)?;
    }
    match writer.path().cloned() {
        Some(note_path) if quality.dropped_chunks == 0 => {
            corti_vagus::note::flip_state(&note_path).context("flipping the note's state line")?;
            info!(
                target: "corti::live",
                note_path = %note_path.display(),
                "live note finalized — state flipped to transcribed"
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

/// The slice of `LiveTranscriber` the consumer loop needs — a seam so the loop is unit-testable with a
/// scripted channel instead of the real ONNX models.
trait LiveChannel {
    fn push(&mut self, samples: &[f32], sample_rate: u32);
    fn poll_words(&mut self) -> Option<Vec<Word>>;
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
    fn finish(&mut self) -> Vec<Word> {
        corti_transcribe_local::LiveTranscriber::finish(self)
    }
}

/// Incremental twin of `corti_transcribe::segment::words_to_segments`: same gap rule (`SEGMENT_GAP`),
/// but words arrive in poll batches and a segment is only *closed* (returned) when a later word starts
/// past the gap — or at [`take`](Self::take) on finish.
struct Segmenter {
    speaker: Speaker,
    cur: Option<TranscriptSegment>,
}

impl Segmenter {
    fn new(speaker: Speaker) -> Self {
        Self { speaker, cur: None }
    }

    /// Feed a batch of words; return the segments this batch closed.
    fn push_words(&mut self, words: &[Word]) -> Vec<TranscriptSegment> {
        let mut closed = Vec::new();
        for w in words {
            if w.text.is_empty() {
                continue;
            }
            match self.cur.as_mut() {
                Some(seg) if w.start - seg.end <= SEGMENT_GAP => {
                    seg.text.push(' ');
                    seg.text.push_str(&w.text);
                    seg.end = w.end;
                }
                _ => {
                    if let Some(done) = self.cur.take() {
                        closed.push(done);
                    }
                    self.cur = Some(TranscriptSegment {
                        speaker: self.speaker.clone(),
                        start: w.start,
                        end: w.end,
                        text: w.text.clone(),
                    });
                }
            }
        }
        closed
    }

    /// The still-open trailing segment, if any (call at finish).
    fn take(&mut self) -> Option<TranscriptSegment> {
        self.cur.take()
    }
}

/// How a note gets created — a seam so [`NoteWriter`] is testable against temp files. The production
/// impl shells out to `vagus add-note --print-path` (the ADR 0001 boundary).
trait NoteFiler {
    fn create_note(&self, title: &str, source: &str, body: &str) -> Result<PathBuf>;
}

/// Production filer: vagus is discovered lazily, at first-segment time — a missing binary is a
/// live-path error and the batch path (with its own re-discovery) takes over.
#[cfg(feature = "local")]
struct VagusFiler;

#[cfg(feature = "local")]
impl NoteFiler for VagusFiler {
    fn create_note(&self, title: &str, source: &str, body: &str) -> Result<PathBuf> {
        corti_vagus::Vagus::discover()?.add_note(title, source, body)
    }
}

/// Lazily creates the inbox note on the first finalized segment and appends one rendered line per
/// segment. Reports the created path to the pipeline (`PipelineMsg::LiveNoteCreated`) so it is
/// persisted into the queue row as soon as it exists.
struct NoteWriter<F: NoteFiler> {
    filer: F,
    meta: RecordingMeta,
    pipe_tx: Option<Sender<PipelineMsg>>,
    note: Option<PathBuf>,
}

impl<F: NoteFiler> NoteWriter<F> {
    fn new(filer: F, meta: RecordingMeta, pipe_tx: Option<Sender<PipelineMsg>>) -> Self {
        Self {
            filer,
            meta,
            pipe_tx,
            note: None,
        }
    }

    /// Append one segment, creating the note first if this is the first one. The line is rendered by
    /// the same code the batch note uses (`DiarizedTranscript::to_markdown` over a single segment), so
    /// live and batch notes are line-for-line identical in shape.
    fn append_segment(&mut self, segment: &TranscriptSegment) -> Result<()> {
        if self.note.is_none() {
            self.create()?;
        }
        let line = DiarizedTranscript::new(vec![segment.clone()]).to_markdown();
        corti_vagus::note::append(self.note.as_ref().expect("just created"), &line)
    }

    fn create(&mut self) -> Result<()> {
        let path = self
            .filer
            .create_note(
                &self.meta.note_title(),
                &self.meta.source(),
                &corti_vagus::live_initial_body(&self.meta),
            )
            .context("creating the live inbox note")?;
        info!(
            target: "corti::live",
            note_path = %path.display(),
            "live inbox note created (State: transcribing)"
        );
        if let Some(tx) = &self.pipe_tx {
            let _ = tx.send(PipelineMsg::LiveNoteCreated {
                meta: self.meta.clone(),
                note_path: path.clone(),
            });
        }
        self.note = Some(path);
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
    use corti_transcribe::segment::words_to_segments;
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
        fn finish(&mut self) -> Vec<Word> {
            std::mem::take(&mut self.tail)
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
        fn create_note(&self, title: &str, source: &str, body: &str) -> Result<PathBuf> {
            let p = self.note();
            std::fs::write(
                &p,
                format!(
                    "---\ncreated: x\nstatus: inbox\nsource: {source}\n---\n\n# {title}\n\n{body}"
                ),
            )?;
            Ok(p)
        }
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }

    /// The incremental segmenter must reproduce `words_to_segments` exactly, no matter how the word
    /// stream is split into poll batches.
    #[test]
    fn segmenter_matches_batch_words_to_segments() {
        let words = [
            word(0.0, 0.4, "Morning"),
            word(0.5, 0.8, "team."),
            word(3.0, 3.4, "Thanks"),
            word(3.5, 3.9, "all."),
            word(9.0, 9.5, "Bye"),
        ];
        let batch = words_to_segments(&words, Speaker::Me, SEGMENT_GAP);

        for split in [1usize, 2, 3, 5] {
            let mut seg = Segmenter::new(Speaker::Me);
            let mut got = Vec::new();
            for chunk in words.chunks(split) {
                got.extend(seg.push_words(chunk));
            }
            got.extend(seg.take());
            assert_eq!(got, batch, "split size {split}");
        }
    }

    /// Lazy creation, exact appended strings, and delete-on-discard.
    #[test]
    fn note_writer_creates_lazily_appends_exact_lines_and_discards() {
        let filer = TempFiler::new("writer");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);

        // No segment yet ⇒ no note (the whole point of lazy creation).
        assert!(!note.exists());
        assert!(writer.path().is_none());

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
            content.contains("State: transcribing\n\n"),
            "got: {content}"
        );
        assert!(content.contains("## Transcript\n\n"));
        // The segment line is byte-identical to DiarizedTranscript::to_markdown's rendering.
        assert!(
            content.ends_with("**[00:00] Me:** hello there\n\n"),
            "got: {content}"
        );

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

    /// End-to-end over the loop seams: empty-mic chunks never reach the mic channel (the corti-tap
    /// gating), segments appear as they close, the finish tails are merged by start time, and the
    /// state line flips only at finalize.
    #[test]
    fn consume_and_finish_append_segments_and_flip_state() {
        let filer = TempFiler::new("loop");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);

        // them: chunk 1 yields an utterance at 0s; chunk 2 yields one at 5s (closes the first);
        // finish yields a tail at 20s. mic: silent during the call, one tail utterance at 10s.
        let mut them = Scripted::new(
            vec![
                vec![word(0.0, 0.5, "hi"), word(0.6, 1.0, "Xavier")],
                vec![word(5.0, 5.5, "anyway")],
            ],
            vec![word(20.0, 20.5, "bye")],
        );
        let mut mic = Scripted::new(vec![], vec![word(10.0, 10.5, "thanks")]);
        let mut mic_seg = Segmenter::new(Speaker::Me);
        let mut them_seg = Segmenter::new(Speaker::Other("Them".into()));

        let (tx, rx) = sync_channel::<CaptureChunk>(8);
        tx.send(CaptureChunk {
            mic: Vec::new(), // no usable mic data — must not reach the mic channel
            tap: vec![0.0; 4096],
        })
        .unwrap();
        tx.send(CaptureChunk {
            mic: Vec::new(),
            tap: vec![0.0; 4096],
        })
        .unwrap();
        drop(tx); // recorder stopped

        let mut aec = None;
        consume_chunks(
            &rx,
            48_000,
            &mut aec,
            &mut mic,
            &mut them,
            &mut mic_seg,
            &mut them_seg,
            &mut writer,
        )
        .unwrap();

        assert!(
            mic.pushes.is_empty(),
            "empty mic chunks must never be pushed"
        );
        assert_eq!(them.pushes.len(), 2);
        // The first them-utterance closed when the 5s word arrived; the 5s one is still open.
        let mid_call = read(&note);
        assert!(
            mid_call.contains("State: transcribing\n"),
            "got: {mid_call}"
        );
        assert!(
            mid_call.ends_with("**[00:00] Them:** hi Xavier\n\n"),
            "got: {mid_call}"
        );

        let outcome = finish_session(
            48_000,
            aec,
            mic,
            them,
            mic_seg,
            them_seg,
            FinishQuality { dropped_chunks: 0 },
            &mut writer,
        )
        .unwrap();
        let LiveOutcome::Filed { note_path } = outcome else {
            panic!("expected Filed");
        };
        assert_eq!(note_path, note);

        let final_content = read(&note);
        // Tails are merged by start time: the open 5s them-segment, then mic 10s, then them 20s.
        assert!(
            final_content.ends_with(
                "**[00:00] Them:** hi Xavier\n\n\
                 **[00:05] Them:** anyway\n\n\
                 **[00:10] Me:** thanks\n\n\
                 **[00:20] Them:** bye\n\n"
            ),
            "got: {final_content}"
        );
        assert!(
            final_content.contains("State: transcribed \n"),
            "state flipped"
        );
        assert!(!final_content.contains("State: transcribing"));
    }

    /// A lossy tee still flushes decoder tails, but the result is non-canonical: the note stays visibly
    /// transcribing and its existing path is returned for the batch rewrite.
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
            Segmenter::new(Speaker::Me),
            Segmenter::new(Speaker::Other("Them".into())),
            FinishQuality { dropped_chunks: 3 },
            &mut writer,
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
        assert!(reason.contains("3 chunk"), "got: {reason}");
        let content = read(&note);
        assert!(content.contains("**[00:02] Me:** flushed tail"));
        assert!(content.contains("State: transcribing\n"));
        assert!(!content.contains("State: transcribed \n"));
    }

    /// A session with no speech at all creates no note and reports `NoNote` (⇒ batch path).
    #[test]
    fn silent_session_creates_no_note() {
        let filer = TempFiler::new("silent");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);
        let mut them = Scripted::new(vec![], vec![]);
        let mut mic = Scripted::new(vec![], vec![]);
        let mut mic_seg = Segmenter::new(Speaker::Me);
        let mut them_seg = Segmenter::new(Speaker::Other("Them".into()));

        let (tx, rx) = sync_channel::<CaptureChunk>(2);
        tx.send(CaptureChunk {
            mic: Vec::new(),
            tap: vec![0.0; 512],
        })
        .unwrap();
        drop(tx);
        let mut aec = None;
        consume_chunks(
            &rx,
            48_000,
            &mut aec,
            &mut mic,
            &mut them,
            &mut mic_seg,
            &mut them_seg,
            &mut writer,
        )
        .unwrap();
        let outcome = finish_session(
            48_000,
            aec,
            mic,
            them,
            mic_seg,
            them_seg,
            FinishQuality { dropped_chunks: 0 },
            &mut writer,
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
