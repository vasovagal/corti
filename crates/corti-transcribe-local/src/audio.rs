//! Read the 2-track WAV that `corti-capture` writes (ch0 = mic/me, ch1 = system-tap/them) and split it into
//! per-channel mono `f32` buffers for the ONNX pipeline. Both 16-bit int and 32-bit float WAVs are accepted
//! and normalized to `f32` (under ADR 0007 the 2-track capture is itself 32-bit float — the lossless AEC input).

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

/// Read a 16-bit-int or 32-bit-float WAV and deinterleave it. A 1-channel WAV (tap-only / webinar) is treated
/// as the far-end track with an empty mic track.
pub fn read_two_track(path: &Path) -> Result<TwoTrack> {
    let mut reader =
        hound::WavReader::open(path).with_context(|| format!("opening WAV {}", path.display()))?;
    let spec = reader.spec();
    let sample_rate = spec.sample_rate as i32;

    // Read interleaved samples as f32, tolerating Float (AEC-cleaned WAVs) and Int (raw capture / the
    // tap-only "webinar" path that bypasses AEC). Mirrors corti-transcribe-aws::wav and
    // corti-capture::write_clean_wav.
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<Result<_, _>>()
            .context("decoding float WAV samples")?,
        hound::SampleFormat::Int => {
            let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / max))
                .collect::<Result<_, _>>()
                .context("decoding int WAV samples")?
        }
    };

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

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() <= 1.0 / 32_768.0
    }

    /// The raw capture format: a 2-channel **int16** WAV must decode and deinterleave (ch0 = mic, ch1 = tap),
    /// normalized to f32 in [-1, 1]. This is the format the local backend previously rejected.
    #[test]
    fn reads_two_channel_int16() {
        let path = std::env::temp_dir().join("corti-local-read-int16-2ch.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        {
            let mut w = hound::WavWriter::create(&path, spec).unwrap();
            // Two frames: [me, them] = [16384, -16384], [32767, 0].
            for v in [16384i16, -16384, 32767, 0] {
                w.write_sample(v).unwrap();
            }
            w.finalize().unwrap();
        }

        let t = read_two_track(&path).unwrap();
        assert_eq!(t.sample_rate, 48_000);
        assert_eq!(t.mic.len(), 2);
        assert_eq!(t.them.len(), 2);
        assert!(close(t.mic[0], 0.5) && close(t.mic[1], 1.0), "mic = ch0");
        assert!(
            close(t.them[0], -0.5) && close(t.them[1], 0.0),
            "them = ch1"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// The webinar regression: a tap-only recording is a 1-channel **int16** WAV (AEC is skipped, so it
    /// reaches the reader raw). It must decode as the far-end track with an empty mic track.
    #[test]
    fn reads_mono_int16_webinar() {
        let path = std::env::temp_dir().join("corti-local-read-int16-mono.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        {
            let mut w = hound::WavWriter::create(&path, spec).unwrap();
            for v in [16384i16, -16384, 32767] {
                w.write_sample(v).unwrap();
            }
            w.finalize().unwrap();
        }

        let t = read_two_track(&path).unwrap();
        assert!(t.mic.is_empty(), "tap-only has no mic track");
        assert_eq!(t.them.len(), 3);
        assert!(close(t.them[0], 0.5) && close(t.them[1], -0.5) && close(t.them[2], 1.0));

        let _ = std::fs::remove_file(&path);
    }

    /// No regression for the AEC-cleaned format: a 2-channel **float32** WAV still decodes.
    #[test]
    fn reads_two_channel_float() {
        let path = std::env::temp_dir().join("corti-local-read-f32-2ch.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        {
            let mut w = hound::WavWriter::create(&path, spec).unwrap();
            for v in [0.5f32, -0.5, 1.0, 0.0] {
                w.write_sample(v).unwrap();
            }
            w.finalize().unwrap();
        }

        let t = read_two_track(&path).unwrap();
        assert_eq!(t.mic, vec![0.5, 1.0]);
        assert_eq!(t.them, vec![-0.5, 0.0]);

        let _ = std::fs::remove_file(&path);
    }
}
