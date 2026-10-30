//! Chunked / live transcription over the resident Parakeet engine (ADR 0009).
//!
//! [`LiveTranscriber`] is a **pull-based, synchronous** wrapper around the same selected [`crate::Asr`] and
//! sherpa Silero VAD the batch path uses: audio is
//! `push`ed in arbitrary-sized chunks, resampled to 16 kHz, fed to a single stateful Silero VAD, and each
//! **completed** VAD speech region is decoded immediately — so the decode cost lands inside the `push` that
//! closes a region, not on a separate tick. Recognized [`Word`]s queue up; the caller drains them with
//! [`LiveTranscriber::poll_words`], can force a reusable durability boundary with
//! [`LiveTranscriber::checkpoint`], and ends the stream with [`LiveTranscriber::finish`].
//!
//! Timestamps stay **call-relative** across pushes and checkpoints: within an epoch,
//! `SpeechSegment::start()` is VAD-relative; each checkpoint advances a cumulative 16 kHz sample base before
//! resetting that VAD, and decoded offsets add the base back.
//!
//! The core is deliberately sync (guardrail 9 — no runtime in the engine); the optional async `Stream`
//! adapter lives behind the `stream` feature at the bottom of this file.

use std::sync::Arc;

use anyhow::Result;
use corti_transcribe::decode::{DecodeErrorCategory, DecodeReport, RegionOutcome, RegionStatus};
use corti_transcribe::segment::{SpeakerTurn, Word};
use sherpa_onnx::{OfflineSpeakerDiarization, VoiceActivityDetector};

use crate::asr::{Asr, RegionDecoder};
use crate::engine::{self, TARGET_RATE, VAD_WINDOW};
use crate::input_clock::{InputClock, MappingError, SkippedClock};
use crate::resample::Resampler;

#[path = "boundary.rs"]
mod boundary;
use boundary::BoundaryState;
#[path = "terminal.rs"]
mod terminal;
pub use boundary::{
    BoundaryCapacity, BoundaryError, BoundaryInput, BoundaryKind, BoundaryLimits, BoundaryProgress,
    BoundaryResamplerBounds, BoundarySnapshot, BoundaryToken, boundary_resampler_bounds,
};
use terminal::TerminalState;

#[cfg(test)]
#[path = "native_boundary_matrix.rs"]
mod native_boundary_matrix;

/// Visit borrowed completed PCM before popping it, avoiding an extra region-sized allocation.
pub(crate) trait Segmenter: Send {
    fn accept(&mut self, samples: &[f32]);
    fn flush(&mut self);
    fn reset(&mut self);
    fn drain_completed(&mut self, visit: &mut dyn FnMut(u64, &[f32]));
    fn boundary_idle(&self) -> bool {
        false
    }
    /// Fresh-state processing contract for deterministic adapters. Arbitrary native VADs are unknown.
    fn terminal_guard(&self) -> Option<usize> {
        None
    }
}

impl Segmenter for VoiceActivityDetector {
    fn boundary_idle(&self) -> bool {
        !self.detected() && self.is_empty()
    }
    fn accept(&mut self, samples: &[f32]) {
        self.accept_waveform(samples);
    }
    fn flush(&mut self) {
        VoiceActivityDetector::flush(self);
    }
    fn reset(&mut self) {
        VoiceActivityDetector::reset(self);
    }
    fn drain_completed(&mut self, visit: &mut dyn FnMut(u64, &[f32])) {
        while let Some(segment) = self.front() {
            // Negative native positions are unmappable and are handled by the shared drain path.
            visit(
                u64::try_from(segment.start()).unwrap_or(u64::MAX),
                segment.samples(),
            );
            self.pop();
        }
    }
}

/// Accumulates 16 kHz samples and releases them in whole [`VAD_WINDOW`]-sized windows, carrying the
/// sub-window remainder across pushes so the VAD sees exactly the window sequence a batch `.chunks(512)`
/// would. Pure (no model state) — chunk-boundary handling and the absolute-time counter are unit-tested here.
#[derive(Default)]
struct WindowBuffer {
    buf: Vec<f32>,
    /// Total 16 kHz samples released to the VAD so far. Test-only invariant counter — production word
    /// offsets come from `SpeechSegment::start()` (the VAD's own absolute index), never from this.
    fed: u64,
}

impl WindowBuffer {
    /// Append `samples`; return the number of whole windows now ready to release.
    fn extend(&mut self, samples: &[f32]) -> usize {
        self.buf.extend_from_slice(samples);
        self.buf.len() / VAD_WINDOW
    }

    /// Take the complete-window prefix (a multiple of [`VAD_WINDOW`]), retaining the remainder. The prefix
    /// keeps the buffer's original allocation (the small remainder is what gets copied), so this is cheap
    /// even for a whole-channel batch push. Advances the fed-sample counter.
    fn take_windows(&mut self) -> Vec<f32> {
        let n = (self.buf.len() / VAD_WINDOW) * VAD_WINDOW;
        self.fed += n as u64;
        let remainder = self.buf.split_off(n);
        std::mem::replace(&mut self.buf, remainder)
    }

    /// Take whatever remains (the final partial window) — called once at finish. Advances the counter.
    fn take_remainder(&mut self) -> Vec<f32> {
        self.fed += self.buf.len() as u64;
        std::mem::take(&mut self.buf)
    }
}

/// Pull-based, synchronous chunked transcriber over one mono channel.
///
/// Feed audio with [`push`](Self::push) (any sample rate — resampled to 16 kHz internally, continuously
/// across pushes). **Decoding happens inside `push`**: when a VAD speech region closes, it is decoded on the
/// spot and its words are queued. Drain queued words without blocking via [`poll_words`](Self::poll_words),
/// force/reuse a bounded epoch via [`checkpoint`](Self::checkpoint), and flush the final trailing region with
/// [`finish`](Self::finish).
///
/// One `LiveTranscriber` handles one channel (its VAD is stateful); the [`Asr`] engine is shared —
/// pass the same `Arc` to a second instance for the far-end channel.
pub struct LiveTranscriber {
    rec: Arc<dyn RegionDecoder>,
    vad: Box<dyn Segmenter>,
    /// Built lazily on the first non-16 kHz push and reused so resampling is continuous across pushes.
    resampler: Option<Resampler>,
    src_rate: i32,
    win: WindowBuffer,
    pending: Vec<Word>,
    /// Absolute 16 kHz sample offset of the current VAD epoch. `checkpoint()` resets the VAD so its next
    /// `SpeechSegment::start()` begins at zero; adding this base keeps word timestamps call-relative.
    vad_base_samples: u64,
    finished: bool,
    /// One-shot latch so a persistent `LinearResampler::create` failure logs once, not per push.
    resampler_warned: bool,
    input_clock: InputClock,
    skipped_clock: SkippedClock,
    report: DecodeReport,
    decode_complete: bool,
    /// If an input clock is unrepresentable, later words cannot carry trustworthy timestamps.
    clock_available: bool,
    boundary: Option<BoundaryState>,
    terminal: TerminalState,
    #[cfg(test)]
    resampler_factory: Box<dyn FnMut(i32) -> Option<Resampler> + Send>,
}

impl LiveTranscriber {
    /// Wrap a resident ASR engine and a fresh (per-channel) Silero VAD. Build them via
    /// [`crate::Asr`] plus a VAD built by the crate, or use [`crate::LiveEngine`] to load once and spawn
    /// a transcriber per channel.
    /// An externally supplied detector has unknown configuration/state provenance, so terminal
    /// completeness cannot be certified. LiveEngine establishes the pinned fresh-state contract.
    pub fn new(rec: Arc<Asr>, vad: VoiceActivityDetector) -> Self {
        Self::with_parts(rec, Box::new(vad))
    }

    /// Only for a fresh detector just constructed by engine::build_vad with the pinned configuration.
    pub(crate) fn from_pinned_vad(rec: Arc<Asr>, vad: VoiceActivityDetector) -> Self {
        let mut live = Self::new(rec, vad);
        live.terminal.guard = Some(terminal::PINNED_GUARD);
        live
    }

    pub(crate) fn with_parts(rec: Arc<dyn RegionDecoder>, vad: Box<dyn Segmenter>) -> Self {
        let terminal = TerminalState::new(vad.terminal_guard());
        Self {
            rec,
            vad,
            resampler: None,
            src_rate: 0,
            win: WindowBuffer::default(),
            pending: Vec::new(),
            vad_base_samples: 0,
            finished: false,
            resampler_warned: false,
            input_clock: InputClock::default(),
            skipped_clock: SkippedClock::default(),
            report: DecodeReport::default(),
            decode_complete: true,
            clock_available: true,
            boundary: None,
            terminal,
            #[cfg(test)]
            resampler_factory: Box::new(Resampler::new),
        }
    }

    /// Push a chunk of mono audio at `sample_rate`. Resamples to 16 kHz (a no-op when `sample_rate` is
    /// already 16 kHz), feeds the VAD in 512-sample windows, and decodes+queues the words of every region
    /// that closes as a result — so a long region's decode cost is paid by whichever push closes it. Cheap
    /// while a region is still open (just buffering + VAD). No-op after [`finish`](Self::finish).
    pub fn push(&mut self, samples: &[f32], sample_rate: u32) {
        if self.finished || samples.is_empty() {
            return;
        }
        let Some(input_span) = self.input_clock.admit(samples.len()) else {
            self.report.omit_failure();
            self.decode_complete = false;
            self.clock_available = false;
            return;
        };
        if let Some(boundary) = &mut self.boundary {
            if boundary.failed.is_some() {
                self.report.omit_failure();
                self.decode_complete = false;
                return;
            }
            let error = if crate::input_clock::boundary_tick_frequency(sample_rate).is_none() {
                Some(BoundaryError::UnsupportedRate)
            } else if samples.len() > boundary.limits.max_source_push
                || self.input_clock.frames > i64::MAX as u64 / 16_000
            {
                Some(BoundaryError::AdmissionLimit)
            } else {
                None
            };
            if let Some(error) = error {
                boundary.failed = Some(error);
                if error == BoundaryError::UnsupportedRate {
                    self.report.record(RegionOutcome {
                        span: input_span,
                        status: RegionStatus::Failed(DecodeErrorCategory::ResamplerUnavailable),
                    });
                } else {
                    self.report.omit_failure();
                }
                self.decode_complete = false;
                return;
            }
        }
        if !self.clock_available {
            self.report.omit_failure();
            return;
        }
        let rate = sample_rate as i32;
        if rate == TARGET_RATE {
            // A 16 kHz push while a resampler is live: flush its buffered tail first, otherwise those
            // samples are stranded and finish() would inject them out of order after the 16 kHz audio.
            self.flush_resampler_tail();
            if self
                .input_clock
                .map_input(input_span, sample_rate, self.output_position())
            {
                self.report.omit_metadata();
            }
            self.feed_16k(samples);
            return;
        }
        // A mid-stream rate change: flush the old resampler's tail before it's replaced, so no samples are
        // dropped at the switch.
        if self.resampler.is_some() && self.src_rate != rate {
            self.flush_resampler_tail();
        }
        self.ensure_resampler(rate);
        // Resample to an owned buffer first so the resampler borrow ends before `feed_16k`'s `&mut`.
        let up = self.resampler.as_mut().map(|r| r.push(samples));
        match up {
            Some(up) => {
                if self
                    .input_clock
                    .map_input(input_span, sample_rate, self.output_position())
                {
                    self.report.omit_metadata();
                }
                self.feed_16k(&up);
            }
            // `create()` failed: drop the chunk rather than feed source-rate samples as 16 kHz (that garbles
            // words and stretches timestamps silently). Warn once; ensure_resampler retries on later pushes.
            None => {
                // Close any valid speech preceding the loss before advancing the timeline. Reusing an
                // open VAD region here would join unrelated speech across the missing chunk.
                self.flush_decode_tail();
                if let Some(boundary) = &mut self.boundary {
                    // The opt-in owner cannot reset away a pending fence or its source mappings.
                    // Terminate this live path truthfully; the ordinary compatibility path below is unchanged.
                    boundary.failed = Some(BoundaryError::ResamplerUnavailable);
                    self.report.record(RegionOutcome {
                        span: input_span,
                        status: RegionStatus::Failed(DecodeErrorCategory::ResamplerUnavailable),
                    });
                    self.decode_complete = false;
                    return;
                }
                self.preserve_reset_uncertainty();
                self.vad.reset();
                self.terminal.reset_epoch();
                self.input_clock.reset_vad();
                self.report.record(RegionOutcome {
                    span: input_span,
                    status: RegionStatus::Failed(DecodeErrorCategory::ResamplerUnavailable),
                });
                self.decode_complete = false;
                match self
                    .skipped_clock
                    .advance(input_span.end_frame - input_span.start_frame, sample_rate)
                    .and_then(|count| self.win.fed.checked_add(count))
                {
                    Some(position) => self.win.fed = position,
                    None => {
                        self.report.omit_failure();
                        self.clock_available = false;
                    }
                }
                self.vad_base_samples = self.win.fed;
                if !self.resampler_warned {
                    self.resampler_warned = true;
                    tracing::warn!(
                        target: "corti::transcribe::local",
                        src_rate = rate,
                        "could not build a resampler for this sample rate — dropping live audio chunks"
                    );
                }
            }
        }
    }

