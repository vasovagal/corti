//! Shared-path boundary tests; pinned timing fake copied from the immutable C fixture, not model inference.
#[path = "terminal_tests.rs"]
mod terminal_tests;
use super::*;
use crate::asr::DecodedRegion;
use std::collections::VecDeque;
use std::sync::Mutex;

const HOP: usize = 512;
const LOOKAHEAD: usize = 64;
const NATIVE_WINDOW: usize = HOP + LOOKAHEAD;
const MIN_SPEECH: usize = 4_000;
const MIN_SILENCE: usize = 16_000;
const PRE_TRIGGER: usize = MIN_SPEECH + 2 * NATIVE_WINDOW;

#[derive(Clone, Default, Debug)]
struct Probe {
    accepted: usize,
    processed: usize,
    detected: bool,
    queued: usize,
    candidate: bool,
    pending_voiced: usize,
    resets: usize,
    candidate_resets: usize,
    pending_voiced_erased: usize,
}

/// Pinned Silero IsSpeech timing with probability 1 for a nonzero window and 0 otherwise.
#[derive(Default)]
struct ModelTiming {
    current: usize,
    temp_start: usize,
    temp_end: usize,
    triggered: bool,
    /// Samples after a state reset before the model can report speech (real Silero needs context).
    warmup: usize,
}

impl ModelTiming {
    fn is_speech(&mut self, positive: bool, min_silence: usize) -> bool {
        self.current += HOP;
        let positive = positive && self.current > self.warmup;
        if positive && self.temp_end != 0 {
            self.temp_end = 0;
        }
        if positive && self.temp_start == 0 {
            self.temp_start = self.current;
            return false;
        }
        if positive && self.temp_start != 0 && !self.triggered {
            if self.current - self.temp_start < MIN_SPEECH {
                return false;
            }
            self.triggered = true;
            return true;
        }
        if !positive && !self.triggered {
            self.temp_start = 0;
            self.temp_end = 0;
            return false;
        }
        if positive && self.triggered {
            return true;
        }
        if !positive && self.triggered {
            if self.temp_end == 0 {
                self.temp_end = self.current;
            }
            if self.current - self.temp_end < min_silence {
                return true;
            }
            self.temp_start = 0;
            self.temp_end = 0;
            self.triggered = false;
        }
        false
    }
}

struct PendingSegmenter {
    pcm: Vec<f32>,
    processed: usize,
    head: usize,
    start: Option<usize>,
    model: ModelTiming,
    queue: VecDeque<(u64, Vec<f32>)>,
    probe: Arc<Mutex<Probe>>,
}

impl PendingSegmenter {
    fn new(probe: Arc<Mutex<Probe>>) -> Self {
        Self {
            pcm: vec![],
            processed: 0,
            head: 0,
            start: None,
            model: ModelTiming::default(),
            queue: VecDeque::new(),
            probe,
        }
    }
    fn observe(&self) {
        let mut probe = self.probe.lock().unwrap();
        probe.accepted = self.pcm.len();
        probe.processed = self.processed;
        probe.detected = self.start.is_some();
        probe.queued = self.queue.len();
        probe.candidate = self.model.temp_start != 0 && !self.model.triggered;
        probe.pending_voiced = self.pcm[self.processed..]
            .iter()
            .filter(|v| **v != 0.0)
            .count();
    }
}

impl Segmenter for PendingSegmenter {
    fn terminal_guard(&self) -> Option<usize> {
        Some(5727)
    }
    fn boundary_idle(&self) -> bool {
        self.start.is_none() && self.queue.is_empty()
    }
    fn accept(&mut self, samples: &[f32]) {
        self.pcm.extend_from_slice(samples);
        if self.pcm.len() - self.processed < NATIVE_WINDOW {
            self.observe();
            return;
        }
        let min_silence = if self.processed - self.head > 320_000 {
            1_600
        } else {
            MIN_SILENCE
        };
        let mut speech = false;
        while self.pcm.len() - self.processed >= NATIVE_WINDOW {
            let positive = self.pcm[self.processed..self.processed + NATIVE_WINDOW]
                .iter()
                .any(|sample| *sample != 0.0);
            self.processed += HOP;
            speech |= self.model.is_speech(positive, min_silence);
        }
        if speech {
            if self.start.is_none() {
                self.start = Some(self.processed.saturating_sub(PRE_TRIGGER).max(self.head));
            }
        } else {
            if let Some(start) = self.start {
                let end = self.processed - min_silence;
                self.queue
                    .push_back((start as u64, self.pcm[start..end].to_vec()));
                self.head = end;
            } else {
                self.head = self.head.max(self.processed.saturating_sub(PRE_TRIGGER));
            }
            self.start = None;
        }
        self.observe();
    }
    fn flush(&mut self) {
        if let Some(start) = self.start.take() {
            self.queue
                .push_back((start as u64, self.pcm[start..self.processed].to_vec()));
            self.head = self.processed;
        }
        self.observe();
    }
    fn reset(&mut self) {
        {
            let mut probe = self.probe.lock().unwrap();
            probe.resets += 1;
            probe.candidate_resets +=
                usize::from(self.model.temp_start != 0 && !self.model.triggered);
            probe.pending_voiced_erased += self.pcm[self.processed..]
                .iter()
                .filter(|v| **v != 0.0)
                .count();
        }
        self.pcm.clear();
        self.processed = 0;
        self.head = 0;
        self.start = None;
        self.model = ModelTiming {
            warmup: self.model.warmup,
            ..ModelTiming::default()
        };
        self.queue.clear();
        self.observe();
    }
    fn drain_completed(&mut self, visit: &mut dyn FnMut(u64, &[f32])) {
        while let Some((start, pcm)) = self.queue.pop_front() {
            visit(start, &pcm);
        }
        self.observe();
    }
}

#[derive(Clone, Debug)]
struct Attempt {
    start: u64,
    end: u64,
    pcm: Vec<f32>,
}

