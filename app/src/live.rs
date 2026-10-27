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
//! A complete finish flips the state line and reports [`LiveOutcome::Filed`]. Any processing loss still flushes tails
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
use corti_core::{
    DiarizedTranscript, MicTapMismatches as LiveMismatches, RecordingMeta, Speaker,
    TranscriptSegment,
};
use corti_transcribe::decode::{DecodeQualityTracker, DecodeReport, InputSpan};
use corti_transcribe::segment::{
    AudioEvidence, CleanupConfig, CleanupDecisions, CleanupStage, EchoAssessment, EchoCandidate,
    EchoSource, EchoVerdict, SEGMENT_GAP, SpanEvidence, SpeakerTurn, TextRule, Word,
    cleanup_with_protected_decisions, diarize_words, echo_assessment, split_regions,
    words_to_segments,
};
use tracing::{info, warn};

use crate::config::{AppConfig, BackendChoice};
use crate::live_view::LiveTranscriptStore;
use crate::pipeline::PipelineMsg;
use crate::settings::SharedConfig;

/// Bounded tee backlog in chunks (~4096 frames ≈ 85 ms each at 48 kHz, so ≈ 175 s of slack, ≤ ~72 MiB
/// of two-track PCM plus worst-case availability bitmaps, plus < 1 MiB of chunk/epoch metadata;
/// chunks without loss allocate no bitmap).
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

/// Availability follows the canceller's delayed output, independently of rolling text windows.
/// This changes no AEC input, update, or output order: only unavailable output positions are zeroed.
#[derive(Default)]
struct LiveAvailability {
    pending: VecDeque<bool>,
}

impl LiveAvailability {
    fn push(
        &mut self,
        aec: &mut StreamingAec,
        mic: &[f32],
        far: &[f32],
        unavailable: &[bool],
    ) -> Result<Vec<f32>> {
        anyhow::ensure!(
            mic.len() == far.len()
                && mic.len() <= corti_capture::FILTER_FRAMES_PER_CHUNK
                && (unavailable.is_empty() || unavailable.len() == mic.len()),
            "invalid live availability block"
        );
        let lag = aec.max_output_lag_samples();
        anyhow::ensure!(
            lag <= 192_000 * 35
                && self.pending.len().saturating_add(mic.len())
                    <= lag.saturating_add(corti_capture::FILTER_FRAMES_PER_CHUNK),
            "live availability exceeded filter lag bound"
        );
        self.pending
            .extend((0..mic.len()).map(|index| unavailable.get(index).copied().unwrap_or(false)));
        let mut clean = aec.push(mic, far);
        self.mask(&mut clean)?;
        Ok(clean)
    }

    fn mask(&mut self, audio: &mut [f32]) -> Result<()> {
        anyhow::ensure!(
            audio.len() <= self.pending.len(),
            "live filter emitted unpositioned audio"
        );
        for sample in audio {
            if self.pending.pop_front().unwrap_or(true) {
                *sample = 0.0;
            }
        }
        Ok(())
    }

    fn finish(mut self, aec: StreamingAec) -> Result<corti_aec::FinishOutput> {
        let mut finish = aec.finish_with_stats();
        self.mask(&mut finish.audio)?;
        anyhow::ensure!(
            self.pending.is_empty(),
            "live filter withheld positioned audio"
        );
        Ok(finish)
    }
}

fn observe_mismatch(mismatches: &mut LiveMismatches, chunk: &CaptureChunk, sample_rate: u32) {
    if mismatches.observe(
        chunk.epoch,
        chunk.retained_start_frame,
        chunk.mic.len(),
        chunk.tap.len(),
        sample_rate,
    ) {
        warn!(
            target: "corti::live",
            mismatched_chunks = mismatches.count(),
            "live mic/tap length mismatch; padding the shorter track for batch recovery"
        );
    }
}

/// Reused, fixed-size padding storage; input vectors and their available prefixes stay untouched.
fn padded_slice<'a>(
    input: &'a [f32],
    offset: usize,
    take: usize,
    scratch: &'a mut [f32],
) -> &'a [f32] {
    if offset + take <= input.len() {
        &input[offset..offset + take]
    } else {
        let output = &mut scratch[..take];
        output.fill(0.0);
        if let Some(remaining) = input.get(offset..) {
            output[..remaining.len()].copy_from_slice(remaining);
        }
        output
    }
}

/// How a live session ended, collected by the pipeline worker at `Process` time.
#[derive(Debug)]
pub enum LiveOutcome {
    /// The note is fully written and its state line flipped — the job can go straight to `Done`.
    Filed {
        note_path: PathBuf,
        settled_quality: Option<corti_core::QualityRecord>,
    },
    /// The session ran but never produced a segment, so no note was created — run the batch path.
    NoNote {
        settled_quality: Option<corti_core::QualityRecord>,
    },
    /// The live result is not canonical (decode failure, panic, or a lossy tee). Batch transcription must
    /// rewrite `note_path` in place when one exists, never create a second note.
    Fallback {
        reason: String,
        note_path: Option<PathBuf>,
        /// Settled live-stage losses travel separately from immutable capture provenance.
        settled_quality: Option<corti_core::QualityRecord>,
    },
}

impl LiveOutcome {
    /// Any note the batch/discard path must take ownership of.
    fn note_path(&self) -> Option<&PathBuf> {
        match self {
            Self::Filed { note_path, .. } => Some(note_path),
            Self::Fallback { note_path, .. } => note_path.as_ref(),
            Self::NoNote { .. } => None,
        }
    }
}

