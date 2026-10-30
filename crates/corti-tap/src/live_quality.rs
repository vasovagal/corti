//! The live preview owns transient decoder facts until the recorder supplies capture authority.

use std::collections::VecDeque;
use std::path::Path;

use anyhow::{Context, Result};
use corti_aec::StreamingAec;
use corti_core::{CaptureEpoch, MicTapMismatches, QualityRecord, QualityTrack};
use corti_transcribe::decode::{DecodeQualityTracker, DecodeReport};
use corti_transcribe::segment::Word;

// Match the capture writer's bounded admission; larger synthetic/malformed chunks are sliced too.
const BLOCK_FRAMES: usize = 4096;

pub(super) trait Channel {
    fn push(&mut self, samples: &[f32], rate: u32);
    fn poll_words(&mut self) -> Option<Vec<Word>>;
    fn finish(&mut self) -> Vec<Word>;
    fn take_decode_report(&mut self) -> DecodeReport;
    fn decode_complete(&self) -> bool;
}

impl Channel for corti_transcribe_local::LiveTranscriber {
    fn push(&mut self, samples: &[f32], rate: u32) {
        Self::push(self, samples, rate);
    }
    fn poll_words(&mut self) -> Option<Vec<Word>> {
        Self::poll_words(self)
    }
    fn finish(&mut self) -> Vec<Word> {
        Self::finish(self)
    }
    fn take_decode_report(&mut self) -> DecodeReport {
        Self::take_decode_report(self)
    }
    fn decode_complete(&self) -> bool {
        Self::decode_complete(self)
    }
}

pub(super) struct Preview<C> {
    mic: Option<C>,
    them: C,
    aec: Option<StreamingAec>,
    unavailable: VecDeque<bool>,
    started: [bool; 2],
    origin_seconds: [f64; 2],
    received_frames: u64,
    stopped: [bool; 2],
    incomplete: [bool; 2],
    tracker: DecodeQualityTracker,
    mismatches: MicTapMismatches,
    finished: bool,
}

