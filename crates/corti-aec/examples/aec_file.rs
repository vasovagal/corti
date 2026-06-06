//! Audition tool for offline AEC.
//!
//! ```text
//! cargo run -p corti-aec --example aec_file -- path/to/recording.wav
//! ```
//!
//! Reads a 2-track WAV (ch0 = mic "me", ch1 = far-end tap "them"), cancels the speaker bleed on ch0 using
//! ch1 as the reference, and writes `<stem>-clean.wav` (ch0 = cleaned mic, ch1 = raw tap). Prints the
//! wall-clock time so you can confirm it runs far faster than real time, then A/B the channels with
//! `afplay` (split with the `corti-coreaudio` `splitwav` example).

use std::path::PathBuf;
use std::time::Instant;

use corti_aec::{AecConfig, cancel};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arg = std::env::args()
        .nth(1)
        .ok_or("usage: aec_file <2-track.wav>")?;
    let input = PathBuf::from(arg);

    let mut reader = hound::WavReader::open(&input)?;
    let spec = reader.spec();
    if spec.channels != 2 {
        return Err(format!("expected a 2-channel WAV, got {} channel(s)", spec.channels).into());
    }

    // Read interleaved samples as f32 (tolerate Float and Int recordings).
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / max))
                .collect::<Result<_, _>>()?
        }
    };
    let mic: Vec<f32> = samples.iter().step_by(2).copied().collect();
    let tap: Vec<f32> = samples.iter().skip(1).step_by(2).copied().collect();

    let started = Instant::now();
    let clean = cancel(&mic, &tap, spec.sample_rate, &AecConfig::default());
    let elapsed = started.elapsed();

    let dur = mic.len() as f32 / spec.sample_rate as f32;
    println!(
        "AEC: {} frames ({dur:.1}s @ {} Hz) cleaned in {:.3}s ({:.0}× real-time)",
        mic.len(),
        spec.sample_rate,
        elapsed.as_secs_f32(),
        dur / elapsed.as_secs_f32().max(1e-6),
    );

    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "audio".to_string());
    let out = input.with_file_name(format!("{stem}-clean.wav"));
    let out_spec = hound::WavSpec {
        channels: 2,
        sample_rate: spec.sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(&out, out_spec)?;
    let frames = mic.len().max(tap.len());
    for i in 0..frames {
        writer.write_sample(clean.get(i).copied().unwrap_or(0.0))?; // ch0 = cleaned mic
        writer.write_sample(tap.get(i).copied().unwrap_or(0.0))?; // ch1 = raw tap
    }
    writer.finalize()?;
    println!("wrote {}", out.display());
    Ok(())
}
