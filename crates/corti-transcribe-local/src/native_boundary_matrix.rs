//! Synthetic-only native #155 matrix runner. The Python owner seals every input before this test.

use super::*;
use crate::asr::DecodedRegion;
use corti_transcribe::boundary::{
    BoundaryKind as PolicyKind, BoundaryPolicy, BoundaryStep, ForceReason, Observation,
    TrackBoundary,
};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

const CHUNKS: [usize; 7] = [511, 1, 577, 4096, 73, 512, 2003];
const MAX_EVIDENCE_ROWS: usize = 512;
type PendingBoundary = (
    [BoundaryToken; 2],
    String,
    u64,
    u64,
    bool,
    [u64; 2],
    u64,
    [u64; 2],
    [u64; 2],
);

struct Fixture {
    id: String,
    rate: u32,
    frames: usize,
    path: PathBuf,
}

struct Cell {
    id: String,
    waveform: String,
    rate: u32,
    window: u64,
    phase: u64,
    mode: String,
    epoch_limit: u64,
    explicit: Vec<u64>,
}

#[derive(Default)]
struct Audit {
    attempts: Vec<(u64, u64, u64)>,
    pcm_exact_and_once: bool,
}

struct AuditedDecoder {
    real: Arc<Asr>,
    canonical: Arc<Vec<f32>>,
    audit: Arc<Mutex<Audit>>,
}

impl RegionDecoder for AuditedDecoder {
    fn decode(&self, samples: &[f32], offset: f64) -> DecodedRegion {
        let position = offset * 16_000.0;
        let start = position.round() as u64;
        let end = start
            .checked_add(samples.len() as u64)
            .expect("attempt bound");
        let mut audit = self.audit.lock().unwrap();
        assert!(
            audit.attempts.len() < MAX_EVIDENCE_ROWS,
            "attempt evidence cap"
        );
        let exact = position.is_finite()
            && (position - start as f64).abs() < 0.000_001
            && self
                .canonical
                .get(start as usize..end as usize)
                .is_some_and(|expected| {
                    expected
                        .iter()
                        .zip(samples)
                        .all(|(a, b)| a.to_bits() == b.to_bits())
                });
        let once = audit
            .attempts
            .last()
            .is_none_or(|(_, previous_end, _)| start >= *previous_end);
        audit.pcm_exact_and_once &= exact && once;
        // Noncryptographic PCM diagnostic. Input WAVs and source files are independently SHA-256 sealed.
        let fingerprint = samples
            .iter()
            .flat_map(|sample| sample.to_bits().to_le_bytes())
            .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
            });
        audit.attempts.push((start, end, fingerprint));
        drop(audit);
        self.real.decode(samples, offset)
    }
}

struct Run {
    live: LiveTranscriber,
    audit: Arc<Mutex<Audit>>,
    words: Vec<Word>,
    outcomes: Vec<RegionOutcome>,
    omitted_failures: u64,
    omitted_metadata: u64,
    saturated: bool,
    inputs: Vec<(u64, u64, u32)>,
    ownership: Vec<(u64, u64, bool)>,
}

impl Run {
    fn new(engine: &LiveEngine, canonical: Arc<Vec<f32>>) -> Self {
        let audit = Arc::new(Mutex::new(Audit {
            pcm_exact_and_once: true,
            ..Audit::default()
        }));
        let mut live = engine.channel().expect("native channel");
        live.rec = Arc::new(AuditedDecoder {
            real: Arc::clone(&engine.rec),
            canonical,
            audit: Arc::clone(&audit),
        });
        Self {
            live,
            audit,
            words: Vec::new(),
            outcomes: Vec::new(),
            omitted_failures: 0,
            omitted_metadata: 0,
            saturated: false,
            inputs: Vec::new(),
            ownership: Vec::new(),
        }
    }

