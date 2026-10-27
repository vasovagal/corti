//! Portable synthetic throughput receipt for the frozen capture-rate normalizer.
//!
//! Run `cargo run -p corti-core --release --example capture_rate_bench -- --quick` (the default),
//! or use `--full` for nine repeats. JSONL goes to stdout. There is no timing threshold and this
//! does not measure the HAL, writer, AEC, disk, or a loaded real-time capture graph. The checksum
//! sink has a cost, especially on the identity path. This is not a speech/ASR quality benchmark.
//!
//! `fresh_constructor` includes construction; `warm_primed` feeds a 100 ms prefix outside timing
//! through the same instance. Both include finish and drop. Code paths are primed before measurement;
//! neither label claims cold process/cache behavior. Warm timing includes emitting any pending prefix
//! frames, counted explicitly below. Each pattern receives the same authored synthetic corpus.
//! Rotating/reversing run order reduces systematic order bias; it does not isolate a shared machine.
//! Process memory, when desired, is measured externally, e.g. `/usr/bin/time -l` on macOS.

use std::convert::Infallible;
use std::hint::black_box;
use std::time::Instant;

use corti_core::capture_normalizer::CaptureRateNormalizer;
use serde_json::{Value, json};

const IRREGULAR: [usize; 8] = [1, 137, 2, 997, 31, 4_096, 7, 509];
const PATTERNS: [&str; 3] = ["whole", "4096", "irregular"];

#[derive(Clone, Copy)]
struct Case {
    source: u32,
    retained: u32,
    frames: usize,
}

fn corpus(source: u32, frames: usize) -> Vec<f32> {
    let mut state = 0x46a9_2c13_u32;
    let mut pcm = Vec::with_capacity(frames * 2);
    for frame in 0..frames {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let noise = (f64::from(state >> 8) / 16_777_216.0 - 0.5) * 0.02;
        let t = frame as f64 / f64::from(source);
        let impulse = if frame % 997 == 0 { 0.08 } else { 0.0 };
        pcm.push((0.31 * (2.0 * std::f64::consts::PI * 437.0 * t).sin() + noise + impulse) as f32);
        pcm.push((0.23 * (2.0 * std::f64::consts::PI * 1_379.0 * t).sin() - noise) as f32);
    }
    pcm
}

#[derive(Default)]
struct Sink {
    frames: u64,
    checksum: u64,
}

impl Sink {
    #[inline]
    fn frame(&mut self, frame: &[f32], unavailable: bool) -> Result<(), Infallible> {
        let frame = black_box(frame);
        assert!(!unavailable);
        self.checksum = self.checksum.rotate_left(9) ^ u64::from(frame[0].to_bits());
        self.checksum = self.checksum.rotate_left(9) ^ u64::from(frame[1].to_bits());
        self.frames += 1;
        Ok(())
    }
}

fn feed(normalizer: &mut CaptureRateNormalizer, pcm: &[f32], pattern: usize, sink: &mut Sink) {
    let mut begin = 0;
    let mut chunk = 0;
    while begin < pcm.len() {
        let count = match pattern {
            0 => pcm.len() / 2,
            1 => 4_096,
            _ => IRREGULAR[chunk % IRREGULAR.len()],
        };
        let end = (begin + count * 2).min(pcm.len());
        normalizer
            .push(black_box(&pcm[begin..end]), |frame, unavailable| {
                sink.frame(frame, unavailable)
            })
            .unwrap();
        begin = end;
        chunk += 1;
    }
}