struct Decoder {
    attempts: Arc<Mutex<Vec<Attempt>>>,
    statuses: Arc<Mutex<VecDeque<RegionStatus>>>,
}
impl RegionDecoder for Decoder {
    fn decode(&self, pcm: &[f32], offset: f64) -> DecodedRegion {
        let start = (offset * 16_000.0).round() as u64;
        self.attempts.lock().unwrap().push(Attempt {
            start,
            end: start + pcm.len() as u64,
            pcm: pcm.to_vec(),
        });
        let status = self
            .statuses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(RegionStatus::Complete);
        let words = if matches!(
            status,
            RegionStatus::Failed(_) | RegionStatus::RecognizedEmpty
        ) {
            vec![]
        } else {
            vec![Word {
                text: "same repeated ID".into(),
                start: offset,
                end: offset + pcm.len() as f64 / 16_000.0,
            }]
        };
        DecodedRegion { status, words }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Call {
    Accept(usize),
    Flush,
    Reset,
}

struct Counted {
    inner: Box<dyn Segmenter>,
    calls: Arc<Mutex<Vec<Call>>>,
}
impl Segmenter for Counted {
    fn terminal_guard(&self) -> Option<usize> {
        self.inner.terminal_guard()
    }
    fn accept(&mut self, pcm: &[f32]) {
        self.calls.lock().unwrap().push(Call::Accept(pcm.len()));
        self.inner.accept(pcm);
    }
    fn flush(&mut self) {
        self.calls.lock().unwrap().push(Call::Flush);
        self.inner.flush();
    }
    fn reset(&mut self) {
        self.calls.lock().unwrap().push(Call::Reset);
        self.inner.reset();
    }
    fn drain_completed(&mut self, visit: &mut dyn FnMut(u64, &[f32])) {
        self.inner.drain_completed(visit);
    }
    fn boundary_idle(&self) -> bool {
        self.inner.boundary_idle()
    }
}

#[derive(Default)]
struct EachWindow {
    position: u64,
    queue: VecDeque<(u64, Vec<f32>)>,
}
impl Segmenter for EachWindow {
    fn terminal_guard(&self) -> Option<usize> {
        Some(0)
    }
    fn accept(&mut self, pcm: &[f32]) {
        if pcm.iter().any(|s| *s != 0.0) {
            self.queue.push_back((self.position, pcm.to_vec()));
        }
        self.position += pcm.len() as u64;
    }
    fn flush(&mut self) {}
    fn reset(&mut self) {
        self.position = 0;
        self.queue.clear();
    }
    fn drain_completed(&mut self, visit: &mut dyn FnMut(u64, &[f32])) {
        while let Some((start, pcm)) = self.queue.pop_front() {
            visit(start, &pcm);
        }
    }
    fn boundary_idle(&self) -> bool {
        self.queue.is_empty()
    }
}

struct Fixture {
    live: LiveTranscriber,
    attempts: Arc<Mutex<Vec<Attempt>>>,
    statuses: Arc<Mutex<VecDeque<RegionStatus>>>,
    calls: Arc<Mutex<Vec<Call>>>,
}

fn limits() -> BoundaryLimits {
    BoundaryLimits {
        max_retained_samples: 500_000,
        max_epoch_samples: 1_000_000,
        max_source_push: 4096,
    }
}
fn fixture(native_timing: bool) -> Fixture {
    fixture_with_limits(native_timing, limits())
}
fn fixture_with_limits(native_timing: bool, configured_limits: BoundaryLimits) -> Fixture {
    let attempts = Arc::new(Mutex::new(vec![]));
    let statuses = Arc::new(Mutex::new(VecDeque::new()));
    let calls = Arc::new(Mutex::new(vec![]));
    let inner: Box<dyn Segmenter> = if native_timing {
        Box::new(PendingSegmenter::new(Arc::new(
            Mutex::new(Probe::default()),
        )))
    } else {
        Box::new(EachWindow::default())
    };
    let mut live = LiveTranscriber::with_parts(
        Arc::new(Decoder {
            attempts: attempts.clone(),
            statuses: statuses.clone(),
        }),
        Box::new(Counted {
            inner,
            calls: calls.clone(),
        }),
    );
    live.enable_boundaries(configured_limits).unwrap();
    Fixture {
        live,
        attempts,
        statuses,
        calls,
    }
}
fn push(live: &mut LiveTranscriber, pcm: &[f32], rate: u32) {
    for chunk in pcm.chunks(377) {
        live.push(chunk, rate);
    }
}
fn bits(pcm: &[f32]) -> Vec<u32> {
    pcm.iter().map(|s| s.to_bits()).collect()
}

const WORD_SAMPLES: usize = 1600;
const LABELS: [&str; 6] = ["do", "not", "Ada", "twenty", "one", "confirmed"];

struct AmplitudeDecoder(Decoder);
impl RegionDecoder for AmplitudeDecoder {
    fn decode(&self, pcm: &[f32], offset: f64) -> DecodedRegion {
        let mut result = self.0.decode(pcm, offset);
        result.words.clear();
        let mut first = 0;
        while first < pcm.len() {
            let id = pcm[first] as usize;
            let mut end = first + 1;
            while end < pcm.len() && pcm[end] == pcm[first] {
                end += 1;
            }
            // The immutable paused-sentence control rejects a truncated 0.1-second symbol.
            // Distinct occurrences of a repeated ID remain distinct sample spans.
            if (1..=LABELS.len()).contains(&id) && end - first >= WORD_SAMPLES {
                result.words.push(Word {
                    text: LABELS[id - 1].into(),
                    start: offset + first as f64 / 16_000.0,
                    end: offset + end as f64 / 16_000.0,
                });
            }
            first = end;
        }
        if result.words.is_empty() {
            result.status = RegionStatus::RecognizedEmpty;
        }
        result
    }
}

fn amplitude_fixture() -> Fixture {
    let mut f = fixture(true);
    f.live.rec = Arc::new(AmplitudeDecoder(Decoder {
        attempts: f.attempts.clone(),
        statuses: f.statuses.clone(),
    }));
    f
}

fn sentence(start: usize) -> (Vec<f32>, Vec<(String, usize)>) {
    let mut pcm = vec![0.0; start];
    let mut expected = Vec::new();
    for (index, id) in [1_usize, 2, 3, 4, 4, 5, 6].into_iter().enumerate() {
        if index == 4 {
            pcm.push(0.0);
        }
        expected.push((LABELS[id - 1].to_string(), pcm.len()));
        pcm.resize(pcm.len() + WORD_SAMPLES, id as f32);
    }
    pcm.resize(pcm.len() + 2 * MIN_SILENCE, 0.0);
    (pcm, expected)
}

#[test]
fn common_interval_phase_sweep_preserves_whole_regions_and_repeated_ids() {
    use corti_transcribe::boundary::{BoundaryPolicy, BoundaryStep, Observation, TrackBoundary};
    use std::time::Duration;

    let due = 32_768 + 111;
    for mic_phase in 0..=7 {
        for tap_phase in 0..=7 {
            let (mut mic_pcm, mic_expected) = sentence(due - mic_phase * WORD_SAMPLES);
            let (mut tap_pcm, tap_expected) = sentence(due - tap_phase * WORD_SAMPLES + 800);
            let length = mic_pcm.len().max(tap_pcm.len());
            mic_pcm.resize(length, 0.0);
            tap_pcm.resize(length, 0.0);
            let pcm = [mic_pcm, tap_pcm];
            // Established source origins give the two native epochs their original unequal phases.
            let origins = [13, 311];
            let mut channels = [amplitude_fixture(), amplitude_fixture()];
            let mut uncut = [amplitude_fixture(), amplitude_fixture()];
            for reference in &mut uncut {
                reference.live.boundary = None;
            }
            let mut words: [Vec<Word>; 2] = std::array::from_fn(|_| Vec::new());
            let mut policy =
                BoundaryPolicy::new(0, due as u64, 16_000, Duration::from_secs(1)).unwrap();
            let mut selected = None;
            let mut old_attempts = [0; 2];
            let mut received = 0;
            while received < length {
                let count = if selected.is_none() {
                    let step = policy
                        .observe(Observation {
                            received_through: received as u64,
                            tracks: std::array::from_fn(|i| {
                                if received <= origins[i] {
                                    return TrackBoundary::NeverPresent;
                                }
                                let snapshot = channels[i].live.boundary_snapshot().unwrap();
                                TrackBoundary::Established {
                                    start_frame: origins[i] as u64,
                                    delivered_through: origins[i] as u64
                                        + snapshot.delivered_through,
                                    natural: snapshot.natural.map(|interval| SourceInterval {
                                        first: interval.first + origins[i] as u64,
                                        last: interval.last + origins[i] as u64,
                                    }),
                                }
                            }),
                            text_full: false,
                            maintenance_armed: false,
                        })
                        .unwrap();
                    match step {
                        BoundaryStep::Admit { frames } => frames as usize,
                        BoundaryStep::Selected {
                            fence,
                            reset_for_maintenance,
                            ..
                        } => {
                            assert_eq!(
                                fence.kind,
                                corti_transcribe::boundary::BoundaryKind::Natural
                            );
                            assert!(!reset_for_maintenance);
                            for i in 0..2 {
                                let calls = channels[i].calls.lock().unwrap().clone();
                                let token = channels[i]
                                    .live
                                    .latch_boundary(
                                        fence.frame - origins[i] as u64,
                                        BoundaryKind::Natural,
                                    )
                                    .unwrap();
                                assert_eq!(
                                    channels[i].live.advance_boundary(BoundaryInput::Continue),
                                    BoundaryProgress::OldSettled(token)
                                );
                                words[i].extend(channels[i].live.poll_words().unwrap_or_default());
                                old_attempts[i] = channels[i].attempts.lock().unwrap().len();
                                channels[i].live.acknowledge_boundary(token).unwrap();
                                assert_eq!(*channels[i].calls.lock().unwrap(), calls);
                            }
                            selected = Some(fence.frame);
                            continue;
                        }
                    }
                } else {
                    4096
                };
                let end = (received + count).min(length);
                for i in 0..2 {
                    let start = received.max(origins[i]);
                    push(&mut channels[i].live, &pcm[i][start..end], 16_000);
                    push(&mut uncut[i].live, &pcm[i][start..end], 16_000);
                }
                received = end;
            }
            let cut = selected.expect("both channels must certify a common paused-sentence cut");
            for (i, expected) in [mic_expected, tap_expected].into_iter().enumerate() {
                assert_eq!(
                    channels[i]
                        .live
                        .advance_boundary(BoundaryInput::SourceEnded),
                    BoundaryProgress::Ended
                );
                words[i].extend(channels[i].live.poll_words().unwrap_or_default());
                let reference = uncut[i].live.finish();
                assert_eq!(
                    words[i], reference,
                    "ordinary filing must match every uncut word and timestamp"
                );
                assert_eq!(
                    words[i].iter().map(|word| &word.text).collect::<Vec<_>>(),
                    expected.iter().map(|(text, _)| text).collect::<Vec<_>>()
                );
                assert!(
                    words[i]
                        .windows(2)
                        .all(|pair| pair[0].start <= pair[1].start)
                );
                for (word, (_, start)) in words[i].iter().zip(expected) {
                    assert!(
                        (word.start + origins[i] as f64 / 16_000.0 - start as f64 / 16_000.0).abs()
                            <= HOP as f64 / 16_000.0
                    );
                }
                let attempts = channels[i].attempts.lock().unwrap();
                assert!(attempts.windows(2).all(|pair| pair[0].end <= pair[1].start));
                for (j, attempt) in attempts.iter().enumerate() {
                    assert_eq!(
                        bits(&attempt.pcm),
                        bits(
                            &pcm[i][origins[i] + attempt.start as usize
                                ..origins[i] + attempt.end as usize]
                        )
                    );
                    if j < old_attempts[i] {
                        assert!(attempt.end + origins[i] as u64 <= cut);
                    } else {
                        assert!(attempt.start + origins[i] as u64 >= cut);
                    }
                }
                assert!(channels[i].live.decode_complete());
                assert_eq!(
                    *channels[i].calls.lock().unwrap(),
                    *uncut[i].calls.lock().unwrap()
                );
            }
        }
    }
}

#[test]
fn forced_boundary_holds_suffix_until_old_poll_ack_and_preserves_nonhop_pcm() {
    let mut f = fixture(true);
    let cut = 33_190;
    push(&mut f.live, &vec![1.0; cut], 16_000);
    let token = f
        .live
        .latch_boundary(cut as u64, BoundaryKind::Forced)
        .unwrap();
    assert_eq!(
        f.live.advance_boundary(BoundaryInput::Continue),
        BoundaryProgress::Waiting
    );
    push(&mut f.live, &vec![1.0; 6238 + 377], 16_000);
    assert_eq!(
        f.live.advance_boundary(BoundaryInput::Continue),
        BoundaryProgress::OldSettled(token)
    );
    let attempts = f.attempts.lock().unwrap().clone();
    assert_eq!(attempts.len(), 1);
    assert_eq!((attempts[0].start, attempts[0].end), (0, cut as u64));
    assert_eq!(attempts[0].pcm, vec![1.0; cut]);
    assert_eq!(
        f.live.acknowledge_boundary(token),
        Err(BoundaryError::InvalidTransition)
    );
    let words = f.live.poll_words().unwrap();
    assert_eq!(words[0].end, cut as f64 / 16_000.0);
    let old_count = f.attempts.lock().unwrap().len();
    let frames = f.live.input_clock.frames;
    let output = f.live.output_position();
    let remainder = bits(&f.live.win.buf);
    let calls = f.calls.lock().unwrap().clone();
    assert_eq!(
        f.live.advance_boundary(BoundaryInput::Continue),
        BoundaryProgress::OldSettled(token)
    );
    assert_eq!(*f.calls.lock().unwrap(), calls);
    assert_eq!(f.attempts.lock().unwrap().len(), old_count);
    f.live.acknowledge_boundary(token).unwrap();
    assert_eq!(f.live.input_clock.frames, frames);
    assert_eq!(f.live.output_position(), output);
    assert_eq!(bits(&f.live.win.buf), remainder);
    assert_eq!(
        f.live.acknowledge_boundary(token),
        Err(BoundaryError::InvalidTransition)
    );
    push(&mut f.live, &vec![0.0; 24_000], 16_000);
    f.live.advance_boundary(BoundaryInput::SourceEnded);
    let attempts = f.attempts.lock().unwrap();
    assert!(attempts.len() >= 2);
    assert_eq!(attempts[1].start, cut as u64);
    assert!(attempts.windows(2).all(|a| a[0].end <= a[1].start));
    assert_eq!(
        attempts
            .iter()
            .flat_map(|a| &a.pcm)
            .filter(|s| **s != 0.0)
            .count(),
        cut + 6238 + 377
    );
}

#[test]
fn forced_reset_decodes_speech_from_the_cut_despite_vad_warmup() {
    // After a reset the VAD needs context before it reports speech. Without a warm replay the
    // speech just after a forced cut fell into no region and was never decoded.
    let mut f = fixture(true);
    let probe = Arc::new(Mutex::new(Probe::default()));
    let mut segmenter = PendingSegmenter::new(probe);
    segmenter.model.warmup = 8_000;
    f.live.vad = Box::new(Counted {
        inner: Box::new(segmenter),
        calls: f.calls.clone(),
    });
    let silence = 16_000;
    let cut = silence + 33_190;
    let mut pcm = vec![0.0; silence];
    pcm.resize(cut, 1.0);
    push(&mut f.live, &pcm, 16_000);
    let token = f
        .live
        .latch_boundary(cut as u64, BoundaryKind::Forced)
        .unwrap();
    let speech_after = 6238 + 377 + 24_000;
    push(&mut f.live, &vec![1.0; 6238 + 377], 16_000);
    assert_eq!(
        f.live.advance_boundary(BoundaryInput::Continue),
        BoundaryProgress::OldSettled(token)
    );
    f.live.poll_words().unwrap();
    f.live.acknowledge_boundary(token).unwrap();
    push(&mut f.live, &vec![1.0; 24_000], 16_000);
    push(&mut f.live, &vec![0.0; 24_000], 16_000);
    f.live.advance_boundary(BoundaryInput::SourceEnded);
    let attempts = f.attempts.lock().unwrap();
    assert!(attempts.windows(2).all(|a| a[0].end <= a[1].start));
    assert_eq!(
        attempts[1].start,
        cut as u64,
        "{:?}",
        attempts
            .iter()
            .map(|a| (a.start, a.end))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        attempts
            .iter()
            .flat_map(|a| &a.pcm)
            .filter(|s| **s != 0.0)
            .count(),
        cut - silence + speech_after
    );
}

#[test]
fn natural_boundary_preserves_native_calls_clock_report_and_capped_attempted_end() {
    for status in [
        RegionStatus::Complete,
        RegionStatus::RecognizedEmpty,
        RegionStatus::Failed(DecodeErrorCategory::BackendFailure),
        RegionStatus::Partial(DecodeErrorCategory::OutputTruncated),
    ] {
        let mut f = fixture(false);
        f.statuses
            .lock()
            .unwrap()
            .extend(std::iter::repeat_n(status, 257));
        push(&mut f.live, &vec![1.0; 257 * 512], 16_000);
        push(&mut f.live, &vec![0.0; 8192], 16_000);
        let report = f.live.take_decode_report();
        assert_eq!(report.outcomes().len(), 256);
        assert_eq!(report.omitted_metadata() + report.omitted_failures(), 1);
        let snapshot = f.live.boundary_snapshot().unwrap().natural.unwrap();
        assert_eq!(snapshot.first, 257 * 512);
        let before = (
            f.live.input_clock.frames,
            f.live.win.fed,
            bits(&f.live.win.buf),
            f.live.vad_base_samples,
            f.live.skipped_clock.fractional_seconds().to_bits(),
            f.live.decode_complete(),
        );
        let calls = f.calls.lock().unwrap().clone();
        let token = f
            .live
            .latch_boundary(snapshot.first, BoundaryKind::Natural)
            .unwrap();
        assert_eq!(
            f.live.advance_boundary(BoundaryInput::Continue),
            BoundaryProgress::OldSettled(token)
        );
        f.live.poll_words();
        f.live.acknowledge_boundary(token).unwrap();
        assert_eq!(
            (
                f.live.input_clock.frames,
                f.live.win.fed,
                bits(&f.live.win.buf),
                f.live.vad_base_samples,
                f.live.skipped_clock.fractional_seconds().to_bits(),
                f.live.decode_complete()
            ),
            before
        );
        assert_eq!(*f.calls.lock().unwrap(), calls);
        assert!(f.live.take_decode_report().outcomes().is_empty());
    }
}

#[test]
fn forced_boundary_keeps_empty_failed_partial_attempts_out_of_replay() {
    for status in [
        RegionStatus::RecognizedEmpty,
        RegionStatus::Failed(DecodeErrorCategory::BackendFailure),
        RegionStatus::Partial(DecodeErrorCategory::OutputTruncated),
    ] {
        let mut f = fixture(false);
        f.statuses
            .lock()
            .unwrap()
            .extend([RegionStatus::Complete, status]);
        push(&mut f.live, &vec![1.0; 700], 16_000);
        let token = f.live.latch_boundary(700, BoundaryKind::Forced).unwrap();
        push(&mut f.live, &vec![1.0; 7168], 16_000);
        assert_eq!(
            f.live.advance_boundary(BoundaryInput::Continue),
            BoundaryProgress::OldSettled(token)
        );
        let old = f.attempts.lock().unwrap().clone();
        assert_eq!(old.len(), 2);
        assert_eq!((old[1].start, old[1].end), (512, 700));
        let report = f.live.take_decode_report();
        assert_eq!(report.outcomes()[1].status, status);
        f.live.poll_words();
        f.live.acknowledge_boundary(token).unwrap();
        let all = f.attempts.lock().unwrap();
        assert!(all.len() > old.len());
        assert_eq!(all[2].start, 700);
        assert!(all.windows(2).all(|pair| pair[0].end <= pair[1].start));
        assert_eq!(
            all[..2].iter().map(|a| bits(&a.pcm)).collect::<Vec<_>>(),
            old.iter().map(|a| bits(&a.pcm)).collect::<Vec<_>>()
        );
        assert_eq!(f.live.decode_complete(), status.is_complete());
    }
}

#[test]
fn forced_boundary_actual_resampler_uses_left_edge_partition_and_preserves_phase() {
    for rate in [44_100, 48_000] {
        let mut f = fixture(false);
        let prefix = vec![0.25; 700];
        push(&mut f.live, &prefix, rate);
        let source_cut = 300;
        let canonical_cut = (source_cut * 16_000_u64).div_ceil(rate as u64);
        let token = f
            .live
            .latch_boundary(source_cut, BoundaryKind::Forced)
            .unwrap();
        let suffix = vec![0.5; 24_000];
        push(&mut f.live, &suffix, rate);
        assert_eq!(
            f.live.advance_boundary(BoundaryInput::Continue),
            BoundaryProgress::OldSettled(token)
        );
        let mut reference = Resampler::new(rate as i32).unwrap();
        let mut canonical = reference.push(&prefix);
        canonical.extend(reference.push(&suffix));
        let attempts = f.attempts.lock().unwrap().clone();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].end, canonical_cut);
        assert_eq!(
            bits(&attempts[0].pcm),
            bits(&canonical[..canonical_cut as usize])
        );
        let old_words = f.live.poll_words().unwrap();
        assert_eq!(old_words[0].end, canonical_cut as f64 / 16_000.0);
        let source_frames = f.live.input_clock.frames;
        let output_position = f.live.output_position();
        let remainder = bits(&f.live.win.buf);
        f.live.acknowledge_boundary(token).unwrap();
        assert_eq!(f.live.input_clock.frames, source_frames);
        assert_eq!(f.live.output_position(), output_position);
        assert_eq!(bits(&f.live.win.buf), remainder);
        assert!(f.live.resampler.is_some());
        assert_eq!(
            f.live.advance_boundary(BoundaryInput::SourceEnded),
            BoundaryProgress::Ended
        );
        canonical.extend(reference.finish());
        let attempts = f.attempts.lock().unwrap();
        assert!(attempts.windows(2).all(|pair| pair[0].end == pair[1].start));
        assert_eq!(attempts.last().unwrap().end, canonical.len() as u64);
        assert_eq!(
            bits(
                &attempts
                    .iter()
                    .flat_map(|a| a.pcm.iter().copied())
                    .collect::<Vec<_>>()
            ),
            bits(&canonical)
        );
    }
}

