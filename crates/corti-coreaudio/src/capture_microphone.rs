//! The no-file microphone test consumes the same ordered capture descriptors as the WAV writer.

use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Result, anyhow, ensure};
use corti_core::capture_accounting::{CaptureDescriptor, EpochStart, HAL_DESCRIPTOR_CAPACITY};
use corti_core::capture_normalizer::{CaptureRateNormalizer, NormalizationFailure};
use corti_core::{QualityClass, QualityRecord, QualityResolution};

use super::{CaptureChunk, CaptureTee, TEE_FRAMES_PER_CHUNK, frame_mean, ordered_writer};

pub(super) struct MicrophoneOutcome {
    pub(super) frames: u64,
    pub(super) quality: QualityRecord,
}

struct Output {
    tee: CaptureTee,
    mono: Vec<f32>,
    unavailable: Vec<bool>,
    frames: u64,
    dropped: u64,
    epoch: Option<corti_core::CaptureEpoch>,
}

impl Output {
    fn emit(&mut self, frame: &[f32], unavailable: bool) -> Result<()> {
        ensure!(frame.len() == 1, "invalid normalized microphone frame");
        if unavailable && self.unavailable.is_empty() {
            self.unavailable.resize(self.mono.len(), false);
        }
        if unavailable || !self.unavailable.is_empty() {
            self.unavailable.push(unavailable);
        }
        self.mono.push(if unavailable { 0.0 } else { frame[0] });
        self.frames = self
            .frames
            .checked_add(1)
            .ok_or_else(|| anyhow!("microphone output frame overflow"))?;
        if self.mono.len() == TEE_FRAMES_PER_CHUNK {
            self.send()?;
        }
        Ok(())
    }

    fn send(&mut self) -> Result<()> {
        if self.mono.is_empty() {
            return Ok(());
        }
        let epoch = self
            .epoch
            .ok_or_else(|| anyhow!("microphone tee has no epoch"))?;
        let retained_start_frame = self.frames - self.mono.len() as u64;
        let chunk = CaptureChunk {
            epoch,
            retained_start_frame,
            mic: std::mem::replace(&mut self.mono, Vec::with_capacity(TEE_FRAMES_PER_CHUNK)),
            tap: Vec::new(),
            unavailable: std::mem::take(&mut self.unavailable),
        };
        if self.tee.tx.try_send(chunk).is_err() {
            self.tee.dropped.fetch_add(1, Ordering::Relaxed);
            self.dropped = self.dropped.saturating_add(1);
        }
        Ok(())
    }
}