    /// Flush whatever the live resampler still holds through `feed_16k`, then drop it (a later non-16 kHz
    /// push rebuilds a fresh one). Called before a rate change or a 16 kHz push so the buffered tail is
    /// emitted in order rather than stranded until finish().
    fn flush_resampler_tail(&mut self) {
        let tail = self.resampler.as_mut().map(Resampler::finish);
        self.resampler = None;
        self.src_rate = 0;
        if let Some(tail) = tail {
            if !tail.is_empty() {
                self.feed_16k(&tail);
            }
            self.input_clock.end_epoch();
        }
    }

    fn output_position(&self) -> u64 {
        self.win.fed.saturating_add(self.win.buf.len() as u64)
    }

    /// Drain bounded, content-free decode outcomes. Completeness remains sticky after this drain.
    pub fn take_decode_report(&mut self) -> DecodeReport {
        std::mem::take(&mut self.report)
    }

    pub fn decode_complete(&self) -> bool {
        self.decode_complete
    }

    /// The batch path has already performed the canonical resampling, but reports original WAV frames.
    pub(crate) fn push_resampled(
        &mut self,
        samples: &[f32],
        source_frames: usize,
        source_rate: u32,
    ) {
        let Some(span) = self.input_clock.admit(source_frames) else {
            self.report.omit_failure();
            self.decode_complete = false;
            return;
        };
        if self
            .input_clock
            .map_input(span, source_rate, self.output_position())
        {
            self.report.omit_metadata();
        }
        self.feed_16k(samples);
    }

    /// Non-blocking drain of the words decoded so far. `None` when nothing is queued.
    pub fn poll_words(&mut self) -> Option<Vec<Word>> {
        if self.pending.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.pending))
        }
    }

    /// Force the current bounded VAD/resampler tail to become final words without unloading the recognizer,
    /// then reset the VAD epoch so more chunks can be pushed. This is the durable live-filing boundary: a
    /// caller can write the returned words, release its rolling audio, and continue with constant memory.
    ///
    /// Timestamps remain seconds from the start of the full stream, not from this checkpoint. Calling this
    /// with no new audio simply drains any un-polled words and is otherwise a no-op.
    pub fn checkpoint(&mut self) -> Vec<Word> {
        if self.finished {
            return std::mem::take(&mut self.pending);
        }
        self.flush_decode_tail();
        self.preserve_reset_uncertainty();
        self.vad.reset();
        self.terminal.reset_epoch();
        self.vad_base_samples = self.win.fed;
        self.input_clock.reset_vad();
        std::mem::take(&mut self.pending)
    }

    /// Flush the VAD (and the resampler tail), decode the final trailing region, and return **all** remaining
    /// words — those from the flush plus anything queued but not yet polled. Idempotent: a second call
    /// returns whatever has accumulated since (normally empty).
    pub fn finish(&mut self) -> Vec<Word> {
        if self.boundary.is_some() {
            match self.advance_boundary(BoundaryInput::SourceEnded) {
                BoundaryProgress::Ended => {}
                BoundaryProgress::TerminalUnsettled(error) => {
                    self.refuse_compatibility_finish(error)
                }
                BoundaryProgress::OldSettled(_) | BoundaryProgress::Waiting => {
                    self.refuse_compatibility_finish(BoundaryError::InvalidTransition);
                }
            }
            return std::mem::take(&mut self.pending);
        }
        if !self.finished {
            self.flush_resampler_tail();
            let remainder = self.win.take_remainder();
            if !remainder.is_empty() {
                self.accept_native_pcm(&remainder);
                self.drain_regions();
            }
            self.finalize_terminal_vad();
            self.finished = true;
        }
        std::mem::take(&mut self.pending)
    }

    /// Flush one epoch's resampler/VAD tails into `pending`. The `WindowBuffer` sample counter remains
    /// cumulative across epochs and supplies the next checkpoint's absolute timestamp base.
    fn flush_decode_tail(&mut self) {
        self.flush_resampler_tail();
        if self.boundary.is_some() {
            self.accept_boundary_remainder();
            if self
                .boundary
                .as_ref()
                .is_some_and(|state| state.failed.is_some())
            {
                return;
            }
            self.vad.flush();
            self.drain_regions();
            return;
        }
        let remainder = self.win.take_remainder();
        if !remainder.is_empty() {
            self.accept_native_pcm(&remainder);
        }
        self.vad.flush();
        self.drain_regions();
    }

    /// Feed already-16 kHz samples: buffer to whole VAD windows, then for each window accept + drain any
    /// regions the VAD closed.
    fn feed_16k(&mut self, samples_16k: &[f32]) {
        self.retain_reset_tail(samples_16k);
        if self.boundary.is_some() {
            self.feed_boundary_16k(samples_16k);
            return;
        }
        // Zero-copy batch path: with nothing carried, feed whole windows straight from the input slice and
        // buffer only the sub-window remainder — a whole-channel push copies just the tail, not the channel.
        if self.win.buf.is_empty() {
            let (windows, remainder) = samples_16k.as_chunks::<VAD_WINDOW>();
            for window in windows {
                self.accept_native_pcm(window);
                self.drain_regions();
            }
            self.win.fed += (samples_16k.len() - remainder.len()) as u64;
            self.win.buf.extend_from_slice(remainder);
            return;
        }
        // Carry path: append to the retained remainder and release whole windows from the combined buffer.
        if self.win.extend(samples_16k) == 0 {
            return;
        }
        let block = self.win.take_windows();
        for window in block.as_chunks::<VAD_WINDOW>().0 {
            self.accept_native_pcm(window);
            self.drain_regions();
        }
    }

    fn ensure_resampler(&mut self, rate: i32) {
        if rate <= 0 {
            self.resampler = None;
            self.src_rate = rate;
            return;
        }
        if self.resampler.is_none() || self.src_rate != rate {
            #[cfg(not(test))]
            {
                self.resampler = Resampler::new(rate);
            }
            #[cfg(test)]
            {
                self.resampler = (self.resampler_factory)(rate);
            }
            self.src_rate = rate;
        }
    }

    /// The sole completed-region path, shared by native VAD and deterministic fault-injection tests.
    fn drain_regions(&mut self) {
        let Self {
            rec,
            vad,
            pending,
            vad_base_samples,
            input_clock,
            skipped_clock,
            report,
            decode_complete,
            boundary,
            terminal,
            ..
        } = self;
        vad.drain_completed(&mut |relative_start, samples| {
            let canonical = vad_base_samples
                .checked_add(relative_start)
                .and_then(|start| {
                    start
                        .checked_add(samples.len() as u64)
                        .map(|end| (start, end))
                });
            let Some((start, full_end)) = canonical else {
                terminal.ownership_valid = false;
                report.omit_failure();
                *decode_complete = false;
                if let Some(state) = boundary {
                    state.failed = Some(BoundaryError::MappingUnavailable);
                }
                return;
            };
            // Native-only terminal padding never owns source samples or reaches ASR.
            let full_end = terminal.clip_end.map_or(full_end, |end| full_end.min(end));
            if full_end <= start {
                return;
            }
            if terminal.clip_end.is_some() && start < terminal.last_attempt_end {
                terminal.ownership_valid = false;
                report.omit_failure();
                *decode_complete = false;
                return;
            }
            if boundary.is_some()
                && input_clock.input_span(start, full_end) == Err(MappingError::Invalid)
            {
                terminal.ownership_valid = false;
                report.omit_failure();
                *decode_complete = false;
                if let Some(state) = boundary {
                    state.failed = Some(BoundaryError::MappingUnavailable);
                }
                return;
            }
            let end = if let Some(state) = boundary {
                match state.allowed_end(input_clock, start, full_end) {
                    Ok(end) => end,
                    Err(error) => {
                        terminal.ownership_valid = false;
                        state.failed = Some(error);
                        report.omit_failure();
                        *decode_complete = false;
                        return;
                    }
                }
            } else {
                full_end
            };
            if end <= start {
                return;
            }
            let samples = &samples[..(end - start) as usize];
            let span = vad_base_samples
                .checked_add(relative_start)
                .and_then(|start| {
                    start
                        .checked_add(samples.len() as u64)
                        .map(|end| (start, end))
                })
                .ok_or(MappingError::Invalid)
                .and_then(|(start, end)| input_clock.input_span(start, end));
            if span == Err(MappingError::Invalid) {
                terminal.ownership_valid = false;
                // No backend call or timed words can be trusted for invalid native coordinates.
                report.omit_failure();
                *decode_complete = false;
                return;
            }
            // Native VAD indices live on an integer grid; failed input can last a fractional 16 kHz
            // sample. Apply that remainder immediately, including after recovery and checkpoints.
            let offset = absolute_offset_sec(*vad_base_samples, relative_start)
                + skipped_clock.fractional_seconds();
            if let Some(state) = boundary {
                state.last_attempt_end = state.last_attempt_end.max(end);
                match span {
                    Ok(span) => {
                        state.last_attempt_source_end = Some(
                            state
                                .last_attempt_source_end
                                .unwrap_or(0)
                                .max(span.end_frame),
                        )
                    }
                    Err(_) => state.proof_available = false,
                }
            }
            terminal.last_attempt_end = terminal.last_attempt_end.max(end);
            let decoded = rec.decode(samples, offset);
            pending.extend(decoded.words);
            if matches!(
                decoded.status,
                RegionStatus::Failed(_) | RegionStatus::Partial(_)
            ) {
                *decode_complete = false;
            }
            if let Ok(span) = span {
                report.record(RegionOutcome {
                    span,
                    status: decoded.status,
                });
            } else if matches!(
                decoded.status,
                RegionStatus::Complete | RegionStatus::RecognizedEmpty
            ) {
                // Valid canonical timing and successful decoding survive loss of source-span detail.
                report.omit_metadata();
            } else {
                report.omit_failure();
                *decode_complete = false;
            }
        });
    }
}

fn absolute_offset_sec(vad_base_samples: u64, segment_start: u64) -> f64 {
    vad_base_samples.saturating_add(segment_start) as f64 / TARGET_RATE as f64
}

/// A resident local ASR engine: one loaded recognizer ([`Asr`] — sherpa or ggml) plus the VAD parameters
/// needed to spawn a fresh [`LiveTranscriber`] per channel. Each channel needs its own stateful VAD, but all
/// channels share the single (thread-safe) recognizer. Build via [`crate::LocalTranscriber::live_engine`].
pub struct LiveEngine {
    rec: Arc<Asr>,
    models: crate::models::Models,
    vad_threshold: f32,
    vad_min_silence: f32,
    diarizer: Option<OfflineSpeakerDiarization>,
}

impl LiveEngine {
    pub(crate) fn new(
        rec: Asr,
        models: crate::models::Models,
        vad_threshold: f32,
        vad_min_silence: f32,
        diarizer: Option<OfflineSpeakerDiarization>,
    ) -> Self {
        Self {
            rec: Arc::new(rec),
            models,
            vad_threshold,
            vad_min_silence,
            diarizer,
        }
    }

    /// Spawn a [`LiveTranscriber`] for one channel: a fresh Silero VAD sharing the resident recognizer.
    pub fn channel(&self) -> Result<LiveTranscriber> {
        let vad = engine::build_vad(&self.models, self.vad_threshold, self.vad_min_silence)?;
        Ok(LiveTranscriber::from_pinned_vad(self.rec.clone(), vad))
    }

    /// Whether this engine loaded the optional far-end diarizer.
    pub fn diarizes_far_end(&self) -> bool {
        self.diarizer.is_some()
    }

