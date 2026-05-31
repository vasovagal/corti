//! Re-encode a recording to the format AWS Transcribe accepts.
//!
//! corti's recordings are 2-channel **32-bit float** WAVs (ch0 = me, ch1 = them). AWS Transcribe's
//! documented input is **16-bit PCM**, and channel identification needs the two channels preserved, so we
//! transcode float → i16 PCM (same channel count + sample rate) into a temp file before upload. No
//! resampling: AWS accepts up to 48 kHz, which is what we capture.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Read the float (or int) WAV at `input` and write a 16-bit PCM copy to a temp file, preserving channel
/// count and sample rate. Returns the temp path and the sample rate. Caller deletes the temp file.
pub fn to_pcm16_temp(input: &Path) -> Result<(PathBuf, u32)> {
    let mut reader = hound::WavReader::open(input)
        .with_context(|| format!("opening {} for re-encode", input.display()))?;
    let spec = reader.spec();

    // Read every sample as f32 in [-1, 1] (recordings are Float; tolerate Int just in case).
    let samples: Vec<f32> = match spec.sample_format {
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
    };

    let out_spec = hound::WavSpec {
        channels: spec.channels,
        sample_rate: spec.sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "corti-audio".to_string());
    let out = std::env::temp_dir().join(format!("{stem}-pcm16.wav"));

    let mut writer = hound::WavWriter::create(&out, out_spec)
        .with_context(|| format!("creating {}", out.display()))?;
    for s in samples {
        // Clamp before scaling so a >1.0 float can't wrap to a negative i16.
        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
        writer.write_sample(v).context("writing pcm16 sample")?;
    }
    writer.finalize().context("finalizing pcm16 WAV")?;

    Ok((out, spec.sample_rate))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reencodes_float_to_pcm16_preserving_layout() {
        // Write a tiny 2-channel float WAV, re-encode, read it back.
        let src = std::env::temp_dir().join("corti-wav-test-src.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        {
            let mut w = hound::WavWriter::create(&src, spec).unwrap();
            // Two frames: [me, them] = [0.5, -0.5], [1.0, 0.0].
            for v in [0.5f32, -0.5, 1.0, 0.0] {
                w.write_sample(v).unwrap();
            }
            w.finalize().unwrap();
        }

        let (out, rate) = to_pcm16_temp(&src).unwrap();
        assert_eq!(rate, 48_000);

        let mut r = hound::WavReader::open(&out).unwrap();
        let got = r.spec();
        assert_eq!(got.channels, 2);
        assert_eq!(got.sample_rate, 48_000);
        assert_eq!(got.bits_per_sample, 16);
        assert_eq!(got.sample_format, hound::SampleFormat::Int);

        let samples: Vec<i16> = r.samples::<i16>().collect::<Result<_, _>>().unwrap();
        assert_eq!(samples.len(), 4);
        // 0.5 * 32767 ≈ 16384; full-scale 1.0 → 32767; 0.0 → 0.
        assert!((samples[0] - 16384).abs() <= 1);
        assert!((samples[1] + 16384).abs() <= 1);
        assert_eq!(samples[2], i16::MAX);
        assert_eq!(samples[3], 0);

        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&out);
    }
}