#[test]
fn boundary_eof_uses_real_resampler_tail_and_reports_insufficient_lookahead() {
    let mut insufficient = fixture(true);
    push(&mut insufficient.live, &vec![1.0; 33_190], 16_000);
    insufficient
        .live
        .latch_boundary(33_190, BoundaryKind::Forced)
        .unwrap();
    let admitted = insufficient.live.input_clock.frames;
    assert_eq!(
        insufficient
            .live
            .advance_boundary(BoundaryInput::SourceEnded),
        BoundaryProgress::TerminalUnsettled(BoundaryError::InsufficientLookahead)
    );
    assert_eq!(insufficient.live.input_clock.frames, admitted);
    assert!(!insufficient.calls.lock().unwrap().contains(&Call::Reset));
    let calls = insufficient.calls.lock().unwrap().clone();
    assert_eq!(
        insufficient
            .live
            .advance_boundary(BoundaryInput::SourceEnded),
        BoundaryProgress::TerminalUnsettled(BoundaryError::InsufficientLookahead)
    );
    assert_eq!(*insufficient.calls.lock().unwrap(), calls);

    let mut sufficient = fixture(true);
    let rate = 44_100;
    push(&mut sufficient.live, &vec![1.0; 700], rate);
    let token = sufficient
        .live
        .latch_boundary(300, BoundaryKind::Forced)
        .unwrap();
    let total_source = ((109 + ACCEPTED_GUARD + 1) * rate as u64).div_ceil(16_000) as usize;
    push(&mut sufficient.live, &vec![1.0; total_source - 700], rate);
    assert_eq!(
        sufficient.live.advance_boundary(BoundaryInput::Continue),
        BoundaryProgress::Waiting
    );
    let admitted = sufficient.live.input_clock.frames;
    assert_eq!(
        sufficient.live.advance_boundary(BoundaryInput::SourceEnded),
        BoundaryProgress::OldSettled(token)
    );
    assert_eq!(sufficient.live.input_clock.frames, admitted);
    assert!(sufficient.live.resampler.is_none());
    sufficient.live.poll_words();
    sufficient.live.acknowledge_boundary(token).unwrap();
    assert_eq!(
        sufficient.live.advance_boundary(BoundaryInput::SourceEnded),
        BoundaryProgress::Ended
    );
    let calls = sufficient.calls.lock().unwrap().clone();
    assert_eq!(
        sufficient.live.advance_boundary(BoundaryInput::SourceEnded),
        BoundaryProgress::Ended
    );
    assert_eq!(*sufficient.calls.lock().unwrap(), calls);
}