    /// Diarize one bounded far-end audio window and lift its turns onto the full recording timeline.
    /// `samples` may use the capture rate; the diarizer always receives a temporary 16 kHz buffer which is
    /// released when this call returns. `Ok(None)` means diarization was not configured.
    pub fn diarize_chunk(
        &self,
        samples: &[f32],
        sample_rate: u32,
        offset_sec: f64,
    ) -> Result<Option<Vec<SpeakerTurn>>> {
        let Some(diarizer) = self.diarizer.as_ref() else {
            return Ok(None);
        };
        let samples_16k = engine::resample_to_16k(samples, sample_rate as i32)?;
        let mut turns = engine::diarize_channel(diarizer, &samples_16k);
        for turn in &mut turns {
            turn.start += offset_sec;
            turn.end += offset_sec;
        }
        Ok(Some(turns))
    }
}

#[cfg(feature = "stream")]
mod stream {
    //! Async edge adapter (ADR 0009): a `futures_core::Stream<Item = Vec<Word>>` over the sync
    //! [`LiveTranscriber`]. The core stays sync — this owns a dedicated std thread running the transcriber and
    //! bridges it to the async world with a tokio mpsc, so no runtime ever enters the engine.

    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::task::{Context, Poll};
    use std::thread::JoinHandle;

    use corti_transcribe::decode::{
        DecodeErrorCategory, DecodeReport, InputSpan, RegionOutcome, RegionStatus,
    };
    use corti_transcribe::segment::Word;
    use futures_core::Stream;

    use super::LiveTranscriber;

    /// Bounded audio backlog before the sink drops chunks. Mirrors the capture tee's lossy-bounded contract
    /// (ADR 0009): live audio is throwaway, so a slow decoder drops rather than growing memory + word latency
    /// without bound.
    const AUDIO_BACKLOG: usize = 64;

    /// Terminal decode facts for the input admitted by the worker. This is not a source/capture
    /// completeness certificate: rejected sink chunks never entered the transcriber's input clock.
    #[derive(Clone, Debug)]
    pub struct LiveStreamCompletion {
        report: DecodeReport,
        decode_complete: bool,
        worker_failed: bool,
        dropped: Arc<AtomicUsize>,
    }

    impl LiveStreamCompletion {
        /// Bounded region facts, with source spans relative to worker-admitted input only.
        pub fn decode_report(&self) -> &DecodeReport {
            &self.report
        }
        /// Whether decoding completed for admitted input; inspect dropped chunks separately.
        pub fn decode_complete(&self) -> bool {
            self.decode_complete
        }
        /// Whether the worker failed, independently of ordinary typed decoder failures.
        pub fn worker_failed(&self) -> bool {
            self.worker_failed
        }
        /// The same rejected-send counter as the sink, including pushes after worker exit. This is
        /// not a source-frame count or a count of queued audio abandoned when a worker fails.
        pub fn dropped_chunks(&self) -> usize {
            self.dropped.load(Ordering::Relaxed)
        }
    }

    struct StreamState {
        report: DecodeReport,
        decode_complete: bool,
        worker_failed: bool,
        input_frames: u64,
        last_reported_input_frame: u64,
        diagnosed: bool,
        #[cfg(test)]
        diagnostics: usize,
    }

    impl Default for StreamState {
        fn default() -> Self {
            Self {
                report: DecodeReport::default(),
                decode_complete: true,
                worker_failed: false,
                input_frames: 0,
                last_reported_input_frame: 0,
                diagnosed: false,
                #[cfg(test)]
                diagnostics: 0,
            }
        }
    }

    impl StreamState {
        fn collect(&mut self, live: &mut LiveTranscriber) {
            let report = live.take_decode_report();
            self.input_frames = live.input_clock.frames;
            self.decode_complete &= live.decode_complete() && report.is_complete();
            for outcome in report.outcomes() {
                self.last_reported_input_frame =
                    self.last_reported_input_frame.max(outcome.span.end_frame);
            }
            self.report.merge(report);
        }

        fn worker_failed(&mut self) {
            self.decode_complete = false;
            if self.worker_failed {
                return;
            }
            self.worker_failed = true;
            if self.last_reported_input_frame < self.input_frames {
                self.report.record(RegionOutcome {
                    span: InputSpan {
                        start_frame: self.last_reported_input_frame,
                        end_frame: self.input_frames,
                    },
                    status: RegionStatus::Failed(DecodeErrorCategory::BackendFailure),
                });
            } else {
                // No trustworthy nonempty pending span is available; never invent source coordinates.
                self.report.omit_failure();
            }
        }
    }

    fn lock_state(state: &Mutex<StreamState>) -> MutexGuard<'_, StreamState> {
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn diagnose_terminal(state: &Mutex<StreamState>, dropped: &AtomicUsize) {
        let emit = {
            let mut state = lock_state(state);
            let incomplete = !state.decode_complete
                || state.worker_failed
                || dropped.load(Ordering::Relaxed) != 0;
            if incomplete && !state.diagnosed {
                state.diagnosed = true;
                #[cfg(test)]
                {
                    state.diagnostics += 1;
                }
                true
            } else {
                false
            }
        };
        if emit {
            // Observable without a tracing subscriber. No words, panic payload or native error text.
            eprintln!("warning: live transcription stream ended with incomplete output");
        }
    }

    fn run_worker(
        live: &mut LiveTranscriber,
        audio_rx: std::sync::mpsc::Receiver<(Vec<f32>, u32)>,
        words_tx: &tokio::sync::mpsc::UnboundedSender<Vec<Word>>,
        state: &Mutex<StreamState>,
    ) {
        while let Ok((samples, rate)) = audio_rx.recv() {
            let pushed = catch_unwind(AssertUnwindSafe(|| live.push(&samples, rate)));
            {
                // Drain even when this same push already recorded facts and then panicked.
                let mut state = lock_state(state);
                state.collect(live);
                if pushed.is_err() {
                    state.worker_failed();
                }
            }
            if let Some(words) = live.poll_words()
                && words_tx.send(words).is_err()
            {
                return; // Existing consumer-drop behavior: stop, without another decode/finish.
            }
            if pushed.is_err() {
                return;
            }
        }
        let finished = catch_unwind(AssertUnwindSafe(|| live.finish()));
        {
            let mut state = lock_state(state);
            state.collect(live);
            if finished.is_err() {
                state.worker_failed();
            }
        }
        let tail = match finished {
            Ok(words) => words,
            Err(_) => live.poll_words().unwrap_or_default(),
        };
        if !tail.is_empty() {
            let _ = words_tx.send(tail);
        }
    }

    /// Sink half: push audio (any sample rate) from any thread. Never blocks — when the decoder falls behind
    /// real time the bounded queue fills and further chunks are dropped (counted in
    /// [`dropped_chunks`](Self::dropped_chunks)) rather than queued unbounded. Dropping the sink flushes the
    /// transcriber and ends the stream.
    pub struct LiveSink {
        tx: std::sync::mpsc::SyncSender<(Vec<f32>, u32)>,
        dropped: Arc<AtomicUsize>,
    }

    impl LiveSink {
        /// Hand a chunk of mono audio to the worker thread. Ordering is preserved; never blocks. A chunk
        /// that finds the bounded queue full (or the worker gone) is dropped and counted.
        pub fn push(&self, samples: Vec<f32>, sample_rate: u32) {
            if self.tx.try_send((samples, sample_rate)).is_err() {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }

        /// Total chunks dropped so far because the decoder fell behind (queue full) or the worker exited.
        pub fn dropped_chunks(&self) -> usize {
            self.dropped.load(Ordering::Relaxed)
        }
    }

    /// Stream half: yields a `Vec<Word>` each time the worker decodes one or more regions, then `None` once
    /// the worker ends. Reaching `None` joins the worker and exposes [`completion`](Self::completion).
    /// EOF means termination, not complete decoding or complete source input. Queue drops are counted
    /// separately; word times and report spans use only the input actually admitted by the worker.
    pub struct LiveWordStream {
        rx: tokio::sync::mpsc::UnboundedReceiver<Vec<Word>>,
        worker: Option<JoinHandle<()>>,
        completion: Option<LiveStreamCompletion>,
        state: Arc<Mutex<StreamState>>,
        dropped: Arc<AtomicUsize>,
    }

    impl LiveWordStream {
        /// Terminal facts become available after polling EOF. EOF itself means termination only.
        pub fn completion(&self) -> Option<&LiveStreamCompletion> {
            self.completion.as_ref()
        }

        #[cfg(test)]
        fn diagnostic_count(&self) -> usize {
            lock_state(&self.state).diagnostics
        }
    }

    impl Stream for LiveWordStream {
        type Item = Vec<Word>;

        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let this = self.get_mut();
            match this.rx.poll_recv(cx) {
                Poll::Ready(None) => {
                    if this.completion.is_none() {
                        // The sender has gone, so the worker has finished its output. A panic outside
                        // a guarded push/finish (including destruction) still cannot look complete.
                        if let Some(worker) = this.worker.take()
                            && worker.join().is_err()
                        {
                            lock_state(&this.state).worker_failed();
                        }
                        diagnose_terminal(&this.state, &this.dropped);
                        let mut state = lock_state(&this.state);
                        this.completion = Some(LiveStreamCompletion {
                            report: std::mem::take(&mut state.report),
                            decode_complete: state.decode_complete,
                            worker_failed: state.worker_failed,
                            dropped: this.dropped.clone(),
                        });
                    }
                    Poll::Ready(None)
                }
                other => other,
            }
        }
    }