pub(super) fn run_microphone_tee(
    mut pcm: rtrb::Consumer<f32>,
    mut descriptors: rtrb::Consumer<CaptureDescriptor>,
    retained_rate: u32,
    tee: CaptureTee,
) -> Result<MicrophoneOutcome> {
    let mut quality = QualityRecord::observed();
    let mut output = Output {
        tee,
        mono: Vec::with_capacity(TEE_FRAMES_PER_CHUNK),
        unavailable: Vec::new(),
        frames: 0,
        dropped: 0,
        epoch: None,
    };
    let mut current: Option<EpochStart> = None;
    let mut normalizer: Option<CaptureRateNormalizer> = None;
    let mut source_cursor = 0;
    let mut remap_required = false;
    let mut pending = Vec::with_capacity(HAL_DESCRIPTOR_CAPACITY);
    loop {
        let descriptor = match descriptors.pop() {
            Ok(descriptor) => Some(descriptor),
            Err(_) if descriptors.is_abandoned() => None,
            Err(_) => {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
        };
        let audio = match descriptor {
            Some(CaptureDescriptor::Audio(audio)) => Some(audio),
            Some(descriptor) => {
                ensure!(
                    pending.len() < HAL_DESCRIPTOR_CAPACITY,
                    "microphone metadata buffer full"
                );
                pending.push(descriptor);
                continue;
            }
            None => None,
        };
        let fill = ordered_writer::can_fill_run(current, &pending)?;
        for descriptor in pending.drain(..) {
            match descriptor {
                CaptureDescriptor::Epoch(start) => {
                    if let Some(mut old) = normalizer.take() {
                        old.finish(|frame, unavailable| output.emit(frame, unavailable))
                            .map_err(normalization_error)?;
                    }
                    output.send()?;
                    let epoch = start.with_retained_base(output.frames, retained_rate)?;
                    quality.insert_epoch(epoch)?;
                    output.epoch = Some(epoch);
                    normalizer = Some(CaptureRateNormalizer::new(
                        start.format.source_rate_hz,
                        retained_rate,
                        1,
                    )?);
                    source_cursor = start.source_frame_base;
                    current = Some(start);
                    remap_required = false;
                }
                CaptureDescriptor::Loss(span) => {
                    let start = current.ok_or_else(|| anyhow!("microphone loss without epoch"))?;
                    ensure!(
                        span.epoch == start.id && span.start_frame <= source_cursor,
                        "microphone loss is out of order"
                    );
                    if quality.epochs.iter().any(|epoch| epoch.id == span.epoch) {
                        quality.insert_span(span)?;
                    } else {
                        quality.record_omitted_spans(span.class, span.resolution, 1)?;
                    }
                    let missing = span.end_frame.saturating_sub(source_cursor);
                    if fill && missing > 0 {
                        normalizer
                            .as_mut()
                            .ok_or_else(|| anyhow!("microphone rate unavailable"))?
                            .gap(missing, |frame, unavailable| {
                                output.emit(frame, unavailable)
                            })
                            .map_err(normalization_error)?;
                    } else if missing > 0 {
                        remap_required = true;
                    }
                    source_cursor = source_cursor.max(span.end_frame);
                }
                CaptureDescriptor::Overflow(overflow) => overflow.apply_to(&mut quality)?,
                CaptureDescriptor::Audio(_) => {
                    unreachable!("audio descriptors are processed separately")
                }
            }
        }
        let Some(audio) = audio else {
            break;
        };
        let start = current.ok_or_else(|| anyhow!("microphone audio without epoch"))?;
        ensure!(
            !remap_required
                && audio.epoch == start.id
                && audio.format == start.format
                && audio.start_frame == source_cursor,
            "microphone audio is out of order"
        );
        let channels = audio
            .format
            .mic_channels
            .checked_add(audio.format.tap_channels)
            .ok_or_else(|| anyhow!("microphone channel overflow"))? as usize;
        ensure!(
            channels > 0 && channels <= 64 && audio.format.tap_channels == 0,
            "invalid microphone-only format"
        );
        ensure!(
            audio.samples
                == audio
                    .frames
                    .checked_mul(channels as u64)
                    .ok_or_else(|| anyhow!("microphone sample count overflow"))?,
            "microphone descriptor shape mismatch"
        );
        let mut frame = [0.0f32; 64];
        for _ in 0..audio.frames {
            for sample in &mut frame[..channels] {
                *sample = pcm
                    .pop()
                    .map_err(|_| anyhow!("microphone descriptor precedes its PCM"))?;
            }
            let mono = frame_mean(&frame[..channels], 0, channels);
            normalizer
                .as_mut()
                .ok_or_else(|| anyhow!("microphone rate unavailable"))?
                .push(&[mono], |frame, unavailable| {
                    output.emit(frame, unavailable)
                })
                .map_err(normalization_error)?;
        }
        source_cursor = audio
            .start_frame
            .checked_add(audio.frames)
            .ok_or_else(|| anyhow!("microphone source counter overflow"))?;
    }
    ensure!(pcm.slots() == 0, "microphone ended with undescribed PCM");
    if let Some(mut normalizer) = normalizer {
        normalizer
            .finish(|frame, unavailable| output.emit(frame, unavailable))
            .map_err(normalization_error)?;
    }
    output.send()?;
    quality.record_omitted_spans(
        QualityClass::ProcessingLoss,
        QualityResolution::Outstanding,
        output.dropped,
    )?;
    Ok(MicrophoneOutcome {
        frames: output.frames,
        quality,
    })
}

fn normalization_error(error: NormalizationFailure<anyhow::Error>) -> anyhow::Error {
    match error {
        NormalizationFailure::Sink(error) => error,
        NormalizationFailure::Normalization(_) => anyhow!("microphone normalization failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use corti_core::capture_accounting::{AudioBlock, CaptureFormat};
    use corti_core::{
        CaptureEpochReason, QualityCause, QualitySpan, QualityStage, QualityTrack,
        SourceCompleteness,
    };

    fn epoch(id: u32, base: u64, offset: u64, rate: u32) -> EpochStart {
        EpochStart {
            id,
            source_frame_base: base,
            source_start_ns: Some(offset),
            format: CaptureFormat {
                generation: id as u64 + 1,
                source_rate_hz: rate,
                mic_channels: 2,
                tap_channels: 0,
            },
            reason: if id == 0 {
                CaptureEpochReason::Start
            } else {
                CaptureEpochReason::HostDiscontinuity
            },
        }
    }

    fn audio(epoch: EpochStart, start: u64, frames: u64) -> CaptureDescriptor {
        CaptureDescriptor::Audio(AudioBlock {
            epoch: epoch.id,
            start_frame: start,
            frames,
            samples: frames * 2,
            format: epoch.format,
        })
    }

    fn loss(epoch: u32, start: u64, end: u64) -> CaptureDescriptor {
        CaptureDescriptor::Loss(QualitySpan {
            epoch,
            start_frame: start,
            end_frame: end,
            track: QualityTrack::Both,
            cause: QualityCause::RingOverflow,
            class: QualityClass::SourceLoss,
            stage: QualityStage::Capture,
            resolution: QualityResolution::Outstanding,
        })
    }

    fn run(
        samples: &[f32],
        events: &[CaptureDescriptor],
    ) -> Result<(MicrophoneOutcome, Vec<CaptureChunk>)> {
        let (mut pcm, pcm_reader) = rtrb::RingBuffer::new(samples.len().max(1));
        for &sample in samples {
            pcm.push(sample).unwrap();
        }
        drop(pcm);
        let (mut descriptors, descriptor_reader) = rtrb::RingBuffer::new(events.len().max(1));
        for &event in events {
            descriptors.push(event).unwrap();
        }
        drop(descriptors);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        let outcome =
            run_microphone_tee(pcm_reader, descriptor_reader, 48_000, CaptureTee::new(tx))?;
        Ok((outcome, rx.try_iter().collect()))
    }

    #[test]
    fn microphone_tee_keeps_small_and_terminal_losses_on_its_timeline() {
        let first = epoch(0, 0, 0, 48_000);
        for terminal in [false, true] {
            let mut events = vec![
                CaptureDescriptor::Epoch(first),
                audio(first, 0, 2),
                loss(0, 2, 4),
            ];
            let mut samples = vec![0.25, 0.75, 0.5, 1.0];
            let mut expected = vec![0.5, 0.75, 0.0, 0.0];
            if !terminal {
                events.push(audio(first, 4, 2));
                samples.extend([0.0, 0.5, -0.5, 0.0]);
                expected.extend([0.25, -0.25]);
            }
            let (outcome, chunks) = run(&samples, &events).unwrap();
            assert_eq!(outcome.frames, expected.len() as u64);
            assert_eq!(
                outcome.quality.source_completeness(),
                SourceCompleteness::Incomplete
            );
            assert!(chunks.iter().all(|chunk| chunk.tap.is_empty()));
            let mut unavailable = vec![false, false, true, true];
            if !terminal {
                unavailable.extend([false, false]);
            }
            assert_eq!(chunks[0].unavailable, unavailable);
            assert_eq!(
                chunks
                    .into_iter()
                    .flat_map(|chunk| chunk.mic)
                    .collect::<Vec<_>>(),
                expected
            );
        }
    }

    #[test]
    fn large_microphone_loss_requires_a_new_epoch_without_expanding_the_tee() {
        let first = epoch(0, 0, 0, 48_000);
        let next = epoch(1, 528_002, 11_000_041_667, 48_000);
        let samples = [0.5; 8];
        let mut events = vec![
            CaptureDescriptor::Epoch(first),
            audio(first, 0, 2),
            loss(0, 2, 528_002),
        ];
        let mut invalid = events.clone();
        invalid.push(audio(first, 528_002, 2));
        assert!(run(&samples, &invalid).is_err());
        events.extend([CaptureDescriptor::Epoch(next), audio(next, 528_002, 2)]);
        let (outcome, chunks) = run(&samples, &events).unwrap();
        assert_eq!(outcome.frames, 4);
        assert_eq!(
            chunks.len(),
            2,
            "an epoch boundary flushes the prior tee tail"
        );
        assert_eq!(chunks[0].epoch, outcome.quality.epochs[0]);
        assert_eq!(chunks[0].retained_start_frame, 0);
        assert_eq!(chunks[1].epoch, outcome.quality.epochs[1]);
        assert_eq!(chunks[1].retained_start_frame, 2);
        assert!(chunks.iter().all(|chunk| chunk.unavailable.is_empty()));
        assert_eq!(outcome.quality.epochs[1].retained_start_frame, 2);
        assert_eq!(
            outcome.quality.epochs[1].source_start_ns,
            Some(11_000_041_667)
        );
        assert_eq!(
            chunks
                .into_iter()
                .flat_map(|chunk| chunk.mic)
                .collect::<Vec<_>>(),
            vec![0.5; 4]
        );
    }

    #[test]
    fn changed_microphone_rate_keeps_fixed_output_clock_and_unlabelled_pcm_is_rejected() {
        let first = epoch(0, 0, 0, 48_000);
        let next = epoch(1, 2, 41_667, 24_000);
        let events = [
            CaptureDescriptor::Epoch(first),
            audio(first, 0, 2),
            CaptureDescriptor::Epoch(next),
            audio(next, 2, 2),
        ];
        let (outcome, chunks) = run(&[0.5; 8], &events).unwrap();
        assert_eq!(outcome.frames, 6);
        assert_eq!(chunks[0].epoch, outcome.quality.epochs[0]);
        assert_eq!(chunks[1].epoch, outcome.quality.epochs[1]);
        assert_eq!(chunks[1].retained_start_frame, 2);
        assert_eq!(chunks.iter().map(|chunk| chunk.mic.len()).sum::<usize>(), 6);
        assert!(
            chunks
                .iter()
                .flat_map(|chunk| &chunk.mic)
                .all(|sample| (*sample - 0.5).abs() < 1e-6)
        );
        assert!(run(&[0.5], &[]).is_err());
    }
}
