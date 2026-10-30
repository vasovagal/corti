//! The HAL transport: fixed PCM/descriptor rings and allocation-free source accounting.

use std::cell::UnsafeCell;
use std::os::raw::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use coreaudio_sys as ca;
use corti_core::capture_accounting::{
    AccountingOverflow, CallbackInput, CaptureDescriptor, CaptureFormat, CaptureTimeline,
    CaptureTimestamp, DroppedCounts,
};

use super::Shared;
use super::capture_format::{FormatState, HostClock};

struct ProducerState {
    pcm: rtrb::Producer<f32>,
    descriptors: rtrb::Producer<CaptureDescriptor>,
    timeline: CaptureTimeline,
    clock: HostClock,
}

/// HAL is the sole mutator until AudioDeviceDestroyIOProcID returns. Terminal draining then moves to
/// the stopping thread; it is allowed to wait for the writer, unlike the callback.
pub(super) struct Cap {
    state: UnsafeCell<ProducerState>,
    pub(super) shared: Arc<Shared>,
    format: Arc<FormatState>,
    microphone_only: bool,
    enabled: AtomicBool,
}

impl Cap {
    /// Disable new admissions before native teardown; only IO destruction transfers timeline ownership.
    pub(super) fn disable_capture(&self) {
        self.enabled.store(false, Ordering::Release);
    }

    pub(super) fn new(
        pcm: rtrb::Producer<f32>,
        descriptors: rtrb::Producer<CaptureDescriptor>,
        shared: Arc<Shared>,
        format: Arc<FormatState>,
        microphone_only: bool,
    ) -> Result<Self> {
        Ok(Self {
            state: UnsafeCell::new(ProducerState {
                pcm,
                descriptors,
                timeline: CaptureTimeline::new()?,
                clock: HostClock::new()?,
            }),
            shared,
            format,
            microphone_only,
            enabled: AtomicBool::new(true),
        })
    }