    /// Split a [`LiveTranscriber`] into a push [`LiveSink`] and a [`LiveWordStream`], running the transcriber
    /// on a dedicated std thread. Drop the sink to flush and terminate the stream; draining the stream to
    /// `None` joins that thread. Inspect the completion's decode facts and independent dropped count.
    /// A single content-free terminal warning discloses incomplete output. The process's ordinary panic
    /// hook is unchanged; this adapter never formats or logs a panic payload itself.
    pub fn live_word_stream(mut live: LiveTranscriber) -> (LiveSink, LiveWordStream) {
        let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel::<(Vec<f32>, u32)>(AUDIO_BACKLOG);
        let (words_tx, words_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<Word>>();
        let dropped = Arc::new(AtomicUsize::new(0));
        let state = Arc::new(Mutex::new(StreamState::default()));
        lock_state(&state).collect(&mut live);
        // Preserve any already-decoded words even if the worker cannot be started.
        if let Some(words) = live.poll_words() {
            let _ = words_tx.send(words);
        }
        let worker_state = state.clone();
        let worker_dropped = dropped.clone();
        let worker = std::thread::Builder::new()
            .name("corti-live-asr".into())
            .spawn(move || {
                run_worker(&mut live, audio_rx, &words_tx, &worker_state);
                diagnose_terminal(&worker_state, &worker_dropped);
            });
        let worker = match worker {
            Ok(worker) => Some(worker),
            Err(_) => {
                lock_state(&state).worker_failed();
                diagnose_terminal(&state, &dropped);
                None
            }
        };
        (
            LiveSink {
                tx: audio_tx,
                dropped: dropped.clone(),
            },
            LiveWordStream {
                rx: words_rx,
                worker,
                completion: None,
                state,
                dropped,
            },
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::asr::{DecodedRegion, RegionDecoder};
        use crate::live::Segmenter;
        use corti_transcribe::decode::{DecodeErrorCategory, InputSpan, RegionStatus};
        use std::collections::VecDeque;
        use std::sync::Mutex;
        use std::time::{Duration, Instant};

        #[derive(Clone, Copy)]
        enum Plan {
            Complete,
            Empty,
            Failed,
            Partial,
            Panic,
        }

        struct Decoder {
            plans: Vec<Plan>,
            calls: Arc<AtomicUsize>,
            block_first: Option<(
                std::sync::mpsc::SyncSender<()>,
                Mutex<std::sync::mpsc::Receiver<()>>,
            )>,
        }
        impl RegionDecoder for Decoder {
            fn decode(&self, pcm: &[f32], offset: f64) -> DecodedRegion {
                let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
                if call == 1
                    && let Some((started, release)) = &self.block_first
                {
                    started.send(()).unwrap();
                    release
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap();
                }
                let plan = self.plans.get(call - 1).copied().unwrap_or(Plan::Complete);
                let status = match plan {
                    Plan::Complete => RegionStatus::Complete,
                    Plan::Empty => RegionStatus::RecognizedEmpty,
                    Plan::Failed => RegionStatus::Failed(DecodeErrorCategory::BackendFailure),
                    Plan::Partial => RegionStatus::Partial(DecodeErrorCategory::OutputTruncated),
                    // The process's ordinary panic hook remains active. This marker is never logged
                    // by the adapter's own terminal diagnostic.
                    Plan::Panic => panic!("synthetic worker panic hook marker"),
                };
                let words = if matches!(plan, Plan::Empty | Plan::Failed) {
                    vec![]
                } else {
                    vec![Word {
                        start: offset,
                        end: offset + pcm.len() as f64 / 16_000.0,
                        text: format!("region {call}"),
                    }]
                };
                DecodedRegion { words, status }
            }
        }

        struct SegmenterFixture {
            position: u64,
            tail: bool,
            pending: Option<(u64, Vec<f32>)>,
            ready: VecDeque<(u64, Vec<f32>)>,
            flushes: Arc<AtomicUsize>,
        }
        impl Segmenter for SegmenterFixture {
            fn terminal_guard(&self) -> Option<usize> {
                Some(0)
            }
            fn accept(&mut self, pcm: &[f32]) {
                let region = (self.position, pcm.to_vec());
                self.position += pcm.len() as u64;
                if self.tail {
                    if let Some(previous) = self.pending.replace(region) {
                        self.ready.push_back(previous);
                    }
                } else {
                    self.ready.push_back(region);
                }
            }
            fn flush(&mut self) {
                self.flushes.fetch_add(1, Ordering::SeqCst);
                if let Some(region) = self.pending.take() {
                    self.ready.push_back(region);
                }
            }
            fn reset(&mut self) {
                self.position = 0;
                self.pending = None;
                self.ready.clear();
            }
            fn drain_completed(&mut self, visit: &mut dyn FnMut(u64, &[f32])) {
                while let Some((start, pcm)) = self.ready.pop_front() {
                    visit(start, &pcm);
                }
            }
        }

        fn fixture(
            plans: Vec<Plan>,
            tail: bool,
        ) -> (LiveTranscriber, Arc<AtomicUsize>, Arc<AtomicUsize>) {
            fixture_with_block(plans, tail, None)
        }
        fn fixture_with_block(
            plans: Vec<Plan>,
            tail: bool,
            block: Option<(
                std::sync::mpsc::SyncSender<()>,
                Mutex<std::sync::mpsc::Receiver<()>>,
            )>,
        ) -> (LiveTranscriber, Arc<AtomicUsize>, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            let flushes = Arc::new(AtomicUsize::new(0));
            let live = LiveTranscriber::with_parts(
                Arc::new(Decoder {
                    plans,
                    calls: calls.clone(),
                    block_first: block,
                }),
                Box::new(SegmenterFixture {
                    position: 0,
                    tail,
                    pending: None,
                    ready: VecDeque::new(),
                    flushes: flushes.clone(),
                }),
            );
            (live, calls, flushes)
        }

        struct ThreadWake(std::thread::Thread);
        impl std::task::Wake for ThreadWake {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.0.unpark();
            }
        }
        fn drain(stream: &mut LiveWordStream) -> Vec<Word> {
            let waker = std::task::Waker::from(Arc::new(ThreadWake(std::thread::current())));
            let mut cx = Context::from_waker(&waker);
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut words = Vec::new();
            loop {
                match Pin::new(&mut *stream).poll_next(&mut cx) {
                    Poll::Ready(Some(chunk)) => words.extend(chunk),
                    Poll::Ready(None) => return words,
                    Poll::Pending => {
                        assert!(Instant::now() < deadline, "worker did not terminate");
                        std::thread::park_timeout(Duration::from_millis(10));
                    }
                }
            }
        }
        fn run(live: LiveTranscriber, chunks: Vec<Vec<f32>>) -> (Vec<Word>, LiveWordStream) {
            let (sink, mut stream) = live_word_stream(live);
            assert!(stream.completion().is_none());
            for pcm in chunks {
                sink.push(pcm, 16_000);
            }
            assert_eq!(sink.dropped_chunks(), 0);
            drop(sink);
            let words = drain(&mut stream);
            (words, stream)
        }
        fn expected(call: usize) -> Word {
            Word {
                start: (call - 1) as f64 * 512.0 / 16_000.0,
                end: call as f64 * 512.0 / 16_000.0,
                text: format!("region {call}"),
            }
        }
        fn repeat_eof(stream: &mut LiveWordStream, diagnostics: usize) {
            let mut cx = Context::from_waker(std::task::Waker::noop());
            for _ in 0..3 {
                assert!(matches!(
                    Pin::new(&mut *stream).poll_next(&mut cx),
                    Poll::Ready(None)
                ));
                assert!(stream.completion().is_some());
            }
            assert_eq!(stream.diagnostic_count(), diagnostics);
        }

        #[test]
        fn stream_middle_failure_retains_exact_neighbors_spans_and_terminal_status() {
            let (live, calls, flushes) =
                fixture(vec![Plan::Complete, Plan::Failed, Plan::Complete], false);
            let (words, mut stream) = run(live, vec![vec![1.0; 512]; 3]);
            assert_eq!(words, [expected(1), expected(3)]);
            assert_eq!(calls.load(Ordering::SeqCst), 3);
            assert_eq!(flushes.load(Ordering::SeqCst), 1);
            let completion = stream
                .completion()
                .expect("EOF must retain terminal decode facts");
            assert!(!completion.decode_complete());
            assert!(!completion.worker_failed());
            assert_eq!(completion.dropped_chunks(), 0);
            let outcomes = completion.decode_report().outcomes();
            assert_eq!(outcomes.len(), 3);
            assert_eq!(
                outcomes[1].span,
                InputSpan {
                    start_frame: 512,
                    end_frame: 1024
                }
            );
            assert_eq!(
                outcomes[1].status,
                RegionStatus::Failed(DecodeErrorCategory::BackendFailure)
            );
            repeat_eof(&mut stream, 1);
        }

        #[test]
        fn stream_partial_and_tail_failures_preserve_available_words_without_retry() {
            for (plans, tail, wanted) in [
                (
                    vec![Plan::Complete, Plan::Partial, Plan::Complete],
                    false,
                    vec![expected(1), expected(2), expected(3)],
                ),
                (
                    vec![Plan::Complete, Plan::Complete, Plan::Failed],
                    true,
                    vec![expected(1), expected(2)],
                ),
            ] {
                let (live, calls, flushes) = fixture(plans, tail);
                let (words, mut stream) = run(live, vec![vec![1.0; 1536]]);
                assert_eq!(words, wanted);
                assert_eq!(calls.load(Ordering::SeqCst), 3);
                assert_eq!(flushes.load(Ordering::SeqCst), 1);
                let completion = stream.completion().expect("tail failure must survive EOF");
                assert!(!completion.decode_complete());
                assert!(!completion.worker_failed());
                let lost = if tail { 2 } else { 1 };
                assert_eq!(
                    completion.decode_report().outcomes()[lost].span,
                    InputSpan {
                        start_frame: lost as u64 * 512,
                        end_frame: (lost as u64 + 1) * 512
                    }
                );
                repeat_eof(&mut stream, 1);
            }
        }

        #[test]
        fn stream_valid_empty_and_metadata_only_completion_remain_quiet() {
            let (live, calls, _) =
                fixture(vec![Plan::Complete, Plan::Empty, Plan::Complete], false);
            let (words, mut stream) = run(live, vec![vec![1.0; 1536]]);
            assert_eq!(words, [expected(1), expected(3)]);
            assert_eq!(calls.load(Ordering::SeqCst), 3);
            let completion = stream.completion().unwrap();
            assert!(completion.decode_complete());
            assert!(!completion.worker_failed());
            assert_eq!(
                completion.decode_report().outcomes()[1].status,
                RegionStatus::RecognizedEmpty
            );
            repeat_eof(&mut stream, 0);

            let (mut live, calls, _) = fixture(vec![Plan::Empty; 257], false);
            live.push(&vec![0.0; 257 * 512], 16_000);
            let (words, mut stream) = run(live, vec![]);
            assert!(words.is_empty());
            assert_eq!(calls.load(Ordering::SeqCst), 257);
            let completion = stream.completion().unwrap();
            assert!(completion.decode_complete());
            assert_eq!(completion.decode_report().omitted_metadata(), 1);
            assert_eq!(completion.decode_report().omitted_failures(), 0);
            assert_eq!(completion.decode_report().outcomes().len(), 256);
            repeat_eof(&mut stream, 0);
        }

        #[test]
        fn stream_same_push_panic_preserves_pending_words_reports_and_known_loss() {
            let (live, calls, flushes) =
                fixture(vec![Plan::Complete, Plan::Failed, Plan::Panic], false);
            let (words, mut stream) = run(live, vec![vec![1.0; 1536]]);
            assert_eq!(
                words,
                [expected(1)],
                "already decoded words must survive the same-call panic"
            );
            assert_eq!(calls.load(Ordering::SeqCst), 3);
            assert_eq!(
                flushes.load(Ordering::SeqCst),
                0,
                "do not resume/finish a panicked decoder"
            );
            let completion = stream.completion().unwrap();
            assert!(!completion.decode_complete());
            assert!(completion.worker_failed());
            let failures: Vec<_> = completion
                .decode_report()
                .outcomes()
                .iter()
                .filter(|outcome| !outcome.status.is_complete())
                .map(|outcome| outcome.span)
                .collect();
            assert_eq!(
                failures,
                [
                    InputSpan {
                        start_frame: 512,
                        end_frame: 1024
                    },
                    InputSpan {
                        start_frame: 1024,
                        end_frame: 1536
                    }
                ]
            );
            repeat_eof(&mut stream, 1);
        }

        #[test]
        fn stream_tail_panic_is_terminal_and_never_retries_the_backend() {
            let (live, calls, flushes) =
                fixture(vec![Plan::Complete, Plan::Complete, Plan::Panic], true);
            let (words, mut stream) = run(live, vec![vec![1.0; 1536]]);
            assert_eq!(words, [expected(1), expected(2)]);
            assert_eq!(calls.load(Ordering::SeqCst), 3);
            assert_eq!(flushes.load(Ordering::SeqCst), 1);
            let completion = stream.completion().unwrap();
            assert!(completion.worker_failed());
            assert!(!completion.decode_complete());
            assert_eq!(
                completion.decode_report().outcomes().last().unwrap().span,
                InputSpan {
                    start_frame: 1024,
                    end_frame: 1536
                }
            );
            repeat_eof(&mut stream, 1);
        }

        #[test]
        fn stream_unhandled_worker_panic_keeps_known_facts_and_reports_join_failure() {
            let state = Arc::new(Mutex::new(StreamState::default()));
            let worker_state = state.clone();
            let dropped = Arc::new(AtomicUsize::new(0));
            let (words_tx, words_rx) = tokio::sync::mpsc::unbounded_channel();
            let worker = std::thread::spawn(move || {
                let mut state = lock_state(&worker_state);
                state.report.record(RegionOutcome {
                    span: InputSpan {
                        start_frame: 0,
                        end_frame: 512,
                    },
                    status: RegionStatus::Complete,
                });
                state.input_frames = 1024;
                state.last_reported_input_frame = 512;
                words_tx.send(vec![expected(1)]).unwrap();
                // Exercise join failure and poisoned-state recovery outside guarded native calls.
                panic!("synthetic unhandled worker panic hook marker");
            });
            let mut stream = LiveWordStream {
                rx: words_rx,
                worker: Some(worker),
                completion: None,
                state,
                dropped,
            };
            assert_eq!(drain(&mut stream), [expected(1)]);
            let completion = stream.completion().unwrap();
            assert!(completion.worker_failed());
            assert!(!completion.decode_complete());
            assert_eq!(completion.decode_report().outcomes().len(), 2);
            assert_eq!(
                completion.decode_report().outcomes()[0].status,
                RegionStatus::Complete
            );
            assert_eq!(
                completion.decode_report().outcomes()[1].span,
                InputSpan {
                    start_frame: 512,
                    end_frame: 1024
                }
            );
            repeat_eof(&mut stream, 1);
        }

        #[test]
        fn stream_consumer_drop_stops_at_send_without_finish_or_extra_backend_calls() {
            let (live, calls, flushes) = fixture(vec![Plan::Complete], false);
            let (sink, mut stream) = live_word_stream(live);
            let worker = stream.worker.take().unwrap();
            drop(stream);
            sink.push(vec![1.0; 512], 16_000);
            worker.join().unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(flushes.load(Ordering::SeqCst), 0);
            sink.push(vec![1.0; 512], 16_000);
            assert_eq!(
                sink.dropped_chunks(),
                1,
                "disconnected sink retains its existing counter"
            );
        }

        #[test]
        fn stream_backlog_drops_are_visible_separately_from_admitted_decode_status() {
            let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
            let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
            let (live, calls, _) =
                fixture_with_block(vec![], false, Some((started_tx, Mutex::new(release_rx))));
            let (sink, mut stream) = live_word_stream(live);
            sink.push(vec![1.0; 512], 16_000);
            started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            for _ in 0..AUDIO_BACKLOG {
                sink.push(vec![1.0; 512], 16_000);
            }
            sink.push(vec![1.0; 512], 16_000);
            assert_eq!(sink.dropped_chunks(), 1);
            drop(sink);
            release_tx.send(()).unwrap();
            let words = drain(&mut stream);
            assert_eq!(words.len(), AUDIO_BACKLOG + 1);
            assert_eq!(calls.load(Ordering::SeqCst), AUDIO_BACKLOG + 1);
            let completion = stream.completion().unwrap();
            assert!(
                completion.decode_complete(),
                "only admitted input was decoded"
            );
            assert!(!completion.worker_failed());
            assert_eq!(completion.dropped_chunks(), 1);
            assert_eq!(completion.decode_report().omitted_failures(), 0);
            assert_eq!(
                completion
                    .decode_report()
                    .outcomes()
                    .last()
                    .unwrap()
                    .span
                    .end_frame,
                (AUDIO_BACKLOG as u64 + 1) * 512
            );
            repeat_eof(&mut stream, 1);
        }

        #[test]
        fn stream_report_merge_preserves_metadata_failure_priority_and_saturation() {
            let mut plans = vec![Plan::Complete; 256];
            plans.push(Plan::Failed);
            let (mut live, calls, _) = fixture(plans, false);
            live.push(&vec![1.0; 257 * 512], 16_000);
            let (words, mut stream) = run(live, vec![vec![1.0; 512]]);
            assert_eq!(words.len(), 257);
            assert_eq!(calls.load(Ordering::SeqCst), 258);
            let completion = stream.completion().unwrap();
            assert!(!completion.decode_complete());
            assert_eq!(completion.decode_report().outcomes().len(), 256);
            assert_eq!(completion.decode_report().omitted_metadata(), 2);
            assert_eq!(
                completion
                    .decode_report()
                    .outcomes()
                    .iter()
                    .filter(|o| !o.status.is_complete())
                    .count(),
                1
            );
            repeat_eof(&mut stream, 1);

            for failure in [false, true] {
                let (mut live, calls, _) = fixture(vec![], false);
                live.report.omit_metadata();
                if failure {
                    live.report.omit_failure();
                }
                for _ in 0..64 {
                    live.report.merge(live.report.clone());
                }
                let (_, mut stream) = run(live, vec![]);
                let completion = stream.completion().unwrap();
                assert_eq!(completion.decode_complete(), !failure);
                assert_eq!(completion.decode_report().omitted_metadata(), u64::MAX);
                assert!(completion.decode_report().metadata_counter_saturated());
                assert_eq!(
                    completion.decode_report().omitted_failures(),
                    if failure { u64::MAX } else { 0 }
                );
                assert_eq!(calls.load(Ordering::SeqCst), 0);
                repeat_eof(&mut stream, usize::from(failure));
            }
        }
    }
}

#[cfg(feature = "stream")]
pub use stream::{LiveSink, LiveStreamCompletion, LiveWordStream, live_word_stream};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asr::DecodedRegion;
    use corti_transcribe::decode::InputSpan;
    use sherpa_onnx::LinearResampler;