/// Capture-quality facts frozen when the recorder closes its tee. The detector delivers this verdict
/// immediately, so a later pipeline backlog cannot make the live thread guess whether its transcript was
/// lossless.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FinishQuality {
    dropped_chunks: u64,
    capture: Option<corti_core::QualityRecord>,
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
        self.finish_with_quality(id, None);
    }

    pub fn finish_with_quality(&self, id: &str, capture: Option<&corti_core::QualityRecord>) {
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
        let _ = active.verdict_tx.send(Verdict::Finish(FinishQuality {
            dropped_chunks,
            capture: capture.cloned(),
        }));
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
        settled_quality: None,
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

    fn finished_with_processing(
        &self,
        meta: &RecordingMeta,
        processing: &corti_capture::CaptureProcessing,
    ) {
        self.manager
            .finish_with_quality(&corti_queue::job_id(meta), processing.quality.as_ref());
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
    if let Some(hosted) = publisher.hosted.as_ref()
        && let Err(error) = hosted.begin_live_session(&publisher.id)
    {
        tracing::warn!(
            target: "corti::hosted",
            ?error,
            "hosted live session could not be opened; this recording stays raw"
        );
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
        Ok(Err(e)) => writer.fallback_outcome(format!("{e:#}")),
        Err(_) => writer.fallback_outcome("live transcription panicked".to_string()),
    };
    match &outcome {
        LiveOutcome::Filed { .. } => publisher.complete("Call complete — transcript filed."),
        LiveOutcome::NoNote { .. } => publisher.complete("Call complete — no note was filed."),
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
    .context("reserving the bounded live transcript window")?
    .with_lexicon(crate::lexicon::load_compiled().map(std::sync::Arc::new));
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
            consume_with_panic_boundary(|| {
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
            })
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

    let Some(quality) = await_capture_verdict(verdict_rx, consumed, writer)? else {
        return Ok(LiveOutcome::NoNote {
            settled_quality: None,
        });
    };
    let p = parts?;
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

fn consume_with_panic_boundary(work: impl FnOnce() -> Result<()>) -> Result<()> {
    // A failed decoder is never resumed. Keep its positioned loss facts alive until the recorder
    // supplies Finish/Discard, including the Start epoch when its tee chunks were not received.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
        .unwrap_or_else(|_| Err(anyhow::anyhow!("live chunk consumption panicked")))
}

fn await_capture_verdict<F: NoteFiler>(
    verdict_rx: &Receiver<Verdict>,
    consumed: Result<()>,
    writer: &mut NoteWriter<F>,
) -> Result<Option<FinishQuality>> {
    match verdict_rx.recv() {
        Ok(Verdict::Finish(quality)) => {
            writer.freeze_live_quality(&quality)?;
            consumed?;
            Ok(Some(quality))
        }
        Ok(Verdict::Discard) => {
            writer.discard()?;
            Ok(None)
        }
        // Manager gone (app shutting down mid-call): leave whatever was written; don't flip.
        Err(_) => anyhow::bail!("live session received no finish/discard verdict"),
    }
}

struct RawEchoSource {
    segment: TranscriptSegment,
    /// Raw pause-separated children supply both text comparisons and actual speech intervals.
    /// The whole-result envelope also contains known pauses and cannot authorize acoustic overlap.
    speech_segments: Vec<TranscriptSegment>,
}

/// Original far-end decoder polls, each possibly containing several VAD regions, bounded independently
/// of a filing window. The two live stages
/// use the same retention rule. Missing sources cannot authorize deletion, including time-pruned
/// context that may still overlap an earlier-starting mic poll not yet returned by its decoder.
struct RawEchoSources {
    segments: Vec<RawEchoSource>,
    bytes: usize,
    omitted: usize,
    incomplete_until: f64,
}

impl Default for RawEchoSources {
    fn default() -> Self {
        Self {
            segments: Vec::new(),
            bytes: 0,
            omitted: 0,
            incomplete_until: f64::NEG_INFINITY,
        }
    }
}

impl RawEchoSources {
    fn remember(&mut self, words: &[Word], lookback: f64) {
        let omitted_before = self.omitted;
        // A pause is a display boundary, not permission to split a raw critical-word comparison.
        // Keep the complete decoder result even when it spans several pause-separated display rows.
        for segment in words_to_segments(words, Speaker::Other("Them".to_string()), f64::INFINITY) {
            let source = RawEchoSource {
                segment,
                speech_segments: words_to_segments(
                    words,
                    Speaker::Other("Them".to_string()),
                    SEGMENT_GAP,
                ),
            };
            let bytes = raw_source_bytes(&source);
            if bytes > MAX_BUFFERED_TRANSCRIPT_BYTES {
                self.omit(&source, lookback);
                continue;
            }
            while self.segments.len() >= MAX_LIVE_ECHO_SOURCES
                || self.bytes.saturating_add(bytes) > MAX_BUFFERED_TRANSCRIPT_BYTES
            {
                let removed = self.segments.remove(0);
                self.bytes -= raw_source_bytes(&removed);
                self.omit(&removed, lookback);
            }
            self.bytes += bytes;
            self.segments.push(source);
        }
        if self.omitted != omitted_before {
            warn!(
                target: "corti::live",
                omitted_sources = self.omitted - omitted_before,
                total_omitted_sources = self.omitted,
                "raw echo source cap reached; affected mic spans will be kept"
            );
        }
    }

    fn omit(&mut self, source: &RawEchoSource, lookback: f64) {
        self.omitted = self.omitted.saturating_add(1);
        let until = source.segment.end + lookback;
        self.incomplete_until = self.incomplete_until.max(if until.is_finite() {
            until
        } else {
            f64::INFINITY
        });
    }

    fn prune_before(&mut self, horizon: f64, lookback: f64) {
        let mut incomplete_until = self.incomplete_until;
        self.segments.retain(|source| {
            if source.segment.end >= horizon {
                return true;
            }
            // Decoder polls can arrive out of source-time order across tracks. Forgetting text is
            // bounded, but cannot prove that no still-undecoded mic poll needs its raw comparison.
            let until = source.segment.end + lookback;
            incomplete_until = incomplete_until.max(if until.is_finite() {
                until
            } else {
                f64::INFINITY
            });
            false
        });
        // Normal expiry is not a count/byte-cap omission and must not inflate that diagnostic.
        self.incomplete_until = incomplete_until;
        self.bytes = self.segments.iter().map(raw_source_bytes).sum();
    }

    /// Missing raw sources can hide a critical difference. Always query the original mic interval,
    /// never a source interval, and never authorize deletion while that inventory is incomplete.
    fn span_evidence(
        &self,
        blocks: &[corti_aec::BlockStats],
        start: f64,
        end: f64,
    ) -> Option<SpanEvidence> {
        if start <= self.incomplete_until {
            None
        } else {
            crate::transcribe::span_evidence(blocks, start, end)
        }
    }

    /// Whole raw text can protect across a pause; only an actual speech interval can supply acoustic
    /// overlap. The shared verdict remains authoritative, with deletion attributed to that interval.
    fn assess(
        &self,
        subject: &EchoCandidate,
        cfg: &CleanupConfig,
        evidence: Option<SpanEvidence>,
    ) -> (EchoAssessment, Option<SpanEvidence>) {
        let overlap = self
            .segments
            .iter()
            .flat_map(|source| &source.speech_segments)
            .find(|speech| subject.start() < speech.end && speech.start < subject.end());
        let evidence =
            evidence.filter(|_| subject.start() > self.incomplete_until && overlap.is_some());
        let sources: Vec<_> = self
            .segments
            .iter()
            .flat_map(|source| std::iter::once(&source.segment).chain(&source.speech_segments))
            .map(EchoCandidate::from_segment)
            .collect();
        let mut assessment = echo_assessment(subject, &sources, cfg, evidence);
        if assessment.verdict == EchoVerdict::DropAcoustic {
            assessment.source = overlap.map(|speech| EchoSource {
                me: false,
                start: speech.start,
                end: speech.end,
            });
        }
        (assessment, evidence)
    }

    fn assess_mic_poll(
        &self,
        subject: &EchoCandidate,
        poll: &MicPollContext,
        cfg: &CleanupConfig,
        evidence: Option<SpanEvidence>,
    ) -> (EchoAssessment, Option<SpanEvidence>) {
        let MicPollContext::Known(raw) = poll else {
            return (EchoAssessment::default(), None);
        };
        // A child can outlive the missing source's horizon while its original poll still intersects it.
        // Comparison incompleteness therefore gates acoustics at poll scope, not just child start time.
        let evidence = evidence.filter(|_| !self.mic_comparison_incomplete(subject, poll));
        let (whole, _) = self.assess(&EchoCandidate::from_segment(&raw.segment), cfg, None);
        if whole.verdict == EchoVerdict::KeepCriticalDifference {
            (whole, evidence)
        } else {
            self.assess(subject, cfg, evidence)
        }
    }

    fn mic_comparison_incomplete(&self, subject: &EchoCandidate, poll: &MicPollContext) -> bool {
        match poll {
            MicPollContext::Omitted => true,
            MicPollContext::Known(raw) => {
                raw.segment.start <= self.incomplete_until
                    || subject.start() <= self.incomplete_until
            }
        }
    }
}

/// Original returned mic poll, shared by all of its pause-separated children. Omission is explicit:
/// missing comparison context cannot authorize acoustic or automatic text deletion.
#[derive(Clone)]
enum MicPollContext {
    Known(Arc<RawEchoSource>),
    Omitted,
}

impl MicPollContext {
    fn new(words: &[Word], available: usize) -> (Self, usize) {
        let text_bytes = words
            .iter()
            .fold(words.len().saturating_sub(1), |sum, word| {
                sum.saturating_add(word.text.len())
            });
        let minimum_bytes = std::mem::size_of::<RawEchoSource>()
            .saturating_add(2 * std::mem::size_of::<usize>())
            .saturating_add(std::mem::size_of::<Arc<RawEchoSource>>())
            .saturating_add(text_bytes);
        if words.is_empty() || minimum_bytes > available {
            return (Self::Omitted, 0);
        }
        let segment = TranscriptSegment {
            speaker: Speaker::Me,
            start: words[0].start,
            end: words[words.len() - 1].end,
            text: words
                .iter()
                .map(|word| word.text.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        };
        let raw = RawEchoSource {
            segment,
            speech_segments: words_to_segments(words, Speaker::Me, SEGMENT_GAP),
        };
        let bytes = raw_source_bytes(&raw)
            .saturating_add(2 * std::mem::size_of::<usize>())
            .saturating_add(std::mem::size_of::<Arc<RawEchoSource>>());
        if bytes > available {
            return (Self::Omitted, 0);
        }
        (Self::Known(Arc::new(raw)), bytes)
    }

    fn omitted(&self) -> bool {
        matches!(self, Self::Omitted)
    }
}

/// Per-recording rolling state. Every collection is bounded by either the configured time window or an
/// independent byte cap; after a successful durable append, lengths reset to zero and allocations are reused.
struct TranscriptWindow {
    sample_rate: u32,
    frame_limit: u64,
    start_frame: u64,
    frames: u64,
    /// Released decoder regions keep their original grouping and any raw critical-word protection.
    mic_regions: Vec<ReleasedMicRegion>,
    /// Original mic polls, charged once to a separate 1 MiB logical payload budget. This inventory also
    /// retains polls whose acoustic children were withheld, so far-side protection stays symmetric.
    mic_polls: Vec<Arc<RawEchoSource>>,
    mic_poll_bytes: usize,
    mic_polls_omitted: usize,
    them_words: Vec<Word>,
    /// End indices in them_words, preserving original returned polls before any pause/diarization split.
    them_poll_ends: Vec<usize>,
    tap_audio: Vec<f32>,
    buffered_text_bytes: usize,
    diarize_far_end: bool,
    /// Deterministic segment cleanup applied to each window before its one append (#149).
    cleanup: CleanupConfig,
    /// The learned correction lexicon, applied as the last cleanup pass on every window. Loaded once
    /// per session, so a rule added mid-call lands on the next session (`corti --review` says so).
    lexicon: Option<std::sync::Arc<corti_lexicon::CompiledLexicon>>,
    /// Raw far-end decoder polls, before diarization or text cleanup. Includes the current window and
    /// every earlier source still inside the lookback, even across several short windows. Never rendered
    /// from this inventory. Capped at 256 sources and 1 MiB; omitted sources disable acoustic deletion
    /// over their remaining lookback and are counted without retaining transcript content.
    carry: RawEchoSources,
    /// Live early drop (#149 phase 2). Per-recording state like `carry`, not per-window: a mic region
    /// withheld just before a boundary is still judged against far-end regions that close after it.
    early_drop: EarlyDrop,
    /// Bounded in-memory decisions, including original regions withheld by early echo cleanup. The
    /// evidence writer added later consumes these; no content from this buffer enters tracing.
    cleanup_decisions: CleanupDecisions,
    early_decision_index: usize,
    /// The live canceller's per-block echo record covering this window plus `echo_window_seconds` of
    /// lookback (#149 phase 3b), drained from `StreamingAec` after every push and trimmed at every flush.
    /// `t_start_secs` is on the cleaned timeline — the same timeline these segments are timestamped on —
    /// so a block time and a segment time compare directly. Empty when AEC is off, which simply leaves the
    /// echo pass unable to delete a suspected duplicate.
    aec_blocks: Vec<corti_aec::BlockStats>,
    aec_availability: LiveAvailability,
    /// A late first track starts its decoder clock at zero. Shift its words once at ingress rather
    /// than replaying an arbitrarily long silent prefix. These offsets do not repair tee-drop gaps.
    mic_origin_frame: Option<u64>,
    tap_origin_frame: Option<u64>,
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
            mic_regions: Vec::new(),
            mic_polls: Vec::new(),
            mic_poll_bytes: 0,
            mic_polls_omitted: 0,
            them_words: Vec::new(),
            them_poll_ends: Vec::new(),
            tap_audio,
            buffered_text_bytes: 0,
            diarize_far_end,
            early_drop: EarlyDrop::new(cleanup.clone()),
            cleanup_decisions: CleanupDecisions::default(),
            early_decision_index: 0,
            cleanup,
            lexicon: None,
            carry: RawEchoSources::default(),
            aec_blocks: Vec::new(),
            aec_availability: LiveAvailability::default(),
            mic_origin_frame: None,
            tap_origin_frame: None,
        })
    }

    /// Attach the learned lexicon (or none) to every window flush.
    #[cfg_attr(not(feature = "local"), allow(dead_code))]
    fn with_lexicon(
        mut self,
        lexicon: Option<std::sync::Arc<corti_lexicon::CompiledLexicon>>,
    ) -> Self {
        self.lexicon = lexicon;
        self
    }

    /// Seconds from call start at which the *next* window begins.
    fn next_start_sec(&self) -> f64 {
        self.start_frame.saturating_add(self.frames) as f64 / f64::from(self.sample_rate)
    }

    /// Take the blocks a `StreamingAec::block_stats()` drain produced. Ordered by `t_start_secs` because
    /// the drain is, and the two clocks (window frames, canceller blocks) advance together.
    fn push_aec_blocks(&mut self, mut blocks: Vec<corti_aec::BlockStats>) {
        let origin = self.mic_origin_frame.unwrap_or(0) as f64 / f64::from(self.sample_rate);
        for block in &mut blocks {
            block.t_start_secs += origin;
        }
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

    #[cfg(test)]
    fn push_mic_words(&mut self, words: Vec<Word>) {
        self.push_mic_region(ReleasedMicRegion::kept(words));
    }

    fn push_mic_region(&mut self, region: ReleasedMicRegion) {
        self.buffered_text_bytes = self
            .buffered_text_bytes
            .saturating_add(buffered_word_bytes(&region.words))
            .saturating_add(std::mem::size_of::<ReleasedMicRegion>());
        self.mic_regions.push(region);
    }

    fn remember_mic_poll(&mut self, words: &[Word]) -> MicPollContext {
        let (context, bytes) = MicPollContext::new(
            words,
            MAX_BUFFERED_TRANSCRIPT_BYTES.saturating_sub(self.mic_poll_bytes),
        );
        if let MicPollContext::Known(raw) = &context {
            self.mic_poll_bytes += bytes;
            self.buffered_text_bytes = self.buffered_text_bytes.saturating_add(bytes);
            self.mic_polls.push(Arc::clone(raw));
        } else {
            self.mic_polls_omitted = self.mic_polls_omitted.saturating_add(1);
            if self.mic_polls_omitted == 1 {
                warn!(target: "corti::live", "raw mic poll context cap reached; affected rows will be kept");
            }
        }
        context
    }

    fn push_them_words(&mut self, words: Vec<Word>) {
        if words.is_empty() {
            return;
        }
        self.carry
            .remember(&words, self.cleanup.echo_window_seconds);
        self.buffered_text_bytes = self
            .buffered_text_bytes
            .saturating_add(buffered_word_bytes(&words))
            .saturating_add(std::mem::size_of::<usize>());
        self.them_words.extend(words);
        self.them_poll_ends.push(self.them_words.len());
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
        self.mic_regions.clear();
        self.mic_polls.clear();
        self.mic_poll_bytes = 0;
        self.mic_polls_omitted = 0;
        self.them_words.clear();
        self.them_poll_ends.clear();
        self.tap_audio.clear();
        self.buffered_text_bytes = 0;
        self.trim_aec_blocks();
        let horizon = self.start_sec() - self.cleanup.echo_window_seconds;
        self.carry
            .prune_before(horizon, self.cleanup.echo_window_seconds);
    }

    #[cfg(test)]
    fn retained_bytes(&self) -> usize {
        self.tap_audio.capacity() * std::mem::size_of::<f32>() + self.buffered_text_bytes
    }
}

fn raw_source_bytes(source: &RawEchoSource) -> usize {
    std::mem::size_of::<RawEchoSource>()
        .saturating_add(source.segment.text.len())
        .saturating_add(source.segment.speaker.display().len())
        .saturating_add(
            source
                .speech_segments
                .iter()
                .fold(0usize, |bytes, segment| {
                    bytes
                        .saturating_add(std::mem::size_of::<TranscriptSegment>())
                        .saturating_add(segment.text.len())
                        .saturating_add(segment.speaker.display().len())
                }),
        )
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
    poll: MicPollContext,
    /// Call time (seconds) at which the hold expires even if the far end never closes a region.
    deadline: f64,
}

/// Every released region reaches the caller, including an acoustic echo withheld from publication.
/// Words and their original timing are retained separately from the content-free verdict.
struct ReleasedMicRegion {
    words: Vec<Word>,
    poll: MicPollContext,
    /// Missing comparison context is sticky even if another stage later has a smaller inventory.
    comparison_incomplete: bool,
    assessment: EchoAssessment,
    evidence: Option<SpanEvidence>,
}

impl ReleasedMicRegion {
    #[cfg(test)]
    fn kept(words: Vec<Word>) -> Self {
        let (poll, _) = MicPollContext::new(&words, MAX_BUFFERED_TRANSCRIPT_BYTES);
        Self::kept_with_poll(words, poll)
    }

    fn kept_with_poll(words: Vec<Word>, poll: MicPollContext) -> Self {
        Self {
            words,
            comparison_incomplete: poll.omitted(),
            poll,
            assessment: EchoAssessment::default(),
            evidence: None,
        }
    }
}

/// What the live early drop did over one session. Logged once at the end, next to the per-window
/// cleanup counts, so a reader who noticed a missing row can tell a drop from a decode failure.
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
/// At release the shared acoustic verdict decides: published with original words and timestamps, or
/// withheld from publication and returned with its decision. Text alone never deletes a region. Only
/// Them→Me is judged here; the far-end channel is never delayed.
///
/// Publication is strictly FIFO. A long region arriving while something is held releases what is held
/// first, so a late row can never overtake an earlier mic row.
struct EarlyDrop {
    cfg: CleanupConfig,
    /// Recently closed far-end regions, oldest first, as read-only echo sources.
    sources: RawEchoSources,
    /// Withheld mic regions, oldest first.
    held: VecDeque<HeldMicRegion>,
    stats: EarlyDropStats,
}

impl EarlyDrop {
    fn new(cfg: CleanupConfig) -> Self {
        Self {
            cfg,
            sources: RawEchoSources::default(),
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

    /// A mic poll produced words. Returns all released word groups and decisions, in order.
    #[cfg(test)]
    fn offer_mic(
        &mut self,
        words: Vec<Word>,
        now: f64,
        evidence: Option<AudioEvidence<'_>>,
    ) -> Vec<ReleasedMicRegion> {
        let (poll, _) = MicPollContext::new(&words, MAX_BUFFERED_TRANSCRIPT_BYTES);
        self.offer_mic_with_poll(words, now, evidence, poll)
    }

    fn offer_mic_with_poll(
        &mut self,
        words: Vec<Word>,
        now: f64,
        evidence: Option<AudioEvidence<'_>>,
        poll: MicPollContext,
    ) -> Vec<ReleasedMicRegion> {
        if !self.enabled() || words.is_empty() {
            return vec![ReleasedMicRegion::kept_with_poll(words, poll)];
        }
        let regions = split_regions(&words, SEGMENT_GAP);
        if regions.is_empty() {
            return vec![ReleasedMicRegion::kept_with_poll(words, poll)];
        }
        let mut out = Vec::with_capacity(regions.len());
        for region in regions {
            let candidate = EchoCandidate::from_words(true, &region);
            if !self.is_short(&candidate) {
                // A sentence publishes now — but never ahead of a region that arrived before it.
                out.append(&mut self.drain_held(evidence));
                out.push(ReleasedMicRegion::kept_with_poll(region, poll.clone()));
                continue;
            }
            let deadline = candidate.end() + self.cfg.echo_window_seconds;
            self.held.push_back(HeldMicRegion {
                words: region,
                candidate,
                poll: poll.clone(),
                deadline,
            });
            self.stats.held += 1;
            // Everything popped here is older than what was just pushed, so order still holds.
            while self.held.len() > MAX_HELD_MIC_REGIONS {
                self.release_oldest(&mut out, evidence);
            }
        }
        self.prune_sources(now);
        out
    }

    /// A far-end poll produced words: they become echo sources, and a closed far-end region is exactly the
    /// event a held region was waiting for. Returns the mic word groups this released.
    fn offer_them(
        &mut self,
        words: &[Word],
        now: f64,
        evidence: Option<AudioEvidence<'_>>,
    ) -> Vec<ReleasedMicRegion> {
        if !self.enabled() || words.is_empty() {
            return Vec::new();
        }
        self.sources.remember(words, self.cfg.echo_window_seconds);
        let released = self.drain_held(evidence);
        self.prune_sources(now);
        released
    }

    /// Call time advanced. Releases every hold whose deadline has passed; deadlines are non-decreasing, so
    /// the front of the queue is always the next to expire.
    fn due(&mut self, now: f64, evidence: Option<AudioEvidence<'_>>) -> Vec<ReleasedMicRegion> {
        if !self.enabled() || self.held.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        while self.held.front().is_some_and(|h| h.deadline <= now) {
            self.release_oldest(&mut out, evidence);
        }
        self.prune_sources(now);
        out
    }

    /// Release everything, whatever the deadline: a durable append is about to render the note, or the
    /// session is over. Nothing is lost, only judged with the sources that exist by then.
    fn release_all(&mut self, evidence: Option<AudioEvidence<'_>>) -> Vec<ReleasedMicRegion> {
        if !self.enabled() {
            return Vec::new();
        }
        self.drain_held(evidence)
    }

    fn drain_held(&mut self, evidence: Option<AudioEvidence<'_>>) -> Vec<ReleasedMicRegion> {
        let mut out = Vec::with_capacity(self.held.len());
        while !self.held.is_empty() {
            self.release_oldest(&mut out, evidence);
        }
        out
    }

    /// Judge the oldest held region against the far-end ring and return it even when publication is
    /// suppressed. The caller can preserve every original word alongside its decision.
    fn release_oldest(
        &mut self,
        out: &mut Vec<ReleasedMicRegion>,
        evidence: Option<AudioEvidence<'_>>,
    ) {
        let Some(held) = self.held.pop_front() else {
            return;
        };
        let span = evidence.and_then(|get| get(held.candidate.start(), held.candidate.end()));
        let comparison_incomplete = self
            .sources
            .mic_comparison_incomplete(&held.candidate, &held.poll);
        let (assessment, span) =
            self.sources
                .assess_mic_poll(&held.candidate, &held.poll, &self.cfg, span);
        if assessment.verdict == EchoVerdict::DropAcoustic {
            self.stats.released_dropped += 1;
        } else {
            self.stats.released_published += 1;
        }
        out.push(ReleasedMicRegion {
            words: held.words,
            poll: held.poll,
            comparison_incomplete,
            assessment,
            evidence: span,
        });
    }

    /// Retain text relative to the oldest decoded hold, or the current clock when there is none.
    /// Pruning also records its missing-context horizon: the other decoder may later return a poll
    /// starting before this floor, and that poll must not be deleted using an incomplete comparison.
    fn prune_sources(&mut self, now: f64) {
        let floor = self.held.front().map_or(now, |h| h.candidate.start());
        let horizon = floor - self.cfg.echo_window_seconds;
        self.sources
            .prune_before(horizon, self.cfg.echo_window_seconds);
    }
}

/// Publish one decoded mic region, or withhold it until the far end has had its chance to prove it was an
/// echo (#149 phase 2). Publication and the note's buffer move together, so the reader and the note never
/// disagree about which regions existed.
fn publish_mic_words<P: TranscriptPublisher>(
    window: &mut TranscriptWindow,
    publisher: &P,
    mut words: Vec<Word>,
) {
    if words.is_empty() {
        return;
    }
    let origin = window.mic_origin_frame.unwrap_or(0) as f64 / f64::from(window.sample_rate);
    for word in &mut words {
        word.start += origin;
        word.end += origin;
    }
    let poll = window.remember_mic_poll(&words);
    let now = window.next_start_sec();
    let sources = &window.carry;
    let evidence = |start, end| sources.span_evidence(&window.aec_blocks, start, end);
    let released = window
        .early_drop
        .offer_mic_with_poll(words, now, Some(&evidence), poll);
    emit_mic_words(window, publisher, released);
}

/// Publish one decoded far-end region. The far end is never delayed; its closing is what releases held mic
/// regions, and it becomes an echo source for the ones still to come.
fn publish_them_words<P: TranscriptPublisher>(
    window: &mut TranscriptWindow,
    publisher: &P,
    mut words: Vec<Word>,
) {
    let origin = window.tap_origin_frame.unwrap_or(0) as f64 / f64::from(window.sample_rate);
    for word in &mut words {
        word.start += origin;
        word.end += origin;
    }
    let now = window.next_start_sec();
    let offset = window.them_words.len();
    window.push_them_words(words);
    let words = &window.them_words[offset..];
    let sources = &window.carry;
    let evidence = |start, end| sources.span_evidence(&window.aec_blocks, start, end);
    let released = window.early_drop.offer_them(words, now, Some(&evidence));
    publisher.words(Speaker::Other("Them".to_string()), words);
    emit_mic_words(window, publisher, released);
}

/// Call time advanced: a hold that no far-end region closed out still expires on its own.
fn release_due_mic_words<P: TranscriptPublisher>(window: &mut TranscriptWindow, publisher: &P) {
    let now = window.next_start_sec();
    let sources = &window.carry;
    let evidence = |start, end| sources.span_evidence(&window.aec_blocks, start, end);
    let released = window.early_drop.due(now, Some(&evidence));
    emit_mic_words(window, publisher, released);
}

/// Release everything still held. Required before every [`flush_window`]: a held region is not in
/// `window.mic_regions` yet, so leaving one behind would drop it from the note.
fn release_held_mic_words<P: TranscriptPublisher>(window: &mut TranscriptWindow, publisher: &P) {
    let sources = &window.carry;
    let evidence = |start, end| sources.span_evidence(&window.aec_blocks, start, end);
    let released = window.early_drop.release_all(Some(&evidence));
    emit_mic_words(window, publisher, released);
}

fn emit_mic_words<P: TranscriptPublisher>(
    window: &mut TranscriptWindow,
    publisher: &P,
    released: Vec<ReleasedMicRegion>,
) {
    for region in released {
        for original in words_to_segments(&region.words, Speaker::Me, SEGMENT_GAP) {
            window.cleanup_decisions.record_echo_assessment(
                CleanupStage::EarlyEcho,
                window.early_decision_index,
                &original,
                region.assessment,
                region.evidence,
            );
            window.early_decision_index = window.early_decision_index.saturating_add(1);
        }
        if region.assessment.verdict != EchoVerdict::DropAcoustic {
            publisher.words(Speaker::Me, &region.words);
            window.push_mic_region(region);
        }
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
    let mut mic_padding = [0.0; corti_capture::FILTER_FRAMES_PER_CHUNK];
    let mut tap_padding = [0.0; corti_capture::FILTER_FRAMES_PER_CHUNK];
    let mut mic_unavailable = [false; corti_capture::FILTER_FRAMES_PER_CHUNK];
    while let Ok(chunk) = rx.recv() {
        let chunk_frames = chunk.mic.len().max(chunk.tap.len());
        observe_mismatch(&mut writer.mismatches, &chunk, sample_rate);
        anyhow::ensure!(
            chunk.unavailable.is_empty() || chunk.unavailable.len() == chunk_frames,
            "invalid capture availability length"
        );
        let now = window.start_frame.saturating_add(window.frames);
        if chunk_frames != 0 && (chunk.epoch.mic_channels != 0 || !chunk.mic.is_empty()) {
            window.mic_origin_frame.get_or_insert(now);
        }
        if chunk_frames != 0 && (chunk.epoch.tap_channels != 0 || !chunk.tap.is_empty()) {
            window.tap_origin_frame.get_or_insert(now);
        }
        // Once a track starts, legitimate absent-track epochs advance that same decoder/filter
        // clock with bounded silence. A call that never has that track still never feeds its ASR.
        let has_mic = window.mic_origin_frame.is_some();
        let has_tap = window.tap_origin_frame.is_some();
        let mut offset = 0usize;
        while offset < chunk_frames {
            let take = window
                .remaining_frames()
                .min(chunk_frames - offset)
                .min(corti_capture::FILTER_FRAMES_PER_CHUNK);
            let end = offset + take;
            for (index, unavailable) in mic_unavailable[..take].iter_mut().enumerate() {
                // A missing reference does not mean the observed mic sample is unavailable.
                *unavailable = offset + index >= chunk.mic.len()
                    || chunk
                        .unavailable
                        .get(offset + index)
                        .copied()
                        .unwrap_or(false);
            }
            let unavailable = &mic_unavailable[..take];
            let mic_slice = if has_mic {
                padded_slice(&chunk.mic, offset, take, &mut mic_padding)
            } else {
                &[]
            };
            let tap_slice = padded_slice(&chunk.tap, offset, take, &mut tap_padding);

            // Bind decoder input before AEC. Its ordered one-for-one output retains this coordinate
            // through lookahead and finish; tee holes and late channel starts never compress it.
            let retained = chunk
                .retained_start_frame
                .checked_add(offset as u64)
                .context("live received input position overflow")?;
            if has_mic {
                writer.decode_quality.observe_input(
                    corti_core::QualityTrack::Mic,
                    chunk.epoch,
                    retained,
                    take as u64,
                );
            }
            if has_tap {
                writer.decode_quality.observe_input(
                    corti_core::QualityTrack::Tap,
                    chunk.epoch,
                    retained,
                    take as u64,
                );
            }

            let clean = match aec.as_mut() {
                Some(aec) if !mic_slice.is_empty() && mic_slice.len() == tap_slice.len() => {
                    let cleaned =
                        window
                            .aec_availability
                            .push(aec, mic_slice, tap_slice, unavailable)?;
                    // --- #149 phase 3b: hand the window this push's per-block echo record. ---
                    window.push_aec_blocks(aec.block_stats());
                    // --- end phase 3b ---
                    cleaned
                }
                Some(_) => Vec::new(),
                None => mic_slice
                    .iter()
                    .enumerate()
                    .map(|(index, &sample)| {
                        if unavailable.get(index).copied().unwrap_or(false) {
                            0.0
                        } else {
                            sample
                        }
                    })
                    .collect(),
            };
            if !clean.is_empty() {
                writer.delivered_input(corti_core::QualityTrack::Mic, clean.len())?;
                writer.call_channel(corti_core::QualityTrack::Mic, mic, |channel| {
                    channel.push(&clean, sample_rate)
                });
            }
            if has_tap {
                writer.delivered_input(corti_core::QualityTrack::Tap, tap_slice.len())?;
                writer.call_channel(corti_core::QualityTrack::Tap, them, |channel| {
                    channel.push(tap_slice, sample_rate)
                });
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
                        writer.mark_incomplete();
                        window.clear_after_flush();
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
    let mic_words =
        writer.call_channel(corti_core::QualityTrack::Mic, mic, LiveChannel::checkpoint);
    publish_mic_words(window, publisher, mic_words);
    let them_words =
        writer.call_channel(corti_core::QualityTrack::Tap, them, LiveChannel::checkpoint);
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
    // Echo decisions use decoder regions and raw source identities before any display transform. A
    // protected early region keeps that exact assessment even if its source later leaves the bounded
    // inventory. The remaining regions use the same shared verdict as batch and early cleanup.
    let mut segments = Vec::new();
    let mut protected_input_indices = Vec::new();
    let mic_candidates: Vec<_> = window
        .mic_polls
        .iter()
        .flat_map(|raw| std::iter::once(&raw.segment).chain(&raw.speech_segments))
        .map(EchoCandidate::from_segment)
        .collect();
    let mut echo_dropped_audio = 0usize;
    let mut input_index = 0usize;
    for region in &window.mic_regions {
        for original in words_to_segments(&region.words, Speaker::Me, SEGMENT_GAP) {
            let candidate = EchoCandidate::from_segment(&original);
            let (assessment, evidence) = if region.assessment.verdict
                == EchoVerdict::KeepCriticalDifference
            {
                (region.assessment, region.evidence)
            } else {
                let evidence = window
                    .carry
                    .span_evidence(&window.aec_blocks, original.start, original.end)
                    .filter(|_| !region.comparison_incomplete);
                window
                    .carry
                    .assess_mic_poll(&candidate, &region.poll, &window.cleanup, evidence)
            };
            if assessment.verdict != EchoVerdict::Keep {
                window.cleanup_decisions.record_echo_assessment(
                    CleanupStage::Echo,
                    input_index,
                    &original,
                    assessment,
                    evidence,
                );
            }
            input_index += 1;
            if assessment.verdict == EchoVerdict::DropAcoustic {
                echo_dropped_audio += 1;
            } else {
                if assessment.verdict == EchoVerdict::KeepCriticalDifference
                    || region.comparison_incomplete
                    || window
                        .carry
                        .mic_comparison_incomplete(&candidate, &region.poll)
                {
                    protected_input_indices.push(segments.len());
                }
                segments.push(original);
            }
        }
    }
    if !window.them_words.is_empty() {
        let turns = if window.diarize_far_end {
            diarizer
                .diarize_chunk(&window.tap_audio, window.sample_rate, window.start_sec())?
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        // Bind protection to the ORIGINAL returned poll before any pause or diarization split. Every
        // displayed descendant keeps the flag; neither text lookup nor the bounded ledger is authority.
        let mut poll_start = 0;
        for &poll_end in &window.them_poll_ends {
            let words = &window.them_words[poll_start..poll_end];
            poll_start = poll_end;
            let original = TranscriptSegment {
                speaker: Speaker::Other("Them".to_string()),
                start: words[0].start,
                end: words[words.len() - 1].end,
                text: words
                    .iter()
                    .map(|word| word.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
            };
            let assessment = echo_assessment(
                &EchoCandidate::from_segment(&original),
                &mic_candidates,
                &window.cleanup,
                None,
            );
            if assessment.verdict != EchoVerdict::Keep {
                window.cleanup_decisions.record_echo_assessment(
                    CleanupStage::Echo,
                    input_index,
                    &original,
                    assessment,
                    None,
                );
            }
            input_index += 1;
            // Whole-poll protection supplements child matches: unrelated speech in the same returned
            // poll must not conceal a critical match belonging to one pause-separated child.
            for child in split_regions(words, SEGMENT_GAP) {
                let original_child =
                    words_to_segments(&child, Speaker::Other("Them".to_string()), f64::INFINITY)
                        .remove(0);
                let child_assessment = echo_assessment(
                    &EchoCandidate::from_segment(&original_child),
                    &mic_candidates,
                    &window.cleanup,
                    None,
                );
                if child_assessment.verdict != EchoVerdict::Keep {
                    window.cleanup_decisions.record_echo_assessment(
                        CleanupStage::Echo,
                        input_index,
                        &original_child,
                        child_assessment,
                        None,
                    );
                }
                input_index += 1;
                let protected = assessment.verdict == EchoVerdict::KeepCriticalDifference
                    || child_assessment.verdict == EchoVerdict::KeepCriticalDifference
                    || window.mic_polls_omitted > 0;
                let rendered = if window.diarize_far_end {
                    diarize_words(&child, &turns, SEGMENT_GAP, "Them")
                } else {
                    vec![original_child]
                };
                for segment in rendered {
                    if protected {
                        protected_input_indices.push(segments.len());
                    }
                    segments.push(segment);
                }
            }
        }
    }
    // Structural/text cleanup runs only after the raw echo verdict. Diarized fragments and rewritten
    // display text must never become new echo candidates or replace the raw source inventory.
    let rule = window
        .lexicon
        .as_deref()
        .map(|lexicon| lexicon as &dyn TextRule);
    let render_cleanup = CleanupConfig {
        echo_drop: false,
        ..window.cleanup.clone()
    };
    let result = cleanup_with_protected_decisions(
        segments,
        &render_cleanup,
        &[],
        None,
        rule,
        &protected_input_indices,
    );
    let segments = result.segments;
    let mut stats = result.stats;
    stats.echo_dropped_audio = echo_dropped_audio;
    window.cleanup_decisions.append(result.decisions);
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
            raw_mic_polls_omitted = window.mic_polls_omitted,
            buffered_audio_bytes = window.tap_audio.len() * std::mem::size_of::<f32>(),
            "durable live transcript chunk synced"
        );
    }
    writer.advance_window();
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
    writer.freeze_live_quality(&quality)?;
    if let Some(aec) = aec.take() {
        // `finish` consumes the filter, so the tail blocks — every block of a call shorter than the
        // lookahead — are only reachable through `finish_with_stats`. The final window's cleanup is the
        // one that needs them (#149 phase 3b).
        let fin = std::mem::take(&mut window.aec_availability).finish(aec)?;
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
            writer.delivered_input(corti_core::QualityTrack::Mic, fin.audio.len())?;
            writer.call_channel(corti_core::QualityTrack::Mic, &mut mic, |channel| {
                channel.push(&fin.audio, sample_rate)
            });
        }
    }
    if let Some(words) = mic.poll_words() {
        publish_mic_words(&mut window, publisher, words);
    }
    if let Some(words) = them.poll_words() {
        publish_them_words(&mut window, publisher, words);
    }
    let mic_words =
        writer.call_channel(corti_core::QualityTrack::Mic, &mut mic, LiveChannel::finish);
    publish_mic_words(&mut window, publisher, mic_words);
    let them_words = writer.call_channel(
        corti_core::QualityTrack::Tap,
        &mut them,
        LiveChannel::finish,
    );
    publish_them_words(&mut window, publisher, them_words);
    // The session is over: every held region is judged now against everything the far end ever said in
    // range, and published or dropped. Nothing waits past the last append.
    release_held_mic_words(&mut window, publisher);
    let early_drop = window.early_drop.stats;
    // Freeze after both decoder tails, including failure-only outcomes. Keep it available even if
    // the final append fails and the session's error boundary takes ownership of the partial note.
    writer.freeze_live_quality(&quality)?;
    if let Err(error) = flush_window(&diarizer, &mut window, writer) {
        writer.mark_incomplete();
        writer.freeze_live_quality(&quality)?;
        return Err(error);
    }
    if early_drop != EarlyDropStats::default() {
        info!(
            target: "corti::live",
            held = early_drop.held,
            released_published = early_drop.released_published,
            released_dropped = early_drop.released_dropped,
            "live early drop of short mic echoes"
        );
    }

    let mismatch_count = writer.mismatches.count();
    let capture = writer.freeze_live_quality(&quality)?;
    writer.settle_capture_quality(&capture, quality.capture.is_some())?;
    // Capture authority already retains unchanged facts. Carry any additional settled facts through
    // successful completion too; ordinary metadata capacity does not require a fallback or new ASR.
    let settled_quality =
        (capture != quality.capture.clone().unwrap_or_default()).then(|| capture.clone());
    match writer.path().cloned() {
        Some(note_path)
            if quality.dropped_chunks == 0
                && capture.completeness() == corti_core::SourceCompleteness::Complete =>
        {
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
            Ok(LiveOutcome::Filed {
                note_path,
                settled_quality,
            })
        }
        Some(note_path) => Ok(LiveOutcome::Fallback {
            reason: format!(
                "capture completeness {:?}; live tee dropped {} chunk(s); mic/tap mismatched {} chunk(s)",
                capture.completeness(),
                quality.dropped_chunks,
                mismatch_count
            ),
            note_path: Some(note_path),
            settled_quality: Some(capture),
        }),
        None if capture.requires_batch_recovery() => Ok(LiveOutcome::Fallback {
            reason: "live processing coverage is incomplete".into(),
            note_path: None,
            settled_quality: Some(capture),
        }),
        None => Ok(LiveOutcome::NoNote { settled_quality }),
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
            && let Err(error) = hosted.try_observe_finalized_rows(&self.id, rows)
        {
            tracing::warn!(
                target: "corti::hosted",
                ?error,
                "hosted row handoff saturated; these rows stay raw"
            );
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
    fn take_decode_report(&mut self) -> DecodeReport {
        DecodeReport::default()
    }
    fn decode_complete(&self) -> bool {
        true
    }
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
    fn take_decode_report(&mut self) -> DecodeReport {
        corti_transcribe_local::LiveTranscriber::take_decode_report(self)
    }
    fn decode_complete(&self) -> bool {
        corti_transcribe_local::LiveTranscriber::decode_complete(self)
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
    final_ledger_unavailable: bool,
    mismatches: LiveMismatches,
    decode_quality: DecodeQualityTracker,
    decode_incomplete: [bool; 2],
    /// Samples actually supplied to the decoder; pre-AEC received positions can be ahead of this.
    delivered_input_frames: [u64; 2],
    window_input_start: [u64; 2],
    /// Original Finish authority, never a previously settled snapshot. Tail errors can add facts
    /// after the first freeze; fallback recomputes from this baseline without recounting omissions.
    finish_capture: Option<corti_core::QualityRecord>,
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
            final_ledger_unavailable: false,
            mismatches: LiveMismatches::default(),
            decode_quality: DecodeQualityTracker::default(),
            decode_incomplete: [false; 2],
            delivered_input_frames: [0; 2],
            window_input_start: [0; 2],
            finish_capture: None,
        }
    }

    fn freeze_live_quality(&mut self, finish: &FinishQuality) -> Result<corti_core::QualityRecord> {
        let capture = finish.capture.clone().unwrap_or_default();
        if let Some(original) = &self.finish_capture {
            anyhow::ensure!(
                original == &capture,
                "conflicting live Finish capture authority"
            );
        } else {
            self.finish_capture = Some(capture.clone());
        }
        Ok(self
            .decode_quality
            .settle(&self.mismatches.settle(&capture)?)?)
    }

    fn fallback_quality(&self) -> Option<corti_core::QualityRecord> {
        self.finish_capture
            .as_ref()
            .and_then(|capture| {
                self.mismatches
                    .settle(capture)
                    .and_then(|quality| self.decode_quality.settle(&quality))
                    .ok()
            })
            .or_else(|| {
                let mut quality = self.decode_quality.provisional_quality();
                if self.mismatches.count() != 0 {
                    // Both are supplementary records. The pipeline still reloads mandatory capture
                    // authority; neither an early panic nor a missing sidecar establishes clean storage.
                    quality
                        .merge_snapshot(&self.mismatches.provisional_quality())
                        .ok()?;
                }
                (!quality.spans.is_empty() || quality.overflow != Default::default())
                    .then_some(quality)
            })
    }

    /// Keep already-produced facts if a later region panics in this same decoder call. The original
    /// panic immediately resumes into the existing session fallback; the decoder is never resumed.
    fn call_channel<C: LiveChannel, T>(
        &mut self,
        track: corti_core::QualityTrack,
        channel: &mut C,
        operation: impl FnOnce(&mut C) -> T,
    ) -> T {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| operation(channel)));
        self.drain_decode_report(track, channel);
        match result {
            Ok(value) => value,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    fn drain_decode_report<C: LiveChannel>(
        &mut self,
        track: corti_core::QualityTrack,
        channel: &mut C,
    ) {
        let index = usize::from(track == corti_core::QualityTrack::Tap);
        let mut report = channel.take_decode_report();
        if !channel.decode_complete() && !self.decode_incomplete[index] && report.is_complete() {
            report.omit_failure();
        }
        self.decode_incomplete[index] |= !channel.decode_complete() || !report.is_complete();
        self.decode_quality.record_report(track, report);
    }

    fn advance_window(&mut self) {
        self.window_input_start = self.delivered_input_frames;
    }

    fn delivered_input(&mut self, track: corti_core::QualityTrack, frames: usize) -> Result<()> {
        let index = usize::from(track == corti_core::QualityTrack::Tap);
        self.delivered_input_frames[index] = self.delivered_input_frames[index]
            .checked_add(frames as u64)
            .context("live delivered input position overflow")?;
        Ok(())
    }

    fn fallback_outcome(&self, reason: String) -> LiveOutcome {
        LiveOutcome::Fallback {
            reason,
            note_path: self.path().cloned(),
            settled_quality: self.fallback_quality(),
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
        if !self.final_ledger_unavailable {
            let added = segments.iter().fold(0usize, |total, segment| {
                total
                    .saturating_add(segment.text.len())
                    .saturating_add(segment.speaker.display().len())
                    .saturating_add(std::mem::size_of::<TranscriptSegment>())
            });
            if self.final_transcript_bytes.saturating_add(added) > MAX_FINAL_TRANSCRIPT_BYTES {
                self.final_segments.clear();
                self.final_transcript_bytes = 0;
                self.final_ledger_unavailable = true;
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
        for (index, track) in [corti_core::QualityTrack::Mic, corti_core::QualityTrack::Tap]
            .into_iter()
            .enumerate()
        {
            self.decode_quality.record_window_not_filed(
                track,
                InputSpan {
                    start_frame: self.window_input_start[index],
                    end_frame: self.delivered_input_frames[index],
                },
            );
        }
        self.advance_window();
        self.final_segments.clear();
        self.final_transcript_bytes = 0;
        self.final_ledger_unavailable = true;
    }

    fn final_transcript(&self) -> Option<DiarizedTranscript> {
        (!self.final_ledger_unavailable).then(|| {
            // Windows append `Me` and `Them` interleaved by start already, but the assembled final
            // transcript is what the hosted Final lane and the note see: keep it in time order (#157).
            let mut segments = self.final_segments.clone();
            segments.sort_by(|a, b| a.start.total_cmp(&b.start));
            DiarizedTranscript::new(segments)
        })
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

    /// Update content-free capture provenance independently of the optional final transcript ledger.
    /// Known loss creates a partial note even when the decoder produced no words.
    fn settle_capture_quality(
        &mut self,
        quality: &corti_core::QualityRecord,
        durable: bool,
    ) -> Result<()> {
        let storage = if durable {
            crate::transcribe::RecordingStorageStatus::AudioDurable
        } else {
            crate::transcribe::RecordingStorageStatus::LegacyUnknown
        };
        crate::provenance::attach_quality(&mut self.provenance, quality, Some(storage))?;
        let partial = quality.completeness() != corti_core::SourceCompleteness::Complete;
        if self.note.is_none()
            && partial
            && (!quality.spans.is_empty() || quality.overflow != Default::default())
        {
            self.create()?;
        }
        if let Some(path) = &self.note {
            corti_vagus::note::CurrentNote::from_returned_path(path.clone())?
                .refresh_provenance(&self.provenance)?;
            if partial {
                let body = crate::provenance::quality_body(
                    &self.meta,
                    &DiarizedTranscript::new(Vec::new()),
                    Some(quality),
                    Some(storage),
                );
                if let Some(start) = body.find("\n\n>") {
                    corti_vagus::note::append(path, &body[start..])?;
                }
            }
        }
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

    #[test]
    fn live_aec_masks_source_loss_after_prediction_and_tail_without_moving_later_audio() {
        let sample_rate = 8_000;
        let frames = 48_037usize;
        let mut seed = 17u32;
        let mut far: Vec<f32> = (0..frames)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed as f64 / u32::MAX as f64 - 0.5) as f32 * 0.4
            })
            .collect();
        let mut mic: Vec<f32> = (0..frames)
            .map(|index| 0.6 * far[index.saturating_sub(11)] + (index as f32 * 0.013).sin() * 0.01)
            .collect();
        let mut unavailable = vec![false; frames];
        for interval in [33_001..33_134, frames - 31..frames] {
            mic[interval.clone()].fill(0.0);
            far[interval.clone()].fill(0.0);
            unavailable[interval].fill(true);
        }
        for chunks in [[4096, 4096, 4096], [1, 377, 2049]] {
            let mut baseline = new_live_aec(sample_rate, corti_aec::AecConfig::default());
            let mut filter = new_live_aec(sample_rate, corti_aec::AecConfig::default());
            let mut availability = LiveAvailability::default();
            let mut original = Vec::new();
            let mut masked = Vec::new();
            let mut cursor = 0;
            let mut block = 0;
            while cursor < frames {
                let end = (cursor + chunks[block % chunks.len()]).min(frames);
                original.extend(baseline.push(&mic[cursor..end], &far[cursor..end]));
                masked.extend(
                    availability
                        .push(
                            &mut filter,
                            &mic[cursor..end],
                            &far[cursor..end],
                            &unavailable[cursor..end],
                        )
                        .unwrap(),
                );
                assert!(availability.pending.len() <= filter.max_output_lag_samples());
                cursor = end;
                block += 1;
            }
            original.extend(baseline.finish());
            masked.extend(availability.finish(filter).unwrap().audio);
            assert_eq!(masked.len(), frames);
            assert_eq!(original.len(), frames);
            assert!(
                original
                    .iter()
                    .zip(&unavailable)
                    .any(|(&sample, &missing)| missing && sample != 0.0),
                "fixture must demonstrate predictor output at unavailable input"
            );
            for index in 0..frames {
                assert_eq!(
                    masked[index].to_bits(),
                    if unavailable[index] {
                        0.0f32.to_bits()
                    } else {
                        original[index].to_bits()
                    },
                    "source position {index}"
                );
            }
        }
    }

    #[test]
    fn live_availability_does_not_extend_an_unfilled_jump_into_retained_audio() {
        // An unfilled source jump creates a new source epoch, not retained silence. Its C samples
        // arrive with no unavailable bitmap; neither the old loss nor a window flush can mask them.
        let mut filter = new_live_aec(8_000, corti_aec::AecConfig::default());
        let mut reference = new_live_aec(8_000, corti_aec::AecConfig::default());
        let mut availability = LiveAvailability::default();
        let mut expected = Vec::new();
        let mut actual = Vec::new();
        for value in [0.125, 0.375] {
            let mic = vec![value; 4096];
            let far = vec![0.0; mic.len()];
            expected.extend(reference.push(&mic, &far));
            actual.extend(availability.push(&mut filter, &mic, &far, &[]).unwrap());
        }
        expected.extend(reference.finish());
        actual.extend(availability.finish(filter).unwrap().audio);
        assert_eq!(
            actual
                .iter()
                .map(|value| value.to_bits())
                .collect::<Vec<_>>(),
            expected
                .iter()
                .map(|value| value.to_bits())
                .collect::<Vec<_>>()
        );
        assert_eq!(actual.len(), 8192);
        assert!(actual[4096..].iter().any(|&value| value != 0.0));
    }

    #[test]
    fn live_availability_rejects_malformed_or_unpositioned_filter_output() {
        let mut filter = new_live_aec(8_000, corti_aec::AecConfig::default());
        let mut availability = LiveAvailability::default();
        assert!(
            availability
                .push(&mut filter, &[0.0, 0.0], &[0.0, 0.0], &[true])
                .is_err()
        );
        assert!(availability.pending.is_empty());
        assert!(availability.mask(&mut [0.5]).is_err());
    }

    #[test]
    fn consume_and_finish_keep_availability_through_rolling_windows() {
        struct AudioSink(std::rc::Rc<RefCell<Vec<f32>>>);
        impl LiveChannel for AudioSink {
            fn push(&mut self, samples: &[f32], _: u32) {
                self.0.borrow_mut().extend_from_slice(samples);
            }
            fn poll_words(&mut self) -> Option<Vec<Word>> {
                None
            }
            fn checkpoint(&mut self) -> Vec<Word> {
                Vec::new()
            }
            fn finish(&mut self) -> Vec<Word> {
                Vec::new()
            }
        }
        let sample_rate = 8_000;
        let mut raw: Vec<f32> = (0..40_164)
            .map(|index| (index as f32 * 0.031).sin() * 0.2)
            .collect();
        let mut far: Vec<f32> = raw.iter().map(|sample| sample * 0.7).collect();
        let mut missing = vec![false; raw.len()];
        for interval in [20_000..20_133, 40_133..40_164] {
            raw[interval.clone()].fill(0.0);
            far[interval.clone()].fill(0.0);
            missing[interval].fill(true);
        }
        let mut reference = new_live_aec(sample_rate, corti_aec::AecConfig::default());
        let mut expected = Vec::new();
        for (mic, tap) in raw.chunks(4096).zip(far.chunks(4096)) {
            expected.extend(reference.push(mic, tap));
        }
        expected.extend(reference.finish());
        for (sample, &unavailable) in expected.iter_mut().zip(&missing) {
            if unavailable {
                *sample = 0.0;
            }
        }
        let (tx, rx) = sync_channel(1);
        tx.send(CaptureChunk {
            epoch: fixture_epoch(sample_rate, true, true),
            retained_start_frame: 0,
            mic: raw,
            tap: far.clone(),
            unavailable: missing,
        })
        .unwrap();
        drop(tx);
        let mic_audio = std::rc::Rc::new(RefCell::new(Vec::new()));
        let tap_audio = std::rc::Rc::new(RefCell::new(Vec::new()));
        let mut mic = AudioSink(mic_audio.clone());
        let mut tap = AudioSink(tap_audio.clone());
        let mut aec = Some(new_live_aec(sample_rate, corti_aec::AecConfig::default()));
        let mut window =
            TranscriptWindow::new(sample_rate, 1, false, CleanupConfig::default()).unwrap();
        window.frame_limit = 7_001; // Deliberately split filter blocks at several text-window boundaries.
        let mut writer = NoteWriter::new(TempFiler::new("availability-windows"), meta(), None);
        consume_chunks(
            &rx,
            sample_rate,
            &mut aec,
            &mut mic,
            &mut tap,
            &NoDiarizer,
            &mut window,
            &mut writer,
            &NoopPublisher,
        )
        .unwrap();
        assert!(window.start_frame > 0);
        finish_session(
            sample_rate,
            aec,
            mic,
            tap,
            NoDiarizer,
            window,
            FinishQuality {
                dropped_chunks: 0,
                capture: Some(corti_core::QualityRecord::observed()),
            },
            &mut writer,
            &NoopPublisher,
        )
        .unwrap();
        assert_eq!(
            mic_audio
                .borrow()
                .iter()
                .map(|sample| sample.to_bits())
                .collect::<Vec<_>>(),
            expected
                .iter()
                .map(|sample| sample.to_bits())
                .collect::<Vec<_>>()
        );
        assert_eq!(*tap_audio.borrow(), far);
    }

    fn fixture_epoch(sample_rate: u32, mic: bool, tap: bool) -> corti_core::CaptureEpoch {
        corti_core::CaptureEpoch {
            id: 0,
            source_start_ns: Some(0),
            source_rate_hz: sample_rate,
            source_frame_base: 0,
            retained_start_frame: 0,
            retained_rate_hz: sample_rate,
            mic_channels: u32::from(mic),
            tap_channels: u32::from(tap),
            reason: corti_core::CaptureEpochReason::Start,
        }
    }

    /// Scripted stand-in for `LiveTranscriber`: each `push` queues the next scripted word batch;
    /// `finish` returns the scripted tail.
    struct Scripted {
        on_push: VecDeque<Vec<Word>>,
        pending: Vec<Word>,
        tail: Vec<Word>,
        pushes: Vec<usize>,
        audio: std::rc::Rc<RefCell<Vec<f32>>>,
        clock_tail: Option<&'static str>,
        sample_rate: u32,
        panic_on_push: Option<usize>,
        panic_on_finish: bool,
        panic_after_report: Option<&'static str>,
        on_push_reports: VecDeque<DecodeReport>,
        checkpoint_report: DecodeReport,
        finish_report: DecodeReport,
        pending_report: DecodeReport,
        decode_complete: bool,
    }

    impl Scripted {
        fn new(on_push: Vec<Vec<Word>>, tail: Vec<Word>) -> Self {
            Self {
                on_push: on_push.into(),
                pending: Vec::new(),
                tail,
                pushes: Vec::new(),
                audio: Default::default(),
                clock_tail: None,
                sample_rate: 1,
                panic_on_push: None,
                panic_on_finish: false,
                panic_after_report: None,
                on_push_reports: VecDeque::new(),
                checkpoint_report: DecodeReport::default(),
                finish_report: DecodeReport::default(),
                pending_report: DecodeReport::default(),
                decode_complete: true,
            }
        }
    }

    impl LiveChannel for Scripted {
        fn push(&mut self, samples: &[f32], sample_rate: u32) {
            self.pushes.push(samples.len());
            self.audio.borrow_mut().extend_from_slice(samples);
            self.sample_rate = sample_rate;
            assert_ne!(
                self.panic_on_push,
                Some(self.pushes.len()),
                "scripted decoder panic"
            );
            if let Some(words) = self.on_push.pop_front() {
                self.pending.extend(words);
            }
            if let Some(report) = self.on_push_reports.pop_front() {
                self.decode_complete &= report.is_complete();
                self.pending_report.merge(report);
            }
            assert_ne!(
                self.panic_after_report,
                Some("push"),
                "scripted push panic after report"
            );
        }
        fn poll_words(&mut self) -> Option<Vec<Word>> {
            if self.pending.is_empty() {
                None
            } else {
                Some(std::mem::take(&mut self.pending))
            }
        }
        fn checkpoint(&mut self) -> Vec<Word> {
            let report = std::mem::take(&mut self.checkpoint_report);
            self.decode_complete &= report.is_complete();
            self.pending_report.merge(report);
            assert_ne!(
                self.panic_after_report,
                Some("checkpoint"),
                "scripted checkpoint panic after report"
            );
            std::mem::take(&mut self.pending)
        }
        fn finish(&mut self) -> Vec<Word> {
            assert!(!self.panic_on_finish, "scripted finish panic");
            let report = std::mem::take(&mut self.finish_report);
            self.decode_complete &= report.is_complete();
            self.pending_report.merge(report);
            assert_ne!(
                self.panic_after_report,
                Some("finish"),
                "scripted finish panic after report"
            );
            let mut words = std::mem::take(&mut self.pending);
            words.append(&mut self.tail);
            if let Some(text) = self.clock_tail.take() {
                let frames = self.audio.borrow().len();
                words.push(word(
                    frames.saturating_sub(1) as f64 / f64::from(self.sample_rate),
                    frames as f64 / f64::from(self.sample_rate),
                    text,
                ));
            }
            words
        }
        fn take_decode_report(&mut self) -> DecodeReport {
            std::mem::take(&mut self.pending_report)
        }
        fn decode_complete(&self) -> bool {
            self.decode_complete
        }
    }

    #[test]
    fn decode_failures_cross_push_checkpoint_and_finish_without_losing_survivors() {
        use corti_transcribe::decode::{DecodeErrorCategory, RegionOutcome, RegionStatus};
        for stage in ["push", "checkpoint", "finish"] {
            let filer = TempFiler::new(&format!("decode-{stage}"));
            let note = filer.note();
            let mut writer = NoteWriter::new(filer, meta(), None);
            let expected = vec![
                word(0.0, 0.5, "Synthetic first."),
                word(2.0, 2.5, "Synthetic third."),
                word(2.5, 3.0, "Synthetic tail."),
            ];
            let mut them = Scripted::new(
                vec![vec![expected[0].clone()], vec![], vec![expected[1].clone()]],
                vec![expected[2].clone()],
            );
            let mut report = DecodeReport::default();
            report.record(RegionOutcome {
                span: InputSpan {
                    start_frame: 10,
                    end_frame: 20,
                },
                status: RegionStatus::Failed(DecodeErrorCategory::NoResult),
            });
            match stage {
                "push" => them.on_push_reports = vec![DecodeReport::default(), report].into(),
                "checkpoint" => them.checkpoint_report = report,
                _ => them.finish_report = report,
            }
            let mut mic = Scripted::new(vec![], vec![]);
            let (tx, rx) = sync_channel(3);
            let epoch = fixture_epoch(10, false, true);
            for index in 0..3 {
                tx.send(CaptureChunk {
                    epoch,
                    retained_start_frame: index * 10,
                    unavailable: vec![],
                    mic: vec![],
                    tap: vec![0.0; 10],
                })
                .unwrap();
            }
            drop(tx);
            let publisher = RecordingPublisher::default();
            let mut window = TranscriptWindow::new(10, 1, false, CleanupConfig::default()).unwrap();
            consume_chunks(
                &rx,
                10,
                &mut None,
                &mut mic,
                &mut them,
                &NoDiarizer,
                &mut window,
                &mut writer,
                &publisher,
            )
            .unwrap();
            if stage == "checkpoint" {
                checkpoint_and_flush(
                    &mut mic,
                    &mut them,
                    &NoDiarizer,
                    &mut window,
                    &mut writer,
                    &publisher,
                )
                .unwrap();
            }
            let mut capture = corti_core::QualityRecord::observed();
            capture.insert_epoch(epoch).unwrap();
            let outcome = finish_session(
                10,
                None,
                mic,
                them,
                NoDiarizer,
                window,
                FinishQuality {
                    dropped_chunks: 0,
                    capture: Some(capture),
                },
                &mut writer,
                &publisher,
            )
            .unwrap();
            let LiveOutcome::Fallback {
                settled_quality: Some(quality),
                note_path: Some(path),
                ..
            } = outcome
            else {
                panic!("{stage}: failure must remain a positioned fallback")
            };
            assert_eq!(path, note);
            assert_eq!(quality.spans.len(), 1);
            let span = quality.spans[0];
            assert_eq!((span.start_frame, span.end_frame), (10, 20));
            assert_eq!(span.cause, corti_core::QualityCause::NoResult);
            assert_eq!(span.track, corti_core::QualityTrack::Tap);
            assert!(quality.requires_batch_recovery());
            let published: Vec<_> = publisher
                .0
                .borrow()
                .iter()
                .flat_map(|(_, words)| words.clone())
                .collect();
            assert_eq!(
                published, expected,
                "{stage}: exact words and timestamps survive including tail"
            );
            let content = read(&note);
            assert!(content.contains("no_result"));
            assert!(content.contains("State: transcribing\n"));
        }
    }

    #[test]
    fn decode_source_binding_survives_late_mic_tee_holes_and_aec_tail() {
        use corti_transcribe::decode::{DecodeErrorCategory, RegionOutcome, RegionStatus};
        for aec_enabled in [false, true] {
            let sample_rate = 8_000;
            let first = fixture_epoch(sample_rate, false, true);
            let later = corti_core::CaptureEpoch {
                id: 1,
                source_start_ns: Some(2_000_000),
                source_frame_base: 1_000,
                retained_start_frame: 16,
                mic_channels: 1,
                reason: corti_core::CaptureEpochReason::LayoutChange,
                ..first
            };
            let mut capture = corti_core::QualityRecord::observed();
            capture.insert_epoch(first).unwrap();
            capture.insert_epoch(later).unwrap();
            let (tx, rx) = sync_channel(3);
            tx.send(CaptureChunk {
                epoch: first,
                retained_start_frame: 0,
                mic: vec![],
                tap: vec![0.0; 16],
                unavailable: vec![],
            })
            .unwrap();
            for retained_start_frame in [80, 160] {
                tx.send(CaptureChunk {
                    epoch: later,
                    retained_start_frame,
                    mic: vec![0.0; 16],
                    tap: vec![0.0; 16],
                    unavailable: vec![],
                })
                .unwrap();
            }
            drop(tx);
            let mut mic = Scripted::new(vec![], vec![]);
            mic.finish_report.record(RegionOutcome {
                span: InputSpan {
                    start_frame: 16,
                    end_frame: 32,
                },
                status: RegionStatus::Failed(DecodeErrorCategory::BackendFailure),
            });
            let observed_mic = mic.audio.clone();
            let mut tap = Scripted::new(vec![], vec![]);
            let mut aec =
                aec_enabled.then(|| new_live_aec(sample_rate, corti_aec::AecConfig::default()));
            let mut window =
                TranscriptWindow::new(sample_rate, 1, false, CleanupConfig::default()).unwrap();
            let mut writer = NoteWriter::new(
                TempFiler::new(&format!("decode-input-map-{aec_enabled}")),
                meta(),
                None,
            );
            consume_chunks(
                &rx,
                sample_rate,
                &mut aec,
                &mut mic,
                &mut tap,
                &NoDiarizer,
                &mut window,
                &mut writer,
                &NoopPublisher,
            )
            .unwrap();
            let outcome = finish_session(
                sample_rate,
                aec,
                mic,
                tap,
                NoDiarizer,
                window,
                FinishQuality {
                    dropped_chunks: 2,
                    capture: Some(capture),
                },
                &mut writer,
                &NoopPublisher,
            )
            .unwrap();
            assert_eq!(
                observed_mic.borrow().len(),
                32,
                "AEC tail completes original mic input"
            );
            let LiveOutcome::Fallback {
                settled_quality: Some(quality),
                ..
            } = outcome
            else {
                panic!("decode loss must survive finish")
            };
            assert_eq!(quality.spans.len(), 1);
            let span = quality.spans[0];
            assert_eq!(
                (span.epoch, span.start_frame, span.end_frame),
                (1, 1_144, 1_160)
            );
            assert_eq!(span.track, corti_core::QualityTrack::Mic);
            assert_eq!(span.cause, corti_core::QualityCause::DecodeFailure);
        }
    }

    #[test]
    fn fallback_after_later_finish_panic_retains_new_aec_tail_decode_loss() {
        use corti_transcribe::decode::{DecodeErrorCategory, RegionOutcome, RegionStatus};
        for first_received_later_epoch in [false, true] {
            let sample_rate = 8_000;
            let first = fixture_epoch(sample_rate, true, true);
            let received = if first_received_later_epoch {
                corti_core::CaptureEpoch {
                    id: 1,
                    source_start_ns: Some(2_000_000),
                    source_frame_base: 1_000,
                    retained_start_frame: 16,
                    reason: corti_core::CaptureEpochReason::LayoutChange,
                    ..first
                }
            } else {
                first
            };
            let mut capture = corti_core::QualityRecord::observed();
            capture.insert_epoch(first).unwrap();
            capture.insert_epoch(received).unwrap();
            capture
                .record_omitted_spans(
                    corti_core::QualityClass::ProcessingLoss,
                    corti_core::QualityResolution::Outstanding,
                    2,
                )
                .unwrap();
            let mut report = DecodeReport::default();
            report.record(RegionOutcome {
                span: InputSpan {
                    start_frame: 16,
                    end_frame: 32,
                },
                status: RegionStatus::Failed(DecodeErrorCategory::BackendFailure),
            });
            report.omit_failure();
            let mut mic = Scripted::new(vec![], vec![]);
            mic.on_push_reports.push_back(report);
            let mut tap = Scripted::new(vec![], vec![]);
            tap.panic_on_finish = true;
            let (tx, rx) = sync_channel(1);
            tx.send(CaptureChunk {
                epoch: received,
                retained_start_frame: 80,
                mic: vec![0.0; 32],
                tap: vec![0.0; 32],
                unavailable: vec![],
            })
            .unwrap();
            drop(tx);
            let mut aec = Some(new_live_aec(sample_rate, corti_aec::AecConfig::default()));
            let mut window =
                TranscriptWindow::new(sample_rate, 1, false, CleanupConfig::default()).unwrap();
            let mut writer = NoteWriter::new(
                TempFiler::new(&format!("decode-tail-panic-{first_received_later_epoch}")),
                meta(),
                None,
            );
            consume_chunks(
                &rx,
                sample_rate,
                &mut aec,
                &mut mic,
                &mut tap,
                &NoDiarizer,
                &mut window,
                &mut writer,
                &NoopPublisher,
            )
            .unwrap();
            assert!(
                mic.pushes.is_empty(),
                "the new decode failure must arrive only in the AEC finish tail"
            );
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                finish_session(
                    sample_rate,
                    aec,
                    mic,
                    tap,
                    NoDiarizer,
                    window,
                    FinishQuality {
                        dropped_chunks: 0,
                        capture: Some(capture.clone()),
                    },
                    &mut writer,
                    &NoopPublisher,
                )
            }));
            assert!(
                result.is_err(),
                "the later channel finish must take the outer panic fallback"
            );
            let LiveOutcome::Fallback {
                settled_quality: Some(quality),
                ..
            } = writer.fallback_outcome("synthetic later finish panic".into())
            else {
                panic!("fallback must keep quality")
            };
            let span = quality
                .spans
                .iter()
                .find(|span| span.cause == corti_core::QualityCause::DecodeFailure)
                .expect(
                    "fallback must include the decode failure recorded after its initial freeze",
                );
            let expected_start = if first_received_later_epoch {
                1_080
            } else {
                96
            };
            assert_eq!(
                (span.epoch, span.start_frame, span.end_frame),
                (received.id, expected_start, expected_start + 16)
            );
            assert_eq!(
                quality.epochs, capture.epochs,
                "Finish supplies the omitted Start binding"
            );
            assert_eq!(
                quality.overflow.outstanding_omitted_spans[1], 3,
                "original capture omissions plus new report omissions exactly once"
            );
            assert_eq!(
                writer.fallback_quality(),
                Some(quality),
                "fallback replay must not accumulate omissions"
            );
        }
    }

    #[test]
    fn failed_aec_window_files_loss_for_delivered_tail_and_delayed_prefix() {
        for tail_only in [true, false] {
            let sample_rate = 8_000;
            let epoch = fixture_epoch(sample_rate, true, true);
            let frames = if tail_only { 16 } else { 8_208 };
            let (tx, rx) = sync_channel(1);
            tx.send(CaptureChunk {
                epoch,
                retained_start_frame: 0,
                mic: vec![0.0; frames],
                tap: vec![0.0; frames],
                unavailable: vec![],
            })
            .unwrap();
            drop(tx);
            let mut mic = Scripted::new(
                vec![vec![word(
                    0.0,
                    16.0 / f64::from(sample_rate),
                    "Synthetic delayed microphone.",
                )]],
                vec![],
            );
            let delivered = mic.audio.clone();
            let mut tap = Scripted::new(vec![], vec![]);
            let mut window =
                TranscriptWindow::new(sample_rate, 1, false, CleanupConfig::default()).unwrap();
            window.frame_limit = 16;
            let mut aec = Some(new_live_aec(sample_rate, corti_aec::AecConfig::default()));
            let mut writer = NoteWriter::new(
                FlakyFiler::new(&format!("decode-window-delivered-{tail_only}"), 1),
                meta(),
                None,
            );
            consume_chunks(
                &rx,
                sample_rate,
                &mut aec,
                &mut mic,
                &mut tap,
                &NoDiarizer,
                &mut window,
                &mut writer,
                &NoopPublisher,
            )
            .unwrap();
            let delivered_before_finish = delivered.borrow().len();
            assert_eq!(
                delivered_before_finish == 0,
                tail_only,
                "fixture distinguishes final tail from later ordinary window"
            );
            let mut capture = corti_core::QualityRecord::observed();
            capture.insert_epoch(epoch).unwrap();
            let result = finish_session(
                sample_rate,
                aec,
                mic,
                tap,
                NoDiarizer,
                window,
                FinishQuality {
                    dropped_chunks: 0,
                    capture: Some(capture),
                },
                &mut writer,
                &NoopPublisher,
            );
            if tail_only {
                assert!(result.is_err(), "final tail append must fail");
            } else {
                assert!(result.is_ok(), "ordinary failed append is contained");
            }
            assert_eq!(
                delivered.borrow().len(),
                frames,
                "AEC delivers every observed mic frame exactly once"
            );
            let quality = writer
                .fallback_quality()
                .expect("failed delivered window must retain quality");
            let loss = quality
                .spans
                .iter()
                .find(|span| {
                    span.track == corti_core::QualityTrack::Mic
                        && span.cause == corti_core::QualityCause::WindowNotFiled
                })
                .expect("failed window must mark the delayed mic input it actually received");
            assert_eq!(
                loss.start_frame, 0,
                "an earlier silent checkpoint never filed this delayed prefix"
            );
            assert_eq!(
                loss.end_frame,
                if tail_only {
                    16
                } else {
                    delivered_before_finish as u64
                }
            );
            assert!(quality.requires_batch_recovery());
        }
    }

    #[test]
    fn successful_metadata_caps_reach_filed_and_no_note_without_decode_loss() {
        use corti_core::{QualityClass, QualityResolution};
        use corti_transcribe::decode::{RegionOutcome, RegionStatus};
        for stage in ["push", "checkpoint", "finish"] {
            for with_words in [false, true] {
                for metadata in [false, true] {
                    let mut report = DecodeReport::default();
                    if metadata {
                        for frame in 0..257 {
                            report.record(RegionOutcome {
                                span: InputSpan {
                                    start_frame: frame,
                                    end_frame: frame + 1,
                                },
                                status: if with_words {
                                    RegionStatus::Complete
                                } else {
                                    RegionStatus::RecognizedEmpty
                                },
                            });
                        }
                    }
                    assert!(report.is_complete());
                    let mut mic = Scripted::new(
                        vec![],
                        if with_words {
                            vec![word(0.0, 257.0 / 16_000.0, "Synthetic complete words.")]
                        } else {
                            vec![]
                        },
                    );
                    match stage {
                        "push" => mic.on_push_reports.push_back(report),
                        "checkpoint" => mic.checkpoint_report = report,
                        _ => mic.finish_report = report,
                    }
                    let mut tap = Scripted::new(vec![], vec![]);
                    let epoch = fixture_epoch(16_000, true, true);
                    let mut capture = corti_core::QualityRecord::observed();
                    capture.insert_epoch(epoch).unwrap();
                    capture
                        .record_omitted_spans(
                            QualityClass::DegradedOnly,
                            QualityResolution::Outstanding,
                            2,
                        )
                        .unwrap();
                    let mut expected = capture.clone();
                    expected
                        .record_omitted_spans(
                            QualityClass::DegradedOnly,
                            QualityResolution::Outstanding,
                            u64::from(metadata),
                        )
                        .unwrap();
                    let (tx, rx) = sync_channel(1);
                    tx.send(CaptureChunk {
                        epoch,
                        retained_start_frame: 0,
                        mic: vec![0.0; 257],
                        tap: vec![0.0; 257],
                        unavailable: vec![],
                    })
                    .unwrap();
                    drop(tx);
                    let mut writer = NoteWriter::new(
                        TempFiler::new(&format!("metadata-{stage}-{with_words}-{metadata}")),
                        meta(),
                        None,
                    );
                    let mut window =
                        TranscriptWindow::new(16_000, 1, false, CleanupConfig::default()).unwrap();
                    if stage == "checkpoint" {
                        window.frame_limit = 257;
                    }
                    let publisher = RecordingPublisher::default();
                    consume_chunks(
                        &rx,
                        16_000,
                        &mut None,
                        &mut mic,
                        &mut tap,
                        &NoDiarizer,
                        &mut window,
                        &mut writer,
                        &publisher,
                    )
                    .unwrap();
                    assert!(mic.decode_complete());
                    let outcome = finish_session(
                        16_000,
                        None,
                        mic,
                        tap,
                        NoDiarizer,
                        window,
                        FinishQuality {
                            dropped_chunks: 0,
                            capture: Some(capture.clone()),
                        },
                        &mut writer,
                        &publisher,
                    )
                    .unwrap();
                    let quality = match outcome {
                        LiveOutcome::Filed {
                            settled_quality, ..
                        } if with_words => settled_quality,
                        LiveOutcome::NoNote { settled_quality } if !with_words => settled_quality,
                        _ => panic!(
                            "ordinary metadata capacity must preserve the successful live outcome"
                        ),
                    };
                    assert_eq!(quality, metadata.then(|| expected.clone()));
                    assert_eq!(
                        writer
                            .freeze_live_quality(&FinishQuality {
                                dropped_chunks: 0,
                                capture: Some(capture.clone())
                            })
                            .unwrap(),
                        expected
                    );
                    assert_eq!(
                        writer.finish_capture.as_ref(),
                        Some(&capture),
                        "settlement preserves original authority"
                    );
                    assert!(!expected.requires_batch_recovery());
                    assert_eq!(writer.decode_incomplete, [false, false]);
                    let words: Vec<_> = publisher
                        .0
                        .borrow()
                        .iter()
                        .flat_map(|(_, words)| words.clone())
                        .collect();
                    assert_eq!(words.len(), usize::from(with_words));
                    if with_words {
                        assert_eq!(
                            (words[0].start, words[0].end, words[0].text.as_str()),
                            (0.0, 257.0 / 16_000.0, "Synthetic complete words.")
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn metadata_report_survives_same_call_panic_without_processing_loss() {
        use corti_core::{QualityClass, QualityResolution};
        use corti_transcribe::decode::{RegionOutcome, RegionStatus};
        for stage in ["push", "checkpoint", "finish"] {
            let mut report = DecodeReport::default();
            for frame in 0..257 {
                report.record(RegionOutcome {
                    span: InputSpan {
                        start_frame: frame,
                        end_frame: frame + 1,
                    },
                    status: RegionStatus::RecognizedEmpty,
                });
            }
            let mut mic = Scripted::new(vec![], vec![]);
            mic.panic_after_report = Some(stage);
            match stage {
                "push" => mic.on_push_reports.push_back(report),
                "checkpoint" => mic.checkpoint_report = report,
                _ => mic.finish_report = report,
            }
            let mut tap = Scripted::new(vec![], vec![]);
            let epoch = fixture_epoch(16_000, true, true);
            let mut capture = corti_core::QualityRecord::observed();
            capture.insert_epoch(epoch).unwrap();
            capture
                .record_omitted_spans(
                    QualityClass::DegradedOnly,
                    QualityResolution::Outstanding,
                    2,
                )
                .unwrap();
            let finish = FinishQuality {
                dropped_chunks: 0,
                capture: Some(capture.clone()),
            };
            let (tx, rx) = sync_channel(1);
            tx.send(CaptureChunk {
                epoch,
                retained_start_frame: 0,
                mic: vec![0.0; 257],
                tap: vec![0.0; 257],
                unavailable: vec![],
            })
            .unwrap();
            drop(tx);
            let mut window =
                TranscriptWindow::new(16_000, 1, false, CleanupConfig::default()).unwrap();
            if stage == "checkpoint" {
                window.frame_limit = 257;
            }
            let mut writer = NoteWriter::new(
                TempFiler::new(&format!("metadata-panic-{stage}")),
                meta(),
                None,
            );
            let consumed = consume_with_panic_boundary(|| {
                consume_chunks(
                    &rx,
                    16_000,
                    &mut None,
                    &mut mic,
                    &mut tap,
                    &NoDiarizer,
                    &mut window,
                    &mut writer,
                    &NoopPublisher,
                )
            });
            if stage == "finish" {
                consumed.unwrap();
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| finish_session(
                        16_000,
                        None,
                        mic,
                        tap,
                        NoDiarizer,
                        window,
                        finish,
                        &mut writer,
                        &NoopPublisher
                    )))
                    .is_err()
                );
            } else {
                assert!(consumed.is_err());
                let (tx, rx) = sync_channel(1);
                tx.send(Verdict::Finish(finish)).unwrap();
                assert!(await_capture_verdict(&rx, consumed, &mut writer).is_err());
            }
            let mut expected = capture.clone();
            expected
                .record_omitted_spans(
                    QualityClass::DegradedOnly,
                    QualityResolution::Outstanding,
                    1,
                )
                .unwrap();
            assert_eq!(writer.fallback_quality(), Some(expected.clone()));
            assert_eq!(writer.fallback_quality(), Some(expected));
            assert_eq!(writer.finish_capture.as_ref(), Some(&capture));
            assert_eq!(writer.decode_incomplete, [false, false]);
        }
    }

    #[test]
    fn fallback_without_finish_keeps_mapping_omissions_and_omits_clean_quality() {
        let mut writer = NoteWriter::new(TempFiler::new("decode-no-finish-mapping"), meta(), None);
        assert!(writer.fallback_quality().is_none());
        let epoch = fixture_epoch(8_000, true, true);
        for index in 0..257 {
            writer
                .decode_quality
                .observe_input(corti_core::QualityTrack::Mic, epoch, index * 2, 1);
        }
        let (tx, rx) = sync_channel(1);
        drop(tx);
        assert!(await_capture_verdict(&rx, Ok(()), &mut writer).is_err());
        let quality = writer
            .fallback_quality()
            .expect("mapping overflow survives a disconnected Finish channel");
        assert_eq!(quality.overflow.omitted_spans, [0, 0, 1]);
        assert!(
            !quality.requires_batch_recovery(),
            "mapping inventory alone does not require ASR fallback"
        );
        assert!(quality.spans.is_empty());
    }

    #[test]
    fn queued_decode_report_survives_same_call_panic_at_every_channel_boundary() {
        use corti_transcribe::decode::{DecodeErrorCategory, RegionOutcome, RegionStatus};
        for stage in ["push", "checkpoint", "finish"] {
            let sample_rate = 8_000;
            let first = fixture_epoch(sample_rate, true, true);
            let later = corti_core::CaptureEpoch {
                id: 1,
                source_start_ns: Some(2_000_000),
                source_frame_base: 1_000,
                retained_start_frame: 16,
                reason: corti_core::CaptureEpochReason::LayoutChange,
                ..first
            };
            let mut capture = corti_core::QualityRecord::observed();
            capture.insert_epoch(first).unwrap();
            capture.insert_epoch(later).unwrap();
            let finish = FinishQuality {
                dropped_chunks: 0,
                capture: Some(capture),
            };
            let mut report = DecodeReport::default();
            report.record(RegionOutcome {
                span: InputSpan {
                    start_frame: 0,
                    end_frame: 16,
                },
                status: RegionStatus::Failed(DecodeErrorCategory::BackendFailure),
            });
            let mut mic = Scripted::new(vec![], vec![]);
            mic.panic_after_report = Some(stage);
            match stage {
                "push" => mic.on_push_reports.push_back(report),
                "checkpoint" => mic.checkpoint_report = report,
                _ => mic.finish_report = report,
            }
            let mut tap = Scripted::new(vec![], vec![]);
            let (tx, rx) = sync_channel(1);
            tx.send(CaptureChunk {
                epoch: later,
                retained_start_frame: 80,
                mic: vec![0.0; 16],
                tap: vec![0.0; 16],
                unavailable: vec![],
            })
            .unwrap();
            drop(tx);
            let mut window =
                TranscriptWindow::new(sample_rate, 1, false, CleanupConfig::default()).unwrap();
            window.frame_limit = if stage == "checkpoint" { 16 } else { 64 };
            let mut writer = NoteWriter::new(
                TempFiler::new(&format!("decode-report-panic-{stage}")),
                meta(),
                None,
            );
            let consumed = consume_with_panic_boundary(|| {
                consume_chunks(
                    &rx,
                    sample_rate,
                    &mut None,
                    &mut mic,
                    &mut tap,
                    &NoDiarizer,
                    &mut window,
                    &mut writer,
                    &NoopPublisher,
                )
            });
            if stage == "finish" {
                consumed.unwrap();
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| finish_session(
                        sample_rate,
                        None,
                        mic,
                        tap,
                        NoDiarizer,
                        window,
                        finish,
                        &mut writer,
                        &NoopPublisher
                    )))
                    .is_err()
                );
            } else {
                assert!(consumed.is_err());
                let (tx, rx) = sync_channel(1);
                tx.send(Verdict::Finish(finish)).unwrap();
                assert!(await_capture_verdict(&rx, consumed, &mut writer).is_err());
            }
            let quality = writer
                .fallback_quality()
                .expect("original Finish capture remains available");
            let loss = quality
                .spans
                .iter()
                .find(|span| span.cause == corti_core::QualityCause::DecodeFailure)
                .expect("a queued report must survive its own call's panic");
            assert_eq!(
                (loss.epoch, loss.start_frame, loss.end_frame),
                (1, 1_064, 1_080)
            );
            assert_eq!(quality.spans.len(), 1);
            assert!(quality.requires_batch_recovery());
        }
    }

    #[test]
    fn mismatched_chunks_keep_both_tracks_in_filter_order() {
        for aec_enabled in [false, true] {
            for (mic_short, missing_frames) in
                [(false, 37), (true, 37), (false, 4096), (true, 4096)]
            {
                let sample_rate = 8_000;
                let frames = 4_096;
                let mut expected_mic = Vec::new();
                let mut expected_tap = Vec::new();
                let mut missing_mic = Vec::new();
                let (tx, rx) = sync_channel(3);
                for chunk_index in 0..3 {
                    let mic_len =
                        frames - usize::from(chunk_index == 1 && mic_short) * missing_frames;
                    let tap_len =
                        frames - usize::from(chunk_index == 1 && !mic_short) * missing_frames;
                    let mic: Vec<_> = (0..mic_len)
                        .map(|frame| ((chunk_index * frames + frame) as f32 * 0.031).sin() * 0.2)
                        .collect();
                    let tap: Vec<_> = (0..tap_len)
                        .map(|frame| ((chunk_index * frames + frame) as f32 * 0.019).cos() * 0.1)
                        .collect();
                    expected_mic.extend_from_slice(&mic);
                    expected_mic.resize((chunk_index + 1) * frames, 0.0);
                    expected_tap.extend_from_slice(&tap);
                    expected_tap.resize((chunk_index + 1) * frames, 0.0);
                    missing_mic.extend((0..frames).map(|frame| frame >= mic_len));
                    tx.send(CaptureChunk {
                        epoch: fixture_epoch(sample_rate, true, true),
                        retained_start_frame: (chunk_index * frames) as u64,
                        mic,
                        tap,
                        unavailable: Vec::new(),
                    })
                    .unwrap();
                }
                drop(tx);
                if aec_enabled {
                    let mut reference = new_live_aec(sample_rate, corti_aec::AecConfig::default());
                    let mut filtered = Vec::new();
                    for (mic, tap) in expected_mic.chunks(4096).zip(expected_tap.chunks(4096)) {
                        filtered.extend(reference.push(mic, tap));
                    }
                    filtered.extend(reference.finish());
                    for (sample, &missing) in filtered.iter_mut().zip(&missing_mic) {
                        if missing {
                            *sample = 0.0;
                        }
                    }
                    expected_mic = filtered;
                }
                let mut mic = Scripted::new(Vec::new(), Vec::new());
                let mut tap = Scripted::new(Vec::new(), Vec::new());
                mic.clock_tail = Some("later microphone");
                tap.clock_tail = Some("later far end");
                let actual_mic = mic.audio.clone();
                let actual_tap = tap.audio.clone();
                let mut aec =
                    aec_enabled.then(|| new_live_aec(sample_rate, corti_aec::AecConfig::default()));
                let mut window =
                    TranscriptWindow::new(sample_rate, 1, false, CleanupConfig::default()).unwrap();
                window.frame_limit = 7_001;
                let publisher = RecordingPublisher::default();
                let mut capture = corti_core::QualityRecord::observed();
                capture
                    .insert_epoch(fixture_epoch(sample_rate, true, true))
                    .unwrap();
                let mut writer = NoteWriter::new(TempFiler::new("mismatch-order"), meta(), None);
                consume_chunks(
                    &rx,
                    sample_rate,
                    &mut aec,
                    &mut mic,
                    &mut tap,
                    &NoDiarizer,
                    &mut window,
                    &mut writer,
                    &publisher,
                )
                .unwrap();
                assert_eq!(writer.mismatches.count(), 1, "once per received chunk");
                let outcome = finish_session(
                    sample_rate,
                    aec,
                    mic,
                    tap,
                    NoDiarizer,
                    window,
                    FinishQuality {
                        dropped_chunks: 0,
                        capture: Some(capture.clone()),
                    },
                    &mut writer,
                    &publisher,
                )
                .unwrap();
                assert_eq!(
                    *actual_tap.borrow(),
                    expected_tap,
                    "AEC={aec_enabled}, mic_short={mic_short}: tap timeline"
                );
                assert_eq!(
                    *actual_mic.borrow(),
                    expected_mic,
                    "AEC={aec_enabled}, mic_short={mic_short}: mic timeline"
                );
                let LiveOutcome::Fallback {
                    settled_quality: Some(settled),
                    ..
                } = outcome
                else {
                    panic!("mismatch must retain settled quality through fallback");
                };
                assert!(
                    capture.spans.is_empty(),
                    "capture provenance stays immutable"
                );
                assert_eq!(settled.spans.len(), 1);
                let span = settled.spans[0];
                assert_eq!(span.start_frame, (frames * 2 - missing_frames) as u64);
                assert_eq!(span.end_frame, (frames * 2) as u64);
                assert_eq!(span.cause, corti_core::QualityCause::MicTapMismatch);
                assert_eq!(span.class, corti_core::QualityClass::ProcessingLoss);
                assert_eq!(
                    span.track,
                    if mic_short {
                        corti_core::QualityTrack::Mic
                    } else {
                        corti_core::QualityTrack::Tap
                    }
                );
                for label in ["later microphone", "later far end"] {
                    let end = publisher
                        .0
                        .borrow()
                        .iter()
                        .flat_map(|(_, words)| words)
                        .find(|word| word.text == label)
                        .map(|word| word.end);
                    assert_eq!(end, Some((frames * 3) as f64 / f64::from(sample_rate)));
                }
            }
        }
    }

    #[test]
    fn single_track_layouts_do_not_create_mismatches() {
        for aec_enabled in [false, true] {
            for mic_only in [false, true] {
                let epoch = fixture_epoch(8_000, mic_only, !mic_only);
                let mut capture = corti_core::QualityRecord::observed();
                capture.insert_epoch(epoch).unwrap();
                let audio: Vec<_> = (0..4_133)
                    .map(|frame| (frame as f32 * 0.013).sin() * 0.2)
                    .collect();
                let (tx, rx) = sync_channel(2);
                for (index, samples) in audio.chunks(4_096).enumerate() {
                    tx.send(CaptureChunk {
                        epoch,
                        retained_start_frame: (index * 4_096) as u64,
                        mic: if mic_only {
                            samples.to_vec()
                        } else {
                            Vec::new()
                        },
                        tap: if mic_only {
                            Vec::new()
                        } else {
                            samples.to_vec()
                        },
                        unavailable: Vec::new(),
                    })
                    .unwrap();
                }
                drop(tx);
                let mut mic = Scripted::new(Vec::new(), Vec::new());
                let mut tap = Scripted::new(Vec::new(), Vec::new());
                let actual_mic = mic.audio.clone();
                let actual_tap = tap.audio.clone();
                let mut aec =
                    aec_enabled.then(|| new_live_aec(8_000, corti_aec::AecConfig::default()));
                let mut window =
                    TranscriptWindow::new(8_000, 1, false, CleanupConfig::default()).unwrap();
                let mut writer =
                    NoteWriter::new(TempFiler::new("single-track-layout"), meta(), None);
                consume_chunks(
                    &rx,
                    8_000,
                    &mut aec,
                    &mut mic,
                    &mut tap,
                    &NoDiarizer,
                    &mut window,
                    &mut writer,
                    &NoopPublisher,
                )
                .unwrap();
                assert_eq!(writer.mismatches.count(), 0);
                let outcome = finish_session(
                    8_000,
                    aec,
                    mic,
                    tap,
                    NoDiarizer,
                    window,
                    FinishQuality {
                        dropped_chunks: 0,
                        capture: Some(capture),
                    },
                    &mut writer,
                    &NoopPublisher,
                )
                .unwrap();
                assert!(matches!(
                    outcome,
                    LiveOutcome::NoNote {
                        settled_quality: None
                    }
                ));
                assert_eq!(
                    actual_mic.borrow().len(),
                    if mic_only { audio.len() } else { 0 }
                );
                assert_eq!(
                    actual_tap.borrow().len(),
                    if mic_only { 0 } else { audio.len() }
                );
                if mic_only {
                    let expected = if aec_enabled {
                        let mut reference = new_live_aec(8_000, corti_aec::AecConfig::default());
                        let mut filtered = Vec::new();
                        for samples in audio.chunks(4_096) {
                            filtered.extend(reference.push(samples, &vec![0.0; samples.len()]));
                        }
                        filtered.extend(reference.finish());
                        filtered
                    } else {
                        audio
                    };
                    assert_eq!(*actual_mic.borrow(), expected);
                } else {
                    assert_eq!(*actual_tap.borrow(), audio);
                }
            }
        }
    }

    #[test]
    fn mismatch_uses_writer_position_after_a_tee_drop_and_unfilled_jump() {
        for unfilled_jump in [false, true] {
            let initial = fixture_epoch(8_000, true, true);
            let later = if unfilled_jump {
                corti_core::CaptureEpoch {
                    id: 1,
                    source_start_ns: Some(12_000_000_000),
                    source_frame_base: 96_000,
                    retained_start_frame: 8_192,
                    reason: corti_core::CaptureEpochReason::HostDiscontinuity,
                    ..initial
                }
            } else {
                initial
            };
            let mut capture = corti_core::QualityRecord::observed();
            capture.insert_epoch(initial).unwrap();
            if unfilled_jump {
                capture.insert_epoch(later).unwrap();
            }
            capture
                .insert_span(corti_core::QualitySpan {
                    epoch: 0,
                    start_frame: 4_096,
                    end_frame: 8_192,
                    track: corti_core::QualityTrack::Both,
                    cause: corti_core::QualityCause::TeeDrop,
                    class: corti_core::QualityClass::ProcessingLoss,
                    stage: corti_core::QualityStage::Live,
                    resolution: corti_core::QualityResolution::Outstanding,
                })
                .unwrap();
            let (tx, rx) = sync_channel(3);
            // Retained 4096..8192 was dropped by the tee. The consumer has only 4096 prior frames.
            for (epoch, start, mic_len) in [
                (initial, 0, 4_096),
                (later, 8_192, 4_059),
                (later, 12_288, 4_096),
            ] {
                tx.send(CaptureChunk {
                    epoch,
                    retained_start_frame: start,
                    mic: vec![0.2; mic_len],
                    tap: vec![0.1; 4_096],
                    unavailable: Vec::new(),
                })
                .unwrap();
            }
            drop(tx);
            let mut mic = Scripted::new(Vec::new(), Vec::new());
            let mut tap = Scripted::new(Vec::new(), Vec::new());
            let mut window =
                TranscriptWindow::new(8_000, 1, false, CleanupConfig::default()).unwrap();
            window.frame_limit = 5_003;
            let mut writer = NoteWriter::new(TempFiler::new("mismatch-position"), meta(), None);
            consume_chunks(
                &rx,
                8_000,
                &mut None,
                &mut mic,
                &mut tap,
                &NoDiarizer,
                &mut window,
                &mut writer,
                &NoopPublisher,
            )
            .unwrap();
            assert_eq!(writer.mismatches.count(), 1);
            let outcome = finish_session(
                8_000,
                None,
                mic,
                tap,
                NoDiarizer,
                window,
                FinishQuality {
                    dropped_chunks: 1,
                    capture: Some(capture.clone()),
                },
                &mut writer,
                &NoopPublisher,
            )
            .unwrap();
            let LiveOutcome::Fallback {
                settled_quality: Some(quality),
                ..
            } = outcome
            else {
                panic!("mismatch lost at fallback")
            };
            let span = quality
                .spans
                .iter()
                .find(|span| span.cause == corti_core::QualityCause::MicTapMismatch)
                .unwrap();
            let source_start = if unfilled_jump {
                96_000 + 4_059
            } else {
                8_192 + 4_059
            };
            assert_eq!(
                (span.epoch, span.start_frame, span.end_frame),
                (later.id, source_start, source_start + 37)
            );
            assert!(
                quality.spans.contains(&capture.spans[0]),
                "tee loss must survive settlement"
            );
            assert_eq!(capture.spans.len(), 1, "capture is immutable");
        }
    }

    #[test]
    fn mismatch_warnings_report_the_leading_edge_and_each_hundred() {
        use tracing_subscriber::layer::SubscriberExt;
        struct Warnings(Arc<Mutex<Vec<u64>>>);
        impl tracing_subscriber::Layer<tracing_subscriber::Registry> for Warnings {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _: tracing_subscriber::layer::Context<'_, tracing_subscriber::Registry>,
            ) {
                struct Counts<'a>(&'a mut Vec<u64>);
                impl tracing::field::Visit for Counts<'_> {
                    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                        if field.name() == "mismatched_chunks" {
                            self.0.push(value);
                        }
                    }
                    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {
                    }
                }
                event.record(&mut Counts(&mut self.0.lock().unwrap()));
            }
        }
        let epoch = fixture_epoch(8_000, true, true);
        let mut mismatch = LiveMismatches::default();
        let warnings = Arc::new(Mutex::new(Vec::new()));
        tracing::subscriber::with_default(
            tracing_subscriber::registry().with(Warnings(warnings.clone())),
            || {
                for index in 0..300 {
                    observe_mismatch(
                        &mut mismatch,
                        &CaptureChunk {
                            epoch,
                            retained_start_frame: index * 4,
                            mic: vec![0.1; 2],
                            tap: vec![0.2; 4],
                            unavailable: Vec::new(),
                        },
                        8_000,
                    );
                }
            },
        );
        assert_eq!(*warnings.lock().unwrap(), [1, 100, 200, 300]);
        assert_eq!(mismatch.count(), 300);
    }

    #[test]
    fn late_track_origins_and_absent_epochs_preserve_word_and_filter_clocks() {
        for aec_enabled in [false, true] {
            for layout in [
                [(true, true), (false, true), (true, true)],
                [(false, true), (false, true), (true, true)],
                [(true, false), (true, false), (true, true)],
                [(true, true), (true, false), (true, true)],
            ] {
                let frames = 1_024;
                let rate = 8_000;
                let mut capture = corti_core::QualityRecord::observed();
                let mut raw_mic = Vec::new();
                let mut raw_tap = Vec::new();
                let mut unavailable = Vec::new();
                let (tx, rx) = sync_channel(3);
                for (index, &(mic, tap)) in layout.iter().enumerate() {
                    let start = index * frames;
                    let epoch = corti_core::CaptureEpoch {
                        id: index as u32,
                        source_frame_base: start as u64,
                        retained_start_frame: start as u64,
                        source_start_ns: Some(start as u64 * 1_000_000_000 / u64::from(rate)),
                        reason: if index == 0 {
                            corti_core::CaptureEpochReason::Start
                        } else {
                            corti_core::CaptureEpochReason::LayoutChange
                        },
                        ..fixture_epoch(rate, mic, tap)
                    };
                    capture.insert_epoch(epoch).unwrap();
                    let m: Vec<_> = (0..frames)
                        .map(|j| {
                            if mic {
                                ((start + j) as f32 * 0.031).sin() * 0.2
                            } else {
                                0.0
                            }
                        })
                        .collect();
                    let t: Vec<_> = (0..frames)
                        .map(|j| {
                            if tap {
                                ((start + j) as f32 * 0.017).cos() * 0.1
                            } else {
                                0.0
                            }
                        })
                        .collect();
                    raw_mic.extend_from_slice(&m);
                    raw_tap.extend_from_slice(&t);
                    unavailable.extend(vec![!mic; frames]);
                    tx.send(CaptureChunk {
                        epoch,
                        retained_start_frame: start as u64,
                        mic: if mic { m } else { Vec::new() },
                        tap: if tap { t } else { Vec::new() },
                        unavailable: Vec::new(),
                    })
                    .unwrap();
                }
                drop(tx);
                let mic_origin = layout.iter().position(|&(mic, _)| mic).unwrap() * frames;
                let tap_origin = layout.iter().position(|&(_, tap)| tap).unwrap() * frames;
                let mut mic = Scripted::new(
                    vec![vec![word(0.0, 0.001, "microphone first sample")]],
                    Vec::new(),
                );
                let mut tap = Scripted::new(
                    vec![vec![word(0.0, 0.001, "reference first sample")]],
                    Vec::new(),
                );
                mic.clock_tail = Some("microphone final sample");
                tap.clock_tail = Some("reference final sample");
                let mic_audio = mic.audio.clone();
                let tap_audio = tap.audio.clone();
                let mut aec =
                    aec_enabled.then(|| new_live_aec(rate, corti_aec::AecConfig::default()));
                let mut window =
                    TranscriptWindow::new(rate, 1, false, CleanupConfig::default()).unwrap();
                window.frame_limit = 1_501;
                let mut writer = NoteWriter::new(TempFiler::new("layout-clocks"), meta(), None);
                let publisher = RecordingPublisher::default();
                consume_chunks(
                    &rx,
                    rate,
                    &mut aec,
                    &mut mic,
                    &mut tap,
                    &NoDiarizer,
                    &mut window,
                    &mut writer,
                    &publisher,
                )
                .unwrap();
                assert_eq!(writer.mismatches.count(), 0);
                assert_eq!(window.mic_origin_frame, Some(mic_origin as u64));
                assert_eq!(window.tap_origin_frame, Some(tap_origin as u64));
                if aec_enabled {
                    assert!(
                        window
                            .aec_blocks
                            .iter()
                            .all(|block| block.t_start_secs >= mic_origin as f64 / f64::from(rate))
                    );
                }
                finish_session(
                    rate,
                    aec,
                    mic,
                    tap,
                    NoDiarizer,
                    window,
                    FinishQuality {
                        dropped_chunks: 0,
                        capture: Some(capture),
                    },
                    &mut writer,
                    &publisher,
                )
                .unwrap();
                assert_eq!(*tap_audio.borrow(), raw_tap[tap_origin..]);
                let expected_mic = if aec_enabled {
                    let mut reference = new_live_aec(rate, corti_aec::AecConfig::default());
                    let mut filtered = Vec::new();
                    for (mic, tap) in raw_mic[mic_origin..]
                        .chunks(4_096)
                        .zip(raw_tap[mic_origin..].chunks(4_096))
                    {
                        filtered.extend(reference.push(mic, tap));
                    }
                    filtered.extend(reference.finish());
                    for (sample, missing) in filtered.iter_mut().zip(&unavailable[mic_origin..]) {
                        if *missing {
                            *sample = 0.0;
                        }
                    }
                    filtered
                } else {
                    raw_mic[mic_origin..].to_vec()
                };
                assert_eq!(*mic_audio.borrow(), expected_mic);
                for (label, origin) in [
                    ("microphone first sample", mic_origin),
                    ("reference first sample", tap_origin),
                ] {
                    let start = publisher
                        .0
                        .borrow()
                        .iter()
                        .flat_map(|(_, words)| words)
                        .find(|word| word.text == label)
                        .map(|word| word.start);
                    assert_eq!(start, Some(origin as f64 / f64::from(rate)));
                }
                for label in ["microphone final sample", "reference final sample"] {
                    let end = publisher
                        .0
                        .borrow()
                        .iter()
                        .flat_map(|(_, words)| words)
                        .find(|word| word.text == label)
                        .map(|word| word.end);
                    assert_eq!(end, Some((frames * 3) as f64 / f64::from(rate)));
                }
            }
        }
    }

    #[test]
    fn late_microphone_origin_shifts_acoustic_evidence_once() {
        let mut window = TranscriptWindow::new(8_000, 1, false, CleanupConfig::default()).unwrap();
        window.mic_origin_frame = Some(8_000);
        let block = corti_aec::BlockStats {
            t_start_secs: 0.25,
            mic_energy: 0.1,
            far_energy: 0.2,
            echo_estimate_energy: 0.1,
            error_energy: 0.01,
            double_talk: false,
            suppressed: false,
        };
        window.push_aec_blocks(vec![block]);
        assert_eq!(window.aec_blocks[0].t_start_secs, 1.25);
        window.clear_after_flush();
        window.push_aec_blocks(vec![corti_aec::BlockStats {
            t_start_secs: 0.5,
            ..block
        }]);
        assert_eq!(
            window
                .aec_blocks
                .iter()
                .map(|block| block.t_start_secs)
                .collect::<Vec<_>>(),
            [1.25, 1.5]
        );
    }

    #[test]
    fn mismatch_history_survives_final_filing_failure_later_consume_error_and_panic() {
        use corti_core::{
            QualityCause, QualityClass, QualityResolution, QualitySpan, QualityStage, QualityTrack,
        };
        for failure in ["final_filer", "consume", "panic"] {
            let epoch = fixture_epoch(8_000, true, true);
            let later_epoch = corti_core::CaptureEpoch {
                id: 1,
                source_start_ns: Some(500_000),
                source_frame_base: 4,
                retained_start_frame: 4,
                mic_channels: 2,
                reason: corti_core::CaptureEpochReason::LayoutChange,
                ..epoch
            };
            let mut capture = corti_core::QualityRecord::observed();
            capture.insert_epoch(epoch).unwrap();
            capture.insert_epoch(later_epoch).unwrap();
            let immutable_capture = capture.clone();
            let (tx, rx) = sync_channel(3);
            for index in 0..3 {
                tx.send(CaptureChunk {
                    epoch: if index == 0 { epoch } else { later_epoch },
                    retained_start_frame: index * 4,
                    mic: vec![0.1; if index == 1 { 2 } else { 4 }],
                    tap: vec![0.2; 4],
                    unavailable: if failure == "consume" && index == 2 {
                        vec![true]
                    } else {
                        Vec::new()
                    },
                })
                .unwrap();
            }
            drop(tx);
            let mut mic = Scripted::new(
                Vec::new(),
                vec![word(0.001, 0.002, "final microphone words")],
            );
            mic.panic_on_push = (failure == "panic").then_some(2);
            let mut tap = Scripted::new(Vec::new(), Vec::new());
            let mut writer = NoteWriter::new(FlakyFiler::new("mismatch-error", 1), meta(), None);
            let mut window =
                TranscriptWindow::new(8_000, 1, false, CleanupConfig::default()).unwrap();
            let consumed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                consume_chunks(
                    &rx,
                    8_000,
                    &mut None,
                    &mut mic,
                    &mut tap,
                    &NoDiarizer,
                    &mut window,
                    &mut writer,
                    &NoopPublisher,
                )
            }));
            let finish = FinishQuality {
                dropped_chunks: 0,
                capture: Some(capture.clone()),
            };
            match failure {
                "final_filer" => {
                    consumed.unwrap().unwrap();
                    assert!(
                        finish_session(
                            8_000,
                            None,
                            mic,
                            tap,
                            NoDiarizer,
                            window,
                            finish,
                            &mut writer,
                            &NoopPublisher
                        )
                        .is_err()
                    );
                }
                "consume" => {
                    assert!(consumed.unwrap().is_err());
                    // The session waits for Finish, freezes quality, then propagates consumed?.
                    writer.freeze_live_quality(&finish).unwrap();
                }
                "panic" => assert!(consumed.is_err()),
                _ => unreachable!(),
            }
            let LiveOutcome::Fallback {
                settled_quality: Some(quality),
                ..
            } = writer.fallback_outcome(failure.to_string())
            else {
                panic!("known mismatch history was lost")
            };
            let mut expected_spans = vec![QualitySpan {
                epoch: 1,
                start_frame: 6,
                end_frame: 8,
                track: QualityTrack::Mic,
                cause: QualityCause::MicTapMismatch,
                class: QualityClass::ProcessingLoss,
                stage: QualityStage::Live,
                resolution: QualityResolution::Outstanding,
            }];
            if failure == "final_filer" {
                // The failed final append also loses both delivered tracks across both epochs.
                // Consume errors and decoder panics never attempted that append in this fixture.
                for (epoch, start_frame, end_frame) in [(0, 0, 4), (1, 4, 12)] {
                    for track in [QualityTrack::Mic, QualityTrack::Tap] {
                        expected_spans.push(QualitySpan {
                            epoch,
                            start_frame,
                            end_frame,
                            track,
                            cause: QualityCause::WindowNotFiled,
                            class: QualityClass::ProcessingLoss,
                            stage: QualityStage::Live,
                            resolution: QualityResolution::Outstanding,
                        });
                    }
                }
            }
            expected_spans.sort_unstable();
            assert_eq!(
                quality,
                corti_core::QualityRecord {
                    spans: expected_spans,
                    ..immutable_capture.clone()
                },
                "exact settled facts for {failure}"
            );
            assert!(quality.requires_batch_recovery());
            assert_eq!(capture, immutable_capture);
            let mut handed_off = capture.clone();
            handed_off.merge_snapshot(&quality).unwrap();
            assert_eq!(
                handed_off, quality,
                "mandatory handoff keeps every exact source interval"
            );
            assert_eq!(capture, immutable_capture);
        }
    }

    #[test]
    fn consumed_panic_waits_for_finish_when_the_start_epoch_tee_was_dropped() {
        for verdict in ["finish", "discard", "disconnect"] {
            let first = fixture_epoch(8_000, true, true);
            let later = corti_core::CaptureEpoch {
                id: 1,
                source_start_ns: Some(500_000),
                source_frame_base: 4,
                retained_start_frame: 4,
                mic_channels: 2,
                reason: corti_core::CaptureEpochReason::LayoutChange,
                ..first
            };
            let mut capture = corti_core::QualityRecord::observed();
            capture.insert_epoch(first).unwrap();
            capture.insert_epoch(later).unwrap();
            let (verdict_tx, verdict_rx) = std::sync::mpsc::channel();
            let (parked_tx, parked_rx) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                let filer = TempFiler::new("mismatch-skipped-start-panic");
                let note = filer.note();
                let mut writer = NoteWriter::new(filer, meta(), None);
                writer
                    .append_segment(&TranscriptSegment {
                        speaker: Speaker::Me,
                        start: 0.0,
                        end: 0.0005,
                        text: "owned prefix".into(),
                    })
                    .unwrap();
                let (tx, rx) = sync_channel(1);
                tx.send(CaptureChunk {
                    epoch: later,
                    retained_start_frame: 4,
                    mic: vec![0.1; 2],
                    tap: vec![0.2; 4],
                    unavailable: Vec::new(),
                })
                .unwrap();
                drop(tx);
                let mut mic = Scripted::new(Vec::new(), Vec::new());
                mic.panic_on_push = Some(1);
                let mut tap = Scripted::new(Vec::new(), Vec::new());
                let mut window =
                    TranscriptWindow::new(8_000, 1, false, CleanupConfig::default()).unwrap();
                let consumed = consume_with_panic_boundary(|| {
                    consume_chunks(
                        &rx,
                        8_000,
                        &mut None,
                        &mut mic,
                        &mut tap,
                        &NoDiarizer,
                        &mut window,
                        &mut writer,
                        &NoopPublisher,
                    )
                });
                assert!(consumed.is_err());
                parked_tx.send(()).unwrap();
                let outcome = match await_capture_verdict(&verdict_rx, consumed, &mut writer) {
                    Ok(None) => LiveOutcome::NoNote {
                        settled_quality: None,
                    },
                    Ok(Some(_)) => panic!("a panicked decoder must never resume finalization"),
                    Err(error) => writer.fallback_outcome(format!("{error:#}")),
                };
                (outcome, note)
            });
            parked_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            assert!(
                !worker.is_finished(),
                "consumption failure must await recording ownership"
            );
            match verdict {
                "finish" => verdict_tx
                    .send(Verdict::Finish(FinishQuality {
                        dropped_chunks: 1,
                        capture: Some(capture.clone()),
                    }))
                    .unwrap(),
                "discard" => verdict_tx.send(Verdict::Discard).unwrap(),
                "disconnect" => {}
                _ => unreachable!(),
            }
            drop(verdict_tx);
            let (outcome, note) = worker.join().unwrap();
            if verdict == "discard" {
                assert!(matches!(
                    outcome,
                    LiveOutcome::NoNote {
                        settled_quality: None
                    }
                ));
                assert!(
                    !note.exists(),
                    "Discard retains ownership after a consume panic"
                );
                continue;
            }
            let LiveOutcome::Fallback {
                settled_quality: Some(quality),
                note_path: Some(owned),
                ..
            } = outcome
            else {
                panic!("panic fallback must retain note and known mismatch facts");
            };
            assert_eq!(owned, note);
            assert!(note.exists());
            if verdict == "finish" {
                assert_eq!(quality.spans.len(), 1);
                assert_eq!(
                    (
                        quality.spans[0].epoch,
                        quality.spans[0].start_frame,
                        quality.spans[0].end_frame
                    ),
                    (1, 6, 8)
                );
                capture.merge_snapshot(&quality).unwrap();
                assert_eq!(capture.spans, quality.spans);
            } else {
                assert!(
                    quality.spans.is_empty(),
                    "no invented Start when authority never arrives"
                );
                assert_eq!(quality.overflow.outstanding_omitted_spans[1], 1);
            }
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
            epoch: fixture_epoch(1, false, true),
            retained_start_frame: 0,
            unavailable: Vec::new(),
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
                epoch: fixture_epoch(1, false, true),
                retained_start_frame: 0,
                unavailable: Vec::new(),
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
            FinishQuality {
                dropped_chunks: 0,
                capture: Some(corti_core::QualityRecord::observed()),
            },
            &mut writer,
            &NoopPublisher,
        )
        .unwrap();
        let LiveOutcome::Filed { note_path, .. } = outcome else {
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

    /// Without acoustic evidence, duplicates within and across windows survive. Raw critical
    /// protection belongs to the original poll, so its backchannel child also survives. Unprotected
    /// fragments still merge before the append.
    ///
    /// Synthetic script — this repo is public, so the shapes are reproduced with invented words.
    #[test]
    fn live_windows_keep_protected_poll_children_and_merge_unprotected_fragments() {
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
                    // One returned poll: the later phrase's critical difference protects both children.
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
                epoch: fixture_epoch(1, true, true),
                retained_start_frame: 0,
                unavailable: Vec::new(),
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
                 **[00:04] Me:** Yeah.\n\n\
                 **[00:08] Me:** Rotate the widget calibration.\n\n\
                 **[00:40] Me:** I will send the summary after this call.\n\n\
                 **[00:57] Them:** The gateway timeout hits the queue depth.\n\n\
                 **[01:02] Me:** Gateway timeout queue depth.\n\n\
                 **[01:10] Me:** Sending the notes now.\n\n"
            ),
            "unexpected note body:\n{content}"
        );
        assert!(
            content.contains("**[00:04] Me:** Yeah."),
            "a protected original-poll child was deleted as a backchannel"
        );
        assert!(
            content.contains("Me:** Rotate the widget calibration."),
            "in-window duplicate was deleted without evidence"
        );
        assert!(
            content.contains("Me:** Gateway timeout queue depth."),
            "cross-window duplicate was deleted without evidence"
        );
    }

    /// A separate ordinary acknowledgement has no matching raw poll and still follows the default
    /// backchannel rule. This control must not inherit protection from an unrelated mic phrase.
    #[test]
    fn live_unprotected_backchannel_is_removed_from_its_own_poll() {
        let filer = TempFiler::new("ordinary-backchannel-poll");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);
        let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
        publish_them_words(
            &mut window,
            &NoopPublisher,
            vec![word(
                2.0,
                6.0,
                "We should rotate the widget calibration before Friday.",
            )],
        );
        publish_mic_words(&mut window, &NoopPublisher, vec![word(4.0, 4.3, "Yeah.")]);
        release_held_mic_words(&mut window, &NoopPublisher);
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
        let content = read(&note);
        assert!(content.ends_with(
            "**[00:02] Them:** We should rotate the widget calibration before Friday.\n\n"
        ));
        assert!(
            !content.contains("Me:**"),
            "unexpected note body:\n{content}"
        );
    }

    /// A short mic region decoded while the far end is still mid-region is withheld, then kept when the
    /// source closes: text similarity without acoustic evidence cannot prove that it was an echo.
    ///
    /// Synthetic script — this repo is public, so the shape is reproduced with invented words.
    #[test]
    fn a_short_text_duplicate_is_published_without_acoustic_evidence() {
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
                epoch: fixture_epoch(1, true, true),
                retained_start_frame: 0,
                unavailable: Vec::new(),
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
                released_published: 1,
                released_dropped: 0,
            }
        );
        assert_eq!(
            publisher
                .0
                .borrow()
                .iter()
                .map(|(speaker, _)| speaker.as_str())
                .collect::<Vec<_>>(),
            vec!["Them", "Me"],
            "the duplicate was lost without acoustic evidence"
        );

        finish_session(
            1,
            None,
            mic,
            them,
            NoDiarizer,
            window,
            FinishQuality {
                dropped_chunks: 0,
                capture: Some(corti_core::QualityRecord::observed()),
            },
            &mut writer,
            &publisher,
        )
        .unwrap();
        let content = read(&note);
        assert!(
            content.contains("Me:** Settlement gateway."),
            "unexpected note body:\n{content}"
        );
    }

    /// Synthetic corrections and intentional repeats must reach the reader with their original timing,
    /// regardless of channel order or which decoder closes its region first. Missing evidence and the
    /// existing double-talk guard both forbid echo deletion.
    #[test]
    fn live_early_echo_keeps_critical_words_and_repeats_in_both_channel_orders() {
        let cases = [
            ("Deploy the service.", "Do not deploy the service."),
            ("The budget is 1500 euros.", "The budget is 15000 euros."),
            ("Publica la versión.", "No publiques la versión."),
            ("Restart the gateway.", "Restart the gateway."),
            ("The answer is yes.", "No."),
            ("The count is 50.", "15."),
            ("Check the satellite.", "The kettle is ready."),
        ];
        for (left, right) in cases {
            for (far, near) in [(left, right), (right, left)] {
                for double_talk in [false, true] {
                    for far_first in [false, true] {
                        let mut window =
                            TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
                        if double_talk {
                            window.push_aec_blocks(vec![block(1.0, 1.0, true)]);
                        }
                        let publisher = RecordingPublisher::default();
                        let original = vec![word(1.0, 1.5, near)];
                        let source = vec![word(0.0, 3.0, far)];
                        if far_first {
                            publish_them_words(&mut window, &publisher, source);
                            publish_mic_words(&mut window, &publisher, original.clone());
                        } else {
                            publish_mic_words(&mut window, &publisher, original.clone());
                            publish_them_words(&mut window, &publisher, source);
                        }
                        release_held_mic_words(&mut window, &publisher);
                        let published = publisher.0.borrow();
                        let mic = published.iter().find(|(speaker, _)| speaker == "Me");
                        assert_eq!(mic.map(|(_, words)| words), Some(&original));
                        assert_eq!(window.early_drop.stats.released_dropped, 0);
                        assert_eq!(
                            published
                                .iter()
                                .filter(|(speaker, _)| speaker == "Them")
                                .count(),
                            1
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn live_early_echo_protects_raw_corrections_even_with_quiet_mic_evidence() {
        for (left, right) in [
            ("Deploy the service.", "Do not deploy the service."),
            ("The budget is 1500 euros.", "The budget is 15000 euros."),
            ("Publica el servicio.", "No publica el servicio."),
            (
                "Nora owns the production gateway today.",
                "Omar owns the production gateway today.",
            ),
            ("Use version 1.5 now.", "Use version 1.6 now."),
        ] {
            for (far, near) in [(left, right), (right, left)] {
                let mut window =
                    TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
                window.push_aec_blocks(vec![block(1.0, 1.0, false)]);
                let publisher = RecordingPublisher::default();
                let original = vec![word(1.0, 1.5, near)];
                publish_mic_words(&mut window, &publisher, original.clone());
                publish_them_words(&mut window, &publisher, vec![word(0.0, 3.0, far)]);
                let published = publisher.0.borrow();
                assert_eq!(
                    published
                        .iter()
                        .find(|(speaker, _)| speaker == "Me")
                        .map(|(_, words)| words),
                    Some(&original)
                );
                assert_eq!(window.early_drop.stats.released_dropped, 0);
            }
        }
    }

    /// Suppression changes publication, not the caller's access to the original words or the reason.
    #[test]
    fn live_early_echo_returns_suppressed_words_and_acoustic_source_decision() {
        let mut early = EarlyDrop::new(CleanupConfig::default());
        let original = vec![word(1.0, 1.5, "Restart the gateway.")];
        let quiet = |_, _| {
            Some(SpanEvidence {
                mic_db: -20.0,
                echo_estimate_db: -20.0,
                double_talk_fraction: 0.0,
                blocks: 1,
            })
        };
        assert!(
            early
                .offer_mic(original.clone(), 1.5, Some(&quiet))
                .is_empty()
        );
        let released =
            early.offer_them(&[word(0.0, 3.0, "Restart the gateway.")], 3.0, Some(&quiet));
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].words, original);
        assert_eq!(released[0].assessment.verdict, EchoVerdict::DropAcoustic);
        assert_eq!(
            released[0]
                .assessment
                .source
                .as_ref()
                .map(|source| source.start),
            Some(0.0)
        );
        assert_eq!(released[0].evidence, quiet(1.0, 1.5));
        assert_eq!(early.stats.released_dropped, 1);

        let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
        let publisher = RecordingPublisher::default();
        emit_mic_words(&mut window, &publisher, released);
        assert!(publisher.0.borrow().is_empty());
        assert!(window.mic_regions.is_empty());
        assert_eq!(window.cleanup_decisions.entries().len(), 1);
        let decision = &window.cleanup_decisions.entries()[0];
        assert_eq!(decision.originals[0].text, original[0].text);
        assert_eq!(decision.originals[0].start, original[0].start);
        assert_eq!(decision.originals[0].end, original[0].end);
        assert!(decision.replacements.is_empty());
        assert_eq!(decision.stage, CleanupStage::EarlyEcho);
    }

    /// A short mic region that is *not* an echo is published late — after the far-end region whose closing
    /// released it — carrying its original words and timestamps. The reader orders rows by time, so a late
    /// row still lands in its place.
    #[test]
    fn a_short_genuine_answer_is_published_late_with_its_original_timestamps() {
        for (prompt, reply) in [
            ("which region is the replica in?", "Frankfurt."),
            ("Deploy the service.", "Do not deploy the service."),
            ("The budget is 1500 euros.", "The budget is 15000 euros."),
            ("Publica el servicio.", "No publica el servicio."),
            ("Restart the gateway.", "Restart the gateway."),
        ] {
            let filer = TempFiler::new("early-drop-answer");
            let note = filer.note();
            let mut writer = NoteWriter::new(filer, meta(), None);

            let mut them = Scripted::new(
                vec![
                    vec![word(0.0, 1.0, prompt)],
                    Vec::new(),
                    Vec::new(),
                    vec![word(4.0, 4.6, "Got it.")],
                ],
                Vec::new(),
            );
            let answer = word(1.4, 1.9, reply);
            let mut mic = Scripted::new(vec![Vec::new(), vec![answer.clone()]], Vec::new());

            let (tx, rx) = sync_channel::<CaptureChunk>(8);
            for _ in 0..5 {
                tx.send(CaptureChunk {
                    epoch: fixture_epoch(1, true, true),
                    retained_start_frame: 0,
                    unavailable: Vec::new(),
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
                vec![("Them", prompt), ("Them", "Got it."), ("Me", reply),],
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
                FinishQuality {
                    dropped_chunks: 0,
                    capture: Some(corti_core::QualityRecord::observed()),
                },
                &mut writer,
                &publisher,
            )
            .unwrap();
            let content = read(&note);
            assert!(
                content.contains(&format!("Me:** {reply}")),
                "unexpected note body:\n{content}"
            );
        }
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
                epoch: fixture_epoch(1, true, true),
                retained_start_frame: 0,
                unavailable: Vec::new(),
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
            FinishQuality {
                dropped_chunks: 0,
                capture: Some(corti_core::QualityRecord::observed()),
            },
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
                .offer_mic(vec![word(start, start + 0.05, "ping")], start, None)
                .len();
        }
        assert_eq!(early.held.len(), MAX_HELD_MIC_REGIONS);
        assert_eq!(published, 20, "overflow lost the regions it evicted");
        published += early.release_all(None).len();
        assert!(early.is_idle());
        assert_eq!(published, MAX_HELD_MIC_REGIONS + 20);
        assert_eq!(early.stats.released_dropped, 0, "there was nothing to echo");

        // Far-end regions are normally pruned by time; the hard cap catches a pathological burst inside one
        // instant, which no horizon can trim.
        let mut early = EarlyDrop::new(CleanupConfig::default());
        for _ in 0..(MAX_LIVE_ECHO_SOURCES + 10) {
            early.offer_them(&[word(0.0, 0.5, "chatter")], 0.0, None);
        }
        assert_eq!(early.sources.segments.len(), MAX_LIVE_ECHO_SOURCES);
        assert_eq!(early.sources.omitted, 10);
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
        early.offer_them(
            &[word(0.0, 3.0, "the settlement gateway rollout")],
            3.0,
            None,
        );
        let released = early.offer_mic(ghost.clone(), 3.5, None);
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].words, ghost);
        assert_eq!(released[0].assessment.verdict, EchoVerdict::Keep);
        assert!(early.is_idle());
        assert_eq!(early.stats, EarlyDropStats::default());
    }

    /// A hold never survives a durable append: the note is rendered from the window's buffered words, so
    /// `checkpoint_and_flush` releases every held region before it flushes.
    #[test]
    fn a_hold_is_released_before_the_durable_append() {
        for reply in [
            "Frankfurt.",
            "Do not deploy the service.",
            "The budget is 15000 euros.",
            "No publica el servicio.",
            "Restart the gateway.",
        ] {
            let filer = TempFiler::new("early-drop-boundary");
            let note = filer.note();
            let mut writer = NoteWriter::new(filer, meta(), None);

            let mut them = Scripted::new(Vec::new(), Vec::new());
            // Decoded at 58 s, so the hold would not expire on its own until 64.5 s — past this window's end.
            let mut mic =
                Scripted::new(vec![Vec::new(), vec![word(58.0, 58.5, reply)]], Vec::new());

            let (tx, rx) = sync_channel::<CaptureChunk>(4);
            for _ in 0..2 {
                tx.send(CaptureChunk {
                    epoch: fixture_epoch(1, true, true),
                    retained_start_frame: 0,
                    unavailable: Vec::new(),
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
                content.contains(&format!("**[00:58] Me:** {reply}")),
                "the held region missed its window:\n{content}"
            );
        }
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

        window.push_them_words(vec![word(1.0, 2.0, "An early turn nobody echoes.")]);
        window.push_them_words(vec![word(
            57.0,
            59.0,
            "The gateway timeout hits the queue depth.",
        )]);
        window.frames = 60;
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();

        // 60 s window, 6 s echo window ⇒ only segments ending at or after 54 s are carried.
        assert_eq!(window.carry.segments.len(), 1);
        assert_eq!(window.carry.segments[0].segment.start, 57.0);

        // The carried segment is an echo source, not content: it is not re-appended by the next flush.
        window.push_mic_words(vec![word(62.0, 63.0, "Gateway timeout queue depth.")]);
        window.frames = 60;
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
        assert!(
            window.carry.segments.is_empty(),
            "nothing survived the second window"
        );
    }

    #[test]
    fn raw_carry_protects_a_correction_after_the_lexicon_rewrites_its_source() {
        let filer = TempFiler::new("raw-carry-lexicon");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);
        let document = corti_lexicon::LexiconDocument::from_rules(
            1,
            vec![
                corti_lexicon::LexiconRule::new(
                    "rust",
                    "REST",
                    corti_lexicon::RuleSource::Manual,
                    "2026-01-01T00:00:00Z",
                )
                .unwrap()
                .case_sensitive(),
            ],
        )
        .unwrap();
        let lexicon = corti_lexicon::CompiledLexicon::compile(&document).unwrap();
        let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default())
            .unwrap()
            .with_lexicon(Some(std::sync::Arc::new(lexicon)));
        let publisher = RecordingPublisher::default();
        publish_them_words(
            &mut window,
            &publisher,
            vec![word(57.0, 59.0, "We deploy the rust gateway today.")],
        );
        window.frames = 60;
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
        assert!(read(&note).contains("Them:** We deploy the REST gateway today."));

        window.frames = 5;
        window.push_aec_blocks(vec![block(61.0, 1.0, false)]);
        let original = vec![word(61.0, 62.0, "We deploy the REST gateway today.")];
        publish_mic_words(&mut window, &publisher, original.clone());
        publish_them_words(
            &mut window,
            &publisher,
            vec![word(60.0, 65.0, "The unrelated weather report continues.")],
        );
        assert_eq!(
            publisher.0.borrow().last(),
            Some(&("Me".to_string(), original))
        );
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();

        assert!(read(&note).contains("**[01:01] Me:** We deploy the REST gateway today."));
        let protected = window
            .cleanup_decisions
            .entries()
            .iter()
            .filter(|decision| {
                decision.reason
                    == corti_transcribe::segment::CleanupReason::Echo(
                        EchoVerdict::KeepCriticalDifference,
                    )
                    && decision.originals[0].speaker == Speaker::Me
                    && decision.originals[0].start == 61.0
            })
            .collect::<Vec<_>>();
        assert_eq!(protected.len(), 2, "early and flush verdicts must agree");
        for decision in protected {
            assert_eq!(decision.originals[0].end, 62.0);
            assert_eq!(decision.source.unwrap().start, 57.0);
            assert_eq!(decision.source.unwrap().end, 59.0);
            assert!(!decision.source.unwrap().me);
        }
    }

    #[test]
    fn raw_source_protects_a_negation_after_diarization_splits_the_source() {
        let filer = TempFiler::new("raw-source-diarization");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);
        let diarizer = ScriptedDiarizer {
            turns: vec![
                SpeakerTurn {
                    start: 0.0,
                    end: 2.0,
                    label: "Them 1".into(),
                },
                SpeakerTurn {
                    start: 2.0,
                    end: 5.0,
                    label: "Them 2".into(),
                },
            ],
            calls: RefCell::new(Vec::new()),
        };
        let mut window = TranscriptWindow::new(1, 1, true, CleanupConfig::default()).unwrap();
        let publisher = RecordingPublisher::default();
        window.push_audio(&[0.0; 5], 5);
        window.push_aec_blocks(vec![block(3.0, 1.0, false)]);
        let original = vec![word(
            3.0,
            4.0,
            "We should not deploy the payment gateway today.",
        )];
        publish_mic_words(&mut window, &publisher, original.clone());
        publish_them_words(
            &mut window,
            &publisher,
            vec![
                word(0.0, 2.0, "We should deploy"),
                word(2.0, 5.0, "the payment gateway today."),
            ],
        );
        assert_eq!(
            publisher.0.borrow().last(),
            Some(&("Me".to_string(), original))
        );
        flush_window(&diarizer, &mut window, &mut writer).unwrap();

        let content = read(&note);
        assert!(content.contains("Them 1:** We should deploy"));
        assert!(content.contains("Them 2:** the payment gateway today."));
        assert!(
            content.contains("**[00:03] Me:** We should not deploy the payment gateway today.")
        );
        let protected = window
            .cleanup_decisions
            .entries()
            .iter()
            .filter(|decision| {
                decision.reason
                    == corti_transcribe::segment::CleanupReason::Echo(
                        EchoVerdict::KeepCriticalDifference,
                    )
                    && decision.originals[0].speaker == Speaker::Me
                    && decision.originals[0].start == 3.0
            })
            .collect::<Vec<_>>();
        assert_eq!(protected.len(), 2, "early and flush verdicts must agree");
        for decision in protected {
            assert_eq!(decision.originals[0].end, 4.0);
            assert_eq!(decision.source.unwrap().start, 0.0);
            assert_eq!(decision.source.unwrap().end, 5.0);
            assert!(!decision.source.unwrap().me);
        }
    }

    #[test]
    fn raw_region_boundaries_preserve_an_early_protection_at_flush() {
        let filer = TempFiler::new("raw-region-boundaries");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);
        let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
        let publisher = RecordingPublisher::default();
        window.frames = 8;
        window.push_aec_blocks(vec![block(3.0, 1.0, false), block(4.0, 1.0, false)]);
        publish_mic_words(
            &mut window,
            &publisher,
            vec![word(3.0, 4.0, "Do not deploy the payment gateway today.")],
        );
        publish_them_words(
            &mut window,
            &publisher,
            vec![word(0.0, 5.0, "Deploy the payment gateway today.")],
        );
        // This separate VAD region is close enough for a whole-window words_to_segments call to
        // absorb it. Its unrelated vocabulary must not weaken the preceding region's raw match.
        publish_mic_words(
            &mut window,
            &publisher,
            vec![word(
                4.5,
                8.0,
                "A separate unrelated sentence about weather and traffic.",
            )],
        );
        release_held_mic_words(&mut window, &publisher);
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();

        assert!(read(&note).contains("Do not deploy the payment gateway today."));
        assert!(window.cleanup_decisions.entries().iter().any(|decision| {
            decision.stage == CleanupStage::Echo
                && decision.originals[0].start == 3.0
                && decision.originals[0].end == 4.0
                && decision
                    .source
                    .is_some_and(|source| !source.me && source.start == 0.0 && source.end == 5.0)
                && decision.reason
                    == corti_transcribe::segment::CleanupReason::Echo(
                        EchoVerdict::KeepCriticalDifference,
                    )
        }));
    }

    #[test]
    fn raw_source_pause_does_not_split_critical_word_protection() {
        let filer = TempFiler::new("raw-source-pause");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);
        let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
        let publisher = RecordingPublisher::default();
        window.frames = 5;
        window.push_aec_blocks(vec![block(3.0, 1.0, false)]);
        let original = vec![word(
            3.0,
            4.0,
            "We should not deploy the payment gateway today.",
        )];
        publish_mic_words(&mut window, &publisher, original.clone());
        publish_them_words(
            &mut window,
            &publisher,
            vec![
                word(0.0, 1.0, "We should deploy"),
                word(2.75, 5.0, "the payment gateway today."),
            ],
        );
        assert_eq!(
            publisher.0.borrow().last(),
            Some(&("Me".to_string(), original))
        );
        assert_eq!(window.carry.segments.len(), 1);
        assert_eq!(window.carry.segments[0].segment.start, 0.0);
        assert_eq!(window.carry.segments[0].segment.end, 5.0);
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
        assert!(
            read(&note).contains("**[00:03] Me:** We should not deploy the payment gateway today.")
        );
    }

    #[test]
    fn raw_source_pause_never_supplies_acoustic_overlap_to_early_or_flush_cleanup() {
        for early in [false, true] {
            let filer = TempFiler::new("raw-source-pause-overlap");
            let note = filer.note();
            let mut writer = NoteWriter::new(filer, meta(), None);
            let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
            let publisher = RecordingPublisher::default();
            window.frames = 5;
            window.push_aec_blocks(vec![block(1.5, 1.0, false)]);
            let original = vec![word(1.5, 2.0, "Gateway harness.")];
            let far = vec![
                word(0.0, 1.0, "An introductory statement."),
                word(3.0, 5.0, "The unrelated weather report."),
            ];
            if early {
                publish_mic_words(&mut window, &publisher, original.clone());
                publish_them_words(&mut window, &publisher, far);
                assert_eq!(
                    publisher.0.borrow().last(),
                    Some(&("Me".to_string(), original))
                );
            } else {
                window.push_mic_words(original);
                window.push_them_words(far);
            }
            flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
            assert!(read(&note).contains("Me:** Gateway harness."));
            assert!(!window.cleanup_decisions.entries().iter().any(|decision| {
                decision.reason
                    == corti_transcribe::segment::CleanupReason::Echo(EchoVerdict::DropAcoustic)
            }));
        }
    }

    #[test]
    fn raw_source_speech_overlap_authorizes_echo_deletion_with_actual_interval_attribution() {
        for early in [false, true] {
            let filer = TempFiler::new("raw-source-actual-overlap");
            let note = filer.note();
            let mut writer = NoteWriter::new(filer, meta(), None);
            let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
            let publisher = RecordingPublisher::default();
            window.frames = 5;
            window.push_aec_blocks(vec![block(3.5, 1.0, false)]);
            let original = vec![word(3.5, 4.0, "Gateway harness.")];
            let far = vec![
                word(0.0, 1.0, "An introductory statement."),
                word(3.0, 5.0, "The unrelated weather report."),
            ];
            if early {
                publish_mic_words(&mut window, &publisher, original);
                publish_them_words(&mut window, &publisher, far);
                assert_eq!(window.early_drop.stats.released_dropped, 1);
            } else {
                window.push_mic_words(original);
                window.push_them_words(far);
            }
            flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
            assert!(!read(&note).contains("Gateway harness."));
            let decision = window
                .cleanup_decisions
                .entries()
                .iter()
                .find(|decision| {
                    decision.reason
                        == corti_transcribe::segment::CleanupReason::Echo(EchoVerdict::DropAcoustic)
                })
                .unwrap();
            assert_eq!(decision.source.unwrap().start, 3.0);
            assert_eq!(decision.source.unwrap().end, 5.0);
            assert!(!decision.source.unwrap().me);
            assert_eq!(decision.evidence.unwrap().blocks, 1);
        }
    }

    #[test]
    fn raw_source_interval_metadata_counts_toward_the_byte_cap() {
        let mut sources = RawEchoSources::default();
        let count = MAX_BUFFERED_TRANSCRIPT_BYTES / (std::mem::size_of::<(f64, f64)>() + 2) + 1;
        let words = (0..count)
            .map(|index| {
                let start = index as f64 * 3.0;
                word(start, start + 1.0, "x")
            })
            .collect::<Vec<_>>();
        sources.remember(&words, 6.0);
        assert!(sources.segments.is_empty());
        assert_eq!(sources.bytes, 0);
        assert_eq!(sources.omitted, 1);
        assert!(sources.incomplete_until >= words.last().unwrap().end);
    }

    #[test]
    fn raw_source_count_overflow_keeps_speech_until_the_missing_source_expires() {
        let filer = TempFiler::new("raw-source-count-cap");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);
        let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
        let publisher = RecordingPublisher::default();
        publish_them_words(
            &mut window,
            &publisher,
            vec![word(0.0, 5.0, "Deploy the payment gateway today.")],
        );
        for _ in 0..MAX_LIVE_ECHO_SOURCES {
            publish_them_words(
                &mut window,
                &publisher,
                vec![word(1.0, 5.0, "An unrelated weather report.")],
            );
        }
        assert_eq!(window.carry.segments.len(), MAX_LIVE_ECHO_SOURCES);
        assert_eq!(window.carry.omitted, 1);
        assert_eq!(window.early_drop.sources.omitted, 1);
        window.push_aec_blocks(vec![block(3.0, 1.0, false), block(12.0, 1.0, false)]);
        let original = vec![word(3.0, 4.0, "Do not deploy the payment gateway today.")];
        publish_mic_words(&mut window, &publisher, original.clone());
        release_held_mic_words(&mut window, &publisher);
        assert_eq!(
            publisher.0.borrow().last(),
            Some(&("Me".to_string(), original))
        );
        assert!(
            window
                .carry
                .span_evidence(&window.aec_blocks, 3.0, 4.0)
                .is_none()
        );
        assert!(
            window
                .carry
                .span_evidence(&window.aec_blocks, 12.0, 13.0)
                .is_some()
        );
        window.frames = 6;
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
        assert!(read(&note).contains("Do not deploy the payment gateway today."));
    }

    #[test]
    fn raw_source_byte_overflow_keeps_the_early_protection_and_records_the_omission() {
        let filer = TempFiler::new("raw-source-byte-cap");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);
        let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
        let publisher = RecordingPublisher::default();
        window.push_aec_blocks(vec![block(3.0, 1.0, false)]);
        publish_mic_words(
            &mut window,
            &publisher,
            vec![word(3.0, 4.0, "Do not deploy the payment gateway today.")],
        );
        publish_them_words(
            &mut window,
            &publisher,
            vec![word(0.0, 5.0, "Deploy the payment gateway today.")],
        );
        let large = "x".repeat(MAX_BUFFERED_TRANSCRIPT_BYTES / 2);
        for _ in 0..2 {
            publish_them_words(&mut window, &publisher, vec![word(1.0, 5.0, &large)]);
        }
        assert_eq!(window.carry.segments.len(), 1);
        assert!(window.carry.bytes <= MAX_BUFFERED_TRANSCRIPT_BYTES);
        assert_eq!(window.carry.omitted, 2);
        assert_eq!(window.early_drop.sources.omitted, 2);
        assert!(
            window.due(),
            "duplicated raw storage is independently bounded"
        );
        window.frames = 6;
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
        assert!(read(&note).contains("Do not deploy the payment gateway today."));
        assert!(window.cleanup_decisions.entries().iter().any(|decision| {
            decision.stage == CleanupStage::Echo
                && decision.originals[0].start == 3.0
                && decision.source.is_some_and(|source| source.start == 0.0)
                && decision.reason
                    == corti_transcribe::segment::CleanupReason::Echo(
                        EchoVerdict::KeepCriticalDifference,
                    )
        }));
    }

    #[test]
    fn delayed_mic_poll_keeps_critical_words_after_temporal_source_eviction() {
        for flush_between_polls in [false, true] {
            for evidence in [Some(false), None, Some(true)] {
                let filer = TempFiler::new("delayed-mic-source-expiry");
                let note = filer.note();
                let mut writer = NoteWriter::new(filer, meta(), None);
                let mut window =
                    TranscriptWindow::new(2, 30, false, CleanupConfig::default()).unwrap();
                let publisher = RecordingPublisher::default();
                window.frames = 11;
                publish_them_words(
                    &mut window,
                    &publisher,
                    vec![word(0.0, 5.0, "The gateway dimensions are in mm today.")],
                );
                if flush_between_polls {
                    flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
                }
                window.frames = 25 - window.start_frame;
                publish_them_words(
                    &mut window,
                    &publisher,
                    vec![word(10.0, 12.0, "The orchard grows purple plums.")],
                );
                assert_eq!(window.early_drop.sources.segments.len(), 1);
                assert_eq!(window.early_drop.sources.omitted, 0);
                if flush_between_polls {
                    flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
                    assert_eq!(window.carry.segments.len(), 1);
                    assert_eq!(window.carry.omitted, 0);
                }
                if let Some(double_talk) = evidence {
                    window.push_aec_blocks(
                        (10..18)
                            .map(|time| block(f64::from(time), 1.0, double_talk))
                            .collect(),
                    );
                }
                let mic = vec![
                    word(1.0, 2.0, "The"),
                    word(2.0, 4.0, "gateway"),
                    word(4.0, 7.0, "dimensions"),
                    word(7.0, 8.0, "are"),
                    word(8.0, 9.0, "in"),
                    word(9.0, 12.0, "um"),
                    word(12.0, 18.0, "today."),
                ];
                window.frames = 37 - window.start_frame;
                publish_mic_words(&mut window, &publisher, mic.clone());
                release_held_mic_words(&mut window, &publisher);
                assert_eq!(publisher.0.borrow().last(), Some(&("Me".into(), mic)));
                assert_eq!(window.early_drop.stats.released_dropped, 0);
                assert!(window.mic_regions[0].comparison_incomplete);
                flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
                assert!(
                    read(&note).contains("**[00:01] Me:** The gateway dimensions are in um today.")
                );
            }
        }
    }

    #[test]
    fn temporal_source_eviction_still_allows_later_acoustic_echo_deletion() {
        let filer = TempFiler::new("source-expiry-later-echo");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);
        let mut window = TranscriptWindow::new(2, 30, false, CleanupConfig::default()).unwrap();
        let publisher = RecordingPublisher::default();
        window.frames = 11;
        publish_them_words(
            &mut window,
            &publisher,
            vec![word(0.0, 5.0, "The gateway dimensions are in mm today.")],
        );
        window.frames = 45;
        publish_them_words(
            &mut window,
            &publisher,
            vec![word(20.0, 22.0, "The orchard grows purple plums.")],
        );
        window.push_aec_blocks(vec![block(20.0, 1.0, false)]);
        publish_mic_words(
            &mut window,
            &publisher,
            vec![word(20.0, 21.0, "Transient echo.")],
        );
        release_held_mic_words(&mut window, &publisher);
        assert_eq!(window.early_drop.stats.released_dropped, 1);
        assert_eq!(window.early_drop.sources.omitted, 0);
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
        assert!(!read(&note).contains("Transient echo."));
    }

    #[test]
    fn raw_sources_accumulate_across_short_windows_and_expire_on_source_time() {
        let filer = TempFiler::new("raw-short-windows");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);
        let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
        let publisher = RecordingPublisher::default();
        publish_them_words(
            &mut window,
            &publisher,
            vec![word(0.0, 1.0, "Deploy the payment gateway today.")],
        );
        window.frames = 2;
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
        publish_them_words(
            &mut window,
            &publisher,
            vec![word(2.0, 3.0, "The weather report continues.")],
        );
        window.frames = 2;
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
        assert_eq!(window.carry.segments.len(), 2);
        assert_eq!(window.carry.segments[0].segment.start, 0.0);
        window.push_aec_blocks(vec![block(4.0, 1.0, false)]);
        publish_mic_words(
            &mut window,
            &publisher,
            vec![word(4.0, 4.5, "Do not deploy the payment gateway today.")],
        );
        publish_them_words(
            &mut window,
            &publisher,
            vec![word(4.0, 5.0, "The weather report continues.")],
        );
        window.frames = 2;
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
        assert!(read(&note).contains("Do not deploy the payment gateway today."));
        assert_eq!(
            read(&note)
                .matches("Them:** Deploy the payment gateway today.")
                .count(),
            1
        );
        window.frames = 6;
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
        assert!(window.carry.segments.is_empty());
        assert_eq!(window.carry.bytes, 0);
    }

    /// A failed checkpoint degrades the session instead of ending it: the window is dropped, the note is
    /// marked for the batch rewrite, the reader is told once, and the next checkpoint recovers.
    #[test]
    fn live_far_poll_protection_survives_pause_split_units() {
        let filer = TempFiler::new("raw-far-poll-unit");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);
        let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
        let publisher = RecordingPublisher::default();
        publish_mic_words(
            &mut window,
            &publisher,
            vec![word(0.0, 10.0, "The gateway dimensions are in mm today.")],
        );
        // Gaps exceed the default merge threshold, so merging cannot hide a lost protection flag.
        publish_them_words(
            &mut window,
            &publisher,
            vec![
                word(1.0, 2.0, "The gateway dimensions are in"),
                word(5.0, 6.0, "um"),
                word(9.0, 10.0, "today."),
            ],
        );
        release_held_mic_words(&mut window, &publisher);
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
        let content = std::fs::read_to_string(&note).unwrap_or_default();
        assert!(content.contains("**[00:05] Them:** um"), "{content}");
    }

    #[test]
    fn live_mic_poll_protection_survives_pause_split_units() {
        for far_first in [false, true] {
            for with_evidence in [false, true] {
                let filer = TempFiler::new("raw-mic-poll-unit");
                let note = filer.note();
                let mut writer = NoteWriter::new(filer, meta(), None);
                let mut window =
                    TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
                if with_evidence {
                    window.push_aec_blocks(vec![
                        block(1.0, 1.0, false),
                        block(5.0, 1.0, false),
                        block(9.0, 1.0, false),
                    ]);
                }
                let publisher = RecordingPublisher::default();
                let far = vec![word(0.0, 10.0, "The gateway dimensions are in mm today.")];
                let near = vec![
                    word(1.0, 2.0, "The gateway dimensions are in"),
                    word(5.0, 6.0, "um"),
                    word(9.0, 10.0, "today."),
                ];
                if far_first {
                    publish_them_words(&mut window, &publisher, far);
                    publish_mic_words(&mut window, &publisher, near);
                } else {
                    publish_mic_words(&mut window, &publisher, near);
                    publish_them_words(&mut window, &publisher, far);
                }
                release_held_mic_words(&mut window, &publisher);
                flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
                let content = std::fs::read_to_string(&note).unwrap_or_default();
                assert!(content.contains("**[00:05] Me:** um"), "{content}");
                assert_eq!(window.early_drop.stats.released_dropped, 0);
            }
        }
    }

    #[test]
    fn live_far_poll_protection_survives_pause_split_backchannels() {
        let filer = TempFiler::new("raw-far-poll-backchannel");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);
        let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
        let publisher = RecordingPublisher::default();
        publish_mic_words(
            &mut window,
            &publisher,
            vec![word(0.0, 7.0, "OKAY! Maybe we proceed.")],
        );
        publish_them_words(
            &mut window,
            &publisher,
            vec![word(1.0, 2.0, "Okay."), word(5.0, 6.0, "Maybe we proceed.")],
        );
        release_held_mic_words(&mut window, &publisher);
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
        let content = std::fs::read_to_string(&note).unwrap_or_default();
        assert!(content.contains("**[00:01] Them:** Okay."), "{content}");
    }

    #[test]
    fn missing_far_context_blocks_acoustic_deletion_after_its_time_horizon() {
        let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
        let publisher = RecordingPublisher::default();
        publish_them_words(
            &mut window,
            &publisher,
            vec![word(0.0, 10.0, "The gateway dimensions are in mm today.")],
        );
        for _ in 0..MAX_LIVE_ECHO_SOURCES {
            publish_them_words(
                &mut window,
                &publisher,
                vec![word(0.0, 10.0, "The orchard grows purple plums.")],
            );
        }
        window.push_aec_blocks(vec![block(20.0, 1.0, false)]);
        publish_mic_words(
            &mut window,
            &publisher,
            vec![
                word(1.0, 2.0, "The gateway dimensions are in"),
                word(20.0, 21.0, "um"),
                word(24.0, 25.0, "today."),
            ],
        );
        publish_them_words(
            &mut window,
            &publisher,
            vec![word(20.0, 26.0, "The orchard grows purple plums.")],
        );
        release_held_mic_words(&mut window, &publisher);
        assert_eq!(window.early_drop.stats.released_dropped, 0);
        assert!(window.mic_regions.iter().any(|region| {
            region.words.iter().any(|word| word.text == "um") && region.comparison_incomplete
        }));
    }

    #[test]
    fn sticky_incomplete_comparison_blocks_flush_acoustics_with_a_complete_inventory() {
        let filer = TempFiler::new("raw-sticky-incomplete");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);
        let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
        let publisher = RecordingPublisher::default();
        let poll = window.remember_mic_poll(&[
            word(1.0, 2.0, "The gateway dimensions are in"),
            word(20.0, 21.0, "um"),
            word(24.0, 25.0, "today."),
        ]);
        window.push_mic_region(ReleasedMicRegion {
            words: vec![word(20.0, 21.0, "um")],
            poll,
            comparison_incomplete: true,
            assessment: EchoAssessment::default(),
            evidence: None,
        });
        window.push_aec_blocks(vec![block(20.0, 1.0, false)]);
        publish_them_words(
            &mut window,
            &publisher,
            vec![word(20.0, 26.0, "The orchard grows purple plums.")],
        );
        assert_eq!(window.carry.omitted, 0);
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
        let content = std::fs::read_to_string(&note).unwrap_or_default();
        assert!(content.contains("**[00:20] Me:** um"));
    }

    #[test]
    fn raw_child_sources_protect_the_opposite_track_inside_a_broader_poll() {
        for broad_is_mic in [false, true] {
            let filer = TempFiler::new("raw-child-source");
            let note = filer.note();
            let mut writer = NoteWriter::new(filer, meta(), None);
            let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
            let publisher = RecordingPublisher::default();
            let broad = vec![
                word(1.0, 2.0, "Got IT."),
                word(5.0, 6.0, "The orchard grows purple plums."),
            ];
            let ordinary = vec![word(0.0, 7.0, "Got it.")];
            if broad_is_mic {
                publish_them_words(&mut window, &publisher, ordinary);
                publish_mic_words(&mut window, &publisher, broad);
            } else {
                publish_them_words(&mut window, &publisher, broad);
                publish_mic_words(&mut window, &publisher, ordinary);
            }
            release_held_mic_words(&mut window, &publisher);
            flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
            let content = std::fs::read_to_string(&note).unwrap_or_default();
            let speaker = if broad_is_mic { "Them" } else { "Me" };
            assert!(content.contains(&format!("**[00:00] {speaker}:** Got it.")));
        }
    }

    #[test]
    fn far_child_protection_is_kept_when_the_whole_poll_does_not_match() {
        let filer = TempFiler::new("raw-far-child-protection");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);
        let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
        let publisher = RecordingPublisher::default();
        publish_mic_words(&mut window, &publisher, vec![word(0.0, 7.0, "Got IT.")]);
        publish_them_words(
            &mut window,
            &publisher,
            vec![
                word(1.0, 2.0, "Got it."),
                word(5.0, 6.0, "The orchard grows purple plums."),
            ],
        );
        release_held_mic_words(&mut window, &publisher);
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
        let content = std::fs::read_to_string(&note).unwrap_or_default();
        assert!(content.contains("**[00:01] Them:** Got it."));
    }

    #[test]
    fn omitted_far_context_preserves_mic_descendants_through_automatic_cleanup() {
        let filer = TempFiler::new("raw-far-context-omission");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);
        let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
        let publisher = RecordingPublisher::default();
        publish_them_words(
            &mut window,
            &publisher,
            vec![word(0.0, 10.0, "The gateway dimensions are in mm today.")],
        );
        for _ in 0..MAX_LIVE_ECHO_SOURCES {
            publish_them_words(
                &mut window,
                &publisher,
                vec![word(0.0, 10.0, "The orchard grows purple plums.")],
            );
        }
        assert!(window.carry.omitted > 0);
        publish_mic_words(
            &mut window,
            &publisher,
            vec![
                word(1.0, 2.0, "The gateway dimensions are in"),
                word(5.0, 6.0, "um"),
                word(9.0, 10.0, "today."),
            ],
        );
        release_held_mic_words(&mut window, &publisher);
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
        let content = std::fs::read_to_string(&note).unwrap_or_default();
        assert!(content.contains("**[00:05] Me:** um"));
    }

    #[test]
    fn mic_poll_context_is_shared_bounded_and_fails_closed_when_omitted() {
        let filer = TempFiler::new("raw-mic-poll-context-cap");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);
        let mut window = TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
        let publisher = RecordingPublisher::default();
        publish_mic_words(
            &mut window,
            &publisher,
            vec![word(1.0, 2.0, "First."), word(5.0, 6.0, "Second.")],
        );
        assert_eq!(window.mic_polls.len(), 1);
        assert_eq!(window.early_drop.held.len(), 2);
        for held in &window.early_drop.held {
            let MicPollContext::Known(raw) = &held.poll else {
                panic!("small raw poll context was omitted");
            };
            assert!(Arc::ptr_eq(raw, &window.mic_polls[0]));
        }
        let retained_bytes = window.mic_poll_bytes;
        window.push_aec_blocks(vec![block(8.0, 1.0, false)]);
        publish_mic_words(
            &mut window,
            &publisher,
            vec![word(8.0, 9.0, &"x".repeat(MAX_BUFFERED_TRANSCRIPT_BYTES))],
        );
        assert_eq!(window.mic_poll_bytes, retained_bytes);
        assert!(window.mic_poll_bytes <= MAX_BUFFERED_TRANSCRIPT_BYTES);
        assert_eq!(window.mic_polls_omitted, 1);
        publish_them_words(&mut window, &publisher, vec![word(0.0, 10.0, "Okay.")]);
        release_held_mic_words(&mut window, &publisher);
        assert_eq!(window.early_drop.stats.released_dropped, 0);
        assert!(
            window
                .mic_regions
                .iter()
                .any(|region| region.poll.omitted())
        );
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
        let content = std::fs::read_to_string(&note).unwrap_or_default();
        assert!(content.contains("Them:** Okay."));
        assert!(content.contains("xxx"));
        assert!(window.mic_polls.is_empty());
        assert_eq!(window.mic_poll_bytes, 0);
        assert_eq!(window.mic_polls_omitted, 0);
        assert!(window.them_poll_ends.is_empty());
    }

    #[test]
    fn live_raw_protection_survives_later_backchannel_and_filler_passes() {
        for (first, second) in [
            ("Okay.", "OKAY!"),
            ("Got it.", "Got IT."),
            (
                "Report the gateway dimensions in mm today.",
                "Report the gateway dimensions in um, today.",
            ),
        ] {
            for reverse in [false, true] {
                let (far, near) = if reverse {
                    (second, first)
                } else {
                    (first, second)
                };
                let filer = TempFiler::new("raw-protection-later-passes");
                let note = filer.note();
                let mut writer = NoteWriter::new(filer, meta(), None);
                let mut window =
                    TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap();
                let publisher = RecordingPublisher::default();
                publish_them_words(&mut window, &publisher, vec![word(0.0, 5.0, far)]);
                publish_mic_words(&mut window, &publisher, vec![word(1.0, 2.0, near)]);
                release_held_mic_words(&mut window, &publisher);
                flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
                let content = std::fs::read_to_string(&note).unwrap_or_default();
                assert!(content.contains(&format!("Them:** {far}")), "{content}");
                assert!(content.contains(&format!("Me:** {near}")), "{content}");
            }
        }
    }

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
        for index in 0..4 {
            tx.send(CaptureChunk {
                epoch: fixture_epoch(1, false, true),
                retained_start_frame: index * 30,
                unavailable: Vec::new(),
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
        let mut capture = corti_core::QualityRecord::observed();
        capture.insert_epoch(fixture_epoch(1, false, true)).unwrap();
        let outcome = finish_session(
            1,
            None,
            mic,
            them,
            NoDiarizer,
            window,
            FinishQuality {
                dropped_chunks: 0,
                capture: Some(capture),
            },
            &mut writer,
            &publisher,
        )
        .unwrap();
        assert!(
            matches!(outcome, LiveOutcome::Fallback { .. }),
            "a recovered writer cannot recover the dropped words: {outcome:?}"
        );
        let content = read(&note);
        assert!(content.contains("State: transcribing\n"));
        assert!(content.contains("window_not_filed"));
    }

    #[test]
    fn final_ledger_cap_does_not_make_durable_windows_partial() {
        let filer = TempFiler::new("decode-ledger-cap");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);
        writer.final_transcript_bytes = MAX_FINAL_TRANSCRIPT_BYTES;
        writer
            .append_segment(&TranscriptSegment {
                speaker: Speaker::Me,
                start: 0.0,
                end: 1.0,
                text: "Synthetic durable cache overflow.".into(),
            })
            .unwrap();
        assert!(writer.final_transcript().is_none());
        let outcome = finish_session(
            1,
            None,
            Scripted::new(vec![], vec![]),
            Scripted::new(vec![], vec![]),
            NoDiarizer,
            TranscriptWindow::new(1, 1, false, CleanupConfig::default()).unwrap(),
            FinishQuality {
                dropped_chunks: 0,
                capture: Some(corti_core::QualityRecord::observed()),
            },
            &mut writer,
            &NoopPublisher,
        )
        .unwrap();
        assert!(matches!(outcome, LiveOutcome::Filed { .. }));
        let content = read(&note);
        assert!(content.contains("Synthetic durable cache overflow."));
        assert!(!content.contains("window_not_filed"));
        assert!(!content.contains("Transcript coverage is partial"));
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
            FinishQuality {
                dropped_chunks: 3,
                capture: Some(corti_core::QualityRecord::observed()),
            },
            &mut writer,
            &NoopPublisher,
        )
        .unwrap();

        let LiveOutcome::Fallback {
            reason,
            note_path: Some(note_path),
            ..
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
            epoch: fixture_epoch(1, false, true),
            retained_start_frame: 0,
            unavailable: Vec::new(),
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
            FinishQuality {
                dropped_chunks: 0,
                capture: Some(corti_core::QualityRecord::observed()),
            },
            &mut writer,
            &NoopPublisher,
        )
        .unwrap();
        assert!(matches!(
            outcome,
            LiveOutcome::NoNote {
                settled_quality: None
            }
        ));
        assert!(!note.exists());
    }

    fn lost_capture_quality() -> corti_core::QualityRecord {
        use corti_core::*;
        let mut quality = QualityRecord::observed();
        quality
            .insert_epoch(CaptureEpoch {
                id: 0,
                source_start_ns: Some(0),
                source_rate_hz: 48_000,
                source_frame_base: 0,
                retained_start_frame: 0,
                retained_rate_hz: 48_000,
                mic_channels: 1,
                tap_channels: 1,
                reason: CaptureEpochReason::Start,
            })
            .unwrap();
        quality
            .insert_span(QualitySpan {
                epoch: 0,
                start_frame: 48_000,
                end_frame: 96_000,
                track: QualityTrack::Both,
                cause: QualityCause::RingOverflow,
                class: QualityClass::SourceLoss,
                stage: QualityStage::Capture,
                resolution: QualityResolution::Outstanding,
            })
            .unwrap();
        quality
    }

    #[test]
    fn capture_loss_requires_partial_fallback_even_without_words_or_a_final_ledger() {
        for with_prefix in [false, true] {
            let filer = TempFiler::new(if with_prefix {
                "quality-ledgerless"
            } else {
                "quality-empty"
            });
            let note = filer.note();
            let mut writer = NoteWriter::new(filer, meta(), None);
            if with_prefix {
                writer
                    .append_segment(&TranscriptSegment {
                        speaker: Speaker::Me,
                        start: 0.0,
                        end: 0.5,
                        text: "Synthetic retained prefix.".into(),
                    })
                    .unwrap();
                // The optional final assembly cannot be required to update capture provenance.
                writer.final_segments.clear();
                writer.final_ledger_unavailable = true;
            }
            let outcome = finish_session(
                48_000,
                None,
                Scripted::new(vec![], vec![]),
                Scripted::new(vec![], vec![]),
                NoDiarizer,
                TranscriptWindow::new(48_000, 1, false, CleanupConfig::default()).unwrap(),
                FinishQuality {
                    dropped_chunks: 0,
                    capture: Some(lost_capture_quality()),
                },
                &mut writer,
                &NoopPublisher,
            )
            .unwrap();
            assert!(matches!(
                outcome,
                LiveOutcome::Fallback {
                    note_path: Some(_),
                    ..
                }
            ));
            let content = read(&note);
            assert!(content.contains("ring_overflow"));
            assert!(content.contains("source_loss"));
            assert!(content.contains("1.000–2.000 s"));
            assert!(content.contains("State: transcribing\n"));
            assert_eq!(
                content.matches("Synthetic retained prefix.").count(),
                usize::from(with_prefix)
            );
            assert!(content.contains("source_completeness"));
            assert!(content.contains("incomplete"));
        }
    }

    #[test]
    fn absent_capture_authority_cannot_certify_a_live_note_complete() {
        let filer = TempFiler::new("quality-legacy");
        let note = filer.note();
        let mut writer = NoteWriter::new(filer, meta(), None);
        let outcome = finish_session(
            48_000,
            None,
            Scripted::new(vec![], vec![word(1.0, 2.0, "Synthetic legacy words.")]),
            Scripted::new(vec![], vec![]),
            NoDiarizer,
            TranscriptWindow::new(48_000, 1, false, CleanupConfig::default()).unwrap(),
            FinishQuality {
                dropped_chunks: 0,
                capture: None,
            },
            &mut writer,
            &NoopPublisher,
        )
        .unwrap();
        assert!(matches!(outcome, LiveOutcome::Fallback { .. }));
        assert!(read(&note).contains("legacy_unknown"));
        assert!(read(&note).contains("State: transcribing\n"));
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
            LiveOutcome::NoNote {
                settled_quality: None,
            }
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
            Some(LiveOutcome::NoNote {
                settled_quality: None
            })
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
                Verdict::Finish(FinishQuality {
                    dropped_chunks: 0,
                    capture: None
                })
            ));
            a_finishing_tx.send(()).unwrap();
            release_a_rx.recv().unwrap();
            LiveOutcome::Filed {
                note_path: note_for_a,
                settled_quality: None,
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
            LiveOutcome::NoNote {
                settled_quality: None,
            }
        });
        assert!(manager.accepts_note("b"));

        let Some(LiveOutcome::Filed { note_path, .. }) = manager.collect("a") else {
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
                LiveOutcome::Filed {
                    note_path: path,
                    settled_quality: None,
                }
            });
            manager.finish(id);
            wait_for_model_slot(&manager);
        }

        for id in ["b", "a", "c"] {
            let Some(LiveOutcome::Filed { note_path, .. }) = manager.collect(id) else {
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
                settled_quality: None,
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
            LiveOutcome::NoNote {
                settled_quality: None,
            }
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
            settled_quality: None,
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
            settled_quality: None,
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