#[test]
fn boundary_tiny_capacity_and_epoch_limits_fail_before_extra_native_admission() {
    for (limits, expected) in [
        (
            BoundaryLimits {
                max_retained_samples: 10,
                ..limits()
            },
            BoundaryError::RetentionLimit,
        ),
        (
            BoundaryLimits {
                max_epoch_samples: 100,
                ..limits()
            },
            BoundaryError::EpochLimit,
        ),
        (
            BoundaryLimits {
                max_source_push: 10,
                ..limits()
            },
            BoundaryError::AdmissionLimit,
        ),
    ] {
        let mut f = fixture_with_limits(false, limits);
        f.live.push(&vec![1.0; 512], 16_000);
        assert_eq!(
            f.live.advance_boundary(BoundaryInput::Continue),
            BoundaryProgress::TerminalUnsettled(expected)
        );
        assert!(f.calls.lock().unwrap().is_empty());
        assert!(!f.live.decode_complete());
        assert_eq!(f.live.take_decode_report().omitted_failures(), 1);
        f.live.push(&vec![1.0; 512], 16_000);
        assert!(f.calls.lock().unwrap().is_empty());
        assert_eq!(f.live.input_clock.frames, 1024);
    }
}

#[test]
fn boundary_rate_failure_cannot_reset_or_bypass_a_pending_source_fence() {
    let mut f = fixture(true);
    push(&mut f.live, &vec![1.0; 7000], 16_000);
    f.live.latch_boundary(7000, BoundaryKind::Forced).unwrap();
    f.live.resampler_factory = Box::new(|_| None);
    f.live.push(&[1.0; 17], 44_100);
    assert_eq!(
        f.live.advance_boundary(BoundaryInput::Continue),
        BoundaryProgress::TerminalUnsettled(BoundaryError::ResamplerUnavailable)
    );
    assert!(!f.calls.lock().unwrap().contains(&Call::Reset));
    assert!(f.attempts.lock().unwrap().iter().all(|a| a.end <= 7000));
    let report = f.live.take_decode_report();
    assert!(report.outcomes().iter().any(|o| o.span.start_frame == 7000
        && o.span.end_frame == 7017
        && o.status == RegionStatus::Failed(DecodeErrorCategory::ResamplerUnavailable)));
    let calls = f.calls.lock().unwrap().clone();
    f.live.push(&[1.0; 512], 16_000);
    assert_eq!(*f.calls.lock().unwrap(), calls);
    assert!(!f.live.decode_complete());
}