    fn drain(&mut self) {
        while let Some(words) = self.live.poll_words() {
            self.words.extend(words);
            assert!(self.words.len() <= MAX_EVIDENCE_ROWS, "word evidence cap");
        }
        let report = self.live.take_decode_report();
        self.omitted_failures += report.omitted_failures();
        self.omitted_metadata += report.omitted_metadata();
        self.saturated |= report.counter_saturated();
        self.outcomes.extend_from_slice(report.outcomes());
        assert!(
            self.outcomes.len() <= MAX_EVIDENCE_ROWS,
            "report evidence cap"
        );
    }

    fn write(&self, output: &mut String, role: &str, id: &str, track: &str) {
        for (start, end, rate) in &self.inputs {
            writeln!(output, "I\t{role}\t{id}\t{track}\t{start}\t{end}\t{rate}").unwrap();
        }
        for (start, end, eof) in &self.ownership {
            writeln!(output, "O\t{role}\t{id}\t{track}\t{start}\t{end}\t{eof}").unwrap();
        }
        for word in &self.words {
            let hex = word
                .text
                .as_bytes()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            writeln!(
                output,
                "W\t{role}\t{id}\t{track}\t{:.17}\t{:.17}\t{hex}",
                word.start, word.end
            )
            .unwrap();
        }
        let audit = self.audit.lock().unwrap();
        for (start, end, bits) in &audit.attempts {
            writeln!(
                output,
                "A\t{role}\t{id}\t{track}\t{start}\t{end}\t{bits:016x}"
            )
            .unwrap();
        }
        for outcome in &self.outcomes {
            writeln!(
                output,
                "Q\t{role}\t{id}\t{track}\t{}\t{}\t{:?}",
                outcome.span.start_frame, outcome.span.end_frame, outcome.status
            )
            .unwrap();
        }
        writeln!(
            output,
            "S\t{role}\t{id}\t{track}\t{}\t{}\t{}\t{}\t{}",
            self.live.decode_complete(),
            audit.pcm_exact_and_once,
            self.omitted_failures,
            self.omitted_metadata,
            self.saturated
        )
        .unwrap();
    }
}

fn read_audio(fixture: &Fixture) -> Vec<f32> {
    let mut reader = hound::WavReader::open(&fixture.path).expect("sealed fixture WAV");
    let spec = reader.spec();
    assert_eq!(
        (spec.channels, spec.bits_per_sample, spec.sample_rate),
        (1, 16, fixture.rate)
    );
    assert_eq!(spec.sample_format, hound::SampleFormat::Int);
    let samples: Vec<_> = reader
        .samples::<i16>()
        .map(|sample| f32::from(sample.unwrap()) / 32768.0)
        .collect();
    assert_eq!(samples.len(), fixture.frames);
    samples
}

fn plan(path: &Path) -> (BTreeMap<String, Fixture>, Vec<Cell>) {
    let contents = std::fs::read_to_string(path).expect("sealed native plan");
    let mut fixtures = BTreeMap::new();
    let mut cells = Vec::new();
    for line in contents.lines() {
        let parts: Vec<_> = line.split('\t').collect();
        match parts.as_slice() {
            ["F", id, rate, frames, path] => {
                let fixture = Fixture {
                    id: (*id).into(),
                    rate: rate.parse().unwrap(),
                    frames: frames.parse().unwrap(),
                    path: PathBuf::from(path),
                };
                assert!(fixtures.insert((*id).into(), fixture).is_none());
            }
            ["C", id, waveform, rate, window, phase, mode, epoch, forced] => cells.push(Cell {
                id: (*id).into(),
                waveform: (*waveform).into(),
                rate: rate.parse().unwrap(),
                window: window.parse().unwrap(),
                phase: phase.parse().unwrap(),
                mode: (*mode).into(),
                epoch_limit: epoch.parse().unwrap(),
                explicit: forced
                    .split(',')
                    .filter(|x| !x.is_empty())
                    .map(|value| value.parse().unwrap())
                    .collect(),
            }),
            _ => panic!("invalid sealed native plan row"),
        }
    }
    assert!(
        !fixtures.is_empty() && cells.len() == 13,
        "incomplete native plan"
    );
    (fixtures, cells)
}

