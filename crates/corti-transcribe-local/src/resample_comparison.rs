//! Test-only comparison of the always-copy wrapper and a borrowed-full-packet alternative.
//!
//! Both retain the same native filter, 512-source-frame packet phase, and final flush schedule.
//! The alternative is deliberately not a production path: exact native results and timings on the
//! pinned target are prerequisites for considering it. No ASR model or recorded audio is needed.

use std::hint::black_box;
use std::time::Instant;

use sherpa_onnx::LinearResampler;

use super::{PACKET_FRAMES, Resampler};
use crate::engine::TARGET_RATE;

const RATES: [i32; 5] = [8_000, 44_100, 48_000, 96_000, 192_000];
const CORPUS_FRAMES: usize = 16 * PACKET_FRAMES;

#[derive(Clone, Copy)]
struct Chunking {
    label: &'static str,
    sizes: &'static [usize],
}

const CHUNKINGS: [Chunking; 6] = [
    Chunking {
        label: "single",
        sizes: &[1],
    },
    Chunking {
        label: "irregular",
        sizes: &[377, 512, 100, 999, 1, 4096],
    },
    Chunking {
        label: "511",
        sizes: &[511],
    },
    Chunking {
        label: "512",
        sizes: &[512],
    },
    Chunking {
        label: "513",
        sizes: &[513],
    },
    Chunking {
        label: "whole",
        sizes: &[usize::MAX],
    },
];

struct BorrowedPackets {
    native: LinearResampler,
    packet: [f32; PACKET_FRAMES],
    filled: usize,
}

trait PacketResampler: Sized {
    fn new(rate: i32) -> Option<Self>;
    fn push(&mut self, input: &[f32]) -> Vec<f32>;
    fn finish(&mut self) -> Vec<f32>;
    fn staging(&self) -> (usize, usize);
}

impl PacketResampler for Resampler {
    fn new(rate: i32) -> Option<Self> {
        Resampler::new(rate)
    }

    fn push(&mut self, input: &[f32]) -> Vec<f32> {
        Resampler::push(self, input)
    }

    fn finish(&mut self) -> Vec<f32> {
        Resampler::finish(self)
    }

    fn staging(&self) -> (usize, usize) {
        (self.filled, self.packet.len())
    }
}

impl PacketResampler for BorrowedPackets {
    fn new(rate: i32) -> Option<Self> {
        Some(Self {
            native: LinearResampler::create(rate, TARGET_RATE)?,
            packet: [0.0; PACKET_FRAMES],
            filled: 0,
        })
    }

    fn push(&mut self, mut input: &[f32]) -> Vec<f32> {
        let mut output = Vec::new();
        if self.filled != 0 {
            let copied = input.len().min(PACKET_FRAMES - self.filled);
            self.packet[self.filled..self.filled + copied].copy_from_slice(&input[..copied]);
            self.filled += copied;
            input = &input[copied..];
            if self.filled == PACKET_FRAMES {
                output.extend(self.native.resample(&self.packet, false));
                self.filled = 0;
            }
        }
        while input.len() >= PACKET_FRAMES {
            let (packet, rest) = input.split_at(PACKET_FRAMES);
            output.extend(self.native.resample(packet, false));
            input = rest;
        }
        if !input.is_empty() {
            self.packet[..input.len()].copy_from_slice(input);
            self.filled = input.len();
        }
        output
    }

    fn finish(&mut self) -> Vec<f32> {
        let mut output = Vec::new();
        if self.filled != 0 {
            output.extend(self.native.resample(&self.packet[..self.filled], false));
            self.filled = 0;
        }
        // The pinned native resampler resets its history when an empty call flushes the epoch.
        output.extend(self.native.resample(&[], true));
        output
    }

    fn staging(&self) -> (usize, usize) {
        (self.filled, self.packet.len())
    }
}

