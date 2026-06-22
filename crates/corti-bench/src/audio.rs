//! Minimal WAV helpers for the harness: read a 2-track recording into mic/far channels and write f32 WAVs
//! (2-track or mono) for AEC scoring. Mirrors the sample reading in `corti_capture::write_clean_wav`
//! (tolerates Float and Int input; emits 32-bit float).

use std::path::Path;

use anyhow::{Context, Result, bail};

/// A de-interleaved 2-track recording: ch0 = mic ("me"), ch1 = far-end tap ("them").
pub struct TwoTrack {
    pub mic: Vec<f32>,
    pub far: Vec<f32>,
    pub sample_rate: u32,
}

/// Read a **2-channel** WAV into separate mic/far channels (Float or Int input → f32). Errors on any other
/// channel count: the AEC path needs both the mic and its echo reference.
pub fn read_2track(path: &Path) -> Result<TwoTrack> {
    let mut reader =
        hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let spec = reader.spec();
    if spec.channels != 2 {
        bail!(
            "AEC needs a 2-channel (mic+tap) WAV, got {} channel(s) in {}",
            spec.channels,
            path.display()
        );
    }
    let samples = read_f32_interleaved(&mut reader, &spec)?;
    let mic = samples.iter().step_by(2).copied().collect();
    let far = samples.iter().skip(1).step_by(2).copied().collect();
    Ok(TwoTrack {
        mic,
        far,
        sample_rate: spec.sample_rate,
    })
}

fn read_f32_interleaved(
    reader: &mut hound::WavReader<std::io::BufReader<std::fs::File>>,
    spec: &hound::WavSpec,
) -> Result<Vec<f32>> {
    Ok(match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<Result<_, _>>()
            .context("reading float samples")?,
        hound::SampleFormat::Int => {
            let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / max))
                .collect::<Result<_, _>>()
                .context("reading int samples")?
        }
    })
}

/// Write a 2-track 32-bit-float WAV: ch0 = `a` (e.g. cleaned mic), ch1 = `b` (e.g. far-end passthrough).
pub fn write_2track(path: &Path, a: &[f32], b: &[f32], sample_rate: u32) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec)
        .with_context(|| format!("creating {}", path.display()))?;
    let frames = a.len().max(b.len());
    for i in 0..frames {
        w.write_sample(a.get(i).copied().unwrap_or(0.0))?;
        w.write_sample(b.get(i).copied().unwrap_or(0.0))?;
    }
    w.finalize().context("finalizing 2-track WAV")?;
    Ok(())
}

/// Write a mono 32-bit-float WAV (for the python ERLE scorer's per-signal inputs).
pub fn write_mono(path: &Path, samples: &[f32], sample_rate: u32) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec)
        .with_context(|| format!("creating {}", path.display()))?;
    for &s in samples {
        w.write_sample(s)?;
    }
    w.finalize().context("finalizing mono WAV")?;
    Ok(())
}

/// Root-mean-square of a signal (0.0 for empty) — a quick non-silence check for captured channels.
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples.iter().map(|&x| (x as f64) * (x as f64)).sum();
    ((sum_sq / samples.len() as f64).sqrt()) as f32
}
