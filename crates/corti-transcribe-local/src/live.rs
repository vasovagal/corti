//! Chunked / live transcription over the resident Parakeet engine (ADR 0009).
//!
//! [`LiveTranscriber`] is a **pull-based, synchronous** wrapper around the same sherpa-onnx pieces the batch
//! path uses ([`engine::build_recognizer`], [`engine::build_vad`], [`engine::asr_segment`]): audio is
//! `push`ed in arbitrary-sized chunks, resampled to 16 kHz, fed to a single stateful Silero VAD, and each
//! **completed** VAD speech region is decoded immediately — so the decode cost lands inside the `push` that
//! closes a region, not on a separate tick. Recognized [`Word`]s queue up; the caller drains them with
//! [`LiveTranscriber::poll_words`] and flushes the tail with [`LiveTranscriber::finish`].
//!
//! Timestamps stay **call-relative** across pushes for free: one VAD is fed the whole (contiguous) 16 kHz
//! stream, and `SpeechSegment::start()` is the absolute sample index over everything fed so far, so
//! `start / 16000` is seconds from the start of the call regardless of how the audio was chunked.
//!
//! The core is deliberately sync (guardrail 9 — no runtime in the engine); the optional async `Stream`
//! adapter lives behind the `stream` feature at the bottom of this file.

use std::sync::Arc;

use anyhow::Result;
use corti_transcribe::segment::Word;
use sherpa_onnx::{LinearResampler, VoiceActivityDetector};

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
/// and flush the trailing region with [`finish`](Self::finish).
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
    finished: bool,
    /// One-shot latch so a persistent `LinearResampler::create` failure logs once, not per push.
    resampler_warned: bool,
}

impl LiveTranscriber {
    /// Wrap a resident ASR engine and a fresh (per-channel) Silero VAD. Build them via
    /// [`crate::Asr`]/[`engine::build_vad`], or use [`crate::LiveEngine`] to load once and spawn
    /// a transcriber per channel.
    pub fn new(rec: Arc<Asr>, vad: VoiceActivityDetector) -> Self {
        Self {
            rec,
            vad,
            resampler: None,
            src_rate: 0,
            win: WindowBuffer::default(),
            pending: Vec::new(),
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

    /// Flush the VAD (and the resampler tail), decode the final trailing region, and return **all** remaining
    /// words — those from the flush plus anything queued but not yet polled. Idempotent: a second call
    /// returns whatever has accumulated since (normally empty).
    pub fn finish(&mut self) -> Vec<Word> {
        if !self.finished {
            // Flush any samples the resampler is still holding internally, then push them through.
            self.flush_resampler_tail();
            // Feed the final sub-window remainder (matches the batch loop's last `.chunks(512)` element),
            // then flush the VAD so trailing buffered speech is emitted, and drain everything.
            let remainder = self.win.take_remainder();
            let rec = self.rec.clone();
            let vad = &self.vad;
            let pending = &mut self.pending;
            if !remainder.is_empty() {
                vad.accept_waveform(&remainder);
            }
            vad.flush();
            drain_regions(vad, &rec, pending);
            self.finished = true;
        }
        std::mem::take(&mut self.pending)
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
            let mut windows = samples_16k.chunks_exact(VAD_WINDOW);
            for window in windows.by_ref() {
                vad.accept_waveform(window);
                drain_regions(vad, &rec, pending);
            }
            let remainder = windows.remainder();
            self.win.fed += (samples_16k.len() - remainder.len()) as u64;
            self.win.buf.extend_from_slice(remainder);
            return;
        }
        // Carry path: append to the retained remainder and release whole windows from the combined buffer.
        if self.win.extend(samples_16k) == 0 {
            return;
        }
        let block = self.win.take_windows();
        for window in block.chunks_exact(VAD_WINDOW) {
            vad.accept_waveform(window);
            drain_regions(vad, &rec, pending);
        }
    }

    fn ensure_resampler(&mut self, rate: i32) {
        if self.resampler.is_none() || self.src_rate != rate {
            self.resampler = LinearResampler::create(rate, TARGET_RATE);
            self.src_rate = rate;
        }
    }
}

/// Pop every completed VAD region, decode it at its absolute offset, and append the words. Shared by `push`
/// and `finish`; `seg.start()` is already the absolute sample index across all audio fed to this VAD.
fn drain_regions(vad: &VoiceActivityDetector, rec: &Asr, out: &mut Vec<Word>) {
    while let Some(seg) = vad.front() {
        let offset = seg.start() as f64 / TARGET_RATE as f64;
        out.extend(rec.asr_segment(seg.samples(), offset));
        vad.pop();
    }
}

/// A resident local ASR engine: one loaded recognizer ([`Asr`] — sherpa or ggml) plus the VAD parameters
/// needed to spawn a fresh [`LiveTranscriber`] per channel. Each channel needs its own stateful VAD, but all
/// channels share the single (thread-safe) recognizer. Build via [`crate::LocalTranscriber::live_engine`].
pub struct LiveEngine {
    rec: Arc<Asr>,
    models: crate::models::Models,
    provider: String,
    vad_threshold: f32,
    vad_min_silence: f32,
}

impl LiveEngine {
    pub(crate) fn new(
        rec: Asr,
        models: crate::models::Models,
        provider: String,
        vad_threshold: f32,
        vad_min_silence: f32,
    ) -> Self {
        Self {
            rec: Arc::new(rec),
            models,
            provider,
            vad_threshold,
            vad_min_silence,
        }
    }