fn controls(engine: &LiveEngine, fixture: &Fixture, output: &mut String) {
    let audio = read_audio(fixture);
    let canonical = Arc::new(
        engine::resample_to_16k(&audio, fixture.rate as i32).expect("reference resampler"),
    );
    let mut batch = Run::new(engine, Arc::clone(&canonical));
    batch
        .live
        .push_resampled(&canonical, audio.len(), fixture.rate);
    batch.inputs.push((0, audio.len() as u64, fixture.rate));
    batch.ownership.push((0, audio.len() as u64, true));
    batch.words.extend(batch.live.finish());
    batch.drain();
    batch.write(output, "batch", &fixture.id, "-");
    let mut uncut = Run::new(engine, canonical);
    let mut position = 0;
    for chunk in CHUNKS.into_iter().cycle() {
        if position == audio.len() {
            break;
        }
        let end = (position + chunk).min(audio.len());
        uncut.live.push(&audio[position..end], fixture.rate);
        uncut
            .inputs
            .push((position as u64, end as u64, fixture.rate));
        uncut.drain();
        position = end;
    }
    uncut.ownership.push((0, audio.len() as u64, true));
    uncut.words.extend(uncut.live.finish());
    uncut.drain();
    uncut.write(output, "uncut", &fixture.id, "-");
}