#[test]
fn natural_boundary_matches_uncut_native_call_pcm_word_and_report_streams() {
    let mut a = fixture(true);
    a.live.boundary = None;
    let mut c = fixture(true);
    let mut a_words = Vec::new();
    let mut c_words = Vec::new();
    for cycle in 0..8 {
        let mut pcm = vec![(cycle + 1) as f32 / 10.0; 8000];
        pcm.extend(vec![0.0; 24_000]);
        push(&mut a.live, &pcm, 16_000);
        push(&mut c.live, &pcm, 16_000);
        let snapshot = c.live.boundary_snapshot().unwrap().natural.unwrap();
        let cut = snapshot.first.max(cycle * 32_000 + 12_000);
        assert!(cut <= snapshot.last);
        let calls = c.calls.lock().unwrap().clone();
        let token = c.live.latch_boundary(cut, BoundaryKind::Natural).unwrap();
        assert_eq!(
            c.live.advance_boundary(BoundaryInput::Continue),
            BoundaryProgress::OldSettled(token)
        );
        a_words.extend(a.live.poll_words().unwrap_or_default());
        c_words.extend(c.live.poll_words().unwrap_or_default());
        c.live.acknowledge_boundary(token).unwrap();
        assert_eq!(*c.calls.lock().unwrap(), calls);
        assert!(c.live.boundary.as_ref().unwrap().retained.len() <= 6238);
    }
    a_words.extend(a.live.finish());
    assert_eq!(
        c.live.advance_boundary(BoundaryInput::SourceEnded),
        BoundaryProgress::Ended
    );
    c_words.extend(c.live.poll_words().unwrap_or_default());
    assert_eq!(a_words, c_words);
    assert_eq!(*a.calls.lock().unwrap(), *c.calls.lock().unwrap());
    let a_attempts = a.attempts.lock().unwrap();
    let c_attempts = c.attempts.lock().unwrap();
    assert_eq!(a_attempts.len(), c_attempts.len());
    for (a, c) in a_attempts.iter().zip(c_attempts.iter()) {
        assert_eq!(
            (a.start, a.end, bits(&a.pcm)),
            (c.start, c.end, bits(&c.pcm))
        );
    }
    let a_report = a.live.take_decode_report();
    let c_report = c.live.take_decode_report();
    assert_eq!(a_report.outcomes(), c_report.outcomes());
    assert_eq!(a_report.omitted_metadata(), c_report.omitted_metadata());
    assert_eq!(a_report.omitted_failures(), c_report.omitted_failures());
}

#[test]
fn repeated_forced_boundaries_bound_replay_and_native_epoch_without_source_reset() {
    let mut f = fixture(false);
    // A forced reset replays at most the settlement plus the warm-up context.
    f.live.boundary.as_mut().unwrap().limits.max_epoch_samples = 16_000 + REPLAY_CONTEXT;
    f.live
        .boundary
        .as_mut()
        .unwrap()
        .limits
        .max_retained_samples = (16_000 + REPLAY_CONTEXT) as usize;
    for n in 1..=16 {
        let cut = n * 7000;
        let delivered = f.live.input_clock.frames;
        push(&mut f.live, &vec![1.0; (cut - delivered) as usize], 16_000);
        let token = f.live.latch_boundary(cut, BoundaryKind::Forced).unwrap();
        push(&mut f.live, &vec![1.0; 6238], 16_000);
        assert_eq!(
            f.live.advance_boundary(BoundaryInput::Continue),
            BoundaryProgress::OldSettled(token)
        );
        f.live.poll_words();
        let delivered = f.live.input_clock.frames;
        let output = f.live.output_position();
        f.live.acknowledge_boundary(token).unwrap();
        assert_eq!(f.live.input_clock.frames, delivered);
        assert_eq!(f.live.output_position(), output);
        let state = f.live.boundary.as_ref().unwrap();
        assert!(state.native_accepted < 7000 + REPLAY_CONTEXT);
        assert!(state.retained.len() as u64 <= 6238 + REPLAY_CONTEXT);
        assert!(f.live.boundary_snapshot().unwrap().epoch_remaining > 9000);
    }
    assert_eq!(
        f.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| **call == Call::Reset)
            .count(),
        16
    );
    assert!(
        f.attempts
            .lock()
            .unwrap()
            .windows(2)
            .all(|pair| pair[0].end <= pair[1].start)
    );
}