    #[derive(Default)]
    struct FakeSegmenter {
        position: u64,
        pending: Option<(u64, Vec<f32>)>,
        completed: std::collections::VecDeque<(u64, Vec<f32>)>,
        join: bool,
    }

    impl Segmenter for FakeSegmenter {
        fn terminal_guard(&self) -> Option<usize> {
            Some(0)
        }
        fn accept(&mut self, samples: &[f32]) {
            if self.join
                && let Some((_, pending)) = &mut self.pending
            {
                pending.extend_from_slice(samples);
            } else {
                if let Some(region) = self.pending.take() {
                    self.completed.push_back(region);
                }
                self.pending = Some((self.position, samples.to_vec()));
            }
            self.position += samples.len() as u64;
        }
        fn flush(&mut self) {
            if let Some(region) = self.pending.take() {
                self.completed.push_back(region);
            }
        }
        fn reset(&mut self) {
            self.position = 0;
            self.pending = None;
            self.completed.clear();
        }
        fn drain_completed(&mut self, visit: &mut dyn FnMut(u64, &[f32])) {
            while let Some((start, samples)) = self.completed.pop_front() {
                visit(start, &samples);
            }
        }
    }

    struct FakeDecoder {
        fail: Option<usize>,
        partial: bool,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl RegionDecoder for FakeDecoder {
        fn decode(&self, samples: &[f32], offset: f64) -> DecodedRegion {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            let blank = samples.iter().all(|sample| *sample == 0.0);
            let failed = self.fail == Some(call);
            let status = if failed && self.partial {
                RegionStatus::Partial(DecodeErrorCategory::OutputTruncated)
            } else if failed {
                RegionStatus::Failed(DecodeErrorCategory::BackendFailure)
            } else if blank {
                RegionStatus::RecognizedEmpty
            } else {
                RegionStatus::Complete
            };
            let words = if (failed && !self.partial) || blank {
                vec![]
            } else {
                vec![Word {
                    start: offset,
                    end: offset + samples.len() as f64 / 16_000.0,
                    text: format!("region {call}"),
                }]
            };
            DecodedRegion { words, status }
        }
    }

    fn fake_live(fail: Option<usize>, partial: bool, join: bool) -> LiveTranscriber {
        LiveTranscriber::with_parts(
            Arc::new(FakeDecoder {
                fail,
                partial,
                calls: std::sync::atomic::AtomicUsize::new(0),
            }),
            Box::new(FakeSegmenter {
                join,
                ..FakeSegmenter::default()
            }),
        )
    }

    #[test]
    fn failed_middle_region_keeps_exact_neighbors_and_flush_tail() {
        let samples = vec![1.0; VAD_WINDOW * 3];
        let mut baseline = fake_live(None, false, false);
        baseline.push(&samples, 16_000);
        let expected = baseline.finish();
        assert_eq!(expected.len(), 3);
        let mut failed = fake_live(Some(2), false, false);
        failed.push(&samples, 16_000);
        let first = failed.poll_words().unwrap();
        assert_eq!(first, expected[..1]);
        let tail = failed.finish();
        assert_eq!(tail, expected[2..]);
        let report = failed.take_decode_report();
        assert_eq!(
            report.outcomes(),
            &[
                RegionOutcome {
                    span: InputSpan {
                        start_frame: 0,
                        end_frame: 512
                    },
                    status: RegionStatus::Complete
                },
                RegionOutcome {
                    span: InputSpan {
                        start_frame: 512,
                        end_frame: 1024
                    },
                    status: RegionStatus::Failed(DecodeErrorCategory::BackendFailure)
                },
                RegionOutcome {
                    span: InputSpan {
                        start_frame: 1024,
                        end_frame: 1536
                    },
                    status: RegionStatus::Complete
                },
            ]
        );
        assert!(!report.is_complete());
        assert!(!failed.decode_complete());
        assert!(failed.take_decode_report().is_complete());
        assert!(!failed.decode_complete(), "draining cannot erase a failure");
        assert!(failed.finish().is_empty());
    }

    #[test]
    fn recognized_empty_and_partial_payload_remain_distinct() {
        let mut empty = fake_live(None, false, false);
        empty.push(&[0.0; VAD_WINDOW], 16_000);
        assert!(empty.finish().is_empty());
        assert!(empty.decode_complete());
        assert_eq!(
            empty.take_decode_report().outcomes()[0].status,
            RegionStatus::RecognizedEmpty
        );
        for amplitude in [0.0, 1.0] {
            let mut partial = fake_live(Some(1), true, false);
            partial.push(&[amplitude; VAD_WINDOW], 16_000);
            assert_eq!(partial.finish().len(), usize::from(amplitude != 0.0));
            assert!(!partial.decode_complete());
            assert_eq!(
                partial.take_decode_report().outcomes()[0].status,
                RegionStatus::Partial(DecodeErrorCategory::OutputTruncated)
            );
        }
    }

    #[test]
    fn batch_and_live_reports_use_original_source_frames() {
        let source = [1.0; VAD_WINDOW * 9];
        let pcm = engine::resample_to_16k(&source, 48_000).unwrap();
        let mut live = fake_live(Some(2), false, false);
        live.push(&source, 48_000);
        let live_words = live.finish();
        let mut batch = fake_live(Some(2), false, false);
        batch.push_resampled(&pcm, source.len(), 48_000);
        assert_eq!(batch.finish(), live_words);
        let expected = [
            RegionOutcome {
                span: InputSpan {
                    start_frame: 0,
                    end_frame: 1536,
                },
                status: RegionStatus::Complete,
            },
            RegionOutcome {
                span: InputSpan {
                    start_frame: 1536,
                    end_frame: 3072,
                },
                status: RegionStatus::Failed(DecodeErrorCategory::BackendFailure),
            },
            RegionOutcome {
                span: InputSpan {
                    start_frame: 3072,
                    end_frame: 4608,
                },
                status: RegionStatus::Complete,
            },
        ];
        assert_eq!(live.take_decode_report().outcomes(), &expected);
        assert_eq!(batch.take_decode_report().outcomes(), &expected);
        assert!(!batch.decode_complete());
    }

    #[test]
    fn cap_eviction_records_metadata_with_per_push_report_drains() {
        for amplitude in [0.0, 1.0] {
            let mut live = fake_live(None, false, false);
            let mut metadata = 0;
            let mut outcomes = 0;
            let mut words = Vec::new();
            for index in 0..257 {
                let rate = if index % 2 == 0 { 16_000 } else { 48_000 };
                live.push(&vec![amplitude; 512 * rate as usize / 16_000], rate);
                words.extend(live.poll_words().unwrap_or_default());
                let report = live.take_decode_report();
                assert!(report.is_complete());
                assert_eq!(report.omitted_failures(), 0);
                metadata += report.omitted_metadata();
                outcomes += report.outcomes().len();
            }
            words.extend(live.finish());
            let tail = live.take_decode_report();
            metadata += tail.omitted_metadata();
            outcomes += tail.outcomes().len();
            assert!(tail.is_complete());
            assert_eq!(metadata, 1, "one removed rate epoch is one metadata fact");
            assert_eq!(outcomes, 257);
            assert_eq!(words.len(), if amplitude == 0.0 { 0 } else { 257 });
            if amplitude != 0.0 {
                assert_eq!(words[0].start, 0.0);
                assert_eq!(words.last().unwrap().end, 257.0 * 512.0 / 16_000.0);
            }
            assert!(live.decode_complete());
            assert_eq!(live.take_decode_report().omitted_metadata(), 0);
        }
    }

    #[test]
    fn cap_evicted_long_success_keeps_words_and_counts_distinct_metadata_facts() {
        for amplitude in [0.0, 1.0] {
            let mut live = fake_live(None, false, true);
            let mut metadata = 0;
            for index in 0..257 {
                let rate = if index % 2 == 0 { 16_000 } else { 48_000 };
                live.push(&vec![amplitude; 16 * rate as usize / 16_000], rate);
                let report = live.take_decode_report();
                assert!(report.is_complete());
                metadata += report.omitted_metadata();
            }
            let words = live.finish();
            let report = live.take_decode_report();
            assert!(
                live.decode_complete(),
                "mapping metadata loss is not failed decoding"
            );
            assert!(report.is_complete());
            assert_eq!(report.omitted_failures(), 0);
            assert_eq!(metadata, 1, "eviction is reported when it happens");
            assert_eq!(
                report.omitted_metadata(),
                1,
                "successful outcome lacks its exact source span"
            );
            assert!(report.outcomes().is_empty());
            assert_eq!(words.len(), usize::from(amplitude != 0.0));
            if amplitude != 0.0 {
                assert_eq!(words[0].start, 0.0);
                assert_eq!(words[0].end, 0.257);
            }
        }
    }

    #[test]
    fn cap_evicted_failed_or_partial_region_still_has_processing_loss() {
        for partial in [false, true] {
            let mut live = fake_live(Some(1), partial, true);
            for index in 0..257 {
                let rate = if index % 2 == 0 { 16_000 } else { 48_000 };
                live.push(&vec![1.0; 16 * rate as usize / 16_000], rate);
            }
            let words = live.finish();
            let report = live.take_decode_report();
            assert!(!live.decode_complete());
            assert!(!report.is_complete());
            assert_eq!(report.omitted_failures(), 1);
            assert_eq!(
                report.omitted_metadata(),
                1,
                "the earlier eviction remains independent"
            );
            assert_eq!(words.len(), usize::from(partial));
            if partial {
                assert_eq!((words[0].start, words[0].end), (0.0, 0.257));
            }
        }
    }