impl<C: Channel> Preview<C> {
    pub(super) fn new(mic: Option<C>, them: C, aec: Option<StreamingAec>) -> Self {
        Self {
            mic,
            them,
            aec,
            unavailable: VecDeque::new(),
            started: [false; 2],
            origin_seconds: [0.0; 2],
            received_frames: 0,
            stopped: [false; 2],
            incomplete: [false; 2],
            tracker: DecodeQualityTracker::default(),
            mismatches: MicTapMismatches::default(),
            finished: false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn process(
        &mut self,
        epoch: CaptureEpoch,
        retained_start: u64,
        mic: &[f32],
        tap: &[f32],
        unavailable: &[bool],
        rate: u32,
        emit: &mut impl FnMut(&str, &[Word]),
    ) {
        let frames = mic.len().max(tap.len());
        self.mismatches
            .observe(epoch, retained_start, mic.len(), tap.len(), rate);
        if self.finished
            || rate == 0
            || epoch.retained_rate_hz != rate
            || (!unavailable.is_empty() && unavailable.len() != frames)
        {
            self.omit_failure();
            self.stopped = [true; 2];
            return;
        }
        if frames != 0 {
            let was_started = self.started;
            self.started[0] |= self.mic.is_some() && (epoch.mic_channels != 0 || !mic.is_empty());
            self.started[1] |= epoch.tap_channels != 0 || !tap.is_empty();
            for (index, was_started) in was_started.into_iter().enumerate() {
                if self.started[index] && !was_started {
                    self.origin_seconds[index] = self.received_frames as f64 / f64::from(rate);
                }
            }
        }
        let Some(received_end) = self.received_frames.checked_add(frames as u64) else {
            self.omit_failure();
            self.stopped = [true; 2];
            return;
        };
        self.received_frames = received_end;
        let mut mic_pad = [0.0; BLOCK_FRAMES];
        let mut tap_pad = [0.0; BLOCK_FRAMES];
        for offset in (0..frames).step_by(BLOCK_FRAMES) {
            let take = BLOCK_FRAMES.min(frames - offset);
            let Some(retained) = retained_start.checked_add(offset as u64) else {
                self.omit_failure();
                return;
            };
            let mic_slice = padded_slice(mic, offset, take, &mut mic_pad);
            let tap_slice = padded_slice(tap, offset, take, &mut tap_pad);
            for (index, track) in [QualityTrack::Mic, QualityTrack::Tap]
                .into_iter()
                .enumerate()
            {
                if self.started[index] {
                    let span = self
                        .tracker
                        .observe_input(track, epoch, retained, take as u64);
                    if self.stopped[index] {
                        // The channel was abandoned after panic; this received interval is now known
                        // to be unprocessed, unlike the panic's possibly buffered region extent.
                        let mut report = DecodeReport::default();
                        report.record(corti_transcribe::decode::RegionOutcome {
                            span,
                            status: corti_transcribe::decode::RegionStatus::Failed(
                                corti_transcribe::decode::DecodeErrorCategory::BackendFailure,
                            ),
                        });
                        self.tracker.record_report(track, report);
                    }
                }
            }
            if self.started[0] && !self.stopped[0] {
                let mask = (0..take).map(|index| {
                    offset + index >= mic.len()
                        || unavailable.get(offset + index).copied().unwrap_or(false)
                });
                let clean = if let Some(aec) = self.aec.as_mut() {
                    if self.unavailable.len().saturating_add(take)
                        > aec.max_output_lag_samples().saturating_add(BLOCK_FRAMES)
                    {
                        self.omit_failure();
                        self.stopped[0] = true;
                        Vec::new()
                    } else {
                        self.unavailable.extend(mask);
                        let mut clean = aec.push(mic_slice, tap_slice);
                        if !mask_output(&mut self.unavailable, &mut clean) {
                            self.omit_failure();
                            self.stopped[0] = true;
                            clean.clear();
                        }
                        clean
                    }
                } else {
                    mic_slice
                        .iter()
                        .zip(mask)
                        .map(|(&sample, missing)| if missing { 0.0 } else { sample })
                        .collect()
                };
                if !clean.is_empty() {
                    let channel = self.mic.as_mut().expect("started microphone");
                    call_channel(
                        &mut self.tracker,
                        &mut self.incomplete[0],
                        &mut self.stopped[0],
                        QualityTrack::Mic,
                        channel,
                        |channel| channel.push(&clean, rate),
                    );
                    drain_words(channel, "Me", self.origin_seconds[0], emit);
                }
            }
            if self.started[1] && !self.stopped[1] {
                call_channel(
                    &mut self.tracker,
                    &mut self.incomplete[1],
                    &mut self.stopped[1],
                    QualityTrack::Tap,
                    &mut self.them,
                    |channel| channel.push(tap_slice, rate),
                );
                drain_words(&mut self.them, "Them", self.origin_seconds[1], emit);
            }
        }
    }

    pub(super) fn finish(&mut self, rate: u32, emit: &mut impl FnMut(&str, &[Word])) {
        if self.finished {
            return;
        }
        self.finished = true;
        if let Some(aec) = self.aec.take() {
            let mut tail = aec.finish();
            if !mask_output(&mut self.unavailable, &mut tail) || !self.unavailable.is_empty() {
                self.omit_failure();
                self.stopped[0] = true;
            }
            if !tail.is_empty()
                && !self.stopped[0]
                && let Some(channel) = self.mic.as_mut()
            {
                call_channel(
                    &mut self.tracker,
                    &mut self.incomplete[0],
                    &mut self.stopped[0],
                    QualityTrack::Mic,
                    channel,
                    |channel| channel.push(&tail, rate),
                );
                drain_words(channel, "Me", self.origin_seconds[0], emit);
            }
        }
        for (index, track, label, channel) in [
            (0, QualityTrack::Mic, "Me", self.mic.as_mut()),
            (1, QualityTrack::Tap, "Them", Some(&mut self.them)),
        ] {
            if let Some(channel) = channel
                && self.started[index]
                && !self.stopped[index]
            {
                if let Some(words) = call_channel(
                    &mut self.tracker,
                    &mut self.incomplete[index],
                    &mut self.stopped[index],
                    track,
                    channel,
                    Channel::finish,
                ) {
                    emit_words(label, words, self.origin_seconds[index], emit);
                }
                drain_words(channel, label, self.origin_seconds[index], emit);
            }
        }
    }

    fn omit_failure(&mut self) {
        let mut report = DecodeReport::default();
        report.omit_failure();
        self.tracker.record_report(QualityTrack::Both, report);
    }

    pub(super) fn settle(&self, capture: &QualityRecord) -> Result<QualityRecord> {
        Ok(self.tracker.settle(&self.mismatches.settle(capture)?)?)
    }

    pub(super) fn settle_and_persist(
        &self,
        wav: &Path,
        capture: Option<&QualityRecord>,
    ) -> Result<QualityRecord> {
        let identity = recording_identity(wav)?;
        // An unsuccessful recorder finish may leave InProgress/Finalizing authority. Reading and
        // updating its history cannot promote that phase, and absence/corruption is not permission
        // to manufacture a capture envelope.
        let read = corti_core::quality_store::load_quality(wav, identity, None)?;
        let snapshot = read
            .snapshot
            .context("capture quality authority is unavailable")?;
        let mut baseline = capture.unwrap_or(snapshot.quality()).clone();
        baseline.merge_snapshot(snapshot.quality())?;
        if let Some(history) = snapshot.settled_quality() {
            baseline.merge_snapshot(history)?;
        }
        let quality = self.settle(&baseline)?;
        corti_core::quality_store::persist_settled_quality(
            wav,
            identity,
            snapshot.settled_quality(),
            &quality,
        )?;
        Ok(quality)
    }
}

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

fn mask_output(pending: &mut VecDeque<bool>, output: &mut [f32]) -> bool {
    if output.len() > pending.len() {
        return false;
    }
    for sample in output {
        if pending.pop_front().unwrap_or(true) {
            *sample = 0.0;
        }
    }
    true
}

fn call_channel<C: Channel, T>(
    tracker: &mut DecodeQualityTracker,
    incomplete: &mut bool,
    stopped: &mut bool,
    track: QualityTrack,
    channel: &mut C,
    work: impl FnOnce(&mut C) -> T,
) -> Option<T> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(channel)));
    let mut report = channel.take_decode_report();
    if !channel.decode_complete() && !*incomplete && report.is_complete() {
        report.omit_failure();
    }
    *incomplete |= !channel.decode_complete() || !report.is_complete();
    if result.is_err() {
        // Known reported spans remain exact; the panic's remaining buffered extent is unknown.
        report.omit_failure();
        *stopped = true;
        *incomplete = true;
    }
    tracker.record_report(track, report);
    result.ok()
}

fn drain_words<C: Channel>(
    channel: &mut C,
    label: &str,
    origin: f64,
    emit: &mut impl FnMut(&str, &[Word]),
) {
    // A read-only queue drain preserves words produced before a later region panicked. It does not
    // resume the decoder; a stopped channel receives no later push or finish.
    if let Some(words) = channel.poll_words() {
        emit_words(label, words, origin, emit);
    }
}

