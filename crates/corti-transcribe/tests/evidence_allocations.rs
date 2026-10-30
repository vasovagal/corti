//! One integration test process measures actual allocator-facing bytes for identical synthetic rows.
//! This is not RSS: allocator bookkeeping and pages retained by the allocator are excluded.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use corti_core::{Speaker, TranscriptSegment};
use corti_transcribe::evidence::*;
use corti_transcribe::segment::{self, CleanupConfig};

struct CountAlloc;
static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
fn acquired(bytes: usize) {
    let current = CURRENT.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(current, Ordering::Relaxed);
}
// SAFETY: every allocation is delegated unchanged to System; counters neither allocate nor touch
// the returned storage. dealloc/realloc receive the original caller's pointer and layout.
unsafe impl GlobalAlloc for CountAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            acquired(layout.size());
        }
        pointer
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            acquired(layout.size());
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) };
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let replacement = unsafe { System.realloc(pointer, layout, size) };
        if !replacement.is_null() {
            CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
            acquired(size);
        }
        replacement
    }
}
#[global_allocator]
static ALLOCATOR: CountAlloc = CountAlloc;

fn measure<T>(run: impl FnOnce() -> T) -> (T, usize, u128) {
    let start = CURRENT.load(Ordering::Relaxed);
    PEAK.store(start, Ordering::Relaxed);
    let time = Instant::now();
    let value = run();
    let elapsed = time.elapsed().as_micros();
    (
        value,
        PEAK.load(Ordering::Relaxed).saturating_sub(start),
        elapsed,
    )
}

#[test]
fn actual_owned_and_temporary_allocations_stay_within_documented_envelopes() {
    for count in [64, 2048] {
        for repeat in 0..3 {
            let input: Vec<_> = (0..count)
                .map(|index| TranscriptSegment {
                    speaker: Speaker::Me,
                    start: index as f64,
                    end: index as f64 + 0.8,
                    text: "Um, Very very important.".into(),
                })
                .collect();
            let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
            let config = CleanupConfig {
                echo_drop: false,
                merge_gap_seconds: 0.0,
                drop_backchannels: false,
                strip_fillers: true,
                ..CleanupConfig::default()
            };
            let legacy_input = input.clone();
            let (legacy, legacy_peak, legacy_us) = measure(|| {
                segment::cleanup_with_protected_decisions(
                    legacy_input,
                    &config,
                    &[],
                    None,
                    None,
                    &[],
                )
            });
            let (result, cleanup_peak, cleanup_us) = measure(|| {
                ledger.cleanup(
                    RecordingIdentity {
                        window: 0,
                        kind: StageKind::TrueAsr,
                        identity: Identity {
                            configuration_hash: None,
                            model_hash: None,
                            runtime_hash: None,
                        },
                        signal: SignalIdentity {
                            source: SignalSource::OfflineInput,
                            aec: AecOrigin::None,
                            source_rate_hz: Some(48_000),
                            lookahead_frames: None,
                        },
                    },
                    input,
                    &config,
                    CleanupInput {
                        origins: &[],
                        source_links: None,
                        carry: &[],
                        carry_links: None,
                        protected_input_indices: &[],
                    },
                    None,
                    None,
                )
            });
            assert_eq!(result.evidence.unwrap().retained, Append::Retained);
            assert_eq!(result.segments.len(), count);
            assert_eq!(result.segments, legacy.segments);
            assert_eq!(result.stats, legacy.stats);
            let decisions: Vec<_> = ledger
                .groups()
                .iter()
                .flat_map(|g| &g.stages)
                .flat_map(|stage| {
                    stage
                        .decisions
                        .iter()
                        .map(move |decision| (stage, decision))
                })
                .collect();
            assert_eq!(decisions.len(), legacy.decisions.entries().len());
            for ((stage, actual), expected) in decisions.into_iter().zip(legacy.decisions.entries())
            {
                assert_eq!(
                    stage.kind,
                    StageKind::Cleanup {
                        pass: expected.stage
                    }
                );
                assert_eq!(actual.reason, expected.reason);
                assert_eq!(
                    actual.input_indices,
                    expected
                        .input_indices
                        .iter()
                        .map(|index| *index as u64)
                        .collect::<Vec<_>>()
                );
                for (ids, rows) in [
                    (&actual.originals, &expected.originals),
                    (&actual.replacements, &expected.replacements),
                ] {
                    let originals: Vec<_> = ids
                        .iter()
                        .map(|id| {
                            &ledger
                                .groups()
                                .iter()
                                .flat_map(|g| &g.rows)
                                .find(|row| row.id == *id)
                                .unwrap()
                                .segment
                        })
                        .collect();
                    assert_eq!(originals, rows.iter().collect::<Vec<_>>());
                }
            }
            let owned = ledger.owned_bytes();
            assert!(owned <= DEFAULT_OWNED_CAP);
            // The adapter's new captured owners, moved legacy decisions and stage/trace scratch are
            // bounded separately from the pre-existing caller pipeline's input/output allocations.
            assert!(cleanup_peak < 4 * DEFAULT_OWNED_CAP);
            let ((), write_peak, write_us) =
                measure(|| ledger.write_json(std::io::sink()).unwrap());
            assert!(
                write_peak < METADATA_RESERVE,
                "streaming must not duplicate transcript JSON"
            );
            let mut bytes = Vec::new();
            ledger.write_json(&mut bytes).unwrap();
            let (loaded, read_peak, read_us) =
                measure(|| Ledger::read_json(&bytes, DEFAULT_CAP).unwrap());
            assert!(loaded.owned_bytes() <= owned);
            assert!(read_peak < 16 * DEFAULT_OWNED_CAP + 4 * DEFAULT_CAP);
            let (_, reconstruct_peak, _) =
                measure(|| loaded.reconstruct_input(InvocationId(1)).unwrap());
            assert!(reconstruct_peak <= count * std::mem::size_of::<&Row>() * 2);
            println!(
                "evidence_allocation rows={count} repeat={repeat} wire={} retained_owned={owned} legacy_peak_delta={legacy_peak} cleanup_peak_delta={cleanup_peak} streaming_peak_delta={write_peak} read_peak_delta={read_peak} reconstruct_peak_delta={reconstruct_peak} legacy_us={legacy_us} cleanup_us={cleanup_us} streaming_us={write_us} read_us={read_us}",
                bytes.len()
            );
            let words: Vec<_> = (0..count)
                .map(|index| segment::Word {
                    start: index as f64 * 3.0,
                    end: index as f64 * 3.0 + 0.8,
                    text: "synthetic".into(),
                })
                .collect();
            let (plain, plain_peak, plain_us) =
                measure(|| segment::words_to_segments(&words, Speaker::Me, 1.5));
            let (traced, traced_peak, traced_us) =
                measure(|| segment::words_to_segments_with_ranges(&words, Speaker::Me, 1.5));
            assert_eq!(traced.segments, plain);
            let ranges = traced.source_ranges.unwrap();
            assert_eq!(ranges.len(), count);
            assert!(ranges.capacity() * std::mem::size_of::<ByteSpan>() <= 256 * 1024);
            assert!(traced_peak <= plain_peak + 2 * 256 * 1024);
            println!(
                "word_range_allocation rows={count} repeat={repeat} plain_peak_delta={plain_peak} traced_peak_delta={traced_peak} range_owned={} plain_us={plain_us} traced_us={traced_us}",
                ranges.capacity() * std::mem::size_of::<ByteSpan>()
            );
        }
    }
}
