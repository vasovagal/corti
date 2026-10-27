//! Give the native windowed-sinc resampler one source-packet schedule for every caller.
//!
//! Sherpa's interior dot product and history-edge accumulation can round differently. Passing arbitrary
//! caller chunks therefore changes low sample bits and can change recognition. Fixed source packets make
//! batch and live choose the same arithmetic paths without changing the filter, cutoff or output clock.

use sherpa_onnx::LinearResampler;

use crate::engine::TARGET_RATE;

pub(crate) const PACKET_FRAMES: usize = 512;

pub(crate) struct Resampler {
    native: LinearResampler,
    // Fixed staging: 2 KiB and at most 511 waiting source frames, independent of recording length.
    // Returned PCM still scales with the supplied input; this does not make a whole-input call bounded.
    packet: [f32; PACKET_FRAMES],
    filled: usize,
}

impl Resampler {
    pub(crate) fn new(from_hz: i32) -> Option<Self> {
        Some(Self {
            native: LinearResampler::create(from_hz, TARGET_RATE)?,
            packet: [0.0; PACKET_FRAMES],
            filled: 0,
        })
    }

    pub(crate) fn push(&mut self, mut samples: &[f32]) -> Vec<f32> {
        let mut output = Vec::new();
        while !samples.is_empty() {
            let copied = samples.len().min(PACKET_FRAMES - self.filled);
            self.packet[self.filled..self.filled + copied].copy_from_slice(&samples[..copied]);
            self.filled += copied;
            samples = &samples[copied..];
            if self.filled == PACKET_FRAMES {
                output.extend(self.native.resample(&self.packet, false));
                self.filled = 0;
            }
        }
        output
    }

    /// Finish an epoch and reset both packet phase and native history. The final partial packet uses
    /// the same non-flushing call as full packets; only the following empty call flushes the native tail.
    /// Empty/repeated finishes emit nothing. Callers must deliver this tail before starting a new epoch.
    pub(crate) fn finish(&mut self) -> Vec<f32> {
        let mut output = Vec::new();
        if self.filled != 0 {
            output.extend(self.native.resample(&self.packet[..self.filled], false));
            self.filled = 0;
        }
        output.extend(self.native.resample(&[], true));
        output
    }
}

#[cfg(test)]
#[path = "resample_comparison.rs"]
mod comparison;