    #[test]
    fn invalid_native_coordinates_never_decode_or_publish_fabricated_times() {
        struct InvalidSegmenter {
            start: u64,
            pcm: Vec<f32>,
        }
        impl Segmenter for InvalidSegmenter {
            fn accept(&mut self, samples: &[f32]) {
                self.pcm.extend_from_slice(samples);
            }
            fn flush(&mut self) {}
            fn reset(&mut self) {}
            fn drain_completed(&mut self, visit: &mut dyn FnMut(u64, &[f32])) {
                if !self.pcm.is_empty() {
                    visit(self.start, &std::mem::take(&mut self.pcm));
                }
            }
        }
        for (base, start) in [(0, u64::MAX), (0, u64::MAX - 8), (u64::MAX - 1, 2), (0, 1)] {
            let decoder = Arc::new(FakeDecoder {
                fail: None,
                partial: false,
                calls: std::sync::atomic::AtomicUsize::new(0),
            });
            let mut live = LiveTranscriber::with_parts(
                decoder.clone(),
                Box::new(InvalidSegmenter { start, pcm: vec![] }),
            );
            live.vad_base_samples = base;
            live.push(&[1.0; 512], 16_000);
            let words = live.finish();
            assert!(
                words.is_empty(),
                "invalid coordinates must not publish words: base={base}, start={start}"
            );
            assert_eq!(decoder.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert!(!live.decode_complete());
            let report = live.take_decode_report();
            assert_eq!(report.omitted_failures(), 1);
            assert_eq!(report.omitted_metadata(), 0);
        }
    }

    #[test]
    fn resampler_failure_preserves_fractional_time_across_recovery_checkpoint_and_rate_change() {
        for chunks in [vec![44_101], vec![1; 44_101], vec![13, 509, 43_579]] {
            let available = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let factory_available = available.clone();
            let mut live = fake_live(None, false, true);
            live.resampler_factory = Box::new(move |rate| {
                factory_available
                    .load(std::sync::atomic::Ordering::SeqCst)
                    .then(|| Resampler::new(rate))
                    .flatten()
            });
            // Valid pending VAD audio must be flushed before the missing chunk and never bridge it.
            live.push(&[1.0; 123], 16_000);
            for chunk in chunks {
                live.push(&vec![1.0; chunk], 44_100);
            }
            assert_eq!(
                live.checkpoint(),
                vec![Word {
                    start: 0.0,
                    end: 123.0 / 16_000.0,
                    text: "region 1".into()
                }]
            );
            assert_eq!(live.win.fed, 16_123);
            available.store(true, std::sync::atomic::Ordering::SeqCst);
            live.push(&vec![1.0; 44_100], 44_100);
            let recovered = live.checkpoint();
            let recovered_start = 16_123.0 / 16_000.0 + 1.0 / 44_100.0;
            assert_eq!(
                recovered,
                vec![Word {
                    start: recovered_start,
                    end: recovered_start + 1.0,
                    text: "region 2".into()
                }]
            );
            // A different-rate skip consumes the previous fractional remainder exactly.
            available.store(false, std::sync::atomic::Ordering::SeqCst);
            live.push(&[1.0; 2], 48_000);
            assert_eq!(live.win.fed, 32_124);
            let losses = live.take_decode_report();
            assert!(!losses.is_complete());
            assert!(losses.outcomes().iter().any(|outcome| outcome.status
                == RegionStatus::Failed(DecodeErrorCategory::ResamplerUnavailable)));
            live.push(&[1.0; VAD_WINDOW], 16_000);
            let final_start = 32_124.0 / 16_000.0 + 13.0 / (441.0 * 16_000.0);
            assert_eq!(
                live.finish(),
                vec![Word {
                    start: final_start,
                    end: final_start + VAD_WINDOW as f64 / 16_000.0,
                    text: "region 3".into()
                }]
            );
            let report = live.take_decode_report();
            assert!(report.is_complete());
            assert!(!live.decode_complete());
            assert_eq!(
                report.outcomes(),
                &[RegionOutcome {
                    span: InputSpan {
                        start_frame: 88_326,
                        end_frame: 88_838
                    },
                    status: RegionStatus::Complete,
                }]
            );
        }
    }

    #[test]
    fn old_native_resampler_and_vad_tail_flush_before_failed_rate_switch() {
        let mut live = fake_live(None, false, true);
        live.resampler_factory = Box::new(|rate| {
            if rate == 44_100 {
                None
            } else {
                Resampler::new(rate)
            }
        });
        live.push(&[1.0; 513], 48_000);
        live.push(&vec![1.0; 44_100], 44_100);
        assert_eq!(
            live.poll_words().unwrap(),
            vec![Word {
                start: 0.0,
                end: 171.0 / 16_000.0,
                text: "region 1".into()
            }]
        );
        live.push(&[1.0; 512], 16_000);
        assert_eq!(
            live.finish(),
            vec![Word {
                start: 16_171.0 / 16_000.0,
                end: 16_171.0 / 16_000.0 + 512.0 / 16_000.0,
                text: "region 2".into()
            }]
        );
        assert_eq!(
            live.take_decode_report().outcomes(),
            &[
                RegionOutcome {
                    span: InputSpan {
                        start_frame: 0,
                        end_frame: 513
                    },
                    status: RegionStatus::Complete
                },
                RegionOutcome {
                    span: InputSpan {
                        start_frame: 513,
                        end_frame: 44_613
                    },
                    status: RegionStatus::Failed(DecodeErrorCategory::ResamplerUnavailable)
                },
                RegionOutcome {
                    span: InputSpan {
                        start_frame: 44_613,
                        end_frame: 45_125
                    },
                    status: RegionStatus::Complete
                },
            ]
        );
        assert!(!live.decode_complete());
    }

    #[test]
    fn fractional_skipped_duration_is_visible_immediately_without_a_later_clock_jump() {
        let mut live = fake_live(None, false, true);
        live.resampler_factory = Box::new(|_| None);
        live.push(&[1.0], 44_100);
        live.push(&[1.0; 512], 16_000);
        let first_start = 1.0 / 44_100.0;
        assert_eq!(
            live.checkpoint(),
            vec![Word {
                start: first_start,
                end: first_start + 512.0 / 16_000.0,
                text: "region 1".into()
            }]
        );
        live.push(&[1.0; 512], 16_000);
        let second_start = 512.0 / 16_000.0 + first_start;
        assert_eq!(
            live.checkpoint(),
            vec![Word {
                start: second_start,
                end: second_start + 512.0 / 16_000.0,
                text: "region 2".into()
            }]
        );
        live.push(&[1.0; 2], 48_000);
        live.push(&[1.0; 512], 16_000);
        let third_start = 1025.0 / 16_000.0 + 13.0 / (441.0 * 16_000.0);
        assert_eq!(
            live.finish(),
            vec![Word {
                start: third_start,
                end: third_start + 512.0 / 16_000.0,
                text: "region 3".into()
            }]
        );
        let report = live.take_decode_report();
        assert_eq!(
            report.outcomes()[1].span,
            InputSpan {
                start_frame: 1,
                end_frame: 513
            }
        );
        assert_eq!(
            report.outcomes()[4].span,
            InputSpan {
                start_frame: 1027,
                end_frame: 1539
            }
        );
        assert!(!live.decode_complete());
    }

    #[test]
    fn unavailable_clock_cannot_emit_later_words_at_compressed_timestamps() {
        // Invalid rate and a deliberately unrepresentable reduced denominator both fail closed.
        for rates in [
            vec![0],
            vec![
                1_000_000_007,
                1_000_000_009,
                1_000_000_021,
                1_000_000_033,
                1_000_000_087,
            ],
        ] {
            let mut live = fake_live(None, false, true);
            live.resampler_factory = Box::new(|_| None);
            live.push(&[1.0; 123], 16_000);
            for rate in &rates {
                live.push(&[1.0], *rate);
            }
            assert!(!live.clock_available);
            assert_eq!(
                live.poll_words().unwrap(),
                vec![Word {
                    start: 0.0,
                    end: 123.0 / 16_000.0,
                    text: "region 1".into(),
                }]
            );
            live.push(&[1.0; 512], 16_000);
            assert!(live.finish().is_empty());
            assert_eq!(live.input_clock.frames, 635 + rates.len() as u64);
            let report = live.take_decode_report();
            assert!(!report.is_complete());
            assert_eq!(
                report.outcomes()[0],
                RegionOutcome {
                    span: InputSpan {
                        start_frame: 0,
                        end_frame: 123
                    },
                    status: RegionStatus::Complete,
                }
            );
            assert_eq!(report.omitted_failures(), 2);
            assert!(!live.decode_complete());
        }
    }

    fn verify_engine(dir: &std::path::Path) -> (crate::models::Models, Arc<Asr>) {
        let engine = std::env::var("CORTI_VERIFY_ASR_ENGINE").unwrap_or_else(|_| "sherpa".into());
        let m = crate::models::discover_for(dir, engine != "ggml", false, "titanet")
            .expect("discover models");
        let rec = match engine.as_str() {
            "sherpa" => {
                Asr::Sherpa(engine::build_recognizer(&m, 4, None, None, None).expect("recognizer"))
            }
            #[cfg(feature = "ggml")]
            "ggml" => {
                let path = crate::ggml::resolve_gguf(None, dir).expect("resolve GGUF");
                Asr::Ggml(crate::ggml::GgmlAsr::load(&path, 4).expect("GGML recognizer"))
            }
            #[cfg(not(feature = "ggml"))]
            "ggml" => panic!("re-run with --features ggml"),
            other => panic!("unknown CORTI_VERIFY_ASR_ENGINE {other:?}"),
        };
        (m, Arc::new(rec))
    }

    fn verify_audio() -> (Vec<f32>, u32) {
        let wav = std::path::PathBuf::from(
            std::env::var("CORTI_VERIFY_WAV").expect("set CORTI_VERIFY_WAV to a speech WAV"),
        );
        let mut reader = hound::WavReader::open(&wav).expect("open WAV");
        let spec = reader.spec();
        let interleaved: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap()).collect(),
            hound::SampleFormat::Int => {
                let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
                reader
                    .samples::<i32>()
                    .map(|s| s.unwrap() as f32 / max)
                    .collect()
            }
        };
        let channels = spec.channels as usize;
        (
            interleaved.iter().step_by(channels).copied().collect(),
            spec.sample_rate,
        )
    }

    struct NativeDecodeReceipt {
        offset: f64,
        pcm: Vec<u32>,
        words: Vec<Word>,
    }

    struct WrappedNativeDecoder {
        native: Arc<Asr>,
        fail_middle: bool,
        receipts: std::sync::Mutex<Vec<NativeDecodeReceipt>>,
    }

    impl RegionDecoder for WrappedNativeDecoder {
        fn decode(&self, samples: &[f32], offset: f64) -> DecodedRegion {
            let mut receipts = self.receipts.lock().unwrap();
            let decoded = if self.fail_middle && receipts.len() == 1 {
                DecodedRegion {
                    words: vec![],
                    status: RegionStatus::Failed(DecodeErrorCategory::BackendFailure),
                }
            } else {
                self.native.decode(samples, offset)
            };
            receipts.push(NativeDecodeReceipt {
                offset,
                pcm: samples.iter().map(|v| v.to_bits()).collect(),
                words: decoded.words.clone(),
            });
            decoded
        }
    }