#[test]
fn pending_future_fence_handles_rate_change_before_projection_and_refuses_evicted_cut() {
    for (prefix_frames, source_cut) in [(700, 1000), (1500, 1700)] {
        let mut f = fixture(false);
        let token = f
            .live
            .latch_boundary(source_cut, BoundaryKind::Forced)
            .unwrap();
        push(&mut f.live, &vec![0.25; prefix_frames], 44_100);
        assert_eq!(
            f.live.advance_boundary(BoundaryInput::Continue),
            BoundaryProgress::Waiting
        );
        push(&mut f.live, &vec![0.5; 24_000], 48_000);
        assert_eq!(
            f.live.advance_boundary(BoundaryInput::Continue),
            BoundaryProgress::OldSettled(token)
        );
        let canonical_cut = (prefix_frames as u64 * 16_000).div_ceil(44_100)
            + ((source_cut - prefix_frames as u64) * 16_000).div_ceil(48_000);
        assert_eq!(
            f.attempts.lock().unwrap().last().unwrap().end,
            canonical_cut
        );
        let mut reference1 = Resampler::new(44_100).unwrap();
        let mut reference2 = Resampler::new(48_000).unwrap();
        let mut canonical = reference1.push(&vec![0.25; prefix_frames]);
        canonical.extend(reference1.finish());
        canonical.extend(reference2.push(&vec![0.5; 24_000]));
        canonical.extend(reference2.finish());
        f.live.poll_words();
        f.live.acknowledge_boundary(token).unwrap();
        assert_eq!(
            f.live.advance_boundary(BoundaryInput::SourceEnded),
            BoundaryProgress::Ended
        );
        let attempts = f.attempts.lock().unwrap();
        assert_eq!(
            bits(
                &attempts
                    .iter()
                    .flat_map(|a| a.pcm.iter().copied())
                    .collect::<Vec<_>>()
            ),
            bits(&canonical)
        );
        assert!(attempts.windows(2).all(|pair| pair[0].end == pair[1].start));
        drop(attempts);
    }
    let mut capped = fixture(false);
    for n in 0..257 {
        capped
            .live
            .push(&[0.0; 48], if n % 2 == 0 { 48_000 } else { 16_000 });
    }
    assert_eq!(
        capped.live.latch_boundary(0, BoundaryKind::Forced),
        Err(BoundaryError::MappingUnavailable)
    );
    let report = capped.live.take_decode_report();
    assert_eq!(report.omitted_metadata(), 1);
    assert_eq!(report.omitted_failures(), 0);
    assert!(capped.live.decode_complete());
}

#[test]
#[ignore = "negative compatibility control: existing checkpoint cannot certify exact forced non-hop ownership"]
fn existing_checkpoint_is_not_forced_nonhop_settlement_negative_control() {
    let mut f = fixture(true);
    f.live.boundary = None;
    push(&mut f.live, &vec![1.0; 33_190], 16_000);
    f.live.checkpoint();
    assert_eq!(
        f.attempts.lock().unwrap().last().unwrap().end,
        33_190,
        "current compatibility checkpoint loses the native pending tail at this forced fence"
    );
}

#[test]
fn fresh_late_track_empty_old_fence_settles_without_guard_or_false_eof_loss() {
    for frames in [0, 111, 700] {
        let mut f = fixture(false);
        let token = f.live.latch_boundary(0, BoundaryKind::Forced).unwrap();
        assert_eq!(
            f.live.advance_boundary(BoundaryInput::Continue),
            BoundaryProgress::OldSettled(token)
        );
        push(&mut f.live, &vec![1.0; frames], 16_000);
        assert!(f.attempts.lock().unwrap().is_empty());
        assert_eq!(
            f.live.advance_boundary(BoundaryInput::SourceEnded),
            BoundaryProgress::OldSettled(token)
        );
        assert!(f.attempts.lock().unwrap().is_empty());
        assert!(f.live.poll_words().is_none());
        assert!(f.live.take_decode_report().is_complete());
        f.live.acknowledge_boundary(token).unwrap();
        assert_eq!(
            f.live.advance_boundary(BoundaryInput::SourceEnded),
            BoundaryProgress::Ended
        );
        let attempts = f.attempts.lock().unwrap();
        assert_eq!(attempts.iter().map(|a| a.pcm.len()).sum::<usize>(), frames);
        assert!(attempts.windows(2).all(|pair| pair[0].end == pair[1].start));
        assert!(f.live.decode_complete());
    }
}

#[test]
fn pinned_filter_bounds_cover_all_supported_rates_and_refuse_native_lcm_overflow() {
    let mut supported = 0;
    let mut unsupported = 0;
    let mut maxima = (0, 0, 0);
    for rate in 8_000..=192_000 {
        let mut a = rate;
        let mut b = 16_000;
        while b != 0 {
            (a, b) = (b, a % b);
        }
        let ticks = u64::from(rate / a) * 16_000;
        match boundary_resampler_bounds(rate) {
            Ok(bounds) => {
                supported += 1;
                assert!(ticks <= i32::MAX as u64);
                assert_eq!(u64::from(bounds.tick_frequency), ticks);
                maxima = (
                    maxima.0.max(bounds.filter_source_lag),
                    maxima.1.max(bounds.filter_canonical_lag),
                    maxima.2.max(bounds.native_remainder_samples),
                );
                assert!(bounds.filter_source_lag <= 73);
                assert!(bounds.filter_canonical_lag <= 13);
                assert!(bounds.native_remainder_samples <= 146);
                assert!(bounds.settlement_source_reserve(16384, 4096).unwrap() > 16384 + 4096);
            }
            Err(BoundaryError::UnsupportedRate) => {
                unsupported += 1;
                assert!(ticks > i32::MAX as u64);
            }
            Err(error) => panic!("unexpected scalar bound error: {error:?}"),
        }
    }
    eprintln!("BOUND_RATES supported={supported} refused={unsupported} maxima={maxima:?}");
    assert_eq!(supported + unsupported, 184001);
    assert_eq!(maxima, (73, 13, 146));
    let mut f = fixture(false);
    f.live.resampler_factory =
        Box::new(|_| panic!("unsafe rate must never reach native construction"));
    f.live.push(&[0.0; 1], 191_999);
    assert_eq!(
        f.live.advance_boundary(BoundaryInput::Continue),
        BoundaryProgress::TerminalUnsettled(BoundaryError::UnsupportedRate)
    );
    assert!(f.calls.lock().unwrap().is_empty());
    assert!(!f.live.decode_complete());
    assert_eq!(f.live.input_clock.frames, 1);
    let report = f.live.take_decode_report();
    assert_eq!(report.outcomes().len(), 1);
    assert_eq!(
        report.outcomes()[0].span,
        corti_transcribe::decode::InputSpan {
            start_frame: 0,
            end_frame: 1
        }
    );
    assert_eq!(
        report.outcomes()[0].status,
        RegionStatus::Failed(DecodeErrorCategory::ResamplerUnavailable)
    );
    assert_eq!(report.omitted_failures(), 0);
    for safe in [191_998, 192_000] {
        let mut f = fixture(false);
        f.live.push(&[0.25; 1], safe);
        assert!(f.live.resampler.is_some());
        assert_eq!(f.live.input_clock.frames, 1);
        assert_eq!(
            f.live.advance_boundary(BoundaryInput::SourceEnded),
            BoundaryProgress::Ended
        );
        assert!(f.live.decode_complete());
    }
}

#[test]
fn pinned_filter_reserve_and_burst_bounds_cover_actual_native_resampler_output() {
    for rate in [
        8_000, 8_001, 11_003, 15_999, 16_000, 44_100, 48_000, 131_071, 134_217, 192_000,
    ] {
        let bounds = boundary_resampler_bounds(rate).unwrap();
        let mut resampler = Resampler::new(rate as i32).unwrap();
        let mut admitted = 0_u64;
        let mut produced = 0_u64;
        for frames in [1, 17, 511, 512, 1025, 4096, 3] {
            let output = resampler.push(&vec![0.25; frames]);
            assert!(output.len() as u64 <= bounds.max_push_output(frames as u64).unwrap());
            admitted += frames as u64;
            produced += output.len() as u64;
            let source_covered = admitted.saturating_sub(511 + bounds.filter_source_lag);
            assert!(
                produced >= (source_covered * 16_000).div_ceil(u64::from(rate)),
                "rate={rate}"
            );
        }
        let output = resampler.finish();
        assert!(output.len() as u64 <= bounds.max_finish_output());
        produced += output.len() as u64;
        assert_eq!(produced, (admitted * 16_000).div_ceil(u64::from(rate)));
    }
}

