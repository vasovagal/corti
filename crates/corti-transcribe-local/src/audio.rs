//! Read the 2-track float WAV that `corti-capture` writes (ch0 = mic/me, ch1 = system-tap/them) and split
//! it into per-channel mono `f32` buffers for the ONNX pipeline.

use std::path::Path;

use anyhow::{Context, Result, bail};

/// One recording, split into its two mono tracks at the source sample rate.
pub struct TwoTrack {
    /// Near-end mic samples (ch0 → `Speaker::Me`).
    pub mic: Vec<f32>,
    /// Far-end system-tap samples (ch1 → far-end speakers).
    pub them: Vec<f32>,
    /// Source sample rate (typically 48 kHz); the engine resamples to 16 kHz as needed.
    pub sample_rate: i32,
}

/// Read a 2-channel 32-bit-float WAV and deinterleave it. A 1-channel WAV (tap-only / webinar) is treated
/// as the far-end track with an empty mic track.
pub fn read_two_track(path: &Path) -> Result<TwoTrack> {
    let mut reader =
        hound::WavReader::open(path).with_context(|| format!("opening WAV {}", path.display()))?;
    let spec = reader.spec();

    if spec.sample_format != hound::SampleFormat::Float {
        bail!(
            "expected a 32-bit float WAV (corti-capture format), got {:?} {}-bit",
            spec.sample_format,
            spec.bits_per_sample
        );
    }

    let samples: Vec<f32> = reader
        .samples::<f32>()
        .collect::<Result<_, _>>()
        .context("decoding float WAV samples")?;
    let sample_rate = spec.sample_rate as i32;

    match spec.channels {
        1 => Ok(TwoTrack {
            mic: Vec::new(),
            them: samples,
            sample_rate,
        }),
        2 => {
            let mut mic = Vec::with_capacity(samples.len() / 2);
            let mut them = Vec::with_capacity(samples.len() / 2);
            for frame in samples.chunks_exact(2) {
                mic.push(frame[0]);
                them.push(frame[1]);
            }
            Ok(TwoTrack {
                mic,
                them,
                sample_rate,
            })
        }
        n => bail!("unsupported channel count {n} (expected 1 or 2)"),
    }
}
