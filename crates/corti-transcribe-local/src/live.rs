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
use corti_transcribe::segment::{SpeakerTurn, Word};
use sherpa_onnx::{LinearResampler, OfflineSpeakerDiarization, VoiceActivityDetector};

use crate::asr::Asr;
use crate::engine::{self, TARGET_RATE, VAD_WINDOW};

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
    rec: Arc<Asr>,
    vad: VoiceActivityDetector,
    /// Built lazily on the first non-16 kHz push and reused so resampling is continuous across pushes.
    resampler: Option<LinearResampler>,
    src_rate: i32,
    win: WindowBuffer,
    pending: Vec<Word>,
    /// Absolute 16 kHz sample offset of the current VAD epoch. `checkpoint()` resets the VAD so its next
    /// `SpeechSegment::start()` begins at zero; adding this base keeps word timestamps call-relative.
    vad_base_samples: u64,
    finished: bool,
    /// One-shot latch so a persistent `LinearResampler::create` failure logs once, not per push.
    resampler_warned: bool,
}

impl LiveTranscriber {
    /// Wrap a resident ASR engine and a fresh (per-channel) Silero VAD. Build them via
    /// [`crate::Asr`] plus a VAD built by the crate, or use [`crate::LiveEngine`] to load once and spawn
    /// a transcriber per channel.
    pub fn new(rec: Arc<Asr>, vad: VoiceActivityDetector) -> Self {
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
        let rate = sample_rate as i32;
        if rate == TARGET_RATE {
            // A 16 kHz push while a resampler is live: flush its buffered tail first, otherwise those
            // samples are stranded and finish() would inject them out of order after the 16 kHz audio.
            self.flush_resampler_tail();
            self.feed_16k(samples);
            return;
        }
        // A mid-stream rate change: flush the old resampler's tail before it's replaced, so no samples are
        // dropped at the switch.
        if self.resampler.is_some() && self.src_rate != rate {
            self.flush_resampler_tail();
        }
        self.ensure_resampler(rate);
        // Resample to an owned buffer first so the `&self.resampler` borrow ends before `feed_16k`'s `&mut`.
        let up = self.resampler.as_ref().map(|r| r.resample(samples, false));
        match up {
            Some(up) => self.feed_16k(&up),
            // `create()` failed: drop the chunk rather than feed source-rate samples as 16 kHz (that garbles
            // words and stretches timestamps silently). Warn once; ensure_resampler retries on later pushes.
            None => {
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
        let tail = self.resampler.as_ref().map(|r| r.resample(&[], true));
        self.resampler = None;
        self.src_rate = 0;
        if let Some(tail) = tail
            && !tail.is_empty()
        {
            self.feed_16k(&tail);
        }
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
        self.vad.reset();
        self.vad_base_samples = self.win.fed;
        std::mem::take(&mut self.pending)
    }

    /// Flush the VAD (and the resampler tail), decode the final trailing region, and return **all** remaining
    /// words — those from the flush plus anything queued but not yet polled. Idempotent: a second call
    /// returns whatever has accumulated since (normally empty).
    pub fn finish(&mut self) -> Vec<Word> {
        if !self.finished {
            self.flush_decode_tail();
            self.finished = true;
        }
        std::mem::take(&mut self.pending)
    }

    /// Flush one epoch's resampler/VAD tails into `pending`. The `WindowBuffer` sample counter remains
    /// cumulative across epochs and supplies the next checkpoint's absolute timestamp base.
    fn flush_decode_tail(&mut self) {
        self.flush_resampler_tail();
        let remainder = self.win.take_remainder();
        let rec = self.rec.clone();
        let vad = &self.vad;
        let pending = &mut self.pending;
        if !remainder.is_empty() {
            vad.accept_waveform(&remainder);
        }
        vad.flush();
        drain_regions(vad, &rec, pending, self.vad_base_samples);
    }

    /// Feed already-16 kHz samples: buffer to whole VAD windows, then for each window accept + drain any
    /// regions the VAD closed.
    fn feed_16k(&mut self, samples_16k: &[f32]) {
        let rec = self.rec.clone();
        let vad = &self.vad;
        let pending = &mut self.pending;
        // Zero-copy batch path: with nothing carried, feed whole windows straight from the input slice and
        // buffer only the sub-window remainder — a whole-channel push copies just the tail, not the channel.
        if self.win.buf.is_empty() {
            let (windows, remainder) = samples_16k.as_chunks::<VAD_WINDOW>();
            for window in windows {
                vad.accept_waveform(window);
                drain_regions(vad, &rec, pending, self.vad_base_samples);
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
            vad.accept_waveform(window);
            drain_regions(vad, &rec, pending, self.vad_base_samples);
        }
    }

    fn ensure_resampler(&mut self, rate: i32) {
        if self.resampler.is_none() || self.src_rate != rate {
            self.resampler = LinearResampler::create(rate, TARGET_RATE);
            self.src_rate = rate;
        }
    }
}

/// Pop every completed VAD region, decode it at its call-relative offset, and append the words. Shared by
/// `push`, `checkpoint`, and `finish`; `seg.start()` is relative to the current VAD epoch, so checkpoints
/// add the cumulative `vad_base_samples` back before either ASR engine decodes the region.
fn drain_regions(
    vad: &VoiceActivityDetector,
    rec: &Asr,
    out: &mut Vec<Word>,
    vad_base_samples: u64,
) {
    while let Some(seg) = vad.front() {
        let offset = absolute_offset_sec(vad_base_samples, seg.start() as u64);
        out.extend(rec.asr_segment(seg.samples(), offset));
        vad.pop();
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
        #[cfg(feature = "offline-tracing")]
        let span = tracing::span!(
            target: "vasovagal::trace",
            tracing::Level::INFO,
            "corti.transcription.channel",
            backend = "local",
            engine = self.rec.trace_engine(),
            model_family = "speech_to_text",
            outcome = tracing::field::Empty,
            error_code = tracing::field::Empty,
        );
        let run = || {
            let vad = engine::build_vad(&self.models, self.vad_threshold, self.vad_min_silence)?;
            Ok(LiveTranscriber::new(self.rec.clone(), vad))
        };
        #[cfg(feature = "offline-tracing")]
        let result = span.in_scope(run);
        #[cfg(not(feature = "offline-tracing"))]
        let result = run();
        #[cfg(feature = "offline-tracing")]
        crate::record_result(&span, &result, "model_unavailable");
        result
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
        #[cfg(feature = "offline-tracing")]
        let span = tracing::span!(
            target: "vasovagal::trace",
            tracing::Level::INFO,
            "corti.transcription.diarize",
            backend = "local",
            engine = "onnx",
            model_family = "diarization",
            sample_rate = u64::from(sample_rate),
            sample_count = u64::try_from(samples.len()).unwrap_or(u64::MAX),
            item_count = tracing::field::Empty,
            outcome = tracing::field::Empty,
            error_code = tracing::field::Empty,
        );
        let run = || {
            let samples_16k = engine::resample_to_16k(samples, sample_rate as i32)?;
            let mut turns = engine::diarize_channel(diarizer, &samples_16k);
            for turn in &mut turns {
                turn.start += offset_sec;
                turn.end += offset_sec;
            }
            Ok(Some(turns))
        };
        #[cfg(feature = "offline-tracing")]
        let result = span.in_scope(run);
        #[cfg(not(feature = "offline-tracing"))]
        let result = run();
        #[cfg(feature = "offline-tracing")]
        {
            crate::record_result(&span, &result, "decode_failed");
            if let Ok(Some(turns)) = &result {
                span.record("item_count", u64::try_from(turns.len()).unwrap_or(u64::MAX));
            }
        }
        result
    }
}

#[cfg(feature = "stream")]
mod stream {
    //! Async edge adapter (ADR 0009): a `futures_core::Stream<Item = Vec<Word>>` over the sync
    //! [`LiveTranscriber`]. The core stays sync — this owns a dedicated std thread running the transcriber and
    //! bridges it to the async world with a tokio mpsc, so no runtime ever enters the engine.

    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};
    use std::thread::JoinHandle;

    use corti_transcribe::segment::Word;
    use futures_core::Stream;

    use super::LiveTranscriber;

    /// Bounded audio backlog before the sink drops chunks. Mirrors the capture tee's lossy-bounded contract
    /// (ADR 0009): live audio is throwaway, so a slow decoder drops rather than growing memory + word latency
    /// without bound.
    const AUDIO_BACKLOG: usize = 64;

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
    /// the [`LiveSink`] is dropped and the final flush has been emitted. Reaching `None` joins the worker
    /// thread (its `finish()` decode is already done by then), so end-of-stream implies the worker is done.
    pub struct LiveWordStream {
        rx: tokio::sync::mpsc::UnboundedReceiver<Vec<Word>>,
        worker: Option<JoinHandle<()>>,
    }

    impl Stream for LiveWordStream {
        type Item = Vec<Word>;

        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let this = self.get_mut();
            match this.rx.poll_recv(cx) {
                Poll::Ready(None) => {
                    // The worker dropped `words_tx` after its final flush, so this join is immediate.
                    if let Some(worker) = this.worker.take() {
                        let _ = worker.join();
                    }
                    Poll::Ready(None)
                }
                other => other,
            }
        }
    }

    /// Split a [`LiveTranscriber`] into a push [`LiveSink`] and a [`LiveWordStream`], running the transcriber
    /// on a dedicated std thread. Drop the sink to flush and terminate the stream; draining the stream to
    /// `None` joins that thread.
    pub fn live_word_stream(mut live: LiveTranscriber) -> (LiveSink, LiveWordStream) {
        let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel::<(Vec<f32>, u32)>(AUDIO_BACKLOG);
        let (words_tx, words_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<Word>>();
        let worker = std::thread::Builder::new()
            .name("corti-live-asr".into())
            .spawn(move || {
                while let Ok((samples, rate)) = audio_rx.recv() {
                    live.push(&samples, rate);
                    if let Some(words) = live.poll_words()
                        && words_tx.send(words).is_err()
                    {
                        return; // consumer dropped the stream
                    }
                }
                let tail = live.finish();
                if !tail.is_empty() {
                    let _ = words_tx.send(tail);
                }
            })
            .expect("spawn corti-live-asr thread");
        (
            LiveSink {
                tx: audio_tx,
                dropped: Arc::new(AtomicUsize::new(0)),
            },
            LiveWordStream {
                rx: words_rx,
                worker: Some(worker),
            },
        )
    }
}

#[cfg(feature = "stream")]
pub use stream::{LiveSink, LiveWordStream, live_word_stream};

#[cfg(test)]
mod tests {
    use super::*;

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
            let resampler = LinearResampler::create(48_000, TARGET_RATE).unwrap();
            emitted += resampler.resample(&one_second, false).len();
            emitted += resampler.resample(&[], true).len();
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

        // Whole-channel push (the batch path).
        let mut whole =
            LiveTranscriber::new(rec.clone(), engine::build_vad(&m, 0.5, 1.0).expect("vad"));
        whole.push(&mono, rate);
        let words_whole = whole.finish();

        // Same audio in irregular chunks straddling 512-sample window boundaries.
        let mut chunked =
            LiveTranscriber::new(rec.clone(), engine::build_vad(&m, 0.5, 1.0).expect("vad"));
        let mut i = 0;
        for (n, step) in [377usize, 512, 100, 999, 1, 4096]
            .iter()
            .cloned()
            .cycle()
            .enumerate()
        {
            if i >= mono.len() {
                break;
            }
            let end = (i + step).min(mono.len());
            chunked.push(&mono[i..end], rate);
            let _ = n;
            i = end;
        }
        let words_chunked = chunked.finish();

        assert_eq!(
            words_whole, words_chunked,
            "chunked push must equal whole-channel push"
        );
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
        let mut live = LiveTranscriber::new(rec, engine::build_vad(&m, 0.5, 1.0).expect("vad"));

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
        let mut live =
            LiveTranscriber::new(rec.clone(), engine::build_vad(&m, 0.5, 1.0).expect("vad"));

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