#[test]
fn natural_eof_defers_real_remainder_until_old_prefix_is_acknowledged() {
    let mut f = fixture(false);
    push(&mut f.live, &[0.0; 8192], 16_000);
    f.live.push(&[0.5; 31], 16_000);
    let token = f.live.latch_boundary(1000, BoundaryKind::Natural).unwrap();
    let calls = f.calls.lock().unwrap().clone();
    assert_eq!(
        f.live.advance_boundary(BoundaryInput::SourceEnded),
        BoundaryProgress::OldSettled(token)
    );
    assert_eq!(
        *f.calls.lock().unwrap(),
        calls,
        "a natural no-reset fence must not pop a next-prefix tail before acknowledgement"
    );
    assert_eq!(bits(&f.live.win.buf), bits(&[0.5; 31]));
    assert!(f.live.poll_words().is_none());
    f.live.acknowledge_boundary(token).unwrap();
    assert_eq!(
        f.live.advance_boundary(BoundaryInput::SourceEnded),
        BoundaryProgress::Ended
    );
    let attempts = f.attempts.lock().unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!((attempts[0].start, attempts[0].end), (8192, 8223));
    assert_eq!(bits(&attempts[0].pcm), bits(&[0.5; 31]));
    let calls = f.calls.lock().unwrap().clone();
    assert_eq!(
        f.live.advance_boundary(BoundaryInput::SourceEnded),
        BoundaryProgress::Ended
    );
    assert_eq!(*f.calls.lock().unwrap(), calls);
}

#[test]
fn natural_maintenance_replays_only_the_retained_future_guard_at_unchanged_historical_cut() {
    for rate in [16_000, 44_100, 48_000] {
        let mut f = fixture(false);
        push(&mut f.live, &vec![1.0; 4096], rate);
        push(&mut f.live, &vec![0.0; (rate * 3) as usize], rate);
        let snapshot = f.live.boundary_snapshot().unwrap().natural.unwrap();
        let cut = snapshot.first;
        let state = f.live.boundary.as_ref().unwrap();
        let accepted = state.accepted_end;
        // The replay starts on the VAD window grid at or before the certified future-start bound.
        let replay_start = (accepted - ACCEPTED_GUARD) / VAD_WINDOW as u64 * VAD_WINDOW as u64;
        assert!(f.live.input_clock.canonical_fence(cut).unwrap() < replay_start);
        assert!(replay_start >= state.last_attempt_end);
        assert!(replay_start >= state.retained_start);
        let retained = bits(&state.retained[(replay_start - state.retained_start) as usize..]);
        let before = (
            f.live.input_clock.frames,
            f.live.output_position(),
            bits(&f.live.win.buf),
            f.live.skipped_clock.fractional_seconds().to_bits(),
        );
        let attempts = f.attempts.lock().unwrap().len();
        let token = f
            .live
            .latch_boundary(cut, BoundaryKind::NaturalMaintenance)
            .unwrap();
        assert_eq!(token.source_frame, cut);
        assert_eq!(
            f.live.advance_boundary(BoundaryInput::Continue),
            BoundaryProgress::OldSettled(token)
        );
        f.live.poll_words();
        f.live.take_decode_report();
        let call_count = f.calls.lock().unwrap().len();
        f.live.acknowledge_boundary(token).unwrap();
        let calls = f.calls.lock().unwrap();
        assert!(
            calls.len() > call_count,
            "maintenance must reset the native epoch"
        );
        assert_eq!(calls[call_count], Call::Reset);
        assert!(
            calls[call_count + 1..]
                .iter()
                .all(|call| matches!(call, Call::Accept(_)))
        );
        assert_eq!(
            calls[call_count + 1..]
                .iter()
                .map(|call| match call {
                    Call::Accept(count) => *count as u64,
                    _ => 0,
                })
                .sum::<u64>(),
            accepted - replay_start
        );
        drop(calls);
        let state = f.live.boundary.as_ref().unwrap();
        assert_eq!(state.owned_source_start, cut);
        assert_eq!(state.accepted_end, accepted);
        assert_eq!(state.native_accepted, accepted - replay_start);
        assert_eq!(state.retained_start, replay_start);
        assert_eq!(bits(&state.retained), retained);
        assert_eq!(f.live.vad_base_samples, replay_start);
        assert_eq!(
            (
                f.live.input_clock.frames,
                f.live.output_position(),
                bits(&f.live.win.buf),
                f.live.skipped_clock.fractional_seconds().to_bits()
            ),
            before
        );
        assert_eq!(f.attempts.lock().unwrap().len(), attempts);
        assert!(f.live.take_decode_report().outcomes().is_empty());
    }
}

#[test]
fn natural_maintenance_uses_retained_guard_even_when_historical_cut_pcm_was_retired() {
    let mut f = fixture(false);
    push(&mut f.live, &[0.0; 32_000], 16_000);
    let first = f
        .live
        .latch_boundary(16_000, BoundaryKind::Natural)
        .unwrap();
    f.live.acknowledge_boundary(first).unwrap();
    let retired = f.live.boundary.as_ref().unwrap().retained_start;
    assert!(retired > 24_000);
    push(&mut f.live, &[0.0; 16_000], 16_000);
    let accepted = f.live.boundary.as_ref().unwrap().accepted_end;
    let token = f
        .live
        .latch_boundary(24_000, BoundaryKind::NaturalMaintenance)
        .unwrap();
    f.live.acknowledge_boundary(token).unwrap();
    let state = f.live.boundary.as_ref().unwrap();
    assert_eq!(state.owned_source_start, 24_000);
    let replay_start = (accepted - ACCEPTED_GUARD) / VAD_WINDOW as u64 * VAD_WINDOW as u64;
    assert_eq!(state.retained_start, replay_start);
    assert_eq!(state.native_accepted, accepted - replay_start);
    assert!(f.attempts.lock().unwrap().is_empty());
}

