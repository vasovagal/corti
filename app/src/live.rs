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
//! for an explicit verdict so a finished recording and a discarded one are never confused (live-path
//! errors also park at the verdict wait, so the thread never outruns its recording):
//! - the pipeline's `Process` handler calls [`LiveManager::finalize`] → finish both transcribers, append
//!   the tails (merged by start time), flip the state line in place, report [`LiveOutcome::Filed`];
//! - a discard (too short) or a failed capture finish sends `PipelineMsg::LiveDiscarded` →
//!   [`LiveManager::discard`] delivers the verdict **without joining** and the thread deletes its own
//!   partial note on the way out.
//!
//! Segments are appended in **finalize order**, which may interleave `Me`/`Them` differently than the
//! batch path's `merge_by_time` — accepted by design; the segment *lines* are byte-identical to
//! `DiarizedTranscript::to_markdown`'s.

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

/// How a live session ended, read by the pipeline worker at `Process` time.
pub enum LiveOutcome {
    /// The note is fully written and its state line flipped — the job can go straight to `Done`.
    Filed { note_path: PathBuf },
    /// The session ran but never produced a segment, so no note was created — run the batch path.
    NoNote,
    /// The live path errored; a partial note may exist (the batch path rewrites it, never double-files).
    Failed {
        error: String,
        note_path: Option<PathBuf>,
    },
}

/// The finish/discard decision the session thread waits for after the tee disconnects.
enum Verdict {
    Finish,
    Discard,
}

/// Owns the (at most one) in-flight live session. Shared between the detector hook (attaches tees,
/// spawns sessions) and the pipeline worker (finalizes or discards them).
pub struct LiveManager {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Tee receiver stashed between `LiveHook::attach` and `LiveHook::started`.
    pending: Option<Pending>,
    active: Option<Active>,
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