    /// Spawn a [`LiveTranscriber`] for one channel: a fresh Silero VAD sharing the resident recognizer.
    pub fn channel(&self) -> Result<LiveTranscriber> {
        let vad = engine::build_vad(
            &self.models,
            &self.provider,
            self.vad_threshold,
            self.vad_min_silence,
        )?;
        Ok(LiveTranscriber::new(self.rec.clone(), vad))
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
    /// real Parakeet + Silero models and a speech WAV:
    ///   CORTI_VERIFY_MODEL_DIR=~/Library/Caches/corti/models CORTI_VERIFY_WAV=/path/to/mono_or_2track.wav \
    ///     cargo test -p corti-transcribe-local live_equals_batch_over_chunking -- --ignored --nocapture
    #[test]
    #[ignore = "needs the real ONNX models + a speech WAV; set CORTI_VERIFY_MODEL_DIR and CORTI_VERIFY_WAV"]
    fn live_equals_batch_over_chunking() {
        use crate::models;
        use std::path::PathBuf;

        let dir = PathBuf::from(
            std::env::var("CORTI_VERIFY_MODEL_DIR")
                .expect("set CORTI_VERIFY_MODEL_DIR to the model cache dir"),
        );
        let wav = PathBuf::from(
            std::env::var("CORTI_VERIFY_WAV").expect("set CORTI_VERIFY_WAV to a speech WAV"),
        );

        // Read the first (or only) channel and its source rate straight from the WAV.
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
        let ch = spec.channels as usize;
        let mono: Vec<f32> = interleaved.iter().step_by(ch).copied().collect();
        let rate = spec.sample_rate;

        let m = models::discover(&dir, false, "titanet").expect("discover models");
        let rec = Arc::new(Asr::Sherpa(
            engine::build_recognizer(&m, "cpu", 4, None, None, None).expect("rec"),
        ));

        // Whole-channel push (the batch path).
        let mut whole = LiveTranscriber::new(
            rec.clone(),
            engine::build_vad(&m, "cpu", 0.5, 1.0).expect("vad"),
        );
        whole.push(&mono, rate);
        let words_whole = whole.finish();

        // Same audio in irregular chunks straddling 512-sample window boundaries.
        let mut chunked = LiveTranscriber::new(
            rec.clone(),
            engine::build_vad(&m, "cpu", 0.5, 1.0).expect("vad"),
        );
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

    /// A mid-stream sample-rate switch (source rate → 16 kHz) must flush the resampler's tail in order, not
    /// strand it and re-emit it after all the 16 kHz audio: word offsets stay non-decreasing. Before the
    /// flush fix the stale tail surfaced at finish() out of order. Gated — needs the real models and a
    /// non-16 kHz speech WAV (same env as `live_equals_batch_over_chunking`).
    #[test]
    #[ignore = "needs the real ONNX models + a non-16 kHz speech WAV; set CORTI_VERIFY_MODEL_DIR and CORTI_VERIFY_WAV"]
    fn live_survives_sample_rate_switch() {
        use crate::models;
        use std::path::PathBuf;

        let dir = PathBuf::from(
            std::env::var("CORTI_VERIFY_MODEL_DIR")
                .expect("set CORTI_VERIFY_MODEL_DIR to the model cache dir"),
        );
        let wav = PathBuf::from(
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
        let ch = spec.channels as usize;
        let mono: Vec<f32> = interleaved.iter().step_by(ch).copied().collect();
        let rate = spec.sample_rate;
        assert_ne!(
            rate, 16_000,
            "this test needs a non-16 kHz WAV to force a resampler"
        );

        let m = models::discover(&dir, false, "titanet").expect("discover models");
        let rec = Arc::new(Asr::Sherpa(
            engine::build_recognizer(&m, "cpu", 4, None, None, None).expect("rec"),
        ));
        let mut live = LiveTranscriber::new(
            rec.clone(),
            engine::build_vad(&m, "cpu", 0.5, 1.0).expect("vad"),
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