    #[test]
    #[ignore = "needs pinned ASR/VAD models and authored say fixture; real Metal required for ggml"]
    fn native_middle_region_failure_preserves_exact_neighbors() {
        let dir = std::path::PathBuf::from(
            std::env::var("CORTI_VERIFY_MODEL_DIR").expect("model directory"),
        );
        let (mono, rate) = verify_audio();
        let (models, rec) = verify_engine(&dir);
        assert!(
            mono.len() < rate as usize * 18,
            "authored utterance must fit one VAD region"
        );
        // Reuse only the authored synthetic fixture, with a two-second separating pause.
        let mut three = Vec::new();
        for _ in 0..3 {
            three.extend_from_slice(&mono);
            three.resize(three.len() + rate as usize * 2, 0.0);
        }
        let baseline = Arc::new(WrappedNativeDecoder {
            native: rec.clone(),
            fail_middle: false,
            receipts: std::sync::Mutex::new(vec![]),
        });
        let mut live = LiveTranscriber::with_parts(
            baseline.clone(),
            Box::new(engine::build_vad(&models, 0.5, 1.0).expect("baseline VAD")),
        );
        live.terminal.guard = Some(terminal::PINNED_GUARD);
        live.push(&three, rate);
        let baseline_words = live.finish();
        assert!(live.decode_complete());
        let baseline_report = live.take_decode_report();
        let expected = baseline.receipts.lock().unwrap();
        assert_eq!(
            expected.len(),
            3,
            "fixture must produce exactly three regions"
        );
        assert!(expected.iter().all(|region| !region.words.is_empty()));
        assert_eq!(
            baseline_words,
            expected
                .iter()
                .flat_map(|region| region.words.clone())
                .collect::<Vec<_>>()
        );

        let injected = Arc::new(WrappedNativeDecoder {
            native: rec,
            fail_middle: true,
            receipts: std::sync::Mutex::new(vec![]),
        });
        let mut live = LiveTranscriber::with_parts(
            injected.clone(),
            Box::new(engine::build_vad(&models, 0.5, 1.0).expect("injected VAD")),
        );
        live.terminal.guard = Some(terminal::PINNED_GUARD);
        live.push(&three, rate);
        let surviving = live.finish();
        let actual = injected.receipts.lock().unwrap();
        assert_eq!(actual.len(), 3);
        for (baseline, injected) in expected.iter().zip(actual.iter()) {
            assert_eq!(baseline.offset, injected.offset);
            assert_eq!(
                baseline.pcm, injected.pcm,
                "region PCM identity must not change"
            );
        }
        assert_eq!(actual[0].words, expected[0].words);
        assert_eq!(actual[2].words, expected[2].words);
        assert!(actual[1].words.is_empty());
        assert_eq!(
            surviving,
            [expected[0].words.clone(), expected[2].words.clone()].concat()
        );
        let report = live.take_decode_report();
        assert_eq!(report.outcomes().len(), 3);
        assert_eq!(report.outcomes()[0], baseline_report.outcomes()[0]);
        assert_eq!(report.outcomes()[2], baseline_report.outcomes()[2]);
        assert_eq!(
            report.outcomes()[1],
            RegionOutcome {
                span: baseline_report.outcomes()[1].span,
                status: RegionStatus::Failed(DecodeErrorCategory::BackendFailure)
            }
        );
        assert!(!report.is_complete());
        assert!(!live.decode_complete());
        eprintln!(
            "NATIVE_DECODE_FAILURE regions=3 failed=2 exact_neighbor_words=true exact_pcm=true rate={rate}"
        );
    }

    // Diagnostics for the unchanged exact native equality gate. These helpers run only after a
    // mismatch, publish counts/timing/hashes rather than PCM or recognized content, and cap their
    // additional retained audio independently of a manually supplied verification WAV.
    const DIAGNOSTIC_MAX_SOURCE_SAMPLES: usize = 4 * 1024 * 1024;
    const DIAGNOSTIC_MAX_SECONDS: usize = 120;
    const DIAGNOSTIC_MAX_REGIONS: usize = 512;
    const DIAGNOSTIC_MAX_REGION_SAMPLES: usize = 22 * TARGET_RATE as usize;
    const DIAGNOSTIC_MAX_VAD_SAMPLES: usize = 2_000_000;
    const VERIFY_CHUNKS: [usize; 6] = [377, 512, 100, 999, 1, 4096];

