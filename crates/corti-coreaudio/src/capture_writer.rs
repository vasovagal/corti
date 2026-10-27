//! Descriptor-ordered, off-HAL capture persistence. PCM without an Audio descriptor is never read.

use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow, bail, ensure};
use corti_core::capture_accounting::{
    AudioBlock, CaptureDescriptor, EpochStart, HAL_DESCRIPTOR_CAPACITY, MAX_LOSS_FILL_SECONDS,
};
use corti_core::capture_normalizer::{CaptureRateNormalizer, NormalizationFailure};
use corti_core::quality_store::{QualityStore, QualityStoreError};
use corti_core::{
    CaptureEpoch, QualityCause, QualityClass, QualityRecord, QualityResolution, QualitySpan,
    QualityStage, QualityTrack,
};

use super::{
    CaptureChunk, CaptureFilterDisposition, CaptureFilterFactory, CaptureTee, FilterStage,
    OutputLayout, TEE_FRAMES_PER_CHUNK, WriterOutcome, frame_mean, write_frame,
};

type Wave = hound::WavWriter<BufWriter<std::fs::File>>;

/// Producer closure also happens on Drop and errors; it is not permission to certify completion.
/// Only the session's successful explicit stop authorizes this one-way transition before closing
/// the producers. Clones retain the default abort intent until that point.
#[derive(Clone, Default)]
pub(super) struct CompletionIntent(Arc<AtomicBool>);

impl CompletionIntent {
    pub(super) fn authorize_complete(&self) {
        self.0.store(true, Ordering::Release);
    }

