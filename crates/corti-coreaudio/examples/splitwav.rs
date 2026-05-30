//! Split an interleaved multichannel WAV into one mono WAV per channel, so you can audition each
//! (e.g. the spike's ch0 = mic, ch1/ch2 = system tap) with macOS's built-in `afplay`.
//!
//! ```sh
//! cargo run -p corti-coreaudio --example splitwav -- /tmp/corti-spike.wav
//! afplay /tmp/corti-spike.ch0.wav   # mic
//! afplay /tmp/corti-spike.ch1.wav   # system L
//! afplay /tmp/corti-spike.ch2.wav   # system R
//! ```

use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let input = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/corti-spike.wav".to_string());
    let input = PathBuf::from(input);

    let mut reader = hound::WavReader::open(&input)?;
    let spec = reader.spec();
    let ch = spec.channels as usize;
    println!(
        "{}: {ch} ch, {} Hz, {:?} {}-bit",
        input.display(),
        spec.sample_rate,
        spec.sample_format,
        spec.bits_per_sample
    );

    // Read all samples as f32 (the spike writes Float32; handle int too just in case).
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

    let mono_spec = hound::WavSpec {
        channels: 1,
        sample_rate: spec.sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };

    for c in 0..ch {
        let out = input.with_extension(format!("ch{c}.wav"));
        let mut w = hound::WavWriter::create(&out, mono_spec)?;
        // Channel c is every ch-th sample starting at offset c.
        let mut peak = 0.0f32;
        for frame in samples.chunks_exact(ch) {
            let v = frame[c];
            peak = peak.max(v.abs());
            w.write_sample(v)?;
        }
        w.finalize()?;
        println!("ch{c} -> {}   (peak {:.3})", out.display(), peak);
    }
    println!(
        "\naudition:  afplay {}",
        input.with_extension("ch0.wav").display()
    );
    Ok(())
}