    fn sample_hash(samples: &[f32]) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        for sample in samples {
            sample.to_bits().hash(&mut hash);
        }
        hash.finish()
    }

    fn word_hash(words: &[Word]) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        for word in words {
            word.start.to_bits().hash(&mut hash);
            word.end.to_bits().hash(&mut hash);
            word.text.hash(&mut hash);
        }
        hash.finish()
    }

    fn diagnostic_resample(samples: &[f32], rate: u32, chunked: bool) -> Vec<f32> {
        if rate == TARGET_RATE as u32 {
            return samples.to_vec();
        }
        let resampler =
            LinearResampler::create(rate as i32, TARGET_RATE).expect("diagnostic resampler");
        let mut output = Vec::new();
        if chunked {
            let mut offset = 0;
            for size in VERIFY_CHUNKS.into_iter().cycle() {
                if offset == samples.len() {
                    break;
                }
                let end = (offset + size).min(samples.len());
                output.extend(resampler.resample(&samples[offset..end], false));
                offset = end;
            }
        } else {
            output.extend(resampler.resample(samples, false));
        }
        output.extend(resampler.resample(&[], true));
        output
    }

    #[derive(Debug, PartialEq)]
    struct RegionDigest {
        start: i32,
        samples: usize,
        hash: u64,
    }

    struct RegionSnapshot {
        digest: RegionDigest,
        samples: Vec<f32>,
    }

    fn exact_regions_equal(left: &[RegionSnapshot], right: &[RegionSnapshot]) -> bool {
        left.len() == right.len()
            && left.iter().zip(right).all(|(left, right)| {
                left.digest.start == right.digest.start
                    && left.samples.len() == right.samples.len()
                    && left
                        .samples
                        .iter()
                        .zip(&right.samples)
                        .all(|(left, right)| left.to_bits() == right.to_bits())
            })
    }

    #[test]
    fn diagnostic_region_equality_checks_bits_and_spans_even_when_hashes_match() {
        let snapshot = |start, samples: Vec<f32>| RegionSnapshot {
            digest: RegionDigest {
                start,
                samples: samples.len(),
                hash: 7,
            },
            samples,
        };
        let original = [snapshot(5, vec![0.0, 1.0])];
        assert!(exact_regions_equal(
            &original,
            &[snapshot(5, vec![0.0, 1.0])]
        ));
        assert!(!exact_regions_equal(
            &original,
            &[snapshot(6, vec![0.0, 1.0])]
        ));
        assert!(!exact_regions_equal(
            &original,
            &[snapshot(5, vec![-0.0, 1.0])]
        ));
        assert!(!exact_regions_equal(&original, &[snapshot(5, vec![0.0])]));
    }

    fn diagnostic_regions(
        samples: &[f32],
        models: &crate::models::Models,
        label: &str,
    ) -> Option<Vec<RegionSnapshot>> {
        let vad = engine::build_vad(models, 0.5, 1.0).expect("diagnostic VAD");
        let mut regions = Vec::new();
        let mut retained_samples = 0usize;
        let mut omitted = 0usize;
        let mut drain = || {
            while let Some(region) = vad.front() {
                let samples = region.samples();
                let digest = RegionDigest {
                    start: region.start(),
                    samples: samples.len(),
                    hash: sample_hash(samples),
                };
                if regions.len() < DIAGNOSTIC_MAX_REGIONS
                    && samples.len() <= DIAGNOSTIC_MAX_VAD_SAMPLES - retained_samples
                {
                    retained_samples += samples.len();
                    eprintln!("CHUNK_DIAGNOSTIC vad={label} region={digest:?}");
                    regions.push(RegionSnapshot {
                        digest,
                        samples: samples.to_vec(),
                    });
                } else {
                    omitted += 1;
                }
                vad.pop();
            }
        };
        for window in samples.chunks(VAD_WINDOW) {
            vad.accept_waveform(window);
            drain();
        }
        vad.flush();
        drain();
        eprintln!(
            "CHUNK_DIAGNOSTIC vad={label} regions={} omitted={omitted}",
            regions.len()
        );
        (omitted == 0).then_some(regions)
    }

    fn diagnose_chunking(
        mono: &[f32],
        rate: u32,
        models: &crate::models::Models,
        rec: &Arc<Asr>,
        model_dir: &std::path::Path,
    ) {
        if !(8_000..=192_000).contains(&rate)
            || mono.len() > DIAGNOSTIC_MAX_SOURCE_SAMPLES
            || mono.len() > rate as usize * DIAGNOSTIC_MAX_SECONDS
        {
            eprintln!("CHUNK_DIAGNOSTIC skipped=fixture_exceeds_diagnostic_bound");
            return;
        }
        let started = std::time::Instant::now();
        let whole = diagnostic_resample(mono, rate, false);
        let chunked = diagnostic_resample(mono, rate, true);
        let mut first_delta = None;
        let mut differing = 0usize;
        let mut max_abs_delta = 0.0f32;
        for (index, (left, right)) in whole.iter().zip(&chunked).enumerate() {
            if left.to_bits() != right.to_bits() {
                first_delta.get_or_insert(index);
                differing += 1;
                max_abs_delta = max_abs_delta.max((left - right).abs());
            }
        }
        let unmatched_samples = whole.len().abs_diff(chunked.len());
        let pcm_equal = unmatched_samples == 0 && differing == 0;
        eprintln!(
            "CHUNK_DIAGNOSTIC rate={rate} whole_samples={} chunked_samples={} whole_hash={:016x} chunked_hash={:016x} pcm_equal={pcm_equal} differing={differing} unmatched_samples={unmatched_samples} first_delta={first_delta:?} max_abs_delta={max_abs_delta:e}",
            whole.len(),
            chunked.len(),
            sample_hash(&whole),
            sample_hash(&chunked),
        );
        let whole_regions = diagnostic_regions(&whole, models, "whole");
        let chunked_regions = diagnostic_regions(&chunked, models, "chunked");
        let regions_equal = whole_regions
            .as_ref()
            .zip(chunked_regions.as_ref())
            .map(|(whole, chunked)| exact_regions_equal(whole, chunked));
        // This replay isolates native resampling/VAD. Equality here does not prove the application's
        // actual feed_16k glue correct; that boundary remains a hypothesis if every replay stage agrees.
        eprintln!("CHUNK_DIAGNOSTIC replay_regions_equal={regions_equal:?}");

        if let Some(region) = whole_regions
            .as_ref()
            .and_then(|regions| regions.first())
            .filter(|region| region.samples.len() <= DIAGNOSTIC_MAX_REGION_SAMPLES)
        {
            let region = &region.samples;
            let first = rec.asr_segment(region, 0.0);
            let repeated = rec.asr_segment(region, 0.0);
            eprintln!(
                "CHUNK_DIAGNOSTIC repeated_region_equal={} first_words={} repeated_words={} first_hash={:016x} repeated_hash={:016x}",
                first == repeated,
                first.len(),
                repeated.len(),
                word_hash(&first),
                word_hash(&repeated),
            );
            if first != repeated {
                let (_, fresh) = verify_engine(model_dir);
                let fresh_words = fresh.asr_segment(region, 0.0);
                eprintln!(
                    "CHUNK_DIAGNOSTIC fresh_region_equal={} fresh_words={} fresh_hash={:016x}",
                    first == fresh_words,
                    fresh_words.len(),
                    word_hash(&fresh_words),
                );
            }
            match rec.as_ref() {
                Asr::Sherpa(sherpa) => {
                    let stream = sherpa.create_stream();
                    stream.accept_waveform(TARGET_RATE, region);
                    sherpa.decode(&stream);
                    if let Some(result) = stream.get_result() {
                        eprintln!(
                            "CHUNK_DIAGNOSTIC tokens={} sentencepiece_boundaries={} ascii_space_boundaries={} timestamp_count={} duration_count={}",
                            result.tokens.len(),
                            result
                                .tokens
                                .iter()
                                .filter(|token| token.starts_with('\u{2581}'))
                                .count(),
                            result
                                .tokens
                                .iter()
                                .filter(|token| token.starts_with(' '))
                                .count(),
                            result.timestamps.as_ref().map_or(0, Vec::len),
                            result.durations.as_ref().map_or(0, Vec::len),
                        );
                    } else {
                        eprintln!("CHUNK_DIAGNOSTIC token_result=absent");
                    }
                }
                #[cfg(feature = "ggml")]
                Asr::Ggml(_) => {}
            }
        }
        eprintln!(
            "CHUNK_DIAGNOSTIC elapsed_ms={}",
            started.elapsed().as_millis()
        );
    }

    /// A window that spans two pushes is still released as one 512-sample window: the boundary remainder is
    /// carried, not dropped or short-fed. Pure — no models.
    #[test]
    fn windows_span_push_boundaries() {
        let mut wb = WindowBuffer::default();

        // 600 samples → one full window ready, 88 carried.
        assert_eq!(wb.extend(&vec![0.0; 600]), 1);
        assert_eq!(wb.take_windows().len(), VAD_WINDOW);
        assert_eq!(wb.buf.len(), 88);

        // +500 → 588 buffered → one more full window, 76 carried.
        assert_eq!(wb.extend(&vec![0.0; 500]), 1);
        assert_eq!(wb.take_windows().len(), VAD_WINDOW);
        assert_eq!(wb.buf.len(), 76);

        // Absolute time base counts only released full-window samples so far.
        assert_eq!(wb.fed, (2 * VAD_WINDOW) as u64);

        // Finish releases the trailing partial and accounts for it.
        let remainder = wb.take_remainder();
        assert_eq!(remainder.len(), 76);
        assert_eq!(wb.fed, (2 * VAD_WINDOW) as u64 + 76);
    }

    #[test]
    fn continuous_synthetic_speech_windowing_stays_bounded_beyond_twenty_seconds() {
        let mut wb = WindowBuffer::default();
        let chunk = vec![0.1; 160];
        let chunks = TARGET_RATE as usize * 25 / chunk.len();
        let mut max_retained = 0usize;
        for _ in 0..chunks {
            if wb.extend(&chunk) > 0 {
                let released = wb.take_windows();
                assert_eq!(released.len() % VAD_WINDOW, 0);
            }
            max_retained = max_retained.max(wb.buf.len());
        }
        assert!(max_retained < VAD_WINDOW);
        assert!(wb.buf.capacity() <= VAD_WINDOW * 2);
        assert!(wb.fed >= (TARGET_RATE as u64 * 25).saturating_sub(VAD_WINDOW as u64));
    }

    #[test]
    fn checkpoint_epoch_offsets_remain_call_relative() {
        let minute = TARGET_RATE as u64 * 60;
        assert_eq!(absolute_offset_sec(0, minute), 60.0);
        assert_eq!(absolute_offset_sec(minute, TARGET_RATE as u64 / 2), 60.5);
        assert_eq!(absolute_offset_sec(minute * 2, 0), 120.0);
    }

    #[test]
    fn resampler_checkpoint_epochs_do_not_accumulate_time_drift() {
        let one_second = vec![0.0f32; 48_000];
        let mut emitted = 0usize;
        for _ in 0..180 {
            let mut resampler = Resampler::new(48_000).unwrap();
            emitted += resampler.push(&one_second).len();
            emitted += resampler.finish().len();
        }
        assert_eq!(
            emitted,
            TARGET_RATE as usize * 180,
            "restarting at durability boundaries must not drift the call-relative clock"
        );
    }

    /// Many sub-window pushes accumulate without releasing a window until the total crosses 512, and the
    /// fed-counter only advances on release — offset accounting across pushes.
    #[test]
    fn tiny_pushes_accumulate_then_release() {
        let mut wb = WindowBuffer::default();
        for _ in 0..5 {
            // 5 × 100 = 500 < 512 → nothing releasable yet, counter stays 0.
            assert_eq!(wb.extend(&vec![0.0; 100]), 0);
        }
        assert_eq!(wb.fed, 0);
        // One more push tips it over 512.
        assert_eq!(wb.extend(&vec![0.0; 100]), 1); // 600 total
        assert_eq!(wb.take_windows().len(), VAD_WINDOW);
        assert_eq!(wb.fed, VAD_WINDOW as u64);
        assert_eq!(wb.buf.len(), 600 - VAD_WINDOW);
    }

    /// The whole point of the carry: concatenating the released windows + final remainder across an
    /// arbitrary push split reproduces the input exactly (no sample lost or duplicated at a seam).
    #[test]
    fn released_windows_plus_remainder_reconstruct_input() {
        let input: Vec<f32> = (0..1400).map(|i| i as f32).collect();
        let mut wb = WindowBuffer::default();
        let mut released: Vec<f32> = Vec::new();
        // Irregular push sizes straddling window boundaries.
        for chunk in [&input[..300], &input[300..800], &input[800..1400]] {
            if wb.extend(chunk) > 0 {
                released.extend(wb.take_windows());
            }
        }
        released.extend(wb.take_remainder());
        assert_eq!(released, input);
        assert_eq!(wb.fed, input.len() as u64);
    }

    /// Live-vs-batch equivalence on a real recording: feeding a WAV in small, boundary-straddling chunks
    /// yields exactly the same words as one whole-channel push (which is the batch path). Gated — needs the
    /// real Parakeet + Silero models and a speech WAV. Set `CORTI_VERIFY_ASR_ENGINE=ggml` and add
    /// `--features ggml` to exercise the transcribe.cpp/Metal arm:
    ///   CORTI_VERIFY_MODEL_DIR=~/Library/Caches/corti/models CORTI_VERIFY_WAV=/path/to/audio.wav \
    ///     cargo test -p corti-transcribe-local --features ggml live_equals_batch -- --ignored --nocapture
    #[test]
    #[ignore = "needs real ASR/VAD models + a speech WAV; set CORTI_VERIFY_MODEL_DIR and CORTI_VERIFY_WAV"]
    fn live_equals_batch_over_chunking() {
        let dir = std::path::PathBuf::from(
            std::env::var("CORTI_VERIFY_MODEL_DIR")
                .expect("set CORTI_VERIFY_MODEL_DIR to the model cache dir"),
        );
        let (mono, rate) = verify_audio();
        let (m, rec) = verify_engine(&dir);

        // Whole-channel live push, compared with both irregular pushes and the real batch entry below.
        let mut whole = LiveTranscriber::from_pinned_vad(
            rec.clone(),
            engine::build_vad(&m, 0.5, 1.0).expect("vad"),
        );
        whole.push(&mono, rate);
        let words_whole = whole.finish();

        // Same audio in irregular chunks straddling 512-sample window boundaries.
        let mut chunked = LiveTranscriber::from_pinned_vad(
            rec.clone(),
            engine::build_vad(&m, 0.5, 1.0).expect("vad"),
        );
        let mut i = 0;
        for (n, step) in VERIFY_CHUNKS.iter().cloned().cycle().enumerate() {
            if i >= mono.len() {
                break;
            }
            let end = (i + step).min(mono.len());
            chunked.push(&mono[i..end], rate);
            let _ = n;
            i = end;
        }
        let words_chunked = chunked.finish();

        if words_whole != words_chunked {
            diagnose_chunking(&mono, rate, &m, &rec, &dir);
        }
        assert_eq!(
            words_whole, words_chunked,
            "chunked push must equal whole-channel push"
        );
        let batch_pcm = engine::resample_to_16k(&mono, rate as i32).expect("batch resampler");
        let words_batch = engine::transcribe_channel(
            rec.clone(),
            engine::build_vad(&m, 0.5, 1.0).expect("batch vad"),
            &batch_pcm,
        );
        assert_eq!(
            words_whole, words_batch,
            "batch preprocessing must equal live source-rate input"
        );
        // Quantify the intentional numerical change against the old one-shot native call. This is
        // reporting, not an equality requirement: caller-dependent rounding is the defect being fixed.
        if rate != TARGET_RATE as u32
            && (8_000..=192_000).contains(&rate)
            && mono.len() <= DIAGNOSTIC_MAX_SOURCE_SAMPLES
            && mono.len() <= rate as usize * DIAGNOSTIC_MAX_SECONDS
        {
            let legacy = LinearResampler::create(rate as i32, TARGET_RATE)
                .expect("legacy resampler")
                .resample(&mono, true);
            let mut differing = 0usize;
            let mut max_abs_delta = 0.0f32;
            for (old, canonical) in legacy.iter().zip(&batch_pcm) {
                if old.to_bits() != canonical.to_bits() {
                    differing += 1;
                    max_abs_delta = max_abs_delta.max((old - canonical).abs());
                }
            }
            let legacy_words = engine::transcribe_channel(
                rec,
                engine::build_vad(&m, 0.5, 1.0).expect("legacy vad"),
                &legacy,
            );
            eprintln!(
                "RESAMPLE_BASELINE rate={rate} legacy_samples={} canonical_samples={} differing={differing} unmatched_samples={} max_abs_delta={max_abs_delta:e} legacy_words={} canonical_words={} words_equal={} legacy_word_hash={:016x} canonical_word_hash={:016x}",
                legacy.len(),
                batch_pcm.len(),
                legacy.len().abs_diff(batch_pcm.len()),
                legacy_words.len(),
                words_batch.len(),
                legacy_words == words_batch,
                word_hash(&legacy_words),
                word_hash(&words_batch),
            );
        }
        eprintln!("live-vs-batch equivalence OK: {} words", words_whole.len());
    }

    /// A real transcribe.cpp session survives the reusable durability checkpoint added by ADR 0012: the
    /// model stays resident, the VAD epoch resets, and post-checkpoint words remain call-relative.
    #[cfg(feature = "ggml")]
    #[test]
    #[ignore = "needs the real GGUF/VAD models + a speech WAV; set CORTI_VERIFY_MODEL_DIR and CORTI_VERIFY_WAV"]
    fn ggml_checkpoint_keeps_call_relative_timestamps() {
        let dir = std::path::PathBuf::from(
            std::env::var("CORTI_VERIFY_MODEL_DIR")
                .expect("set CORTI_VERIFY_MODEL_DIR to the model cache dir"),
        );
        let (mono, rate) = verify_audio();
        let m = crate::models::discover_for(&dir, false, false, "titanet")
            .expect("discover GGML/VAD models");
        let path = crate::ggml::resolve_gguf(None, &dir).expect("resolve GGUF");
        let rec = Arc::new(Asr::Ggml(
            crate::ggml::GgmlAsr::load(&path, 4).expect("GGML recognizer"),
        ));
        let mut live =
            LiveTranscriber::from_pinned_vad(rec, engine::build_vad(&m, 0.5, 1.0).expect("vad"));

        let boundary = (rate as usize * 60).min(mono.len() / 2);
        let end = (boundary + rate as usize * 60).min(mono.len());
        live.push(&mono[..boundary], rate);
        let before = live.checkpoint();
        live.push(&mono[boundary..end], rate);
        let after = live.finish();

        assert!(
            !before.is_empty(),
            "fixture must contain speech before checkpoint"
        );
        assert!(
            !after.is_empty(),
            "fixture must contain speech after checkpoint"
        );
        let boundary_sec = boundary as f64 / rate as f64;
        assert!(
            after.iter().all(|word| word.start >= boundary_sec - 0.1),
            "post-checkpoint timestamps must retain the call-relative epoch near {boundary_sec}s"
        );
        for words in [&before, &after] {
            assert!(
                words.windows(2).all(|pair| pair[1].start >= pair[0].start),
                "timestamps must stay monotonic within each checkpoint epoch"
            );
        }
    }

    /// A mid-stream sample-rate switch (source rate → 16 kHz) must flush the resampler's tail in order, not
    /// strand it and re-emit it after all the 16 kHz audio: word offsets stay non-decreasing. Before the
    /// flush fix the stale tail surfaced at finish() out of order. Gated — needs the real models and a
    /// non-16 kHz speech WAV (same env as `live_equals_batch_over_chunking`).
    #[test]
    #[ignore = "needs real ASR/VAD models + a non-16 kHz speech WAV; set CORTI_VERIFY_MODEL_DIR and CORTI_VERIFY_WAV"]
    fn live_survives_sample_rate_switch() {
        let dir = std::path::PathBuf::from(
            std::env::var("CORTI_VERIFY_MODEL_DIR")
                .expect("set CORTI_VERIFY_MODEL_DIR to the model cache dir"),
        );
        let (mono, rate) = verify_audio();
        assert_ne!(
            rate, 16_000,
            "this test needs a non-16 kHz WAV to force a resampler"
        );

        let (m, rec) = verify_engine(&dir);
        let mut live = LiveTranscriber::from_pinned_vad(
            rec.clone(),
            engine::build_vad(&m, 0.5, 1.0).expect("vad"),
        );

        // First half at the source rate (builds a resampler), then the rest pre-resampled to 16 kHz and
        // pushed as 16 kHz — the switch that must flush the resampler's held tail in order.
        let half = mono.len() / 2;
        live.push(&mono[..half], rate);
        let tail_16k = LinearResampler::create(rate as i32, TARGET_RATE)
            .expect("resampler")
            .resample(&mono[half..], true);
        live.push(&tail_16k, 16_000);
        let words = live.finish();

        for w in words.windows(2) {
            assert!(
                w[1].start >= w[0].start,
                "word offsets must stay monotonic across a rate switch (got {} after {})",
                w[1].start,
                w[0].start
            );
        }
        eprintln!("rate-switch monotonic OK: {} words", words.len());
    }
}
