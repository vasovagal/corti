//! Audition tool for offline AEC.
//!
//! ```text
//! cargo run -p corti-aec --example aec_file -- path/to/recording.wav
//! cargo run -p corti-aec --example aec_file -- --filter-len 8192 --mu 0.5 path/to/recording.wav
//! ```
//!
//! Reads a 2-track WAV (ch0 = mic "me", ch1 = far-end tap "them"), cancels the speaker bleed on ch0 using
//! ch1 as the reference, and writes `<stem>-clean.wav` (ch0 = cleaned mic, ch1 = raw tap). Prints the
//! wall-clock time so you can confirm it runs far faster than real time, then A/B the channels with
//! `afplay` (split with the `corti-coreaudio` `splitwav` example).
//!
//! Pass `--score` to also print echo suppression / near-end preservation metrics.

use std::path::PathBuf;
use std::time::Instant;

use corti_aec::{AecConfig, cancel};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cfg, score, input) = parse_args(&args)?;

    let mut reader = hound::WavReader::open(&input)?;
    let spec = reader.spec();
    if spec.channels != 2 {
        return Err(format!("expected a 2-channel WAV, got {} channel(s)", spec.channels).into());
    }

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

    println!(
        "config: filter_len={} mu={} eps={:.0e} power_smoothing={} double_talk_ratio={} passes={} suppress_residual={}",
        cfg.filter_len,
        cfg.mu,
        cfg.eps,
        cfg.power_smoothing,
        cfg.double_talk_ratio,
        cfg.passes,
        cfg.suppress_residual,
    );

    let started = Instant::now();
    let clean = cancel(&mic, &tap, spec.sample_rate, &cfg);
    let elapsed = started.elapsed();

    let dur = mic.len() as f32 / spec.sample_rate as f32;
    println!(
        "AEC: {} frames ({dur:.1}s @ {} Hz) cleaned in {:.3}s ({:.0}× real-time)",
        mic.len(),
        spec.sample_rate,
        elapsed.as_secs_f32(),
        dur / elapsed.as_secs_f32().max(1e-6),
    );

    if score {
        let s = corti_aec::score::score(&mic, &tap, &clean, spec.sample_rate);
        println!(
            "score: echo_suppression={:.3} near_end_preservation={:.3} combined={:.3}",
            s.echo_suppression, s.near_end_preservation, s.combined,
        );
    }

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
        writer.write_sample(clean.get(i).copied().unwrap_or(0.0))?;
        writer.write_sample(tap.get(i).copied().unwrap_or(0.0))?;
    }
    writer.finalize()?;
    println!("wrote {}", out.display());
    Ok(())
}

fn parse_args(args: &[String]) -> Result<(AecConfig, bool, PathBuf), Box<dyn std::error::Error>> {
    let mut cfg = AecConfig::default();
    let mut score = false;
    let mut input: Option<PathBuf> = None;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--filter-len" => {
                i += 1;
                cfg.filter_len = args
                    .get(i)
                    .ok_or("--filter-len requires a value")?
                    .parse()?;
            }
            "--mu" => {
                i += 1;
                cfg.mu = args.get(i).ok_or("--mu requires a value")?.parse()?;
            }
            "--eps" => {
                i += 1;
                cfg.eps = args.get(i).ok_or("--eps requires a value")?.parse()?;
            }
            "--power-smoothing" => {
                i += 1;
                cfg.power_smoothing = args
                    .get(i)
                    .ok_or("--power-smoothing requires a value")?
                    .parse()?;
            }
            "--double-talk-ratio" => {
                i += 1;
                cfg.double_talk_ratio = args
                    .get(i)
                    .ok_or("--double-talk-ratio requires a value")?
                    .parse()?;
            }
            "--passes" => {
                i += 1;
                cfg.passes = args.get(i).ok_or("--passes requires a value")?.parse()?;
            }
            "--suppress-residual" => {
                i += 1;
                cfg.suppress_residual = args
                    .get(i)
                    .ok_or("--suppress-residual requires a value")?
                    .parse()?;
            }
            "--no-suppress" => {
                cfg.suppress_residual = 0.0;
            }
            "--score" => {
                score = true;
            }
            arg if arg.starts_with('-') => {
                return Err(format!("unknown flag: {arg}\n\nusage: aec_file [OPTIONS] <2-track.wav>\n\noptions:\n  --filter-len <N>       adaptive filter length (default {})\n  --mu <F>               step size (default {})\n  --eps <F>              regularization (default {:.0e})\n  --power-smoothing <F>  far-end power smoothing (default {})\n  --double-talk-ratio <F> double-talk threshold (default {})\n  --passes <N>           offline passes (default {})\n  --suppress-residual <F> residual echo suppression factor (default {}, 0=off)\n  --no-suppress          disable residual echo suppression\n  --score                print echo suppression / preservation metrics",
                    cfg.filter_len, cfg.mu, cfg.eps, cfg.power_smoothing, cfg.double_talk_ratio, cfg.passes, cfg.suppress_residual).into());
            }
            _ => {
                if input.is_some() {
                    return Err("only one input file allowed".into());
                }
                input = Some(PathBuf::from(&args[i]));
            }
        }
        i += 1;
    }

    let input = input.ok_or("usage: aec_file [OPTIONS] <2-track.wav>")?;
    Ok((cfg, score, input))
}