fn generated_pcm(len: usize) -> Vec<f32> {
    let mut state = 0x53a9_1b27_u32;
    (0..len)
        .map(|index| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            // Dyadic values avoid platform-dependent transcendental fixture generation.
            let noise = ((state >> 9) as i32 - 4_194_304) as f32 / 8_388_608.0;
            match index % 1024 {
                0 => 0.75,
                511 => -0.75,
                512 => 0.0,
                1..=15 => 0.0,
                _ => noise,
            }
        })
        .collect()
}

fn native_reference(input: &[f32], rate: i32) -> Vec<f32> {
    let native = LinearResampler::create(rate, TARGET_RATE).expect("native reference resampler");
    let mut output = Vec::new();
    // Independent of both wrappers: chunks includes the final partial packet, always non-flushing.
    for packet in input.chunks(PACKET_FRAMES) {
        output.extend(native.resample(packet, false));
    }
    output.extend(native.resample(&[], true));
    let clock_frames = (input.len() as u64 * TARGET_RATE as u64).div_ceil(rate as u64);
    assert_eq!(
        output.len() as u64,
        clock_frames,
        "native output clock: rate={rate} source_frames={}",
        input.len()
    );
    assert!(output.iter().all(|sample| sample.is_finite()));
    output
}

fn assert_exact(actual: &[f32], expected: &[f32], context: &str) {
    assert_eq!(actual.len(), expected.len(), "output length: {context}");
    assert!(
        actual
            .iter()
            .chain(expected)
            .all(|sample| sample.is_finite()),
        "non-finite PCM: {context}"
    );
    let mismatch = actual
        .iter()
        .zip(expected)
        .position(|(a, b)| a.to_bits() != b.to_bits());
    assert!(mismatch.is_none(), "sample bits at {mismatch:?}: {context}");
}

fn checked_epoch<R: PacketResampler>(
    resampler: &mut R,
    input: &[f32],
    chunking: Chunking,
) -> Vec<f32> {
    let mut output = Vec::new();
    let mut consumed = 0;
    let mut chunks = chunking.sizes.iter().cycle();
    assert!(resampler.push(&[]).is_empty());
    while consumed < input.len() {
        let count = (*chunks.next().unwrap()).min(input.len() - consumed);
        output.extend(resampler.push(&input[consumed..consumed + count]));
        consumed += count;
        assert_eq!(
            resampler.staging(),
            (consumed % PACKET_FRAMES, PACKET_FRAMES)
        );
        assert!(resampler.push(&[]).is_empty());
    }
    output.extend(resampler.finish());
    assert_eq!(resampler.staging(), (0, PACKET_FRAMES));
    assert!(resampler.finish().is_empty(), "repeat finish must be empty");
    output
}