fn run_cell(
    engine: &LiveEngine,
    fixtures: &BTreeMap<String, Fixture>,
    cell: &Cell,
    output: &mut String,
) {
    let tracks = ["mic", "tap"];
    let audio: [Vec<f32>; 2] = std::array::from_fn(|i| {
        read_audio(&fixtures[&format!("{}-{}-{}", cell.waveform, cell.rate, tracks[i])])
    });
    let mut runs: [Run; 2] = std::array::from_fn(|i| {
        Run::new(
            engine,
            Arc::new(
                engine::resample_to_16k(&audio[i], cell.rate as i32).expect("reference resampler"),
            ),
        )
    });
    let make_policy = |start, nominal| {
        BoundaryPolicy::new(start, nominal, cell.rate, Duration::from_secs(1)).unwrap()
    };
    let mut policy = make_policy(0, cell.window - cell.phase);
    let bounds = boundary_resampler_bounds(cell.rate).unwrap();
    let capacity = bounds
        .rolling_capacity(
            0,
            4096,
            4096,
            make_policy(0, cell.window).maximum_owned_frames(),
        )
        .unwrap();
    assert!(
        cell.epoch_limit >= capacity.minimum_epoch_samples,
        "impossible sealed epoch capacity"
    );
    for run in &mut runs {
        run.live
            .enable_boundaries(BoundaryLimits {
                max_retained_samples: capacity.retained_samples,
                max_epoch_samples: cell.epoch_limit,
                max_source_push: 4096,
            })
            .unwrap();
    }
    let total = audio.iter().map(Vec::len).max().unwrap() as u64;
    let mut received = 0_u64;
    let mut owned_start = 0_u64;
    let mut pending: Option<PendingBoundary> = None;
    let mut explicit_index = 0;
    let mut chunk_index = 0;
    let mut boundary_count = 0;
    writeln!(
        output,
        "R\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t511,1,577,4096,73,512,2003\t{}\t{}",
        cell.id,
        cell.rate,
        cell.window,
        cell.phase,
        cell.epoch_limit,
        capacity.settlement_source_reserve,
        capacity.settlement_accept,
        capacity.next_accept,
        capacity.maintenance_trigger,
        capacity.minimum_epoch_samples,
        engine.vad_threshold,
        engine.vad_min_silence
    )
    .unwrap();
    let completed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        loop {
            let ended = received == total;
            if let Some((
                tokens,
                kind,
                due,
                cut,
                maintenance,
                epoch_remaining,
                selected,
                accepted,
                origins,
            )) = pending.as_ref()
            {
                let mut settled = true;
                for (run, token) in runs.iter_mut().zip(tokens) {
                    match run.live.advance_boundary(if ended {
                        BoundaryInput::SourceEnded
                    } else {
                        BoundaryInput::Continue
                    }) {
                        BoundaryProgress::OldSettled(actual) => assert_eq!(actual, *token),
                        BoundaryProgress::Waiting => settled = false,
                        other => panic!("native boundary did not settle: {other:?}"),
                    }
                    run.drain();
                }
                if settled {
                    writeln!(
                        output,
                        "B\t{}\t{kind}\t{due}\t{cut}\t{received}\t{maintenance}\t{}\t{}\t{selected}\t{}\t{}\t{}\t{}\t{}\t{}",
                        cell.id, epoch_remaining[0], epoch_remaining[1],
                        accepted[0], accepted[1], origins[0], origins[1], runs[0].live.win.fed, runs[1].live.win.fed
                    )
                    .unwrap();
                    // All old words/reports are drained before the first acknowledgement can replay.
                    for (run, token) in runs.iter_mut().zip(tokens) {
                        run.live.acknowledge_boundary(*token).unwrap();
                    }
                    for run in &mut runs {
                        run.drain();
                    }
                    for (i, run) in runs.iter_mut().enumerate() {
                        let end = (*cut).min(audio[i].len() as u64);
                        if end > owned_start {
                            run.ownership.push((owned_start, end, false));
                        }
                    }
                    owned_start = *cut;
                    policy = make_policy(owned_start, cell.window);
                    pending = None;
                    boundary_count += 1;
                    assert!(boundary_count <= 256, "boundary evidence cap");
                    continue;
                }
            } else if ended {
                for run in &mut runs {
                    assert_eq!(
                        run.live.advance_boundary(BoundaryInput::SourceEnded),
                        BoundaryProgress::Ended
                    );
                    run.drain();
                }
                for (i, run) in runs.iter_mut().enumerate() {
                    let end = audio[i].len() as u64;
                    if end > owned_start {
                        run.ownership.push((owned_start, end, true));
                    } else if let Some(last) = run.ownership.last_mut() {
                        assert_eq!(last.1, end);
                        last.2 = true;
                    }
                }
                break;
            } else {
                let snapshots =
                    std::array::from_fn::<_, 2, _>(|i| runs[i].live.boundary_snapshot().unwrap());
                let maintenance = snapshots
                    .iter()
                    .any(|snapshot| snapshot.epoch_remaining <= capacity.maintenance_trigger);
                let track_observations = std::array::from_fn(|i| TrackBoundary::Established {
                    start_frame: owned_start.min(snapshots[i].delivered_through),
                    delivered_through: snapshots[i].delivered_through,
                    natural: snapshots[i].natural,
                });
                let selected = if cell.mode == "explicit-forced-probe" {
                    cell.explicit
                        .get(explicit_index)
                        .copied()
                        .filter(|cut| *cut == received)
                        .map(|cut| {
                            (
                                cut,
                                "explicit".to_string(),
                                cell.window,
                                false,
                                BoundaryKind::Forced,
                            )
                        })
                } else {
                    match policy
                        .observe(Observation {
                            received_through: received,
                            tracks: track_observations,
                            text_full: false,
                            maintenance_armed: maintenance,
                        })
                        .unwrap()
                    {
                        BoundaryStep::Selected {
                            fence,
                            reset_for_maintenance,
                            ..
                        } => {
                            let (kind, native_kind) = match fence.kind {
                                PolicyKind::Natural => (
                                    "natural",
                                    if reset_for_maintenance {
                                        BoundaryKind::NaturalMaintenance
                                    } else {
                                        BoundaryKind::Natural
                                    },
                                ),
                                PolicyKind::Forced(ForceReason::Deadline) => {
                                    ("deadline", BoundaryKind::Forced)
                                }
                                PolicyKind::Forced(ForceReason::TextCap) => {
                                    ("text_cap", BoundaryKind::Forced)
                                }
                            };
                            Some((
                                fence.frame,
                                kind.to_string(),
                                policy.due_frame().unwrap(),
                                reset_for_maintenance,
                                native_kind,
                            ))
                        }
                        BoundaryStep::Admit { .. } => None,
                    }
                };
                if let Some((cut, kind, due, reset, native_kind)) = selected {
                    let tokens = std::array::from_fn(|i| {
                        runs[i].live.latch_boundary(cut, native_kind).unwrap()
                    });
                    pending = Some((
                        tokens,
                        kind,
                        due,
                        cut,
                        reset,
                        [snapshots[0].epoch_remaining, snapshots[1].epoch_remaining],
                        received,
                        [runs[0].live.win.fed, runs[1].live.win.fed],
                        [runs[0].live.vad_base_samples, runs[1].live.vad_base_samples],
                    ));
                    explicit_index += usize::from(cell.mode == "explicit-forced-probe");
                    continue;
                }
            }
            let mut count = CHUNKS[chunk_index % CHUNKS.len()] as u64;
            chunk_index += 1;
            if let Some((_, _, _, cut, _, _, _, _, _)) = pending.as_ref() {
                count = count.min(
                    cut.checked_add(capacity.settlement_source_reserve)
                        .unwrap()
                        .checked_sub(received)
                        .expect("settlement reserve exhausted"),
                );
            } else if cell.mode == "explicit-forced-probe" {
                if let Some(cut) = cell.explicit.get(explicit_index) {
                    count = count.min(cut - received);
                }
            } else {
                let snapshots =
                    std::array::from_fn::<_, 2, _>(|i| runs[i].live.boundary_snapshot().unwrap());
                let step = policy
                    .observe(Observation {
                        received_through: received,
                        tracks: std::array::from_fn(|i| TrackBoundary::Established {
                            start_frame: owned_start.min(snapshots[i].delivered_through),
                            delivered_through: snapshots[i].delivered_through,
                            natural: snapshots[i].natural,
                        }),
                        text_full: false,
                        maintenance_armed: snapshots
                            .iter()
                            .any(|s| s.epoch_remaining <= capacity.maintenance_trigger),
                    })
                    .unwrap();
                let BoundaryStep::Admit { frames } = step else {
                    panic!("unconsumed selected boundary")
                };
                count = count.min(frames);
            }
            count = count.min(total - received);
            assert!(count > 0, "native matrix made no progress");
            for (i, run) in runs.iter_mut().enumerate() {
                let start = (received as usize).min(audio[i].len());
                let end = ((received + count) as usize).min(audio[i].len());
                if end > start {
                    run.live.push(&audio[i][start..end], cell.rate);
                    run.inputs.push((start as u64, end as u64, cell.rate));
                    assert!(run.inputs.len() <= 16_384, "source admission evidence cap");
                }
                run.drain();
            }
            received += count;
        }
    }));
    for (track, run) in tracks.into_iter().zip(runs) {
        run.write(output, "cell", &cell.id, track);
    }
    if completed.is_err() {
        writeln!(output, "E\t{}", cell.id).unwrap();
    }
}

#[test]
#[ignore = "requires hash-verified native models and the sealed synthetic boundary matrix"]
fn native_boundary_frozen_matrix() {
    let plan_path = PathBuf::from(std::env::var("CORTI_BOUNDARY_PLAN").expect("sealed plan path"));
    let output_path =
        PathBuf::from(std::env::var("CORTI_BOUNDARY_OUTPUT").expect("native evidence path"));
    let model_dir =
        PathBuf::from(std::env::var("CORTI_VERIFY_MODEL_DIR").expect("verified model path"));
    let (fixtures, cells) = plan(&plan_path);
    let config = crate::LocalConfig {
        model_dir: Some(model_dir),
        ..crate::LocalConfig::default()
    };
    let engine = crate::LocalTranscriber::new(config)
        .live_engine()
        .expect("pinned Sherpa engine");
    let mut output = String::new();
    for fixture in fixtures.values() {
        controls(&engine, fixture, &mut output);
    }
    std::fs::write(&output_path, &output).unwrap();
    for cell in &cells {
        run_cell(&engine, &fixtures, cell, &mut output);
        std::fs::write(&output_path, &output).unwrap();
    }
}