fn measure(
    case: Case,
    pcm: &[f32],
    pattern: usize,
    warm: bool,
    round: usize,
    order: usize,
) -> (Value, u64) {
    let mut sink = Sink::default();
    let mut prepared =
        warm.then(|| CaptureRateNormalizer::new(case.source, case.retained, 2).unwrap());
    let prefix_frames = if warm { case.source as usize / 10 } else { 0 };
    if let Some(normalizer) = &mut prepared {
        feed(
            normalizer,
            &pcm[pcm.len() - prefix_frames * 2..],
            1,
            &mut sink,
        );
    }
    let before_frames = sink.frames;
    let pending_at_start = prepared
        .as_ref()
        .map_or(0, CaptureRateNormalizer::pending_frames);
    let start = Instant::now();
    let mut normalizer = prepared
        .unwrap_or_else(|| CaptureRateNormalizer::new(case.source, case.retained, 2).unwrap());
    feed(&mut normalizer, pcm, pattern, &mut sink);
    normalizer
        .finish(|frame, unavailable| sink.frame(frame, unavailable))
        .unwrap();
    let source_frames = normalizer.source_frames();
    let retained_frames = normalizer.retained_frames();
    let remainder = normalizer.fractional_remainder();
    let radius = normalizer.lookahead_source_frames();
    let emitted = normalizer.emitted_frames();
    drop(normalizer);
    black_box((sink.frames, sink.checksum));
    let elapsed_ns = start.elapsed().as_nanos() as u64;
    assert_eq!(source_frames, (case.frames + prefix_frames) as u64);
    let numerator = u128::from(source_frames) * u128::from(case.retained);
    let expected = (numerator / u128::from(case.source)) as u64;
    assert_eq!(remainder, (numerator % u128::from(case.source)) as u64);
    assert_eq!(retained_frames, expected);
    assert_eq!(emitted, expected);
    assert_eq!(sink.frames, expected);
    let frames = sink.frames - before_frames;
    let duration = case.frames as f64 / f64::from(case.source);
    (
        json!({
            "schema_version":1,"type":"throughput","source_hz":case.source,"retained_hz":case.retained,
            "channels":2,"source_frames_timed":case.frames,"duration_seconds":duration,
            "pattern":PATTERNS[pattern],"lifecycle":if warm {"warm_primed"} else {"fresh_constructor"},
            "round":round,"order":order,"prefix_frames_untimed":prefix_frames,
            "pending_output_frames_at_start":pending_at_start,"output_frames_timed":frames,
            "output_frames_total":sink.frames,"elapsed_ns":elapsed_ns,
            "ns_per_output_frame":elapsed_ns as f64 / frames as f64,
            "realtime_factor":elapsed_ns as f64 / 1e9 / duration,
            "checksum":format!("{:016x}",sink.checksum),"fractional_remainder":remainder,
            "lookahead_source_frames":radius
        }),
        sink.checksum,
    )
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let full = match args.next().as_deref() {
        None | Some("--quick") => false,
        Some("--full") => true,
        _ => return Err("usage: capture_rate_bench [--quick|--full]".into()),
    };
    if args.next().is_some() {
        return Err("usage: capture_rate_bench [--quick|--full]".into());
    }
    let repeats = if full { 9 } else { 3 };
    let cases = [
        (44_100, 48_000),
        (48_000, 44_100),
        (48_000, 48_000),
        (192_000, 8_000),
        (8_000, 192_000),
    ]
    .map(|(source, retained)| Case {
        source,
        retained,
        frames: if source == 192_000 || retained == 192_000 {
            source as usize / if full { 2 } else { 4 }
        } else {
            source as usize * if full { 5 } else { 2 }
        },
    });
    let corpora: Vec<_> = cases
        .iter()
        .map(|case| corpus(case.source, case.frames))
        .collect();
    println!(
        "{}",
        json!({
            "schema_version":1,"type":"metadata","mode":if full {"full"} else {"quick"},
            "repeats":repeats,"arch":std::env::consts::ARCH,"os":std::env::consts::OS,
            "channels":2,"irregular_frames":IRREGULAR,
            "warm_definition":"100 ms prefix through same instance outside timing; fresh includes new; both include finish and drop; no cold-cache claim",
            "sink":"nonallocating black_box plus two bitwise checksum steps per stereo frame"
        })
    );
    for (case, pcm) in cases.iter().zip(&corpora) {
        black_box(measure(*case, pcm, 2, true, usize::MAX, 0));
    }
    let combinations: Vec<_> = (0..cases.len())
        .flat_map(|case| {
            (0..PATTERNS.len())
                .flat_map(move |pattern| [false, true].map(move |warm| (case, pattern, warm)))
        })
        .collect();
    let mut checksums = [[None; 2]; 5];
    for round in 0..repeats {
        for order in 0..combinations.len() {
            let rotated = (order + round * 7) % combinations.len();
            let index = if round % 2 == 0 {
                rotated
            } else {
                combinations.len() - 1 - rotated
            };
            let (case, pattern, warm) = combinations[index];
            let (receipt, checksum) =
                measure(cases[case], &corpora[case], pattern, warm, round, order);
            let expected = checksums[case][usize::from(warm)].get_or_insert(checksum);
            assert_eq!(
                *expected, checksum,
                "output checksum changed across chunks/repeats"
            );
            println!("{receipt}");
        }
    }
    println!(
        "{}",
        json!({
            "schema_version":1,"type":"verified","measurements":repeats*combinations.len(),
            "exact_counts":true,"chunk_and_repeat_checksums_match":true
        })
    );
    Ok(())
}