fn emit_words(
    label: &str,
    mut words: Vec<Word>,
    origin: f64,
    emit: &mut impl FnMut(&str, &[Word]),
) {
    for word in &mut words {
        word.start += origin;
        word.end += origin;
    }
    emit(label, &words);
}

fn recording_identity(wav: &Path) -> Result<&str> {
    wav.file_stem()
        .and_then(|name| name.to_str())
        .context("invalid capture recording identity")
}

/// The capture writer remains the authority. Publication updates only its settled-quality adjunct.
#[cfg(test)]
pub(super) fn persist(wav: &Path, quality: &QualityRecord) -> Result<()> {
    let identity = recording_identity(wav)?;
    corti_core::quality_store::persist_settled_quality(wav, identity, None, quality)?;
    Ok(())
}

pub(super) fn terminal_status(quality: &QualityRecord, dropped: u64) -> Result<()> {
    for span in &quality.spans {
        if span.class != corti_core::QualityClass::DegradedOnly {
            eprintln!(
                "live coverage: {:?}, {:?}, {:?}; epoch {} frames {}..{}; {:?}",
                span.cause,
                span.track,
                span.stage,
                span.epoch,
                span.start_frame,
                span.end_frame,
                span.resolution
            );
        }
    }
    if quality.overflow.outstanding_omitted_spans[0] != 0
        || quality.overflow.outstanding_omitted_spans[1] != 0
        || quality.overflow.counter_saturated
    {
        eprintln!("live coverage: some source/processing locations are unknown");
    }
    anyhow::ensure!(
        dropped == 0 && quality.completeness() == corti_core::SourceCompleteness::Complete,
        "live transcript is incomplete or has unknown coverage; the recording was retained"
    );
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use corti_core::{
        CaptureEpochReason, QualityCause, QualityClass, QualityResolution, QualityStage,
    };
    use corti_transcribe::decode::{DecodeErrorCategory, InputSpan, RegionOutcome, RegionStatus};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Default)]
    struct Calls {
        input: Vec<f32>,
        pushes: usize,
        finishes: usize,
        reports: usize,
    }

    pub(crate) struct Scripted {
        calls: Rc<RefCell<Calls>>,
        pending: Vec<Word>,
        report: DecodeReport,
        complete: bool,
        mode: &'static str,
    }

    impl Scripted {
        fn new(mode: &'static str) -> Self {
            Self {
                calls: Default::default(),
                pending: Vec::new(),
                report: DecodeReport::default(),
                complete: true,
                mode,
            }
        }
        fn add_report(&mut self, span: InputSpan, status: RegionStatus) {
            self.complete &= status.is_complete();
            self.report.record(RegionOutcome { span, status });
        }
    }

    fn word(index: usize) -> Word {
        Word {
            start: index as f64,
            end: index as f64 + 0.5,
            text: format!("synthetic-region-{}", index + 1),
        }
    }

    impl Channel for Scripted {
        fn push(&mut self, audio: &[f32], _: u32) {
            let (start, end, call) = {
                let mut calls = self.calls.borrow_mut();
                let start = calls.input.len();
                calls.input.extend_from_slice(audio);
                calls.pushes += 1;
                (start as u64, calls.input.len() as u64, calls.pushes)
            };
            let span = InputSpan {
                start_frame: start,
                end_frame: end,
            };
            if call == 2 {
                match self.mode {
                    "failed" | "panic" => {
                        self.add_report(span, RegionStatus::Failed(DecodeErrorCategory::NoResult))
                    }
                    "partial" => {
                        self.pending.push(word(1));
                        self.add_report(
                            span,
                            RegionStatus::Partial(DecodeErrorCategory::OutputTruncated),
                        );
                    }
                    "metadata" => {
                        for _ in 0..=corti_core::MAX_QUALITY_SPANS {
                            self.add_report(span, RegionStatus::RecognizedEmpty);
                        }
                    }
                    _ => self.add_report(span, RegionStatus::RecognizedEmpty),
                }
                assert_ne!(self.mode, "panic", "synthetic decoder panic");
            } else {
                self.pending.push(word(call - 1));
                self.add_report(span, RegionStatus::Complete);
            }
        }
        fn poll_words(&mut self) -> Option<Vec<Word>> {
            (!self.pending.is_empty()).then(|| std::mem::take(&mut self.pending))
        }
        fn finish(&mut self) -> Vec<Word> {
            self.calls.borrow_mut().finishes += 1;
            if matches!(self.mode, "tail" | "finish_panic") {
                let end = self.calls.borrow().input.len() as u64;
                self.add_report(
                    InputSpan {
                        start_frame: end - 1,
                        end_frame: end,
                    },
                    RegionStatus::Failed(DecodeErrorCategory::BackendFailure),
                );
                if self.mode == "finish_panic" {
                    self.pending.push(word(3));
                    panic!("synthetic finish panic after report");
                }
            }
            std::mem::take(&mut self.pending)
        }
        fn take_decode_report(&mut self) -> DecodeReport {
            self.calls.borrow_mut().reports += 1;
            std::mem::take(&mut self.report)
        }
        fn decode_complete(&self) -> bool {
            self.complete
        }
    }

    fn epoch(mic: bool, tap: bool) -> CaptureEpoch {
        CaptureEpoch {
            id: 0,
            source_start_ns: Some(0),
            source_rate_hz: 16_000,
            source_frame_base: 0,
            retained_start_frame: 0,
            retained_rate_hz: 16_000,
            mic_channels: u32::from(mic),
            tap_channels: u32::from(tap),
            reason: CaptureEpochReason::Start,
        }
    }
    fn authority(epochs: &[CaptureEpoch]) -> QualityRecord {
        let mut quality = QualityRecord::observed();
        for &epoch in epochs {
            quality.insert_epoch(epoch).unwrap();
        }
        quality
    }

    #[test]
    fn live_preview_failed_partial_empty_and_metadata_keep_exact_survivors() {
        for mode in [
            "failed",
            "partial",
            "empty",
            "metadata",
            "tail",
            "finish_panic",
        ] {
            let channel = Scripted::new(mode);
            let calls = channel.calls.clone();
            let mut preview = Preview::new(None, channel, None);
            let mut output = Vec::new();
            let mut emit = |_: &str, words: &[Word]| output.extend_from_slice(words);
            for index in 0..3 {
                preview.process(
                    epoch(false, true),
                    index * 16,
                    &[],
                    &[0.0; 16],
                    &[],
                    16_000,
                    &mut emit,
                );
            }
            preview.finish(16_000, &mut emit);
            preview.finish(16_000, &mut emit);
            let expected = if mode == "finish_panic" {
                vec![word(0), word(2), word(3)]
            } else if mode == "partial" {
                vec![word(0), word(1), word(2)]
            } else {
                vec![word(0), word(2)]
            };
            assert_eq!(output, expected);
            assert_eq!(
                (
                    calls.borrow().pushes,
                    calls.borrow().finishes,
                    calls.borrow().reports
                ),
                (3, 1, 4)
            );
            let capture = authority(&[epoch(false, true)]);
            let quality = preview.settle(&capture).unwrap();
            assert_eq!(quality, preview.settle(&capture).unwrap());
            assert_eq!(quality.epochs, capture.epochs);
            let incomplete = matches!(mode, "failed" | "partial" | "tail" | "finish_panic");
            assert_eq!(terminal_status(&quality, 0).is_err(), incomplete);
            if incomplete {
                assert_eq!(quality.spans.len(), 1);
                let span = quality.spans[0];
                assert_eq!(
                    (
                        span.epoch,
                        span.track,
                        span.stage,
                        span.class,
                        span.resolution
                    ),
                    (
                        0,
                        QualityTrack::Tap,
                        QualityStage::Live,
                        QualityClass::ProcessingLoss,
                        QualityResolution::Outstanding
                    )
                );
                assert_eq!(
                    (span.start_frame, span.end_frame),
                    if matches!(mode, "tail" | "finish_panic") {
                        (47, 48)
                    } else {
                        (16, 32)
                    }
                );
                assert_eq!(
                    span.cause,
                    if mode == "failed" {
                        QualityCause::NoResult
                    } else {
                        QualityCause::DecodeFailure
                    }
                );
            } else {
                assert!(quality.spans.is_empty());
                assert_eq!(
                    quality.overflow.omitted_spans,
                    if mode == "metadata" {
                        [0, 0, 1]
                    } else {
                        [0; 3]
                    }
                );
            }
        }
    }

    #[test]
    fn live_preview_panic_drains_known_report_and_keeps_other_channel_without_retry() {
        let mic = Scripted::new("panic");
        let mic_calls = mic.calls.clone();
        let tap = Scripted::new("empty");
        let tap_calls = tap.calls.clone();
        let mut preview = Preview::new(Some(mic), tap, None);
        let mut output = Vec::new();
        let mut emit =
            |label: &str, words: &[Word]| output.push((label.to_owned(), words.to_vec()));
        for index in 0..3 {
            preview.process(
                epoch(true, true),
                index * 16,
                &[1.0; 16],
                &[2.0; 16],
                &[],
                16_000,
                &mut emit,
            );
        }
        preview.finish(16_000, &mut emit);
        assert_eq!(
            (mic_calls.borrow().pushes, mic_calls.borrow().finishes),
            (2, 0)
        );
        assert_eq!(
            (tap_calls.borrow().pushes, tap_calls.borrow().finishes),
            (3, 1)
        );
        assert!(output.contains(&("Them".into(), vec![word(2)])));
        let quality = preview.settle(&authority(&[epoch(true, true)])).unwrap();
        assert!(quality.spans.iter().any(|s| s.track == QualityTrack::Mic
            && s.cause == QualityCause::NoResult
            && (s.start_frame, s.end_frame) == (16, 32)));
        assert!(quality.spans.iter().any(|s| s.track == QualityTrack::Mic
            && s.cause == QualityCause::DecodeFailure
            && (s.start_frame, s.end_frame) == (32, 48)));
        assert_eq!(quality.overflow.outstanding_omitted_spans[1], 1);
        assert!(terminal_status(&quality, 0).is_err());
    }

    #[test]
    fn live_preview_alignment_preserves_both_prefixes_and_discontinuous_source_bindings() {
        for (mic_len, tap_len) in [(3, 5), (5, 3), (0, 5)] {
            let mic = Scripted::new("failed");
            let mic_calls = mic.calls.clone();
            let tap = Scripted::new("failed");
            let tap_calls = tap.calls.clone();
            let mut preview = Preview::new(Some(mic), tap, None);
            let first = epoch(true, true);
            let later = CaptureEpoch {
                id: 1,
                retained_start_frame: 20,
                source_frame_base: 200,
                source_start_ns: Some(20_000_000),
                reason: CaptureEpochReason::LayoutChange,
                ..first
            };
            preview.process(first, 0, &[1.0; 5], &[2.0; 5], &[], 16_000, &mut |_, _| {});
            preview.process(
                later,
                20,
                &vec![3.0; mic_len],
                &vec![4.0; tap_len],
                &[],
                16_000,
                &mut |_, _| {},
            );
            preview.process(later, 25, &[5.0; 5], &[6.0; 5], &[], 16_000, &mut |_, _| {});
            preview.finish(16_000, &mut |_, _| {});
            let mut expected_mic = vec![1.0; 5];
            expected_mic.extend(vec![3.0; mic_len]);
            expected_mic.resize(10, 0.0);
            expected_mic.extend([5.0; 5]);
            let mut expected_tap = vec![2.0; 5];
            expected_tap.extend(vec![4.0; tap_len]);
            expected_tap.resize(10, 0.0);
            expected_tap.extend([6.0; 5]);
            assert_eq!(mic_calls.borrow().input, expected_mic);
            assert_eq!(tap_calls.borrow().input, expected_tap);
            let mut capture = authority(&[first, later]);
            let drop = corti_core::QualitySpan {
                epoch: 0,
                start_frame: 5,
                end_frame: 20,
                track: QualityTrack::Both,
                cause: QualityCause::TeeDrop,
                class: QualityClass::ProcessingLoss,
                stage: QualityStage::Live,
                resolution: QualityResolution::Outstanding,
            };
            capture.insert_span(drop).unwrap();
            let quality = preview.settle(&capture).unwrap();
            assert!(quality.spans.contains(&drop));
            assert_eq!(
                quality
                    .spans
                    .iter()
                    .filter(|s| s.cause == QualityCause::NoResult)
                    .count(),
                2
            );
            assert!(
                quality
                    .spans
                    .iter()
                    .filter(|s| s.cause == QualityCause::NoResult)
                    .all(|s| s.epoch == 1 && (s.start_frame, s.end_frame) == (200, 205))
            );
            let mismatch = quality
                .spans
                .iter()
                .find(|s| s.cause == QualityCause::MicTapMismatch)
                .unwrap();
            assert_eq!(
                (mismatch.start_frame, mismatch.end_frame),
                (200 + mic_len.min(tap_len) as u64, 205)
            );
            assert_eq!(
                mismatch.track,
                if mic_len < tap_len {
                    QualityTrack::Mic
                } else {
                    QualityTrack::Tap
                }
            );
            assert_eq!(quality.epochs, vec![first, later]);
        }
        let mut tap_only = Preview::new(None, Scripted::new("empty"), None);
        tap_only.process(
            epoch(false, true),
            0,
            &[],
            &[1.0; 5],
            &[],
            16_000,
            &mut |_, _| {},
        );
        tap_only.finish(16_000, &mut |_, _| {});
        assert_eq!(
            tap_only.settle(&authority(&[epoch(false, true)])).unwrap(),
            authority(&[epoch(false, true)])
        );
        // The first received tee chunk may belong to a later epoch. Do not create a substitute
        // Start epoch; wait for the recorder's real authority to position the failure.
        let first = epoch(false, true);
        let later = CaptureEpoch {
            id: 1,
            retained_start_frame: 20,
            source_frame_base: 200,
            source_start_ns: Some(20_000_000),
            reason: CaptureEpochReason::LayoutChange,
            ..first
        };
        let mut late = Preview::new(None, Scripted::new("failed"), None);
        late.process(later, 20, &[], &[1.0; 5], &[], 16_000, &mut |_, _| {});
        late.process(later, 25, &[], &[2.0; 5], &[], 16_000, &mut |_, _| {});
        late.finish(16_000, &mut |_, _| {});
        let quality = late.settle(&authority(&[first, later])).unwrap();
        assert_eq!(quality.spans.len(), 1);
        assert_eq!(
            (
                quality.spans[0].epoch,
                quality.spans[0].start_frame,
                quality.spans[0].end_frame
            ),
            (1, 205, 210)
        );
    }

    #[test]
    fn live_preview_late_track_origin_and_absent_epoch_keep_clock_and_padding() {
        let mic = Scripted::new("empty");
        let calls = mic.calls.clone();
        let mut preview = Preview::new(Some(mic), Scripted::new("empty"), None);
        let first = epoch(false, true);
        let second = CaptureEpoch {
            id: 1,
            retained_start_frame: 16,
            source_start_ns: Some(1_000_000),
            source_frame_base: 16,
            reason: CaptureEpochReason::LayoutChange,
            mic_channels: 1,
            ..first
        };
        let absent = CaptureEpoch {
            id: 2,
            retained_start_frame: 32,
            source_start_ns: Some(2_000_000),
            source_frame_base: 32,
            mic_channels: 0,
            ..second
        };
        let returned = CaptureEpoch {
            id: 3,
            retained_start_frame: 48,
            source_start_ns: Some(3_000_000),
            source_frame_base: 48,
            mic_channels: 1,
            ..second
        };
        let mut output = Vec::new();
        let mut emit = |label: &str, words: &[Word]| {
            if label == "Me" {
                output.extend_from_slice(words);
            }
        };
        preview.process(first, 0, &[], &[1.0; 16], &[], 16_000, &mut emit);
        assert!(calls.borrow().input.is_empty());
        preview.process(second, 16, &[2.0; 16], &[1.0; 16], &[], 16_000, &mut emit);
        preview.process(absent, 32, &[], &[1.0; 16], &[], 16_000, &mut emit);
        preview.process(returned, 48, &[3.0; 16], &[1.0; 16], &[], 16_000, &mut emit);
        preview.finish(16_000, &mut emit);
        let mut expected = vec![2.0; 16];
        expected.extend([0.0; 16]);
        expected.extend([3.0; 16]);
        assert_eq!(calls.borrow().input, expected);
        let mut words = vec![word(0), word(2)];
        for w in &mut words {
            w.start += 0.001;
            w.end += 0.001;
        }
        assert_eq!(output, words);
        let capture = authority(&[first, second, absent, returned]);
        assert_eq!(preview.settle(&capture).unwrap(), capture);
    }

    #[test]
    fn live_preview_availability_masks_real_aec_tail_and_rejects_malformed_mask() {
        let mic = Scripted::new("tail");
        let calls = mic.calls.clone();
        let config = corti_aec::AecConfig {
            filter_len: 32,
            ..Default::default()
        };
        let mut reference = StreamingAec::new_with_lookahead_seconds(16_000, config.clone(), 0.01);
        let input: Vec<_> = (0..800).map(|i| ((i as f32) * 0.13).sin()).collect();
        let far: Vec<_> = (0..800).map(|i| ((i as f32) * 0.17).cos()).collect();
        let mut preview = Preview::new(
            Some(mic),
            Scripted::new("empty"),
            Some(StreamingAec::new_with_lookahead_seconds(
                16_000, config, 0.01,
            )),
        );
        let mut expected = Vec::new();
        for offset in (0..800).step_by(100) {
            let mask: Vec<_> = (offset..offset + 100).map(|i| i % 7 == 0).collect();
            expected
                .extend(reference.push(&input[offset..offset + 100], &far[offset..offset + 100]));
            preview.process(
                epoch(true, true),
                offset as u64,
                &input[offset..offset + 100],
                &far[offset..offset + 100],
                &mask,
                16_000,
                &mut |_, _| {},
            );
        }
        expected.extend(reference.finish());
        assert!(
            expected
                .iter()
                .enumerate()
                .any(|(index, sample)| index % 7 == 0 && *sample != 0.0),
            "the mask must remove real filter output"
        );
        for (index, sample) in expected.iter_mut().enumerate() {
            if index % 7 == 0 {
                *sample = 0.0;
            }
        }
        preview.finish(16_000, &mut |_, _| {});
        assert_eq!(calls.borrow().input, expected);
        assert_eq!(calls.borrow().input.len(), 800);
        let quality = preview.settle(&authority(&[epoch(true, true)])).unwrap();
        assert!(
            quality
                .spans
                .iter()
                .any(|span| span.track == QualityTrack::Mic
                    && span.cause == QualityCause::DecodeFailure
                    && (span.start_frame, span.end_frame) == (799, 800))
        );
        let before = calls.borrow().pushes;
        let mut invalid = Preview::new(Some(Scripted::new("empty")), Scripted::new("empty"), None);
        invalid.process(
            epoch(true, true),
            800,
            &[1.0; 4],
            &[2.0; 4],
            &[true],
            16_000,
            &mut |_, _| {},
        );
        invalid.finish(16_000, &mut |_, _| {});
        assert_eq!(invalid.mic.as_ref().unwrap().calls.borrow().pushes, 0);
        assert_eq!(invalid.them.calls.borrow().pushes, 0);
        assert!(
            invalid
                .settle(&authority(&[epoch(true, true)]))
                .unwrap()
                .requires_batch_recovery()
        );
        assert!(before > 0);
    }

    struct Fixture {
        dir: std::path::PathBuf,
        path: std::path::PathBuf,
        capture: QualityRecord,
        frames: u64,
    }
    impl Fixture {
        fn new(capture: QualityRecord, frames: u64) -> Self {
            Self::with_state(capture, frames, true)
        }
        fn with_state(capture: QualityRecord, frames: u64, complete: bool) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "corti-live-preview-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&dir).unwrap();
            let path = dir.join("synthetic.wav");
            let mut store =
                corti_core::quality_store::QualityStore::begin(&path, "synthetic", &capture)
                    .unwrap();
            let data_len = u32::try_from(frames * 2).unwrap();
            let mut wav = Vec::new();
            wav.extend(b"RIFF");
            wav.extend((36 + data_len).to_le_bytes());
            wav.extend(b"WAVEfmt ");
            wav.extend(16u32.to_le_bytes());
            wav.extend(1u16.to_le_bytes());
            wav.extend(1u16.to_le_bytes());
            wav.extend(16_000u32.to_le_bytes());
            wav.extend(32_000u32.to_le_bytes());
            wav.extend(2u16.to_le_bytes());
            wav.extend(16u16.to_le_bytes());
            wav.extend(b"data");
            wav.extend(data_len.to_le_bytes());
            wav.resize(44 + data_len as usize, 0);
            std::fs::write(&path, wav).unwrap();
            if complete {
                store.prepare_finalization(&capture, frames).unwrap();
                store.complete_after_audio_sync(frames).unwrap();
            }
            Self {
                dir,
                path,
                capture,
                frames,
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn live_preview_persistence_retains_capture_wav_and_refuses_conflicts() {
        let fixture = Fixture::new(authority(&[epoch(false, true)]), 48);
        let wav = std::fs::read(&fixture.path).unwrap();
        let mut preview = Preview::new(None, Scripted::new("failed"), None);
        for index in 0..3 {
            preview.process(
                epoch(false, true),
                index * 16,
                &[],
                &[0.0; 16],
                &[],
                16_000,
                &mut |_, _| {},
            );
        }
        preview.finish(16_000, &mut |_, _| {});
        let quality = preview.settle(&fixture.capture).unwrap();
        persist(&fixture.path, &quality).unwrap();
        let read =
            corti_core::quality_store::load_quality(&fixture.path, "synthetic", Some(48)).unwrap();
        let snapshot = read.snapshot.unwrap();
        assert_eq!(snapshot.quality(), &fixture.capture);
        assert_eq!(snapshot.settled_quality(), Some(&quality));
        assert_eq!(
            snapshot.state(),
            corti_core::quality_store::QualityStorageState::AudioDurable
        );
        assert_eq!(std::fs::read(&fixture.path).unwrap(), wav);
        assert!(persist(&fixture.path, &fixture.capture).is_err());
        assert_eq!(
            corti_core::quality_store::load_quality(&fixture.path, "synthetic", Some(48))
                .unwrap()
                .snapshot
                .unwrap()
                .settled_quality(),
            Some(&quality)
        );
    }

    thread_local! {
        static SCENARIO: RefCell<Option<Scenario>> = const { RefCell::new(None) };
    }
    struct Scenario {
        fixture: Fixture,
        mode: &'static str,
        output: Vec<(String, Word)>,
        calls: Option<Rc<RefCell<Calls>>>,
        engines: usize,
        channels: usize,
        finish_error: bool,
    }
    static CALLER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    pub(crate) fn record_words(label: &str, words: &[Word]) {
        SCENARIO.with(|state| {
            if let Some(scenario) = state.borrow_mut().as_mut() {
                scenario
                    .output
                    .extend(words.iter().cloned().map(|word| (label.to_owned(), word)));
            }
        });
    }
    pub(crate) mod native {
        #[derive(Default)]
        pub struct LocalConfig {
            _reserved: bool,
        }
        pub struct LocalTranscriber;
        pub struct LiveEngine;
        impl LocalTranscriber {
            pub fn new(_: LocalConfig) -> Self {
                Self
            }
            pub fn live_engine(self) -> anyhow::Result<LiveEngine> {
                super::SCENARIO.with(|s| s.borrow_mut().as_mut().unwrap().engines += 1);
                Ok(LiveEngine)
            }
        }
        impl LiveEngine {
            pub fn channel(&self) -> anyhow::Result<super::Scripted> {
                super::SCENARIO.with(|s| {
                    let mut s = s.borrow_mut();
                    let s = s.as_mut().unwrap();
                    s.channels += 1;
                    let channel = super::Scripted::new(s.mode);
                    s.calls = Some(channel.calls.clone());
                    Ok(channel)
                })
            }
        }
    }
    pub(crate) mod capture {
        use std::sync::{Arc, mpsc::SyncSender};
        pub struct CaptureChunk {
            pub epoch: corti_core::CaptureEpoch,
            pub retained_start_frame: u64,
            pub mic: Vec<f32>,
            pub tap: Vec<f32>,
            pub unavailable: Vec<bool>,
        }
        pub struct CaptureTee {
            tx: SyncSender<CaptureChunk>,
            dropped: Arc<super::AtomicU64>,
        }
        impl CaptureTee {
            pub fn new(tx: SyncSender<CaptureChunk>) -> Self {
                Self {
                    tx,
                    dropped: Arc::new(super::AtomicU64::new(0)),
                }
            }
            pub fn dropped_counter(&self) -> Arc<super::AtomicU64> {
                self.dropped.clone()
            }
        }
        pub struct Recorder {
            path: std::path::PathBuf,
        }
        impl Recorder {
            pub fn start_tap_only_with_tee(
                _: &corti_core::OwningApp,
                _: Option<()>,
                tee: CaptureTee,
            ) -> anyhow::Result<Self> {
                for index in 0..3 {
                    tee.tx.send(CaptureChunk {
                        epoch: super::epoch(false, true),
                        retained_start_frame: index * 16,
                        mic: vec![],
                        tap: vec![0.0; 16],
                        unavailable: vec![],
                    })?;
                }
                crate::STOP.store(
                    super::SCENARIO.with(|s| !s.borrow().as_ref().unwrap().finish_error),
                    super::Ordering::Relaxed,
                );
                Ok(Self {
                    path: super::SCENARIO
                        .with(|s| s.borrow().as_ref().unwrap().fixture.path.clone()),
                })
            }
            pub fn start_with_tee(
                app: &corti_core::OwningApp,
                options: Option<()>,
                tee: CaptureTee,
            ) -> anyhow::Result<Self> {
                Self::start_tap_only_with_tee(app, options, tee)
            }
            pub fn sample_rate(&self) -> u32 {
                16_000
            }
            pub fn output_path(&self) -> &std::path::Path {
                &self.path
            }
            pub fn finish_tap_only_with_processing(
                self,
            ) -> anyhow::Result<corti_capture::FinishedRecording> {
                anyhow::ensure!(
                    !super::SCENARIO.with(|s| s.borrow().as_ref().unwrap().finish_error),
                    "synthetic capture finalization failure"
                );
                let mut processing = corti_capture::CaptureProcessing::not_applicable();
                processing.quality = Some(
                    super::SCENARIO.with(|s| s.borrow().as_ref().unwrap().fixture.capture.clone()),
                );
                Ok(corti_capture::FinishedRecording {
                    path: self.path,
                    processing,
                })
            }
            pub fn finish_with_processing(
                self,
            ) -> anyhow::Result<corti_capture::FinishedRecording> {
                self.finish_tap_only_with_processing()
            }
        }
    }

    #[test]
    fn run_live_caller_preserves_words_wav_and_quality_and_rejects_incomplete() {
        let _lock = CALLER_LOCK.lock().unwrap();
        for mode in ["failed", "partial", "tail", "empty", "metadata"] {
            let fixture = Fixture::new(authority(&[epoch(false, true)]), 48);
            let before = std::fs::read(&fixture.path).unwrap();
            SCENARIO.with(|s| {
                *s.borrow_mut() = Some(Scenario {
                    fixture,
                    mode,
                    output: vec![],
                    calls: None,
                    engines: 0,
                    channels: 0,
                    finish_error: false,
                })
            });
            crate::STOP.store(false, Ordering::Relaxed);
            let args = crate::Args {
                label: "Synthetic preview".into(),
                no_mic: true,
                live: true,
                #[cfg(feature = "inbox")]
                inbox: false,
            };
            assert!(args.live);
            #[cfg(feature = "inbox")]
            assert!(!args.inbox);
            let result = crate::run_live(&args);
            let scenario = SCENARIO.with(|s| s.borrow_mut().take().unwrap());
            assert_eq!(
                result.is_err(),
                matches!(mode, "failed" | "partial" | "tail")
            );
            let expected = if mode == "partial" {
                vec![word(0), word(1), word(2)]
            } else {
                vec![word(0), word(2)]
            };
            assert_eq!(
                scenario.output,
                expected
                    .into_iter()
                    .map(|w| ("Them".into(), w))
                    .collect::<Vec<_>>()
            );
            assert_eq!((scenario.engines, scenario.channels), (1, 1));
            let calls = scenario.calls.unwrap();
            assert_eq!((calls.borrow().pushes, calls.borrow().finishes), (3, 1));
            assert_eq!(calls.borrow().input.len(), 48);
            assert_eq!(std::fs::read(&scenario.fixture.path).unwrap(), before);
            let read = corti_core::quality_store::load_quality(
                &scenario.fixture.path,
                "synthetic",
                Some(scenario.fixture.frames),
            )
            .unwrap();
            let snapshot = read.snapshot.unwrap();
            assert_eq!(snapshot.quality(), &scenario.fixture.capture);
            let settled = snapshot.settled_quality().unwrap();
            assert_eq!(settled.requires_batch_recovery(), result.is_err());
            assert_eq!(
                settled.overflow.omitted_spans[2],
                u64::from(mode == "metadata")
            );
            if mode == "failed" {
                assert_eq!(
                    (settled.spans[0].start_frame, settled.spans[0].end_frame),
                    (16, 32)
                );
            }
        }
    }

    #[test]
    fn run_live_caller_capture_finish_error_preserves_known_history_without_promotion() {
        let _lock = CALLER_LOCK.lock().unwrap();
        for storage in ["in_progress", "missing", "corrupt"] {
            let fixture = Fixture::with_state(authority(&[epoch(false, true)]), 48, false);
            let wav = std::fs::read(&fixture.path).unwrap();
            let sidecar = corti_core::quality_store::quality_sidecar_path(&fixture.path).unwrap();
            match storage {
                "missing" => std::fs::remove_file(&sidecar).unwrap(),
                "corrupt" => std::fs::write(&sidecar, b"synthetic corrupt quality").unwrap(),
                _ => {}
            }
            SCENARIO.with(|s| {
                *s.borrow_mut() = Some(Scenario {
                    fixture,
                    mode: "failed",
                    output: vec![],
                    calls: None,
                    engines: 0,
                    channels: 0,
                    finish_error: true,
                })
            });
            crate::STOP.store(false, Ordering::Relaxed);
            let args = crate::Args {
                label: "Synthetic preview".into(),
                no_mic: true,
                live: true,
                #[cfg(feature = "inbox")]
                inbox: false,
            };
            assert!(args.live);
            #[cfg(feature = "inbox")]
            assert!(!args.inbox);
            let result = crate::run_live(&args);
            let scenario = SCENARIO.with(|s| s.borrow_mut().take().unwrap());
            assert!(
                format!("{:#}", result.unwrap_err())
                    .contains("synthetic capture finalization failure")
            );
            assert_eq!(
                scenario.output,
                vec![("Them".into(), word(0)), ("Them".into(), word(2))]
            );
            let calls = scenario.calls.unwrap();
            assert_eq!((calls.borrow().pushes, calls.borrow().finishes), (3, 1));
            assert_eq!(std::fs::read(&scenario.fixture.path).unwrap(), wav);
            match storage {
                "missing" => assert!(!sidecar.exists()),
                "corrupt" => assert_eq!(
                    std::fs::read(sidecar).unwrap(),
                    b"synthetic corrupt quality"
                ),
                _ => {
                    let snapshot = corti_core::quality_store::load_quality(
                        &scenario.fixture.path,
                        "synthetic",
                        Some(48),
                    )
                    .unwrap()
                    .snapshot
                    .unwrap();
                    assert_eq!(
                        snapshot.state(),
                        corti_core::quality_store::QualityStorageState::InProgress
                    );
                    assert_eq!(snapshot.expected_retained_frames(), Some(0));
                    assert_eq!(snapshot.quality(), &scenario.fixture.capture);
                    let quality = snapshot.settled_quality().unwrap();
                    assert_eq!(quality.spans.len(), 1);
                    assert_eq!(
                        (quality.spans[0].start_frame, quality.spans[0].end_frame),
                        (16, 32)
                    );
                    assert_eq!(quality.spans[0].cause, QualityCause::NoResult);
                }
            }
        }
    }
}