    fn is_authorized(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// The seam also tests failure immediately before dependent audio and between WAV finalization and
/// completion. Production uses the mandatory, bounded, atomic QualityStore for every operation.
trait QualitySink {
    fn before_audio(
        &mut self,
        quality: &QualityRecord,
        frames: u64,
    ) -> Result<(), QualityStoreError>;
    fn finalizing(&mut self, quality: &QualityRecord, frames: u64)
    -> Result<(), QualityStoreError>;
    fn audio_durable(&mut self, frames: u64) -> Result<(), QualityStoreError>;
}

impl QualitySink for QualityStore {
    fn before_audio(
        &mut self,
        quality: &QualityRecord,
        frames: u64,
    ) -> Result<(), QualityStoreError> {
        self.persist_before_audio(quality, frames)
    }

    fn finalizing(
        &mut self,
        quality: &QualityRecord,
        frames: u64,
    ) -> Result<(), QualityStoreError> {
        self.prepare_finalization(quality, frames)
    }

    fn audio_durable(&mut self, frames: u64) -> Result<(), QualityStoreError> {
        self.complete_after_audio_sync(frames)
    }
}

#[cfg(test)]
pub(super) fn run_writer(
    consumer: rtrb::Consumer<f32>,
    descriptors: rtrb::Consumer<CaptureDescriptor>,
    out: PathBuf,
    layout: OutputLayout,
    retained_rate_hz: u32,
    tee: Option<CaptureTee>,
    filter: Option<CaptureFilterFactory>,
) -> Result<WriterOutcome> {
    let completion = CompletionIntent::default();
    completion.authorize_complete();
    run_writer_with_intent(
        consumer,
        descriptors,
        out,
        layout,
        retained_rate_hz,
        tee,
        filter,
        completion,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run_writer_with_intent(
    consumer: rtrb::Consumer<f32>,
    descriptors: rtrb::Consumer<CaptureDescriptor>,
    out: PathBuf,
    layout: OutputLayout,
    retained_rate_hz: u32,
    tee: Option<CaptureTee>,
    filter: Option<CaptureFilterFactory>,
    completion: CompletionIntent,
) -> Result<WriterOutcome> {
    run_writer_with_sink_and_intent(
        consumer,
        descriptors,
        out,
        layout,
        retained_rate_hz,
        tee,
        filter,
        completion,
        QualityStore::begin,
    )
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn run_writer_with_sink<S: QualitySink>(
    consumer: rtrb::Consumer<f32>,
    descriptors: rtrb::Consumer<CaptureDescriptor>,
    out: PathBuf,
    layout: OutputLayout,
    retained_rate_hz: u32,
    tee: Option<CaptureTee>,
    filter: Option<CaptureFilterFactory>,
    begin: impl FnOnce(&Path, &str, &QualityRecord) -> Result<S, QualityStoreError>,
) -> Result<WriterOutcome> {
    let completion = CompletionIntent::default();
    completion.authorize_complete();
    run_writer_with_sink_and_intent(
        consumer,
        descriptors,
        out,
        layout,
        retained_rate_hz,
        tee,
        filter,
        completion,
        begin,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_writer_with_sink_and_intent<S: QualitySink>(
    mut consumer: rtrb::Consumer<f32>,
    mut descriptors: rtrb::Consumer<CaptureDescriptor>,
    out: PathBuf,
    layout: OutputLayout,
    retained_rate_hz: u32,
    tee: Option<CaptureTee>,
    filter: Option<CaptureFilterFactory>,
    completion: CompletionIntent,
    begin: impl FnOnce(&Path, &str, &QualityRecord) -> Result<S, QualityStoreError>,
) -> Result<WriterOutcome> {
    ensure!(retained_rate_hz != 0, "invalid retained capture rate");
    // Preserve the no-callback/no-file behavior, while waiting exclusively on descriptors. The
    // callback publishes PCM first; seeing PCM alone cannot authorize creating or draining a WAV.
    let first = loop {
        match descriptors.pop() {
            Ok(descriptor) => break descriptor,
            Err(_) if descriptors.is_abandoned() => {
                ensure!(consumer.slots() == 0, "capture ended with undescribed PCM");
                ensure!(
                    completion.is_authorized(),
                    "capture completion was not authorized"
                );
                return Ok(WriterOutcome {
                    frames: 0,
                    filter: CaptureFilterDisposition::NotRequested,
                    quality: QualityRecord::observed(),
                });
            }
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    let identity = out
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| anyhow!("invalid capture recording identity"))?;
    let quality = QualityRecord::observed();
    // This must succeed before even WavWriter::create: Hound finalizes its header on Drop.
    let sink = begin(&out, identity, &quality)?;
    let mut state = Writer {
        out,
        layout,
        retained_rate_hz,
        sink,
        quality,
        completion,
        wave: None,
        spec: None,
        current: None,
        source_cursor: 0,
        remap_required: false,
        normalizer: None,
        retained_frames: 0,
        written_frames: 0,
        filter_factory: filter,
        filter_stage: None,
        filter_disposition: CaptureFilterDisposition::NotRequested,
        tee,
        tee_mic: Vec::with_capacity(TEE_FRAMES_PER_CHUNK),
        tee_tap: Vec::with_capacity(TEE_FRAMES_PER_CHUNK),
        tee_unavailable: Vec::new(),
        tee_start: 0,
    };
    // A run can contain changes of rate/layout as well as differently caused losses. Never decide
    // the ten-second cap one marker at a time. Refuse an overlong descriptor sequence without
    // admitting its PCM, rather than allocating in proportion to recording length.
    let mut pending = Vec::with_capacity(HAL_DESCRIPTOR_CAPACITY);
    let mut next = Some(first);
    loop {
        let descriptor = match next.take().or_else(|| descriptors.pop().ok()) {
            Some(descriptor) => descriptor,
            None if descriptors.is_abandoned() => break,
            None => {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
        };
        match descriptor {
            CaptureDescriptor::Audio(audio) => {
                state.apply_pending(&pending)?;
                pending.clear();
                state.audio(&mut consumer, audio)?;
            }
            descriptor => {
                if pending.len() == HAL_DESCRIPTOR_CAPACITY {
                    // Retain sticky omission counts even though this malformed/unbounded run
                    // cannot be replayed safely. No descriptor-dependent PCM has been admitted.
                    for omitted in pending.iter().chain(std::iter::once(&descriptor)) {
                        match *omitted {
                            CaptureDescriptor::Epoch(_) => state.quality.record_omitted_epochs(1),
                            CaptureDescriptor::Loss(span) => state.quality.record_omitted_spans(
                                span.class,
                                span.resolution,
                                1,
                            )?,
                            CaptureDescriptor::Overflow(overflow) => {
                                overflow.apply_to(&mut state.quality)?
                            }
                            CaptureDescriptor::Audio(_) => {
                                unreachable!("pending descriptors contain no audio")
                            }
                        }
                    }
                    state.persist()?;
                    bail!("capture descriptor run exceeded bound");
                }
                pending.push(descriptor);
            }
        }
    }
    // Terminal descriptors are published after IO destruction and before the producer closes.
    // Thus loss accounting always precedes normalizer/filter tail flush and final publication.
    state.apply_pending(&pending)?;
    ensure!(consumer.slots() == 0, "capture ended with undescribed PCM");
    state.finish()
}

struct Writer<S> {
    out: PathBuf,
    layout: OutputLayout,
    retained_rate_hz: u32,
    sink: S,
    quality: QualityRecord,
    completion: CompletionIntent,
    wave: Option<Wave>,
    spec: Option<hound::WavSpec>,
    current: Option<(EpochStart, CaptureEpoch)>,
    source_cursor: u64,
    remap_required: bool,
    normalizer: Option<CaptureRateNormalizer>,
    /// Normalized frames admitted to the filter/tee, including the filter's bounded pending tail.
    retained_frames: u64,
    written_frames: u64,
    filter_factory: Option<CaptureFilterFactory>,
    filter_stage: Option<FilterStage>,
    filter_disposition: CaptureFilterDisposition,
    tee: Option<CaptureTee>,
    tee_mic: Vec<f32>,
    tee_tap: Vec<f32>,
    tee_unavailable: Vec<bool>,
    tee_start: u64,
}

impl<S: QualitySink> Writer<S> {
    fn persist(&mut self) -> Result<()> {
        self.sink
            .before_audio(&self.quality, self.retained_frames)?;
        Ok(())
    }

    fn insert_span(&mut self, span: QualitySpan) -> Result<()> {
        if self
            .quality
            .epochs
            .iter()
            .any(|epoch| epoch.id == span.epoch)
        {
            self.quality.insert_span(span)?;
        } else {
            // The cap can omit an epoch, but cannot turn its loss into a clean capture.
            self.quality
                .record_omitted_spans(span.class, span.resolution, 1)?;
        }
        Ok(())
    }

    fn apply_pending(&mut self, pending: &[CaptureDescriptor]) -> Result<()> {
        let fill = can_fill_run(self.current.map(|(start, _)| start), pending)?;
        for descriptor in pending {
            match *descriptor {
                CaptureDescriptor::Epoch(start) => self.epoch(start)?,
                CaptureDescriptor::Loss(span) => {
                    let (start, _) = self
                        .current
                        .ok_or_else(|| anyhow!("capture loss has no epoch"))?;
                    ensure!(
                        span.epoch == start.id && span.class == QualityClass::SourceLoss,
                        "capture loss does not match its source epoch"
                    );
                    ensure!(
                        span.start_frame <= self.source_cursor && span.end_frame > span.start_frame,
                        "capture loss leaves an undescribed source interval"
                    );
                    self.insert_span(span)?;
                    // In particular, a failure here must not feed zero fill into a filter whose
                    // pending state could later be flushed by a caller's cleanup path.
                    self.persist()?;
                    let missing = span.end_frame.saturating_sub(self.source_cursor);
                    if missing != 0 {
                        if fill {
                            self.normalize_gap(missing)?;
                        } else {
                            self.remap_required = true;
                        }
                        self.source_cursor = span.end_frame;
                    }
                }
                CaptureDescriptor::Overflow(overflow) => {
                    overflow.apply_to(&mut self.quality)?;
                    self.persist()?;
                    // The timeline may put an overflow summary after a fresh mapped epoch (for
                    // example a rate change with a reset sample clock). That epoch is already a
                    // valid binding. Audio still must match its exact source cursor; a skipped
                    // positioned loss separately requires a later resynchronizing epoch.
                }
                CaptureDescriptor::Audio(_) => bail!("audio descriptor in capture loss run"),
            }
        }
        Ok(())
    }

    fn epoch(&mut self, start: EpochStart) -> Result<()> {
        if let Some((previous, _)) = self.current {
            ensure!(
                start.id > previous.id,
                "capture epoch identifiers are not increasing"
            );
        }
        self.finish_normalizer()?;
        self.flush_tee()?;
        let epoch = start.with_retained_base(self.retained_frames, self.retained_rate_hz)?;
        self.quality.insert_epoch(epoch)?;
        self.persist()?;
        let channels = usize::try_from(
            start
                .format
                .mic_channels
                .checked_add(start.format.tap_channels)
                .ok_or_else(|| anyhow!("capture channel count overflow"))?,
        )?;
        self.normalizer = Some(CaptureRateNormalizer::new(
            start.format.source_rate_hz,
            self.retained_rate_hz,
            channels,
        )?);
        self.current = Some((start, epoch));
        self.source_cursor = start.source_frame_base;
        self.remap_required = false;
        Ok(())
    }

    fn ensure_wave(&mut self) -> Result<()> {
        if self.wave.is_some() {
            if self.layout == OutputLayout::AllChannels {
                let (start, _) = self
                    .current
                    .ok_or_else(|| anyhow!("capture audio has no epoch"))?;
                ensure!(
                    self.spec.is_some_and(|spec| u32::from(spec.channels)
                        == start.format.mic_channels + start.format.tap_channels),
                    "diagnostic capture channel count changed"
                );
            }
            return Ok(());
        }
        let (start, _) = self
            .current
            .ok_or_else(|| anyhow!("capture audio has no epoch"))?;
        let channels = self
            .layout
            .out_channels(start.format.mic_channels, start.format.tap_channels);
        let (bits_per_sample, sample_format) = match self.layout {
            OutputLayout::TwoTrack => (32, hound::SampleFormat::Float),
            OutputLayout::TapOnlyMono | OutputLayout::AllChannels => (16, hound::SampleFormat::Int),
        };
        let spec = hound::WavSpec {
            channels: u16::try_from(channels)?,
            sample_rate: self.retained_rate_hz,
            bits_per_sample,
            sample_format,
        };
        // Initial quality and this epoch were already persisted before reaching this creation.
        self.wave = Some(hound::WavWriter::create(&self.out, spec)?);
        self.spec = Some(spec);
        if let Some(make) = self.filter_factory.take()
            && self.layout == OutputLayout::TwoTrack
            && start.format.mic_channels > 0
        {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                FilterStage::new(make(self.retained_rate_hz), self.retained_rate_hz)
            })) {
                Ok(stage) => self.filter_stage = Some(stage),
                Err(_) => self.filter_disposition = CaptureFilterDisposition::RawFallback,
            }
        }
        Ok(())
    }

    fn audio(&mut self, consumer: &mut rtrb::Consumer<f32>, audio: AudioBlock) -> Result<()> {
        let (start, _) = self
            .current
            .ok_or_else(|| anyhow!("capture audio has no epoch"))?;
        ensure!(
            !self.remap_required,
            "capture loss requires a new source mapping"
        );
        ensure!(
            audio.epoch == start.id
                && audio.format == start.format
                && audio.start_frame == self.source_cursor,
            "capture audio does not match its bound epoch"
        );
        let channels = u64::from(start.format.mic_channels) + u64::from(start.format.tap_channels);
        ensure!(
            audio.frames > 0 && audio.frames.checked_mul(channels) == Some(audio.samples),
            "invalid capture audio sample count"
        );
        let end = audio
            .start_frame
            .checked_add(audio.frames)
            .ok_or_else(|| anyhow!("capture source frame overflow"))?;
        if self.layout == OutputLayout::AllChannels
            && self
                .spec
                .is_some_and(|spec| u64::from(spec.channels) != channels)
        {
            self.insert_span(QualitySpan {
                epoch: audio.epoch,
                start_frame: audio.start_frame,
                end_frame: end,
                track: QualityTrack::Both,
                cause: QualityCause::LayoutChange,
                class: QualityClass::SourceLoss,
                stage: QualityStage::Capture,
                resolution: QualityResolution::Outstanding,
            })?;
            self.persist()?;
            bail!("diagnostic capture channel count changed");
        }
        // Audio descriptors are published only after all their PCM. Reading a short block would
        // silently borrow samples from the next source epoch, so fail before consuming any of it.
        ensure!(
            audio.samples <= consumer.slots() as u64,
            "capture descriptor exceeds published PCM"
        );
        self.ensure_wave()?;
        let mut frame = vec![0.0; usize::try_from(channels)?];
        for _ in 0..audio.frames {
            for sample in &mut frame {
                *sample = consumer
                    .pop()
                    .map_err(|_| anyhow!("capture PCM publication mismatch"))?;
            }
            let mut normalizer = self
                .normalizer
                .take()
                .ok_or_else(|| anyhow!("capture normalizer missing"))?;
            let result =
                normalizer.push(&frame, |frame, unavailable| self.emit(frame, unavailable));
            self.normalizer = Some(normalizer);
            match result {
                Ok(()) => {}
                Err(NormalizationFailure::Sink(error)) => return Err(error),
                Err(NormalizationFailure::Normalization(error)) => {
                    self.record_normalization_loss(end)?;
                    return Err(anyhow!("capture normalization failed: {error}"));
                }
            }
        }
        self.source_cursor = end;
        Ok(())
    }

    fn record_normalization_loss(&mut self, end_frame: u64) -> Result<()> {
        let (_, epoch) = self
            .current
            .ok_or_else(|| anyhow!("capture normalization has no epoch"))?;
        let retained = self
            .retained_frames
            .checked_sub(epoch.retained_start_frame)
            .ok_or_else(|| anyhow!("capture normalization precedes its epoch"))?;
        let represented = u128::from(retained) * u128::from(epoch.source_rate_hz)
            / u128::from(epoch.retained_rate_hz);
        let start_frame = epoch
            .source_frame_base
            .checked_add(u64::try_from(represented)?)
            .ok_or_else(|| anyhow!("capture normalization source frame overflow"))?;
        if start_frame < end_frame {
            self.insert_span(QualitySpan {
                epoch: epoch.id,
                start_frame,
                end_frame,
                track: QualityTrack::Both,
                cause: QualityCause::ResamplerFailure,
                class: QualityClass::SourceLoss,
                stage: QualityStage::Capture,
                resolution: QualityResolution::Outstanding,
            })?;
            self.persist()?;
        }
        Ok(())
    }

    fn normalize_gap(&mut self, frames: u64) -> Result<()> {
        let end = self
            .source_cursor
            .checked_add(frames)
            .ok_or_else(|| anyhow!("capture source frame overflow"))?;
        self.ensure_wave()?;
        let mut normalizer = self
            .normalizer
            .take()
            .ok_or_else(|| anyhow!("capture normalizer missing"))?;
        let result = normalizer.gap(frames, |frame, unavailable| self.emit(frame, unavailable));
        self.normalizer = Some(normalizer);
        match result {
            Ok(()) => {}
            Err(NormalizationFailure::Sink(error)) => return Err(error),
            Err(NormalizationFailure::Normalization(error)) => {
                self.record_normalization_loss(end)?;
                return Err(anyhow!("capture gap normalization failed: {error}"));
            }
        }
        Ok(())
    }

    fn finish_normalizer(&mut self) -> Result<()> {
        if let Some(mut normalizer) = self.normalizer.take() {
            match normalizer.finish(|frame, unavailable| self.emit(frame, unavailable)) {
                Ok(()) => {}
                Err(NormalizationFailure::Sink(error)) => return Err(error),
                Err(NormalizationFailure::Normalization(error)) => {
                    self.record_normalization_loss(self.source_cursor)?;
                    return Err(anyhow!("capture normalization tail failed: {error}"));
                }
            }
        }
        Ok(())
    }

    fn emit(&mut self, frame: &[f32], unavailable: bool) -> Result<()> {
        let (start, _) = self
            .current
            .ok_or_else(|| anyhow!("normalized capture has no epoch"))?;
        let mic = start.format.mic_channels as usize;
        let total = mic + start.format.tap_channels as usize;
        ensure!(frame.len() == total, "normalized capture channel mismatch");
        let wave = self
            .wave
            .as_mut()
            .ok_or_else(|| anyhow!("normalized capture has no WAV"))?;
        if let Some(stage) = &mut self.filter_stage {
            stage.stage_with_availability(
                frame_mean(frame, 0, mic),
                frame_mean(frame, mic, total - mic),
                unavailable,
            );
            if stage.is_full() {
                self.written_frames = self
                    .written_frames
                    .checked_add(stage.flush(wave)?)
                    .ok_or_else(|| anyhow!("capture retained frame overflow"))?;
            }
        } else {
            write_frame(wave, frame, self.layout, mic, total)?;
            self.written_frames = self
                .written_frames
                .checked_add(1)
                .ok_or_else(|| anyhow!("capture retained frame overflow"))?;
        }
        if self.tee.is_some() {
            let buffered = self.tee_mic.len().max(self.tee_tap.len());
            if buffered == 0 {
                self.tee_start = self.retained_frames;
            }
            if unavailable && self.tee_unavailable.is_empty() {
                self.tee_unavailable.resize(buffered, false);
            }
            if unavailable || !self.tee_unavailable.is_empty() {
                self.tee_unavailable.push(unavailable);
            }
            if mic > 0 {
                self.tee_mic.push(if unavailable {
                    0.0
                } else {
                    frame_mean(frame, 0, mic)
                });
            }
            if total > mic {
                self.tee_tap.push(if unavailable {
                    0.0
                } else {
                    frame_mean(frame, mic, total - mic)
                });
            }
        }
        self.retained_frames = self
            .retained_frames
            .checked_add(1)
            .ok_or_else(|| anyhow!("capture retained frame overflow"))?;
        if self.tee_mic.len().max(self.tee_tap.len()) == TEE_FRAMES_PER_CHUNK {
            self.flush_tee()?;
        }
        Ok(())
    }

    fn flush_tee(&mut self) -> Result<()> {
        if self.tee_mic.is_empty() && self.tee_tap.is_empty() {
            return Ok(());
        }
        let Some(tee) = &self.tee else { return Ok(()) };
        let (_, epoch) = self
            .current
            .ok_or_else(|| anyhow!("capture tee has no epoch"))?;
        let chunk = CaptureChunk {
            epoch,
            retained_start_frame: self.tee_start,
            mic: std::mem::replace(&mut self.tee_mic, Vec::with_capacity(TEE_FRAMES_PER_CHUNK)),
            tap: std::mem::replace(&mut self.tee_tap, Vec::with_capacity(TEE_FRAMES_PER_CHUNK)),
            unavailable: std::mem::take(&mut self.tee_unavailable),
        };
        if tee.tx.try_send(chunk).is_err() {
            tee.dropped
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // Cover every source frame touched by this retained interval. Ceiling the right edge
            // also retains a meaningful interval when an upsampled chunk covers less than one frame.
            let source = |retained: u64, ceil: bool| -> Result<u64> {
                let delta = retained
                    .checked_sub(epoch.retained_start_frame)
                    .ok_or_else(|| anyhow!("capture tee precedes its epoch"))?;
                let numerator = u128::from(delta) * u128::from(epoch.source_rate_hz);
                let divisor = u128::from(epoch.retained_rate_hz);
                let frames = if ceil {
                    numerator.div_ceil(divisor)
                } else {
                    numerator / divisor
                };
                epoch
                    .source_frame_base
                    .checked_add(u64::try_from(frames)?)
                    .ok_or_else(|| anyhow!("capture tee source frame overflow"))
            };
            self.insert_span(QualitySpan {
                epoch: epoch.id,
                start_frame: source(self.tee_start, false)?,
                end_frame: source(self.retained_frames, true)?,
                track: if epoch.mic_channels == 0 {
                    QualityTrack::Tap
                } else if epoch.tap_channels == 0 {
                    QualityTrack::Mic
                } else {
                    QualityTrack::Both
                },
                cause: QualityCause::TeeDrop,
                class: QualityClass::ProcessingLoss,
                stage: QualityStage::Live,
                resolution: QualityResolution::Outstanding,
            })?;
            self.persist()?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<WriterOutcome> {
        self.finish_normalizer()?;
        self.flush_tee()?;
        if let Some(stage) = self.filter_stage.take() {
            let wave = self
                .wave
                .as_mut()
                .ok_or_else(|| anyhow!("capture filter has no WAV"))?;
            let (frames, disposition) = stage.finish(wave)?;
            self.written_frames = self
                .written_frames
                .checked_add(frames)
                .ok_or_else(|| anyhow!("capture retained frame overflow"))?;
            self.filter_disposition = disposition;
        }
        ensure!(
            self.written_frames == self.retained_frames,
            "capture filter changed retained frame count"
        );
        // With only unmappable terminal losses there may be no WAV. The in-progress sidecar keeps
        // those facts but cannot certify storage; completion is still a capture failure.
        let Some(wave) = self.wave.take() else {
            bail!("capture ended without retained audio");
        };
        let complete = self.completion.is_authorized();
        if complete {
            self.sink.finalizing(&self.quality, self.written_frames)?;
        } else {
            // Keep the exact terminal loss/filter-tail snapshot, but remain in_progress. Hound's
            // explicit finalization below only makes this interrupted audio prefix recoverable.
            self.sink.before_audio(&self.quality, self.written_frames)?;
        }
        wave.finalize()?;
        let reader = hound::WavReader::open(&self.out)?;
        let spec = reader.spec();
        ensure!(
            Some(spec) == self.spec && reader.len().is_multiple_of(u32::from(spec.channels)),
            "finalized capture WAV format mismatch"
        );
        let actual_frames = u64::from(reader.duration());
        ensure!(
            actual_frames == self.written_frames,
            "finalized capture WAV frame count mismatch"
        );
        drop(reader);
        if !complete {
            std::fs::File::open(&self.out)?.sync_all()?;
            let parent = self
                .out
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            std::fs::File::open(parent)?.sync_all()?;
            bail!("capture completion was not authorized");
        }
        // QualityStore syncs the WAV and containing directory, then atomically publishes
        // audio_durable. No Drop path invokes this transition.
        self.sink.audio_durable(actual_frames)?;
        Ok(WriterOutcome {
            frames: actual_frames,
            filter: self.filter_disposition,
            quality: self.quality,
        })
    }
}

/// Exact rational duration, reduced at each addition. If pathological rate churn exhausts u128,
/// conservatively require a remapped epoch instead of guessing that the run fits the fill cap.
pub(super) fn can_fill_run(
    mut current: Option<EpochStart>,
    pending: &[CaptureDescriptor],
) -> Result<bool> {
    let mut numerator = 0u128;
    let mut denominator = 1u128;
    let mut last_end: Option<(u32, u64)> = None;
    for descriptor in pending {
        match *descriptor {
            CaptureDescriptor::Epoch(epoch) => current = Some(epoch),
            CaptureDescriptor::Overflow(_) => return Ok(false),
            CaptureDescriptor::Loss(span) => {
                let epoch = current.ok_or_else(|| anyhow!("capture loss has no rate mapping"))?;
                ensure!(
                    epoch.id == span.epoch
                        && epoch.format.source_rate_hz != 0
                        && span.end_frame > span.start_frame,
                    "capture loss has an invalid rate mapping"
                );
                let begin = last_end
                    .filter(|(id, _)| *id == span.epoch)
                    .map_or(span.start_frame, |(_, end)| span.start_frame.max(end));
                let frames = u128::from(span.end_frame.saturating_sub(begin));
                last_end = Some((span.epoch, span.end_frame.max(begin)));
                let rate = u128::from(epoch.format.source_rate_hz);
                let common = gcd(denominator, rate);
                let left = rate / common;
                let right = denominator / common;
                let Some(next) = numerator
                    .checked_mul(left)
                    .and_then(|n| frames.checked_mul(right).and_then(|f| n.checked_add(f)))
                else {
                    return Ok(false);
                };
                let Some(divisor) = denominator.checked_mul(left) else {
                    return Ok(false);
                };
                let reduce = gcd(next, divisor);
                numerator = next / reduce;
                denominator = divisor / reduce;
                if numerator / denominator > u128::from(MAX_LOSS_FILL_SECONDS)
                    || (numerator / denominator == u128::from(MAX_LOSS_FILL_SECONDS)
                        && !numerator.is_multiple_of(denominator))
                {
                    return Ok(false);
                }
            }
            CaptureDescriptor::Audio(_) => bail!("audio descriptor in capture loss plan"),
        }
    }
    Ok(true)
}

fn gcd(mut left: u128, mut right: u128) -> u128 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use corti_core::capture_accounting::{
        CallbackInput, CaptureFormat, CaptureTimeline, CaptureTimestamp,
    };
    use corti_core::quality_store::{QualityReadStatus, load_quality, quality_sidecar_path};
    use corti_core::{CaptureEpochReason, SourceCompleteness};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let directory = std::env::temp_dir().join(format!(
                "corti-ordered-writer-{}-{}",
                std::process::id(),
                NEXT_TEST.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&directory).unwrap();
            Self(directory.join("recording.wav"))
        }

        fn samples(&self) -> Vec<f32> {
            hound::WavReader::open(&self.0)
                .unwrap()
                .samples::<f32>()
                .collect::<Result<_, _>>()
                .unwrap()
        }

        fn read(&self) -> corti_core::quality_store::QualityReadout {
            let frames = hound::WavReader::open(&self.0)
                .ok()
                .map(|reader| u64::from(reader.duration()));
            load_quality(&self.0, "recording", frames).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(self.0.parent().unwrap()).ok();
        }
    }

    fn format(rate: u32) -> CaptureFormat {
        CaptureFormat {
            generation: 0,
            source_rate_hz: rate,
            mic_channels: 1,
            tap_channels: 1,
        }
    }

    fn epoch(id: u32, base: u64, nanos: u64, rate: u32) -> CaptureDescriptor {
        CaptureDescriptor::Epoch(EpochStart {
            id,
            source_start_ns: Some(nanos),
            source_frame_base: base,
            format: format(rate),
            reason: if id == 0 {
                CaptureEpochReason::Start
            } else {
                CaptureEpochReason::RateChange
            },
        })
    }

    fn audio(id: u32, start: u64, frames: u64, rate: u32) -> CaptureDescriptor {
        CaptureDescriptor::Audio(AudioBlock {
            epoch: id,
            start_frame: start,
            frames,
            samples: frames * 2,
            format: format(rate),
        })
    }

    fn loss(id: u32, start: u64, end: u64) -> CaptureDescriptor {
        CaptureDescriptor::Loss(QualitySpan {
            epoch: id,
            start_frame: start,
            end_frame: end,
            track: QualityTrack::Both,
            cause: QualityCause::RingOverflow,
            class: QualityClass::SourceLoss,
            stage: QualityStage::Capture,
            resolution: QualityResolution::Outstanding,
        })
    }

    fn rings(
        descriptors: &[CaptureDescriptor],
        pcm: &[f32],
    ) -> (rtrb::Consumer<f32>, rtrb::Consumer<CaptureDescriptor>) {
        let (mut samples_tx, samples_rx) = rtrb::RingBuffer::new(pcm.len().max(1));
        let (mut descriptor_tx, descriptor_rx) = rtrb::RingBuffer::new(HAL_DESCRIPTOR_CAPACITY);
        for &sample in pcm {
            samples_tx.push(sample).unwrap();
        }
        for &descriptor in descriptors {
            descriptor_tx.push(descriptor).unwrap();
        }
        (samples_rx, descriptor_rx)
    }

    fn submit(
        timeline: &mut CaptureTimeline,
        samples: &mut rtrb::Producer<f32>,
        descriptors: &mut rtrb::Producer<CaptureDescriptor>,
        first: u64,
        pcm: &[[f32; 2]],
    ) -> bool {
        let admission = timeline
            .plan_callback(
                CallbackInput {
                    frames: pcm.len() as u64,
                    format: format(8_000),
                    timestamp: CaptureTimestamp {
                        sample_time: first as f64,
                        sample_time_valid: true,
                        source_offset_ns: first * 1_000_000_000 / 8_000,
                        host_time_valid: true,
                    },
                },
                samples.slots(),
                descriptors.slots(),
            )
            .unwrap();
        let accepted = admission.accepted();
        if accepted {
            for frame in pcm {
                for &sample in frame {
                    samples.push(sample).unwrap();
                }
            }
        }
        for descriptor in admission {
            descriptors.push(descriptor).unwrap();
        }
        accepted
    }

    fn wait_for(mut ready: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !ready() {
            assert!(
                std::time::Instant::now() < deadline,
                "writer did not drain its bounded rings"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn terminal(timeline: &mut CaptureTimeline, producer: &mut rtrb::Producer<CaptureDescriptor>) {
        for descriptor in timeline.finish().unwrap() {
            wait_for(|| producer.slots() > 0);
            producer.push(descriptor).unwrap();
        }
    }

    #[test]
    fn small_pcm_ring_retains_a_gap_b_c_identically_in_wav_and_tee() {
        let fixture = Fixture::new();
        let (mut pcm_tx, pcm_rx) = rtrb::RingBuffer::new(8);
        let (mut descriptor_tx, descriptor_rx) = rtrb::RingBuffer::new(HAL_DESCRIPTOR_CAPACITY);
        let mut timeline = CaptureTimeline::new().unwrap();
        let a = [[0.0, 0.1], [0.1, 0.2], [0.2, 0.3], [0.3, 0.4]];
        let b = [[0.4, 0.5], [0.5, 0.6]];
        let c = [[0.6, 0.7], [0.7, 0.8], [0.8, 0.9]];
        assert!(submit(
            &mut timeline,
            &mut pcm_tx,
            &mut descriptor_tx,
            0,
            &a
        ));
        assert!(!submit(
            &mut timeline,
            &mut pcm_tx,
            &mut descriptor_tx,
            4,
            &b
        ));
        assert_eq!(timeline.dropped_counts().samples, 4);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        let out = fixture.0.clone();
        let writer = std::thread::spawn(move || {
            run_writer(
                pcm_rx,
                descriptor_rx,
                out,
                OutputLayout::TwoTrack,
                8_000,
                Some(CaptureTee::new(tx)),
                None,
            )
        });
        wait_for(|| pcm_tx.slots() >= 6);
        assert!(submit(
            &mut timeline,
            &mut pcm_tx,
            &mut descriptor_tx,
            6,
            &c
        ));
        terminal(&mut timeline, &mut descriptor_tx);
        drop(pcm_tx);
        drop(descriptor_tx);
        let outcome = writer.join().unwrap().unwrap();
        let expected: Vec<f32> = a
            .into_iter()
            .chain([[0.0; 2]; 2])
            .chain(c)
            .flatten()
            .collect();
        assert_eq!(fixture.samples(), expected);
        let chunks: Vec<_> = rx.try_iter().collect();
        let tee: Vec<f32> = chunks
            .iter()
            .flat_map(|chunk| chunk.mic.iter().zip(&chunk.tap).flat_map(|(&m, &t)| [m, t]))
            .collect();
        assert_eq!(tee, expected);
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0].unavailable,
            vec![false, false, false, false, true, true, false, false, false]
        );
        assert_eq!(outcome.frames, 9);
        assert_eq!(
            outcome.quality.spans,
            vec![match loss(0, 4, 6) {
                CaptureDescriptor::Loss(span) => span,
                _ => unreachable!(),
            }]
        );
        assert_eq!(
            outcome.quality.epochs[0].source_time_ns(6).unwrap(),
            Some(750_000)
        );
        assert_eq!(
            outcome.quality.source_completeness(),
            SourceCompleteness::Incomplete
        );
        let restored = fixture.read();
        assert!(restored.permits_completion());
        assert_eq!(restored.quality(), Some(&outcome.quality));
    }

    #[test]
    fn independently_full_descriptor_ring_admits_no_unlabelled_pcm_and_retains_terminal_loss() {
        let fixture = Fixture::new();
        let (mut pcm_tx, pcm_rx) = rtrb::RingBuffer::new(32);
        let (mut descriptor_tx, descriptor_rx) = rtrb::RingBuffer::new(2);
        let mut timeline = CaptureTimeline::new().unwrap();
        assert!(submit(
            &mut timeline,
            &mut pcm_tx,
            &mut descriptor_tx,
            0,
            &[[0.25, 0.5]]
        ));
        assert!(pcm_tx.slots() >= 8);
        assert_eq!(descriptor_tx.slots(), 0);
        assert!(!submit(
            &mut timeline,
            &mut pcm_tx,
            &mut descriptor_tx,
            1,
            &[[1.0, 1.0]; 4]
        ));
        assert_eq!(pcm_tx.slots(), 30, "descriptor rejection published no PCM");
        let out = fixture.0.clone();
        let writer = std::thread::spawn(move || {
            run_writer(
                pcm_rx,
                descriptor_rx,
                out,
                OutputLayout::TwoTrack,
                8_000,
                None,
                None,
            )
        });
        terminal(&mut timeline, &mut descriptor_tx);
        drop(pcm_tx);
        drop(descriptor_tx);
        let outcome = writer.join().unwrap().unwrap();
        assert_eq!(outcome.frames, 5);
        assert_eq!(fixture.samples(), [vec![0.25, 0.5], vec![0.0; 8]].concat());
        assert_eq!(outcome.quality.spans[0].start_frame, 1);
        assert_eq!(outcome.quality.spans[0].end_frame, 5);
    }

    #[test]
    fn complete_eleven_second_loss_run_across_rates_is_not_filled() {
        let fixture = Fixture::new();
        let descriptors = [
            epoch(0, 0, 0, 8_000),
            audio(0, 0, 2, 8_000),
            loss(0, 2, 48_002),
            epoch(1, 48_002, 6_000_250_000, 16_000),
            loss(1, 48_002, 128_002),
            epoch(2, 128_002, 11_000_250_000, 16_000),
            audio(2, 128_002, 32, 16_000),
        ];
        let pcm = [vec![0.125; 4], vec![0.5; 64]].concat();
        let (pcm, descriptors) = rings(&descriptors, &pcm);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        let outcome = run_writer(
            pcm,
            descriptors,
            fixture.0.clone(),
            OutputLayout::TwoTrack,
            8_000,
            Some(CaptureTee::new(tx)),
            None,
        )
        .unwrap();
        assert_eq!(
            outcome.frames, 18,
            "the six- and five-second markers share one cap"
        );
        assert_eq!(outcome.quality.epochs[1].retained_start_frame, 2);
        let chunks: Vec<_> = rx.try_iter().collect();
        assert_eq!(
            chunks.iter().map(|chunk| chunk.tap.len()).sum::<usize>(),
            18
        );
        assert_eq!(chunks.len(), 2, "each tee chunk has exactly one epoch");
        assert_eq!(chunks[0].epoch, outcome.quality.epochs[0]);
        assert_eq!(chunks[0].retained_start_frame, 0);
        assert_eq!(chunks[1].epoch, outcome.quality.epochs[2]);
        assert_eq!(chunks[1].retained_start_frame, 2);
        assert!(
            chunks.iter().all(|chunk| chunk.unavailable.is_empty()),
            "an omitted large gap must not mark later C unavailable"
        );
        assert_eq!(outcome.quality.epochs[2].retained_start_frame, 2);
        assert_eq!(
            outcome.quality.epochs[2].source_start_ns,
            Some(11_000_250_000)
        );
        assert_eq!(outcome.quality.spans.len(), 2);
        assert!(
            fixture.samples()[4..]
                .iter()
                .all(|sample| (*sample - 0.5).abs() < 1e-6)
        );
        assert_eq!(fixture.read().quality(), Some(&outcome.quality));
        assert_eq!(
            outcome.quality.source_completeness(),
            SourceCompleteness::Incomplete
        );
    }

    #[test]
    fn fill_limit_is_for_the_entire_run_and_exact_at_ten_seconds() {
        let CaptureDescriptor::Epoch(first) = epoch(0, 0, 0, 8_000) else {
            unreachable!()
        };
        let mut second = match loss(0, 48_000, 80_000) {
            CaptureDescriptor::Loss(span) => span,
            _ => unreachable!(),
        };
        second.cause = QualityCause::HostDiscontinuity;
        assert!(
            can_fill_run(
                Some(first),
                &[loss(0, 0, 48_000), CaptureDescriptor::Loss(second)]
            )
            .unwrap()
        );
        second.end_frame += 1;
        assert!(
            !can_fill_run(
                Some(first),
                &[loss(0, 0, 48_000), CaptureDescriptor::Loss(second)]
            )
            .unwrap()
        );
        assert!(
            can_fill_run(
                Some(first),
                &[
                    loss(0, 0, 48_000),
                    epoch(1, 48_000, 6_000_000_000, 16_000),
                    loss(1, 48_000, 112_000)
                ]
            )
            .unwrap()
        );
    }

    #[test]
    fn queued_layout_epochs_bind_the_first_block_and_every_later_downmix() {
        let fixture = Fixture::new();
        let first = CaptureFormat {
            generation: 1,
            source_rate_hz: 8_000,
            mic_channels: 1,
            tap_channels: 2,
        };
        let second = CaptureFormat {
            generation: 2,
            source_rate_hz: 8_000,
            mic_channels: 2,
            tap_channels: 1,
        };
        let descriptors = [
            CaptureDescriptor::Epoch(EpochStart {
                id: 0,
                source_start_ns: Some(0),
                source_frame_base: 0,
                format: first,
                reason: CaptureEpochReason::Start,
            }),
            CaptureDescriptor::Audio(AudioBlock {
                epoch: 0,
                start_frame: 0,
                frames: 1,
                samples: 3,
                format: first,
            }),
            CaptureDescriptor::Epoch(EpochStart {
                id: 1,
                source_start_ns: Some(125_000),
                source_frame_base: 1,
                format: second,
                reason: CaptureEpochReason::LayoutChange,
            }),
            CaptureDescriptor::Audio(AudioBlock {
                epoch: 1,
                start_frame: 1,
                frames: 1,
                samples: 3,
                format: second,
            }),
        ];
        let (pcm, descriptors) = rings(&descriptors, &[0.25, 0.5, 1.0, 0.5, 1.0, 0.25]);
        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        let outcome = run_writer(
            pcm,
            descriptors,
            fixture.0.clone(),
            OutputLayout::TwoTrack,
            8_000,
            Some(CaptureTee::new(tx)),
            None,
        )
        .unwrap();
        assert_eq!(fixture.samples(), vec![0.25, 0.75, 0.75, 0.25]);
        let chunks: Vec<_> = rx.try_iter().collect();
        assert_eq!(chunks[0].mic, vec![0.25]);
        assert_eq!(chunks[0].tap, vec![0.75]);
        assert_eq!(chunks[1].mic, vec![0.75]);
        assert_eq!(chunks[1].tap, vec![0.25]);
        assert_eq!(outcome.quality.epochs[0].mic_channels, 1);
        assert_eq!(outcome.quality.epochs[1].mic_channels, 2);
        assert_eq!(outcome.quality.epochs[1].retained_start_frame, 1);
    }

    #[test]
    fn mic_only_epoch_emits_absent_tap_and_bounded_positioned_chunks() {
        let fixture = Fixture::new();
        let mic_only = CaptureFormat {
            generation: 2,
            mic_channels: 1,
            tap_channels: 0,
            source_rate_hz: 8_000,
        };
        let frames = TEE_FRAMES_PER_CHUNK + 2;
        let descriptors = [
            epoch(0, 0, 0, 8_000),
            audio(0, 0, 1, 8_000),
            CaptureDescriptor::Epoch(EpochStart {
                id: 1,
                source_start_ns: Some(125_000),
                source_frame_base: 1,
                format: mic_only,
                reason: CaptureEpochReason::LayoutChange,
            }),
            CaptureDescriptor::Audio(AudioBlock {
                epoch: 1,
                start_frame: 1,
                frames: frames as u64,
                samples: frames as u64,
                format: mic_only,
            }),
        ];
        let (pcm, descriptors) =
            rings(&descriptors, &[vec![0.1, 0.2], vec![0.25; frames]].concat());
        let (tx, rx) = std::sync::mpsc::sync_channel(3);
        let outcome = run_writer(
            pcm,
            descriptors,
            fixture.0.clone(),
            OutputLayout::TwoTrack,
            8_000,
            Some(CaptureTee::new(tx)),
            None,
        )
        .unwrap();
        let chunks: Vec<_> = rx.try_iter().collect();
        assert_eq!(chunks.len(), 3);
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.retained_start_frame)
                .collect::<Vec<_>>(),
            [0, 1, 4_097]
        );
        assert_eq!(chunks[1].mic.len(), TEE_FRAMES_PER_CHUNK);
        assert_eq!(chunks[2].mic.len(), 2);
        let mut mismatch = corti_core::MicTapMismatches::default();
        for chunk in &chunks {
            mismatch.observe(
                chunk.epoch,
                chunk.retained_start_frame,
                chunk.mic.len(),
                chunk.tap.len(),
                8_000,
            );
            if chunk.epoch.id == 1 {
                assert_eq!(chunk.epoch, outcome.quality.epochs[1]);
                assert!(
                    chunk.tap.is_empty(),
                    "an absent track must not start a live decoder"
                );
            }
        }
        assert_eq!(
            mismatch.count(),
            0,
            "actual writer chunks must satisfy the live layout contract"
        );
        assert!(
            !mismatch
                .settle(&outcome.quality)
                .unwrap()
                .requires_batch_recovery()
        );
        assert_eq!(fixture.samples()[2..], [0.25, 0.0].repeat(frames));
    }

    #[test]
    fn rate_change_with_reset_clock_accepts_the_epoch_before_its_overflow_summary() {
        let fixture = Fixture::new();
        let mut timeline: CaptureTimeline = CaptureTimeline::new().unwrap();
        let mut descriptors = Vec::new();
        for (rate, frames, nanos) in [(8_000, 2, 0), (16_000, 4, 250_000)] {
            let admission = timeline
                .plan_callback(
                    CallbackInput {
                        frames,
                        format: format(rate),
                        timestamp: CaptureTimestamp {
                            sample_time: 0.0,
                            sample_time_valid: true,
                            source_offset_ns: nanos,
                            host_time_valid: true,
                        },
                    },
                    64,
                    HAL_DESCRIPTOR_CAPACITY,
                )
                .unwrap();
            assert!(admission.accepted());
            descriptors.extend(admission);
        }
        descriptors.extend(timeline.finish().unwrap());
        assert!(matches!(descriptors[2], CaptureDescriptor::Epoch(_)));
        assert!(matches!(descriptors[3], CaptureDescriptor::Overflow(_)));
        assert!(matches!(descriptors[4], CaptureDescriptor::Audio(_)));
        let (pcm, descriptors) = rings(&descriptors, &[0.25; 12]);
        let outcome = run_writer(
            pcm,
            descriptors,
            fixture.0.clone(),
            OutputLayout::TwoTrack,
            8_000,
            None,
            None,
        )
        .unwrap();
        assert_eq!(outcome.frames, 4);
        assert_eq!(outcome.quality.epochs[1].retained_start_frame, 2);
        assert_eq!(outcome.quality.overflow.omitted_epochs, 1);
        assert_eq!(
            outcome.quality.source_completeness(),
            SourceCompleteness::Unknown
        );
        assert!(fixture.read().permits_completion());
    }

    #[test]
    fn diagnostic_channel_count_change_records_loss_before_rejecting_pcm() {
        let fixture = Fixture::new();
        let changed = CaptureFormat {
            generation: 1,
            source_rate_hz: 8_000,
            mic_channels: 1,
            tap_channels: 2,
        };
        let descriptors = [
            epoch(0, 0, 0, 8_000),
            audio(0, 0, 1, 8_000),
            CaptureDescriptor::Epoch(EpochStart {
                id: 1,
                source_start_ns: Some(125_000),
                source_frame_base: 1,
                format: changed,
                reason: CaptureEpochReason::LayoutChange,
            }),
            CaptureDescriptor::Audio(AudioBlock {
                epoch: 1,
                start_frame: 1,
                frames: 1,
                samples: 3,
                format: changed,
            }),
        ];
        let (pcm, descriptors) = rings(&descriptors, &[0.25, 0.5, 0.5, 0.75, 1.0]);
        assert!(
            run_writer(
                pcm,
                descriptors,
                fixture.0.clone(),
                OutputLayout::AllChannels,
                8_000,
                None,
                None
            )
            .is_err()
        );
        let mut reader = hound::WavReader::open(&fixture.0).unwrap();
        assert_eq!(reader.spec().channels, 2);
        assert_eq!(
            reader
                .samples::<i16>()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            vec![
                super::super::f32_to_i16(0.25),
                super::super::f32_to_i16(0.5)
            ]
        );
        let restored = fixture.read();
        assert!(!restored.permits_completion());
        assert_eq!(
            restored.quality().unwrap().spans[0].cause,
            QualityCause::LayoutChange
        );
        assert_eq!(
            restored.quality().unwrap().source_completeness(),
            SourceCompleteness::Incomplete
        );
    }

    #[test]
    fn unavailable_filter_output_is_zero_after_lookahead_and_tee_stays_raw() {
        struct Delayed(std::collections::VecDeque<f32>);
        impl super::super::CaptureFilter for Delayed {
            fn max_output_lag_samples(&self) -> usize {
                3
            }
            fn push(&mut self, mic: &[f32], _far: &[f32]) -> Vec<f32> {
                self.0.extend(mic.iter().map(|sample| sample + 0.5));
                self.0.drain(..self.0.len().saturating_sub(3)).collect()
            }
            fn finish(self: Box<Self>) -> Vec<f32> {
                self.0.into_iter().collect()
            }
        }
        let fixture = Fixture::new();
        let (pcm, descriptors) = rings(
            &[
                epoch(0, 0, 0, 8_000),
                audio(0, 0, 2, 8_000),
                loss(0, 2, 5),
                audio(0, 5, 2, 8_000),
            ],
            &[0.1, 0.2, 0.2, 0.3, 0.6, 0.7, 0.7, 0.8],
        );
        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        let outcome = run_writer(
            pcm,
            descriptors,
            fixture.0.clone(),
            OutputLayout::TwoTrack,
            8_000,
            Some(CaptureTee::new(tx)),
            Some(Box::new(|_| Box::new(Delayed(Default::default())))),
        )
        .unwrap();
        assert_eq!(outcome.filter, CaptureFilterDisposition::Applied);
        let samples = fixture.samples();
        assert_eq!(
            &samples[4..10],
            &[0.0; 6],
            "AEC lookahead cannot synthesize available audio in a loss"
        );
        assert_eq!(samples[0], 0.1 + 0.5);
        assert_eq!(samples[10], 0.6 + 0.5);
        let chunk = rx.recv().unwrap();
        assert_eq!(chunk.mic, vec![0.1, 0.2, 0.0, 0.0, 0.0, 0.6, 0.7]);
        assert_eq!(chunk.tap, vec![0.2, 0.3, 0.0, 0.0, 0.0, 0.7, 0.8]);
    }

    #[test]
    fn tee_position_after_a_drop_keeps_the_writer_clock() {
        let fixture = Fixture::new();
        let frames = TEE_FRAMES_PER_CHUNK * 2;
        let (mut pcm_tx, pcm_rx) = rtrb::RingBuffer::new(frames * 2);
        let (mut descriptor_tx, descriptor_rx) = rtrb::RingBuffer::new(4);
        for _ in 0..frames * 2 {
            pcm_tx.push(0.25).unwrap();
        }
        descriptor_tx.push(epoch(0, 0, 0, 8_000)).unwrap();
        descriptor_tx
            .push(audio(0, 0, frames as u64, 8_000))
            .unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let tee = CaptureTee::new(tx);
        let dropped = tee.dropped_counter();
        let out = fixture.0.clone();
        let writer = std::thread::spawn(move || {
            run_writer(
                pcm_rx,
                descriptor_rx,
                out,
                OutputLayout::TwoTrack,
                8_000,
                Some(tee),
                None,
            )
        });
        wait_for(|| dropped.load(Ordering::Relaxed) == 1);
        let first = rx.recv().unwrap();
        assert_eq!(first.retained_start_frame, 0);
        for _ in 0..6 {
            pcm_tx.push(0.5).unwrap();
        }
        descriptor_tx
            .push(audio(0, frames as u64, 3, 8_000))
            .unwrap();
        drop(pcm_tx);
        drop(descriptor_tx);
        let outcome = writer.join().unwrap().unwrap();
        let last = rx.recv().unwrap();
        assert_eq!(last.retained_start_frame, frames as u64);
        assert_eq!(last.epoch, outcome.quality.epochs[0]);
        assert_eq!(last.mic, [0.5; 3]);
        assert_eq!(last.tap, [0.5; 3]);
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn tee_drop_is_positioned_processing_loss_and_does_not_make_source_incomplete() {
        let fixture = Fixture::new();
        let frames = TEE_FRAMES_PER_CHUNK + 3;
        let descriptors = [epoch(0, 0, 0, 8_000), audio(0, 0, frames as u64, 8_000)];
        let (pcm, descriptors) = rings(&descriptors, &vec![0.25; frames * 2]);
        let (tx, _rx) = std::sync::mpsc::sync_channel(1);
        let outcome = run_writer(
            pcm,
            descriptors,
            fixture.0.clone(),
            OutputLayout::TwoTrack,
            8_000,
            Some(CaptureTee::new(tx)),
            None,
        )
        .unwrap();
        assert_eq!(outcome.frames, frames as u64);
        assert_eq!(outcome.quality.spans.len(), 1);
        let span = outcome.quality.spans[0];
        assert_eq!(
            (span.start_frame, span.end_frame),
            (TEE_FRAMES_PER_CHUNK as u64, frames as u64)
        );
        assert_eq!(span.class, QualityClass::ProcessingLoss);
        assert_eq!(span.cause, QualityCause::TeeDrop);
        assert_eq!(
            outcome.quality.source_completeness(),
            SourceCompleteness::Complete
        );
        assert_eq!(fixture.read().quality(), Some(&outcome.quality));
    }

    struct FailingSink {
        store: QualityStore,
        before_calls: usize,
        fail_before: Option<usize>,
        fail_complete: bool,
    }

    impl QualitySink for FailingSink {
        fn before_audio(
            &mut self,
            quality: &QualityRecord,
            frames: u64,
        ) -> Result<(), QualityStoreError> {
            self.before_calls += 1;
            if self.fail_before == Some(self.before_calls) {
                return Err(QualityStoreError::Write);
            }
            self.store.persist_before_audio(quality, frames)
        }
        fn finalizing(
            &mut self,
            quality: &QualityRecord,
            frames: u64,
        ) -> Result<(), QualityStoreError> {
            self.store.prepare_finalization(quality, frames)
        }
        fn audio_durable(&mut self, frames: u64) -> Result<(), QualityStoreError> {
            if self.fail_complete {
                return Err(QualityStoreError::FileSync);
            }
            self.store.complete_after_audio_sync(frames)
        }
    }

    #[test]
    fn initial_persistence_failure_creates_no_wav() {
        let fixture = Fixture::new();
        let (pcm, descriptors) =
            rings(&[epoch(0, 0, 0, 8_000), audio(0, 0, 1, 8_000)], &[0.1, 0.2]);
        let result = run_writer_with_sink::<QualityStore>(
            pcm,
            descriptors,
            fixture.0.clone(),
            OutputLayout::TwoTrack,
            8_000,
            None,
            None,
            |_, _, _| Err(QualityStoreError::Write),
        );
        assert!(result.is_err());
        assert!(!fixture.0.exists());
    }

    #[test]
    fn failed_loss_persistence_admits_no_fill_or_later_pcm_even_when_hound_drops() {
        let fixture = Fixture::new();
        let (pcm, descriptors) = rings(
            &[
                epoch(0, 0, 0, 8_000),
                audio(0, 0, 2, 8_000),
                loss(0, 2, 5),
                audio(0, 5, 1, 8_000),
            ],
            &[0.1, 0.2, 0.3, 0.4, 0.8, 0.9],
        );
        let result = run_writer_with_sink(
            pcm,
            descriptors,
            fixture.0.clone(),
            OutputLayout::TwoTrack,
            8_000,
            None,
            None,
            |path, id, quality| {
                Ok(FailingSink {
                    store: QualityStore::begin(path, id, quality)?,
                    before_calls: 0,
                    fail_before: Some(2),
                    fail_complete: false,
                })
            },
        );
        assert!(result.is_err());
        assert_eq!(fixture.samples(), vec![0.1, 0.2, 0.3, 0.4]);
        let restored = fixture.read();
        assert_eq!(restored.status, QualityReadStatus::InProgress);
        assert!(
            !restored.permits_completion(),
            "Hound Drop header updates cannot publish completion"
        );
        assert_eq!(restored.quality().unwrap().epochs.len(), 1);
        assert!(
            restored.quality().unwrap().spans.is_empty(),
            "the prior durable snapshot survives"
        );
    }

    #[test]
    fn restart_after_finalize_before_completion_preserves_exact_loss_and_large_epoch_mapping() {
        let fixture = Fixture::new();
        let descriptors = [
            epoch(0, 0, 0, 8_000),
            audio(0, 0, 2, 8_000),
            loss(0, 2, 88_002),
            epoch(1, 88_002, 11_000_250_000, 8_000),
            audio(1, 88_002, 2, 8_000),
        ];
        let (pcm, descriptors) = rings(&descriptors, &[0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8]);
        let result = run_writer_with_sink(
            pcm,
            descriptors,
            fixture.0.clone(),
            OutputLayout::TwoTrack,
            8_000,
            None,
            None,
            |path, id, quality| {
                Ok(FailingSink {
                    store: QualityStore::begin(path, id, quality)?,
                    before_calls: 0,
                    fail_before: None,
                    fail_complete: true,
                })
            },
        );
        assert!(result.is_err());
        assert_eq!(
            fixture.samples(),
            vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8]
        );
        let bytes = std::fs::read(quality_sidecar_path(&fixture.0).unwrap()).unwrap();
        let restored = fixture.read();
        assert_eq!(restored.status, QualityReadStatus::Finalizing);
        assert!(!restored.permits_completion());
        let quality = restored.quality().unwrap();
        assert_eq!(quality.epochs[1].source_start_ns, Some(11_000_250_000));
        assert_eq!(quality.epochs[1].retained_start_frame, 2);
        assert_eq!(
            (quality.spans[0].start_frame, quality.spans[0].end_frame),
            (2, 88_002)
        );
        assert_eq!(
            quality.source_completeness(),
            SourceCompleteness::Incomplete
        );
        assert_eq!(
            fixture.read(),
            restored,
            "restart does not promote storage or erase source loss"
        );
        assert_eq!(
            std::fs::read(quality_sidecar_path(&fixture.0).unwrap()).unwrap(),
            bytes
        );
    }

    #[test]
    fn audio_durable_restart_keeps_overflow_and_large_gap_incomplete_after_batch() {
        let fixture = Fixture::new();
        let descriptors = [
            epoch(0, 0, 0, 8_000),
            audio(0, 0, 2, 8_000),
            loss(0, 2, 88_002),
            CaptureDescriptor::Overflow(corti_core::capture_accounting::AccountingOverflow {
                omitted_epochs: 3,
                omitted_source_spans: 2,
                counter_saturated: false,
            }),
            epoch(1, 88_002, 11_000_250_000, 8_000),
            audio(1, 88_002, 2, 8_000),
        ];
        let (pcm, descriptors) = rings(&descriptors, &[0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8]);
        let outcome = run_writer(
            pcm,
            descriptors,
            fixture.0.clone(),
            OutputLayout::TwoTrack,
            8_000,
            None,
            None,
        )
        .unwrap();
        let expected = outcome.quality.clone();
        drop(outcome); // Model loss of the completion message after audio_durable publication.
        let restored = fixture.read();
        assert!(restored.permits_completion());
        assert_eq!(restored.quality(), Some(&expected));
        assert_eq!(expected.overflow.omitted_epochs, 3);
        assert_eq!(expected.overflow.omitted_spans[0], 2);
        assert_eq!(expected.epochs[1].source_start_ns, Some(11_000_250_000));
        assert_eq!(expected.epochs[1].retained_start_frame, 2);
        let mut recovered = restored.quality().unwrap().clone();
        recovered
            .resolve_processing_after_batch(
                corti_core::BatchRecoveryOutcome::SameBackendWholeFileSuccess,
            )
            .unwrap();
        assert_eq!(
            recovered.source_completeness(),
            SourceCompleteness::Incomplete
        );
        assert_eq!(recovered.spans, expected.spans);
        assert_eq!(recovered.overflow, expected.overflow);
    }

    #[test]
    fn changed_rate_failure_records_source_loss_without_promoting_the_prefix() {
        let fixture = Fixture::new();
        let descriptors = [
            epoch(0, 0, 0, 8_000),
            audio(0, 0, 2, 8_000),
            epoch(1, 2, 250_000, 16_000),
            audio(1, 2, 4, 16_000),
        ];
        let (pcm, descriptors) = rings(
            &descriptors,
            &[
                0.1,
                0.2,
                0.3,
                0.4,
                0.5,
                0.6,
                0.7,
                0.8,
                f32::NAN,
                0.9,
                1.0,
                1.0,
            ],
        );
        assert!(
            run_writer(
                pcm,
                descriptors,
                fixture.0.clone(),
                OutputLayout::TwoTrack,
                8_000,
                None,
                None
            )
            .is_err()
        );
        assert_eq!(fixture.samples(), vec![0.1, 0.2, 0.3, 0.4]);
        let restored = fixture.read();
        assert!(!restored.permits_completion());
        assert_eq!(
            restored.quality().unwrap().source_completeness(),
            SourceCompleteness::Incomplete
        );
        let span = restored.quality().unwrap().spans[0];
        assert_eq!(span.cause, QualityCause::ResamplerFailure);
        assert_eq!((span.epoch, span.start_frame, span.end_frame), (1, 2, 6));
    }

    #[test]
    fn explicit_stop_authorizes_completion_but_dropped_intent_keeps_exact_terminal_prefix_in_progress()
     {
        struct CountPhases {
            store: QualityStore,
            calls: Arc<[AtomicU64; 3]>,
        }
        impl QualitySink for CountPhases {
            fn before_audio(
                &mut self,
                quality: &QualityRecord,
                frames: u64,
            ) -> Result<(), QualityStoreError> {
                self.calls[0].fetch_add(1, Ordering::Relaxed);
                self.store.persist_before_audio(quality, frames)
            }
            fn finalizing(
                &mut self,
                quality: &QualityRecord,
                frames: u64,
            ) -> Result<(), QualityStoreError> {
                self.calls[1].fetch_add(1, Ordering::Relaxed);
                self.store.prepare_finalization(quality, frames)
            }
            fn audio_durable(&mut self, frames: u64) -> Result<(), QualityStoreError> {
                self.calls[2].fetch_add(1, Ordering::Relaxed);
                self.store.complete_after_audio_sync(frames)
            }
        }
        for explicit_stop in [false, true] {
            let fixture = Fixture::new();
            let (mut pcm_tx, pcm) = rtrb::RingBuffer::new(4);
            let (mut descriptor_tx, descriptors) = rtrb::RingBuffer::new(8);
            for sample in [0.1, 0.2, 0.3, 0.4] {
                pcm_tx.push(sample).unwrap();
            }
            descriptor_tx.push(epoch(0, 0, 0, 8_000)).unwrap();
            descriptor_tx.push(audio(0, 0, 2, 8_000)).unwrap();
            let completion = CompletionIntent::default();
            let worker_completion = completion.clone();
            let calls = Arc::new([AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)]);
            let worker_calls = calls.clone();
            let out = fixture.0.clone();
            let writer = std::thread::spawn(move || {
                run_writer_with_sink_and_intent(
                    pcm,
                    descriptors,
                    out,
                    OutputLayout::TwoTrack,
                    8_000,
                    None,
                    None,
                    worker_completion,
                    |path, identity, initial| {
                        Ok(CountPhases {
                            store: QualityStore::begin(path, identity, initial)?,
                            calls: worker_calls,
                        })
                    },
                )
            });
            // Model the lifecycle ordering: IO is stopped, then its terminal loss is published,
            // then only successful explicit stop authorizes completion before producer closure.
            descriptor_tx.push(loss(0, 2, 5)).unwrap();
            if explicit_stop {
                completion.authorize_complete();
            }
            drop(completion);
            drop(pcm_tx);
            drop(descriptor_tx);
            let result = writer.join().unwrap();
            assert_eq!(result.is_ok(), explicit_stop);
            assert_eq!(
                fixture.samples(),
                vec![0.1, 0.2, 0.3, 0.4, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
            );
            let restored = fixture.read();
            assert_eq!(restored.permits_completion(), explicit_stop);
            assert_eq!(
                restored.status,
                if explicit_stop {
                    QualityReadStatus::AudioDurable
                } else {
                    QualityReadStatus::InProgress
                }
            );
            assert_eq!(
                restored
                    .snapshot
                    .as_ref()
                    .unwrap()
                    .expected_retained_frames(),
                Some(5)
            );
            let quality = restored.quality().unwrap();
            assert_eq!(
                quality.source_completeness(),
                SourceCompleteness::Incomplete
            );
            assert_eq!(
                (quality.spans[0].start_frame, quality.spans[0].end_frame),
                (2, 5)
            );
            assert_eq!(calls[1].load(Ordering::Relaxed), u64::from(explicit_stop));
            assert_eq!(calls[2].load(Ordering::Relaxed), u64::from(explicit_stop));
            if explicit_stop {
                assert_eq!(&result.unwrap().quality, quality);
            } else {
                assert_eq!(
                    calls[0].load(Ordering::Relaxed),
                    3,
                    "abort must persist the exact final prefix without entering finalizing"
                );
            }
        }
    }

    #[test]
    fn abort_snapshot_failure_keeps_prior_terminal_quality_and_never_promotes_on_drop() {
        let fixture = Fixture::new();
        let (pcm, descriptors) = rings(
            &[epoch(0, 0, 0, 8_000), audio(0, 0, 2, 8_000), loss(0, 2, 5)],
            &[0.1, 0.2, 0.3, 0.4],
        );
        let result = run_writer_with_sink_and_intent(
            pcm,
            descriptors,
            fixture.0.clone(),
            OutputLayout::TwoTrack,
            8_000,
            None,
            None,
            CompletionIntent::default(),
            |path, id, quality| {
                Ok(FailingSink {
                    store: QualityStore::begin(path, id, quality)?,
                    before_calls: 0,
                    fail_before: Some(3),
                    fail_complete: false,
                })
            },
        );
        assert!(result.is_err());
        assert_eq!(
            fixture.samples().len() / 2,
            5,
            "Hound Drop leaves the authorized PCM prefix readable"
        );
        let restored = fixture.read();
        assert_eq!(restored.status, QualityReadStatus::InProgress);
        assert!(!restored.permits_completion());
        assert_eq!(
            restored
                .snapshot
                .as_ref()
                .unwrap()
                .expected_retained_frames(),
            Some(2)
        );
        let quality = restored.quality().unwrap();
        assert_eq!(
            (quality.spans[0].start_frame, quality.spans[0].end_frame),
            (2, 5)
        );
        assert_eq!(
            quality.source_completeness(),
            SourceCompleteness::Incomplete
        );
    }

    #[test]
    fn unlabelled_pcm_and_excessive_descriptor_run_fail_closed() {
        let fixture = Fixture::new();
        let (pcm, descriptors) = rings(&[], &[0.25, 0.5]);
        assert!(
            run_writer(
                pcm,
                descriptors,
                fixture.0.clone(),
                OutputLayout::TwoTrack,
                8_000,
                None,
                None
            )
            .is_err()
        );
        assert!(!fixture.0.exists());
        let (pcm_tx, pcm_rx) = rtrb::RingBuffer::new(1);
        let (mut descriptor_tx, descriptor_rx) = rtrb::RingBuffer::new(HAL_DESCRIPTOR_CAPACITY + 1);
        for _ in 0..=HAL_DESCRIPTOR_CAPACITY {
            descriptor_tx.push(epoch(0, 0, 0, 8_000)).unwrap();
        }
        drop(pcm_tx);
        drop(descriptor_tx);
        assert!(
            run_writer(
                pcm_rx,
                descriptor_rx,
                fixture.0.clone(),
                OutputLayout::TwoTrack,
                8_000,
                None,
                None
            )
            .is_err()
        );
        assert!(!fixture.0.exists());
        assert_eq!(fixture.read().status, QualityReadStatus::InProgress);
        assert_eq!(
            fixture.read().quality().unwrap().overflow.omitted_epochs,
            (HAL_DESCRIPTOR_CAPACITY + 1) as u64
        );
    }
}