#[test]
fn ordinary_policy_boundaries_have_epoch_headroom_through_repeated_natural_and_forced_maintenance()
{
    use corti_transcribe::boundary::{BoundaryPolicy, BoundaryStep, Observation, TrackBoundary};
    use std::time::Duration;

    for (rate, voiced) in [
        (16_000, false),
        (16_000, true),
        (44_100, true),
        (48_000, false),
    ] {
        let nominal = u64::from(rate);
        let mut start = 0;
        let mut policy = BoundaryPolicy::new(start, nominal, rate, Duration::from_secs(1)).unwrap();
        let capacity = boundary_resampler_bounds(rate)
            .unwrap()
            .rolling_capacity(0, 377, 377, policy.maximum_owned_frames())
            .unwrap();
        // The tightest legal epoch for each kind: natural maintenance never replays the forced
        // warm-up pre-roll, so it arms at every ordinary boundary without that reserve.
        let epoch_limit =
            capacity.minimum_epoch_samples - if voiced { 0 } else { FORCED_REPLAY_PREROLL };
        let mut f = fixture_with_limits(
            false,
            BoundaryLimits {
                max_retained_samples: capacity.retained_samples,
                max_epoch_samples: epoch_limit,
                max_source_push: 377,
            },
        );
        let mut completed = 0;
        while completed < 4 {
            f.live.poll_words();
            let snapshot = f.live.boundary_snapshot().unwrap();
            let received = f.live.input_clock.frames;
            let step = policy
                .observe(Observation {
                    received_through: received,
                    tracks: [
                        TrackBoundary::Established {
                            start_frame: start,
                            delivered_through: snapshot.delivered_through,
                            natural: snapshot.natural,
                        },
                        TrackBoundary::NeverPresent,
                    ],
                    text_full: false,
                    maintenance_armed: snapshot.epoch_remaining <= capacity.maintenance_trigger,
                })
                .unwrap();
            match step {
                BoundaryStep::Admit { frames } => {
                    let count = frames.min(377) as usize;
                    f.live
                        .push(&vec![if voiced { 1.0 } else { 0.0 }; count], rate);
                }
                BoundaryStep::Selected {
                    fence,
                    reset_for_maintenance,
                    ..
                } => {
                    assert!(
                        reset_for_maintenance,
                        "tiny epochs arm before the ordinary boundary"
                    );
                    let kind = if voiced {
                        BoundaryKind::Forced
                    } else {
                        BoundaryKind::NaturalMaintenance
                    };
                    assert_eq!(
                        fence.frame,
                        start
                            + if voiced {
                                policy.maximum_owned_frames()
                            } else {
                                nominal
                            }
                    );
                    let token = f.live.latch_boundary(fence.frame, kind).unwrap();
                    loop {
                        match f.live.advance_boundary(BoundaryInput::Continue) {
                            BoundaryProgress::Waiting => {
                                assert!(
                                    f.live.input_clock.frames - fence.frame
                                        < capacity.settlement_source_reserve
                                );
                                f.live.push(&[1.0; 377], rate);
                            }
                            BoundaryProgress::OldSettled(actual) => {
                                assert_eq!(actual, token);
                                f.live.poll_words();
                                let received = f.live.input_clock.frames;
                                let output = f.live.output_position();
                                f.live.acknowledge_boundary(token).unwrap();
                                assert_eq!(f.live.input_clock.frames, received);
                                assert_eq!(f.live.output_position(), output);
                                assert!(
                                    f.live.boundary.as_ref().unwrap().native_accepted
                                        <= capacity.replay_samples
                                );
                                assert!(
                                    f.live.boundary_snapshot().unwrap().epoch_remaining
                                        > capacity.maintenance_trigger
                                );
                                break;
                            }
                            other => panic!("ordinary maintenance failed: {other:?}"),
                        }
                    }
                    completed += 1;
                    start = fence.frame;
                    policy =
                        BoundaryPolicy::new(start, nominal, rate, Duration::from_secs(1)).unwrap();
                }
            }
            let state = f.live.boundary.as_ref().unwrap();
            assert!(state.failed.is_none());
            assert!(state.native_accepted <= epoch_limit);
            assert!(state.retained.len() <= capacity.retained_samples);
        }
        let mut epoch = 0;
        let mut resets = 0;
        for call in f.calls.lock().unwrap().iter() {
            match call {
                Call::Accept(count) => {
                    epoch += *count as u64;
                    assert!(epoch <= epoch_limit);
                }
                Call::Reset => {
                    epoch = 0;
                    resets += 1;
                }
                Call::Flush => {}
            }
        }
        assert_eq!(resets, 4);
        assert!(
            f.attempts
                .lock()
                .unwrap()
                .windows(2)
                .all(|pair| pair[0].end <= pair[1].start)
        );
        assert!(f.live.decode_complete());
    }
}

#[test]
fn no_aec_short_window_capacity_matches_independent_matrix_pins() {
    // Frozen Python/native matrix pins, independently calculated from the pinned f32 filter.
    // Q is a four-second nominal window plus the unchanged 22-second deferral.
    for (rate, reserve, settlement, next, trigger, minimum) in [
        (16_000, 10_851, 12_390, 5118, 433_508, 462_410),
        (44_100, 21_818, 8806, 2183, 426_989, 452_307),
        (48_000, 23_339, 8638, 2047, 426_685, 451_835),
    ] {
        let capacity = boundary_resampler_bounds(rate)
            .unwrap()
            .rolling_capacity(0, 4096, 4096, 26 * u64::from(rate))
            .unwrap();
        assert_eq!(
            (
                capacity.settlement_source_reserve,
                capacity.settlement_accept,
                capacity.next_accept,
                capacity.maintenance_trigger,
                capacity.minimum_epoch_samples,
            ),
            (reserve, settlement, next, trigger, minimum),
            "rate={rate}"
        );
    }
}

#[test]
fn maintenance_capacity_reserves_an_ordinary_owned_window_before_the_next_settlement() {
    for rate in [8_000, 16_000, 44_100, 48_000, 192_000] {
        let owned_source = u64::from(rate) * 82;
        let capacity = boundary_resampler_bounds(rate)
            .unwrap()
            .rolling_capacity(16_384, 4096, 4096 + 16_384, owned_source)
            .unwrap();
        let owned = (owned_source * 16_000).div_ceil(u64::from(rate));
        assert_eq!(
            capacity.maintenance_trigger,
            owned + capacity.settlement_accept + capacity.next_accept,
            "arming must leave the ordinary nominal-plus-deferral window intact at rate {rate}"
        );
        assert_eq!(
            capacity.minimum_epoch_samples,
            owned + 2 * capacity.settlement_accept + capacity.next_accept + REPLAY_CONTEXT + 1
        );
        assert!(
            capacity.minimum_epoch_samples - capacity.replay_samples > capacity.maintenance_trigger
        );
    }
}

#[test]
fn explicit_forced_fences_at_the_maintenance_threshold_have_replay_headroom() {
    let capacity = boundary_resampler_bounds(16_000)
        .unwrap()
        .rolling_capacity(0, 377, 377, 50_000)
        .unwrap();
    let epoch_limit = capacity.minimum_epoch_samples + 1;
    let mut f = fixture_with_limits(
        false,
        BoundaryLimits {
            max_retained_samples: capacity.retained_samples,
            max_epoch_samples: epoch_limit,
            max_source_push: 377,
        },
    );
    assert_eq!(
        f.live.boundary.as_ref().unwrap().retained.capacity(),
        capacity.retained_samples
    );
    let mut pending = None;
    let mut previous_cut = 0;
    let mut completed = 0;
    let mut admitted = 0;
    while completed < 20 {
        let snapshot = f.live.boundary_snapshot().unwrap();
        if pending.is_none() && snapshot.epoch_remaining <= capacity.maintenance_trigger {
            assert!(snapshot.epoch_remaining >= capacity.settlement_accept);
            let cut = f.live.input_clock.frames;
            assert!(cut > previous_cut);
            if completed == 0 {
                assert!(
                    cut < 50_000,
                    "this explicit low-level fence precedes nominal due"
                );
            }
            pending = Some(f.live.latch_boundary(cut, BoundaryKind::Forced).unwrap());
            previous_cut = cut;
        }
        if let Some(token) = pending {
            match f.live.advance_boundary(BoundaryInput::Continue) {
                BoundaryProgress::Waiting => {}
                BoundaryProgress::OldSettled(actual) => {
                    assert_eq!(actual, token);
                    f.live.poll_words();
                    let source_before = f.live.input_clock.frames;
                    f.live.acknowledge_boundary(token).unwrap();
                    assert_eq!(f.live.input_clock.frames, source_before);
                    let state = f.live.boundary.as_ref().unwrap();
                    assert!(state.native_accepted <= capacity.replay_samples);
                    assert!(
                        f.live.boundary_snapshot().unwrap().epoch_remaining
                            > capacity.maintenance_trigger
                    );
                    pending = None;
                    completed += 1;
                    continue;
                }
                other => panic!("maintenance failed instead of settling: {other:?}"),
            }
        }
        f.live.push(&[1.0; 377], 16_000);
        admitted += 377;
        assert_eq!(f.live.input_clock.frames, admitted);
        let state = f.live.boundary.as_ref().unwrap();
        assert!(state.failed.is_none());
        assert!(state.native_accepted <= epoch_limit);
        assert!(state.retained.len() <= capacity.retained_samples);
    }
    assert_eq!(
        f.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| **call == Call::Reset)
            .count(),
        20
    );
    assert!(
        f.attempts
            .lock()
            .unwrap()
            .windows(2)
            .all(|pair| pair[0].end <= pair[1].start)
    );
    assert!(f.live.decode_complete());
    assert_eq!(
        boundary_resampler_bounds(48_000).unwrap().rolling_capacity(
            u64::MAX,
            4096,
            4096,
            60 * 48_000
        ),
        Err(BoundaryError::RetentionLimit)
    );
}