#[test]
fn borrowed_packets_match_native_and_production_across_chunks_tails_and_offsets() {
    for rate in RATES {
        for tail in [0, 1, PACKET_FRAMES - 1] {
            let input = generated_pcm(CORPUS_FRAMES + tail);
            let expected = native_reference(&input, rate);
            assert_exact(
                &crate::engine::resample_to_16k(&input, rate).expect("batch resampler"),
                &expected,
                &format!("batch rate={rate} tail={tail}"),
            );
            // Keep one allocation and shift the same PCM by 0..8 floats (0..32 bytes).
            let mut padded = vec![0.0; input.len() + 8];
            for offset in 0..=8 {
                padded[offset..offset + input.len()].copy_from_slice(&input);
                let shifted = &padded[offset..offset + input.len()];
                assert_exact(
                    &native_reference(shifted, rate),
                    &expected,
                    &format!("raw native rate={rate} tail={tail} offset={offset}"),
                );
                for chunking in CHUNKINGS {
                    let mut production = Resampler::new(rate).expect("production resampler");
                    let mut borrowed = BorrowedPackets::new(rate).expect("borrowed resampler");
                    // Repeat on the same instances to verify native and packet-phase reset.
                    for epoch in 0..2 {
                        let context = format!(
                            "rate={rate} tail={tail} offset={offset} chunks={} epoch={epoch}",
                            chunking.label
                        );
                        assert_exact(
                            &checked_epoch(&mut production, shifted, chunking),
                            &expected,
                            &format!("production {context}"),
                        );
                        assert_exact(
                            &checked_epoch(&mut borrowed, shifted, chunking),
                            &expected,
                            &format!("borrowed {context}"),
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn empty_and_partial_only_epochs_match_native_reference() {
    for rate in RATES {
        let mut production = Resampler::new(rate).expect("production resampler");
        let mut borrowed = BorrowedPackets::new(rate).expect("borrowed resampler");
        for len in [0, 1, PACKET_FRAMES - 1, 0] {
            let input = generated_pcm(len);
            let expected = native_reference(&input, rate);
            for chunking in [CHUNKINGS[0], CHUNKINGS[5]] {
                let context = format!("rate={rate} len={len} chunks={}", chunking.label);
                assert_exact(
                    &checked_epoch(&mut production, &input, chunking),
                    &expected,
                    &format!("production {context}"),
                );
                assert_exact(
                    &checked_epoch(&mut borrowed, &input, chunking),
                    &expected,
                    &format!("borrowed {context}"),
                );
            }
        }
    }
}

fn run_epoch<R: PacketResampler>(resampler: &mut R, input: &[f32], chunking: Chunking) -> Vec<f32> {
    let mut output = Vec::new();
    let mut consumed = 0;
    let mut chunks = chunking.sizes.iter().cycle();
    while consumed < input.len() {
        let count = (*chunks.next().unwrap()).min(input.len() - consumed);
        output.extend(resampler.push(&input[consumed..consumed + count]));
        consumed += count;
    }
    output.extend(resampler.finish());
    output
}

fn timed_epoch<R: PacketResampler>(
    reused: Option<&mut R>,
    input: &[f32],
    rate: i32,
    chunking: Chunking,
) -> (u128, Vec<f32>) {
    let start = Instant::now();
    let output = match reused {
        Some(resampler) => run_epoch(resampler, black_box(input), chunking),
        None => {
            // Includes construction and native destruction, plus the full returned-PCM path.
            let mut resampler = R::new(rate).expect("benchmark resampler");
            run_epoch(&mut resampler, black_box(input), chunking)
        }
    };
    black_box(&output);
    (start.elapsed().as_nanos(), output)
}

fn fixture_fnv1a64(input: &[f32]) -> u64 {
    // Identity metadata only; all correctness comparisons inspect every sample's bits.
    input.iter().fold(0xcbf2_9ce4_8422_2325, |hash, sample| {
        sample
            .to_bits()
            .to_le_bytes()
            .iter()
            .fold(hash, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
            })
    })
}

fn modeled_borrowed_copies(len: usize, chunking: Chunking) -> usize {
    let mut remaining = len;
    let mut filled = 0;
    let mut copied = 0;
    for requested in chunking.sizes.iter().cycle() {
        if remaining == 0 {
            break;
        }
        let mut count = (*requested).min(remaining);
        remaining -= count;
        if filled != 0 {
            let take = count.min(PACKET_FRAMES - filled);
            copied += take;
            count -= take;
            filled = (filled + take) % PACKET_FRAMES;
        }
        let tail = count % PACKET_FRAMES;
        copied += tail;
        filled += tail;
    }
    copied
}

fn distribution(samples: &[u128]) -> (u128, u128, u128) {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    (
        sorted[0],
        sorted[sorted.len() / 2],
        sorted[(sorted.len() * 9).div_ceil(10) - 1],
    )
}

/// Run alone on the pinned native target with `--ignored --nocapture --test-threads=1`.
/// This is a fixed, model-free microbenchmark, not an ASR latency or RSS measurement. Returned PCM
/// allocations, native calls and native allocations remain in both timed paths. Only staging capacity
/// is directly observed; operation/copy counts below are derived from the packet schedule.
#[test]
#[ignore = "bounded native benchmark; run separately and preserve JSON output"]
fn benchmark_production_against_borrowed_packets() {
    const RUNS: usize = 11;
    let input = generated_pcm(CORPUS_FRAMES + PACKET_FRAMES - 1);
    let fixture_hash = fixture_fnv1a64(&input);
    for rate in RATES {
        let expected = native_reference(&input, rate);
        for chunking in CHUNKINGS {
            for cold in [true, false] {
                let mut production = Resampler::new(rate).expect("production benchmark resampler");
                let mut borrowed =
                    BorrowedPackets::new(rate).expect("borrowed benchmark resampler");
                // Equal unmeasured warm-up also establishes reuse semantics before collecting times.
                assert_exact(
                    &run_epoch(&mut production, &input, chunking),
                    &expected,
                    "warmup production",
                );
                assert_exact(
                    &run_epoch(&mut borrowed, &input, chunking),
                    &expected,
                    "warmup borrowed",
                );
                let mut production_times = Vec::with_capacity(RUNS);
                let mut borrowed_times = Vec::with_capacity(RUNS);
                for run in 0..RUNS {
                    let mut production_run = || {
                        timed_epoch(
                            if cold { None } else { Some(&mut production) },
                            &input,
                            rate,
                            chunking,
                        )
                    };
                    let mut borrowed_run = || {
                        timed_epoch(
                            if cold { None } else { Some(&mut borrowed) },
                            &input,
                            rate,
                            chunking,
                        )
                    };
                    let (production_result, borrowed_result) = if run.is_multiple_of(2) {
                        (production_run(), borrowed_run())
                    } else {
                        let borrowed_result = borrowed_run();
                        (production_run(), borrowed_result)
                    };
                    assert_exact(&production_result.1, &expected, "timed production");
                    assert_exact(&borrowed_result.1, &expected, "timed borrowed");
                    production_times.push(production_result.0);
                    borrowed_times.push(borrowed_result.0);
                }
                let (production_min, production_median, production_p90) =
                    distribution(&production_times);
                let (borrowed_min, borrowed_median, borrowed_p90) = distribution(&borrowed_times);
                let native_calls = input.len().div_ceil(PACKET_FRAMES) + 1;
                let borrowed_copies = modeled_borrowed_copies(input.len(), chunking);
                let lifecycle = if cold {
                    "cold_construction"
                } else {
                    "warm_reuse"
                };
                println!(
                    concat!(
                        "{{\"resample_comparison\":{{\"rate_hz\":{},\"source_frames\":{},",
                        "\"fixture_fnv1a64\":\"{:016x}\",\"caller_chunks\":\"{}\",",
                        "\"lifecycle\":\"{}\",\"runs\":{},\"output_frames\":{},",
                        "\"production_ns\":{{\"min\":{},\"median\":{},\"p90\":{},\"samples\":{:?}}},",
                        "\"borrowed_ns\":{{\"min\":{},\"median\":{},\"p90\":{},\"samples\":{:?}}},",
                        "\"modeled_per_epoch\":{{\"native_calls_each\":{},\"production_copied_frames\":{},",
                        "\"borrowed_copied_frames\":{}}},",
                        "\"observed_staging_capacity_frames\":{{\"production\":{},\"borrowed\":{}}},",
                        "\"rust_wrapper_bytes\":{{\"production\":{},\"borrowed\":{}}},",
                        "\"native_allocations_measured\":false,\"rss_measured\":false}}}}"
                    ),
                    rate,
                    input.len(),
                    fixture_hash,
                    chunking.label,
                    lifecycle,
                    RUNS,
                    expected.len(),
                    production_min,
                    production_median,
                    production_p90,
                    production_times,
                    borrowed_min,
                    borrowed_median,
                    borrowed_p90,
                    borrowed_times,
                    native_calls,
                    input.len(),
                    borrowed_copies,
                    production.staging().1,
                    borrowed.staging().1,
                    std::mem::size_of::<Resampler>(),
                    std::mem::size_of::<BorrowedPackets>(),
                );
            }
        }
    }
}