    fn stash_pending(&self, rx: Receiver<CaptureChunk>, dropped: Arc<AtomicU64>) {
        self.inner.lock().unwrap().pending = Some(Pending { rx, dropped });
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
            let thread = std::thread::Builder::new().name("corti-live".into()).spawn(
                move || -> LiveOutcome {
                    session_thread(pending.rx, verdict_rx, meta, sample_rate, cfg, pipe_tx)
                },
            );
            match thread {
                Ok(handle) => {
                    let stale = {
                        let mut inner = self.inner.lock().unwrap();
                        inner.active.replace(Active {
                            id,
                            verdict_tx,
                            handle,
                            dropped,
                        })
                    };
                    // Shouldn't happen (one detector recording at a time), but never leak a session.
                    // Finish, never discard: the previous call's note must survive (its path was
                    // persisted at creation, so the batch/rewrite paths still find it). Non-joining —
                    // this runs on the detect worker, which must not block on a decode.
                    if let Some(stale) = stale {
                        warn!(target: "corti::live", job_id = %stale.id, "stale live session at spawn — finishing it detached");
                        let _ = stale.verdict_tx.send(Verdict::Finish);
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

    /// The recording finished: tell the session to finalize (finish transcribers, append tails, flip
    /// the state line) and return how it ended. `None` when no live session exists for this id. This
    /// is the only joining path: the tee sender died when the recorder stopped (before the triggering
    /// `Process` was sent), so the thread is at (or heading to) its verdict wait and the join is
    /// bounded by the final flush/decode. A panicked thread reports as a live-path failure.
    pub fn finalize(&self, id: &str) -> Option<LiveOutcome> {
        let active = self.take_active(id)?;
        let dropped = active.dropped.load(Ordering::Relaxed);
        if dropped > 0 {
            warn!(
                target: "corti::live",
                job_id = %id,
                dropped_chunks = dropped,
                "live tee dropped chunks (consumer fell behind) — the live transcript may have gaps"
            );
        }
        let _ = active.verdict_tx.send(Verdict::Finish);
        Some(
            active
                .handle
                .join()
                .unwrap_or_else(|_| LiveOutcome::Failed {
                    error: "live transcription thread panicked".to_string(),
                    note_path: None,
                }),
        )
    }

    /// The recording was discarded (too short) or its capture failed to finish: tear the session
    /// down. **Non-joining** — the verdict is delivered and the thread deletes its own note on the
    /// way out, so the caller never blocks on a session that may still be mid-decode.
    pub fn discard(&self, id: &str) {
        self.inner.lock().unwrap().pending.take(); // an un-started tee, if an abort raced oddly
        if let Some(active) = self.take_active(id) {
            info!(target: "corti::live", job_id = %id, "discarding live session (detached)");
            let _ = active.verdict_tx.send(Verdict::Discard);
        }
    }

    /// Whether a live session for `id` is still active (not yet finalized or discarded). Guards the
    /// `LiveNoteCreated` handler against creating a queue row for an already-torn-down session.
    pub fn is_active(&self, id: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .active
            .as_ref()
            .is_some_and(|a| a.id == id)
    }

    /// Take the active session out, if `id` matches.
    fn take_active(&self, id: &str) -> Option<Active> {
        let mut inner = self.inner.lock().unwrap();
        if inner.active.as_ref().is_some_and(|a| a.id == id) {
            inner.active.take()
        } else {
            None
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
        self.manager.stash_pending(rx, tee.dropped_counter());
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

    fn aborted(&self) {
        self.manager.take_pending();
    }

    fn failed(&self, meta: &RecordingMeta) {
        // Capture could not finish, so no `RecordingFinished`/`Process` will ever finalize this
        // session — tear it down on the pipeline thread (which also owns any queue row to close).
        let _ = self.pipe_tx.send(PipelineMsg::LiveDiscarded {
            id: corti_queue::job_id(meta),
        });
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
        Ok(Err(e)) => LiveOutcome::Failed {
            error: format!("{e:#}"),
            note_path: writer.path().cloned(),
        },
        Err(_) => LiveOutcome::Failed {
            error: "live transcription panicked".to_string(),
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
        Ok(Verdict::Finish) => {
            let p = parts?;
            consumed?;
            finish_session(
                sample_rate,
                p.aec,
                p.mic,
                p.them,
                p.mic_seg,
                p.them_seg,
                writer,
            )
        }
        Ok(Verdict::Discard) => {
            writer.discard();
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

/// Finalize: AEC tail into the mic transcriber, finish both channels, append the remaining segments
/// merged by start time, then flip the note's state line in place.
fn finish_session<C: LiveChannel, F: NoteFiler>(
    sample_rate: u32,
    mut aec: Option<StreamingAec>,
    mut mic: C,
    mut them: C,
    mut mic_seg: Segmenter,
    mut them_seg: Segmenter,
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
        Some(note_path) => {
            corti_vagus::note::flip_state(&note_path).context("flipping the note's state line")?;
            info!(
                target: "corti::live",
                note_path = %note_path.display(),
                "live note finalized — state flipped to transcribed"
            );
            Ok(LiveOutcome::Filed { note_path })
        }
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

    /// Delete the note (recording discarded). No-op when none was created.
    fn discard(&mut self) {
        if let Some(path) = self.note.take() {
            match std::fs::remove_file(&path) {
                Ok(()) => info!(
                    target: "corti::live",
                    note_path = %path.display(),
                    "deleted live note of a discarded recording"
                ),
                Err(e) => warn!(
                    target: "corti::live",
                    note_path = %path.display(),
                    error = %e,
                    "could not delete the live note of a discarded recording"
                ),
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

        writer.discard();
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

        let outcome =
            finish_session(48_000, aec, mic, them, mic_seg, them_seg, &mut writer).unwrap();
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
        let outcome =
            finish_session(48_000, aec, mic, them, mic_seg, them_seg, &mut writer).unwrap();
        assert!(matches!(outcome, LiveOutcome::NoNote));
        assert!(!note.exists());
    }

    /// `is_active` guards the `LiveNoteCreated` handler: only a matching, not-yet-torn-down session
    /// counts; `discard` (non-joining) clears it and still delivers the Discard verdict to the thread.
    #[test]
    fn is_active_matches_only_live_sessions_and_discard_is_non_joining() {
        use std::time::Duration;

        let m = LiveManager::new();
        assert!(!m.is_active("a"));

        let (verdict_tx, verdict_rx) = std::sync::mpsc::channel::<Verdict>();
        let (probe_tx, probe_rx) = std::sync::mpsc::channel::<&'static str>();
        let handle = std::thread::spawn(move || {
            let got = match verdict_rx.recv() {
                Ok(Verdict::Discard) => "discard",
                Ok(Verdict::Finish) => "finish",
                Err(_) => "none",
            };
            let _ = probe_tx.send(got);
            LiveOutcome::NoNote
        });
        m.inner.lock().unwrap().active = Some(Active {
            id: "a".into(),
            verdict_tx,
            handle,
            dropped: Arc::new(AtomicU64::new(0)),
        });

        assert!(m.is_active("a"));
        assert!(!m.is_active("b"));
        m.discard("b"); // wrong id: session untouched
        assert!(m.is_active("a"));

        m.discard("a"); // returns without joining; the verdict still reaches the thread
        assert!(!m.is_active("a"));
        assert_eq!(
            probe_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            "discard"
        );
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