    /// Only after destroying the IO proc. Transfer pending terminal losses before either ring closes.
    pub(super) fn finish(&mut self) -> Result<DroppedCounts> {
        let state = self.state.get_mut();
        let dropped = state.timeline.dropped_counts();
        let terminal: Vec<_> = match state.timeline.finish() {
            Ok(terminal) => terminal.collect(),
            Err(_) => vec![CaptureDescriptor::Overflow(AccountingOverflow {
                omitted_epochs: 1,
                omitted_source_spans: 1,
                counter_saturated: true,
            })],
        };
        for descriptor in terminal {
            let mut pending = descriptor;
            loop {
                match state.descriptors.push(pending) {
                    Ok(()) => break,
                    Err(rtrb::PushError::Full(value)) => {
                        if state.descriptors.is_abandoned() {
                            // The writer has already failed. It cannot promote audio_durable afterward.
                            return Ok(dropped);
                        }
                        pending = value;
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
            }
        }
        Ok(dropped)
    }
}

/// Reserve uncommitted PCM, verify its observed format remains current, then admit both rings. Until
/// both reservations succeed, copied bytes are private producer scratch and cannot reach the writer.
pub(super) unsafe extern "C" fn io_proc(
    _device: ca::AudioObjectID,
    _now: *const ca::AudioTimeStamp,
    input: *const ca::AudioBufferList,
    input_time: *const ca::AudioTimeStamp,
    _out: *mut ca::AudioBufferList,
    _out_time: *const ca::AudioTimeStamp,
    client: *mut c_void,
) -> ca::OSStatus {
    if input.is_null() || client.is_null() {
        return 0;
    }
    let cap = unsafe { &*client.cast::<Cap>() };
    if !cap.enabled.load(Ordering::Acquire) {
        return 0;
    }
    cap.shared.callbacks.fetch_add(1, Ordering::Relaxed);
    let state = unsafe { &mut *cap.state.get() };
    let list = unsafe { &*input };
    if list.mNumberBuffers == 0 {
        return 0;
    }
    let buffers =
        unsafe { std::slice::from_raw_parts(list.mBuffers.as_ptr(), list.mNumberBuffers as usize) };
    let Some(first) = buffers.iter().find(|buffer| buffer.mNumberChannels > 0) else {
        return 0;
    };
    let frames = first.mDataByteSize as u64 / (4 * u64::from(first.mNumberChannels));
    let Some(total) = buffers.iter().try_fold(0u32, |total, buffer| {
        total.checked_add(buffer.mNumberChannels)
    }) else {
        let _ = state.timeline.record_unknown_format_loss(frames, u64::MAX);
        return 0;
    };
    if frames == 0 {
        return 0;
    }
    // A physical microphone may provide one buffer per stream/channel. Only an aggregate reserves
    // its first buffer for mic and classifies the remaining buffers as the tap.
    let mic = if cap.microphone_only {
        total
    } else if cap.shared.has_mic {
        buffers[0].mNumberChannels
    } else {
        0
    };
    cap.shared.mic_channels.store(mic, Ordering::Relaxed);
    cap.shared.total_channels.store(total, Ordering::Release);
    let samples = frames.saturating_mul(u64::from(total));
    let format = cap.format.snapshot();
    let shape_valid = total <= 64
        && buffers.iter().all(|buffer| {
            buffer.mNumberChannels == 0
                || (!buffer.mData.is_null()
                    && u64::from(buffer.mDataByteSize)
                        == frames * u64::from(buffer.mNumberChannels) * 4)
        });
    if !shape_valid || format.is_none() {
        let _ = state.timeline.record_unknown_format_loss(frames, samples);
        cap.shared
            .dropped
            .store(state.timeline.dropped_counts().samples, Ordering::Release);
        return 0;
    }
    let (generation, source_rate_hz) = format.unwrap();
    let timestamp = if input_time.is_null() {
        CaptureTimestamp::default()
    } else {
        let native = unsafe { &*input_time };
        let offset = state.clock.offset_ns(
            native.mHostTime,
            native.mFlags & ca::kAudioTimeStampHostTimeValid != 0,
        );
        CaptureTimestamp {
            sample_time: native.mSampleTime,
            sample_time_valid: native.mFlags & ca::kAudioTimeStampSampleTimeValid != 0,
            source_offset_ns: offset.unwrap_or(0),
            host_time_valid: offset.is_some(),
        }
    };
    let callback = CallbackInput {
        frames,
        format: CaptureFormat {
            generation,
            source_rate_hz,
            mic_channels: mic,
            tap_channels: total - mic,
        },
        timestamp,
    };
    let available_pcm = state.pcm.slots();
    let available_descriptors = state.descriptors.slots();
    let Ok(needed) = usize::try_from(samples) else {
        let _ = state.timeline.record_unknown_format_loss(frames, samples);
        return 0;
    };
    if needed > available_pcm {
        let _ = state
            .timeline
            .plan_callback(callback, available_pcm, available_descriptors);
    } else if let Ok(mut pcm) = state.pcm.write_chunk_uninit(needed) {
        let (head, tail) = pcm.as_mut_slices();
        let mut slots = head.iter_mut().chain(tail.iter_mut());
        for frame in 0..frames as usize {
            for buffer in buffers {
                let channels = buffer.mNumberChannels as usize;
                for channel in 0..channels {
                    // Shape and nonnull checks above bound every read. Only this producer owns slots.
                    let sample =
                        unsafe { *buffer.mData.cast::<f32>().add(frame * channels + channel) };
                    if let Some(slot) = slots.next() {
                        slot.write(sample);
                    }
                }
            }
        }
        if cap.format.snapshot() != format {
            // The uncommitted reservation is discarded at branch end; no PCM is published.
            let _ = state.timeline.record_unknown_format_loss(frames, samples);
        } else if let Ok(mut admission) =
            state
                .timeline
                .plan_callback(callback, needed, available_descriptors)
            && admission.accepted()
        {
            // Sole producer: the consumer can only increase capacity since plan_callback sampled it.
            if let Ok(mut descriptors) = state
                .descriptors
                .write_chunk_uninit(admission.descriptor_slots())
            {
                let (head, tail) = descriptors.as_mut_slices();
                for (slot, descriptor) in head
                    .iter_mut()
                    .chain(tail.iter_mut())
                    .zip(admission.by_ref())
                {
                    slot.write(descriptor);
                }
                // Exhaust the admission even after the final zipped slot, closing its state latch.
                let terminal = admission.next();
                debug_assert!(terminal.is_none());
                unsafe { pcm.commit_all() };
                // Release publication of the metadata follows release publication of its exact PCM.
                unsafe { descriptors.commit_all() };
            }
        }
    }
    cap.shared
        .dropped
        .store(state.timeline.dropped_counts().samples, Ordering::Release);
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, AtomicU64};

    #[test]
    fn disabled_callback_admission_keeps_pcm_descriptors_and_accounting_unchanged() {
        let shared = Arc::new(Shared {
            total_channels: AtomicU32::new(0),
            mic_channels: AtomicU32::new(0),
            callbacks: AtomicU32::new(0),
            dropped: AtomicU64::new(0),
            has_mic: true,
        });
        let (pcm, mut samples) = rtrb::RingBuffer::new(8);
        let (descriptors, mut facts) = rtrb::RingBuffer::new(16);
        let mut cap = Cap::new(
            pcm,
            descriptors,
            shared.clone(),
            Arc::new(FormatState::for_test(8_000)),
            true,
        )
        .unwrap();
        let mut audio = [0.125f32, 0.375];
        let input = ca::AudioBufferList {
            mNumberBuffers: 1,
            mBuffers: [ca::AudioBuffer {
                mNumberChannels: 1,
                mDataByteSize: 8,
                mData: audio.as_mut_ptr().cast(),
            }],
        };
        let call = |cap: &Cap| unsafe {
            io_proc(
                0,
                std::ptr::null(),
                &input,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                (cap as *const Cap).cast_mut().cast(),
            )
        };
        assert_eq!(call(&cap), 0);
        assert_eq!(
            (samples.pop().unwrap(), samples.pop().unwrap()),
            (0.125, 0.375)
        );
        assert!(matches!(facts.pop().unwrap(), CaptureDescriptor::Epoch(_)));
        assert!(
            matches!(facts.pop().unwrap(), CaptureDescriptor::Audio(block) if block.frames == 2)
        );
        let callbacks = shared.callbacks.load(Ordering::Acquire);
        cap.disable_capture();
        for _ in 0..3 {
            assert_eq!(call(&cap), 0);
        }
        assert_eq!(shared.callbacks.load(Ordering::Acquire), callbacks);
        assert!(samples.pop().is_err());
        assert!(facts.pop().is_err());
        assert_eq!(cap.finish().unwrap(), DroppedCounts::default());
        assert!(facts.pop().is_err());
    }
}
