//! EOF processing-coverage oracle. Original false-completion REDs remain archived unchanged.
use super::*;
use crate::live::terminal::PINNED_GUARD;

fn observed_fixture() -> (Fixture, Arc<Mutex<Probe>>) {
    let mut f = fixture(true);
    let probe = Arc::new(Mutex::new(Probe::default()));
    f.live.vad = Box::new(Counted {
        inner: Box::new(PendingSegmenter::new(probe.clone())),
        calls: f.calls.clone(),
    });
    (f, probe)
}

#[test]
fn terminal_four_original_eof_cases_process_real_tail_before_certifying() {
    for case in ["partial", "hidden", "natural", "maintenance", "forced"] {
        let (mut f, probe) = observed_fixture();
        let pcm = if case == "forced" {
            push(&mut f.live, &[1.0; 2000], 16_000);
            let token = f.live.latch_boundary(2000, BoundaryKind::Forced).unwrap();
            push(&mut f.live, &[1.0; 8031], 16_000);
            assert_eq!(
                f.live.advance_boundary(BoundaryInput::SourceEnded),
                BoundaryProgress::OldSettled(token)
            );
            assert_eq!(f.live.terminal.padding, 0);
            f.live.poll_words();
            f.live.acknowledge_boundary(token).unwrap();
            vec![1.0; 10031]
        } else {
            let mut pcm = vec![0.0; 8192];
            pcm.extend(vec![0.5; if case == "hidden" { 1024 } else { 31 }]);
            push(&mut f.live, &pcm, 16_000);
            if matches!(case, "natural" | "maintenance") {
                let kind = if case == "natural" {
                    BoundaryKind::Natural
                } else {
                    BoundaryKind::NaturalMaintenance
                };
                let token = f.live.latch_boundary(1000, kind).unwrap();
                assert_eq!(
                    f.live.advance_boundary(BoundaryInput::SourceEnded),
                    BoundaryProgress::OldSettled(token)
                );
                assert_eq!(f.live.terminal.padding, 0);
                f.live.acknowledge_boundary(token).unwrap();
            }
            pcm
        };
        let source_end = f.live.input_clock.frames;
        assert_eq!(
            f.live.advance_boundary(BoundaryInput::SourceEnded),
            BoundaryProgress::Ended
        );
        assert_eq!(f.live.terminal.certified_end, Some(pcm.len() as u64));
        assert!(f.live.decode_complete());
        assert_eq!(f.live.input_clock.frames, source_end);
        assert_eq!(f.live.output_position(), pcm.len() as u64);
        assert_eq!(
            f.live.boundary.as_ref().unwrap().accepted_end,
            pcm.len() as u64
        );
        assert_eq!(f.live.terminal.padding, PINNED_GUARD);
        let probe = probe.lock().unwrap();
        let base = f.live.vad_base_samples;
        assert!(base + probe.processed as u64 >= pcm.len() as u64 + 5152);
        assert!(base + probe.accepted as u64 - PINNED_GUARD as u64 >= pcm.len() as u64);
        assert_eq!(probe.pending_voiced, 0);
        assert!(!probe.candidate);
        let attempts = f.attempts.lock().unwrap();
        assert!(attempts.windows(2).all(|p| p[0].end <= p[1].start));
        for attempt in attempts.iter() {
            assert!(attempt.end <= pcm.len() as u64);
            assert_eq!(
                bits(&attempt.pcm),
                bits(&pcm[attempt.start as usize..attempt.end as usize])
            );
        }
        if case == "forced" {
            assert_eq!(
                attempts
                    .iter()
                    .map(|a| (a.start, a.end))
                    .collect::<Vec<_>>(),
                [(0, 2000), (2000, 10031)]
            );
        } else {
            assert!(
                attempts.is_empty(),
                "processed candidate can be VAD-negative, not processing loss"
            );
        }
        eprintln!(
            "CERTIFIED_EOF case={case} real={} base={base} native={} processed={} padding={} attempts={:?}",
            pcm.len(),
            probe.accepted,
            probe.processed,
            f.live.terminal.padding,
            attempts
                .iter()
                .map(|a| (a.start, a.end))
                .collect::<Vec<_>>()
        );
        drop(attempts);
        drop(probe);
        assert!(f.live.take_decode_report().is_complete());
        f.live.poll_words();
        let calls = f.calls.lock().unwrap().clone();
        let expected: Vec<Call> = [0.0; PINNED_GUARD]
            .chunks(HOP)
            .map(|c| Call::Accept(c.len()))
            .chain([Call::Flush])
            .collect();
        assert!(calls.ends_with(&expected));
        assert_eq!(
            f.live.advance_boundary(BoundaryInput::SourceEnded),
            BoundaryProgress::Ended
        );
        assert!(f.live.finish().is_empty());
        assert_eq!(*f.calls.lock().unwrap(), calls);
        assert!(f.live.take_decode_report().outcomes().is_empty());
    }
}

#[test]
fn terminal_zero_only_and_nonzero_vad_negative_are_complete_without_asr() {
    for frames in [0, 31, 512, 8223, 20_000] {
        let (mut f, probe) = observed_fixture();
        push(&mut f.live, &vec![0.0; frames], 16_000);
        assert_eq!(
            f.live.advance_boundary(BoundaryInput::SourceEnded),
            BoundaryProgress::Ended
        );
        assert!(f.live.decode_complete());
        assert_eq!(
            f.live.terminal.padding,
            if frames == 0 { 0 } else { PINNED_GUARD }
        );
        assert!(f.attempts.lock().unwrap().is_empty());
        assert!(f.live.poll_words().is_none());
        assert_eq!(probe.lock().unwrap().pending_voiced, 0);
    }
    struct Negative {
        accepted: Arc<Mutex<usize>>,
    }
    impl Segmenter for Negative {
        fn terminal_guard(&self) -> Option<usize> {
            Some(PINNED_GUARD)
        }
        fn accept(&mut self, pcm: &[f32]) {
            *self.accepted.lock().unwrap() += pcm.len();
        }
        fn flush(&mut self) {}
        fn reset(&mut self) {
            *self.accepted.lock().unwrap() = 0;
        }
        fn drain_completed(&mut self, _: &mut dyn FnMut(u64, &[f32])) {}
    }
    let mut f = fixture(true);
    let accepted = Arc::new(Mutex::new(0));
    f.live.vad = Box::new(Negative {
        accepted: accepted.clone(),
    });
    push(&mut f.live, &vec![0.25; 20_000], 16_000);
    assert_eq!(
        f.live.advance_boundary(BoundaryInput::SourceEnded),
        BoundaryProgress::Ended
    );
    assert_eq!(*accepted.lock().unwrap(), 20_000 + PINNED_GUARD);
    assert!(f.live.decode_complete());
    assert!(f.live.take_decode_report().is_complete());
    assert!(f.attempts.lock().unwrap().is_empty());
}

#[test]
fn terminal_pending_forced_fence_requires_real_guard_including_legacy_finish() {
    let mut f = fixture(true);
    push(&mut f.live, &[1.0; 2000], 16_000);
    f.live.latch_boundary(2000, BoundaryKind::Forced).unwrap();
    assert_eq!(
        f.live.advance_boundary(BoundaryInput::SourceEnded),
        BoundaryProgress::TerminalUnsettled(BoundaryError::InsufficientLookahead)
    );
    let calls = f.calls.lock().unwrap().clone();
    f.live.finish();
    assert_eq!(*f.calls.lock().unwrap(), calls);
    assert_eq!(f.live.terminal.padding, 0);
    assert!(f.live.terminal.certified_end.is_none());
    assert!(!f.live.decode_complete());
    assert!(!f.live.take_decode_report().is_complete());
}

#[test]
fn terminal_compatibility_finish_cannot_certify_an_unacknowledged_natural_fence() {
    let mut f = fixture(true);
    push(&mut f.live, &[0.0; 8192], 16000);
    f.live.latch_boundary(1000, BoundaryKind::Natural).unwrap();
    f.live.finish();
    assert!(!f.live.decode_complete());
    assert_eq!(f.live.terminal.padding, 0);
    assert!(f.live.terminal.certified_end.is_none());
    assert!(!f.live.take_decode_report().is_complete());
    let calls = f.calls.lock().unwrap().clone();
    f.live.finish();
    assert_eq!(*f.calls.lock().unwrap(), calls);
    assert!(f.live.take_decode_report().outcomes().is_empty());
}

#[test]
fn terminal_unknown_provenance_and_native_budget_refuse_before_synthetic_accept() {
    for boundary in [false, true] {
        let mut f = fixture(true);
        if !boundary {
            f.live.boundary = None;
        }
        f.live.terminal.guard = None;
        push(&mut f.live, &[0.5; 31], 16_000);
        f.live.finish();
        assert!(!f.live.decode_complete());
        assert_eq!(f.live.terminal.padding, 0);
        assert!(f.live.terminal.certified_end.is_none());
        let report = f.live.take_decode_report();
        assert_eq!(report.outcomes().len(), 1);
        assert_eq!(
            (
                report.outcomes()[0].span.start_frame,
                report.outcomes()[0].span.end_frame
            ),
            (0, 31)
        );
    }
    let mut f = fixture(true);
    push(&mut f.live, &[1.0; 8192], 16_000);
    f.live.boundary.as_mut().unwrap().limits.max_epoch_samples = 8192 + PINNED_GUARD as u64 - 1;
    let native = f.live.terminal.native_accepted;
    assert_eq!(
        f.live.advance_boundary(BoundaryInput::SourceEnded),
        BoundaryProgress::TerminalUnsettled(BoundaryError::EpochLimit)
    );
    assert_eq!(f.live.terminal.padding, 0);
    assert_eq!(f.live.terminal.native_accepted, native);
    assert!(!f.live.decode_complete());
}

#[test]
fn terminal_prior_legacy_checkpoint_loss_stays_partial_after_certified_eof() {
    let mut f = fixture(true);
    f.live.boundary = None;
    push(&mut f.live, &[0.5; 31], 16_000);
    assert!(f.live.checkpoint().is_empty());
    assert!(!f.live.decode_complete());
    let report = f.live.take_decode_report();
    assert_eq!(report.outcomes().len(), 1);
    assert_eq!(
        (
            report.outcomes()[0].span.start_frame,
            report.outcomes()[0].span.end_frame
        ),
        (0, 31)
    );
    assert_eq!(f.live.terminal.padding, 0);
    push(&mut f.live, &[0.0; 8192], 16_000);
    f.live.finish();
    assert_eq!(f.live.terminal.certified_end, Some(8223));
    assert!(
        !f.live.decode_complete(),
        "current epoch proof cannot erase the previous reset loss"
    );
    assert!(f.attempts.lock().unwrap().is_empty());
}

#[test]
fn terminal_clipped_failed_or_partial_attempt_never_retries_or_becomes_complete() {
    for status in [
        RegionStatus::Failed(DecodeErrorCategory::BackendFailure),
        RegionStatus::Partial(DecodeErrorCategory::OutputTruncated),
        RegionStatus::RecognizedEmpty,
    ] {
        let mut f = fixture(true);
        f.statuses.lock().unwrap().push_back(status);
        push(&mut f.live, &[1.0; 8223], 16_000);
        assert_eq!(
            f.live.advance_boundary(BoundaryInput::SourceEnded),
            BoundaryProgress::Ended
        );
        assert_eq!(f.live.terminal.certified_end, Some(8223));
        assert_eq!(
            f.live.decode_complete(),
            status == RegionStatus::RecognizedEmpty
        );
        let attempts = f.attempts.lock().unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!((attempts[0].start, attempts[0].end), (0, 8223));
        drop(attempts);
        let report = f.live.take_decode_report();
        assert_eq!(report.outcomes().len(), 1);
        assert_eq!(report.outcomes()[0].status, status);
        f.live.poll_words();
        assert!(f.live.finish().is_empty());
        assert_eq!(f.attempts.lock().unwrap().len(), 1);
    }
}

#[test]
fn terminal_recoverable_final_word_survives_all_terminal_paths_and_phases() {
    for phase in [0, 1, 13, 63, 64, 311, 511] {
        for path in [
            "batch",
            "legacy",
            "boundary",
            "natural",
            "maintenance",
            "forced",
        ] {
            let mut f = amplitude_fixture();
            if matches!(path, "batch" | "legacy") {
                f.live.boundary = None;
            }
            if phase > 0 {
                push(&mut f.live, &vec![0.0; phase], 16_000);
                if matches!(path, "batch" | "legacy") {
                    f.live.checkpoint();
                }
            }
            let mut pcm = vec![0.0; 8192];
            pcm.extend([0.5; 4608]);
            let mut expected = Vec::new();
            for (i, id) in [1usize, 2, 3, 4, 4, 5, 6].into_iter().enumerate() {
                if i == 4 {
                    pcm.push(0.0);
                }
                expected.push(Word {
                    text: LABELS[id - 1].into(),
                    start: (phase + pcm.len()) as f64 / 16000.0,
                    end: (phase + pcm.len() + WORD_SAMPLES) as f64 / 16000.0,
                });
                pcm.extend(vec![id as f32; WORD_SAMPLES]);
            }
            let mut words = Vec::new();
            if path == "batch" {
                f.live.push_resampled(&pcm, pcm.len(), 16000);
            } else if matches!(path, "natural" | "maintenance") {
                push(&mut f.live, &pcm[..8192], 16000);
                let kind = if path == "natural" {
                    BoundaryKind::Natural
                } else {
                    BoundaryKind::NaturalMaintenance
                };
                let token = f.live.latch_boundary((phase + 1000) as u64, kind).unwrap();
                assert_eq!(
                    f.live.advance_boundary(BoundaryInput::Continue),
                    BoundaryProgress::OldSettled(token)
                );
                f.live.acknowledge_boundary(token).unwrap();
                push(&mut f.live, &pcm[8192..], 16000);
            } else if path == "forced" {
                push(&mut f.live, &pcm[..12800], 16000);
                let token = f
                    .live
                    .latch_boundary((phase + 12800) as u64, BoundaryKind::Forced)
                    .unwrap();
                push(&mut f.live, &pcm[12800..], 16000);
                assert_eq!(
                    f.live.advance_boundary(BoundaryInput::SourceEnded),
                    BoundaryProgress::OldSettled(token)
                );
                words.extend(f.live.poll_words().unwrap_or_default());
                f.live.acknowledge_boundary(token).unwrap();
            } else {
                push(&mut f.live, &pcm, 16000);
            }
            words.extend(f.live.finish());
            assert!(f.live.decode_complete(), "path={path} phase={phase}");
            assert_eq!(words.len(), expected.len(), "path={path} phase={phase}");
            for (actual, expected) in words.iter().zip(&expected) {
                assert_eq!(actual.text, expected.text, "path={path} phase={phase}");
                assert!((actual.start - expected.start).abs() < 1e-12);
                assert!((actual.end - expected.end).abs() < 1e-12);
            }
            let attempts = f.attempts.lock().unwrap();
            assert!(attempts.windows(2).all(|p| p[0].end <= p[1].start));
            for a in attempts.iter() {
                assert!(a.start >= phase as u64 && a.end <= (phase + pcm.len()) as u64);
                assert_eq!(
                    bits(&a.pcm),
                    bits(&pcm[a.start as usize - phase..a.end as usize - phase])
                );
            }
            eprintln!(
                "RECOVERED_FAKE_WORD path={path} phase={phase} words={} last={} attempts={} padding={}",
                words.len(),
                words.last().unwrap().text,
                attempts.len(),
                f.live.terminal.padding
            );
        }
    }
}

#[test]
fn terminal_batch_and_live_share_actual_resampler_tail_at_supported_rates_and_origins() {
    for rate in [16000u32, 44100, 48000] {
        for origin in [0usize, 13, 311] {
            let source = vec![0.5; rate as usize + 31];
            let canonical = if rate == 16000 {
                source.clone()
            } else {
                let mut r = Resampler::new(rate as i32).unwrap();
                let mut pcm = r.push(&source);
                pcm.extend(r.finish());
                pcm
            };
            let mut outputs = Vec::new();
            for batch in [false, true] {
                let mut f = fixture(true);
                f.live.boundary = None;
                if origin != 0 {
                    push(&mut f.live, &vec![0.0; origin], 16000);
                    f.live.checkpoint();
                }
                if batch {
                    f.live.push_resampled(&canonical, source.len(), rate);
                } else {
                    push(&mut f.live, &source, rate);
                }
                f.live.finish();
                assert!(f.live.decode_complete());
                assert_eq!(f.live.input_clock.frames, (origin + source.len()) as u64);
                assert_eq!(f.live.output_position(), (origin + canonical.len()) as u64);
                assert!(f.live.resampler.is_none());
                let attempts = f.attempts.lock().unwrap();
                assert_eq!(attempts.len(), 1);
                assert_eq!(
                    (attempts[0].start, attempts[0].end),
                    (origin as u64, (origin + canonical.len()) as u64)
                );
                assert_eq!(bits(&attempts[0].pcm), bits(&canonical));
                outputs.push(f.live.take_decode_report().outcomes().to_vec());
                let calls = f.calls.lock().unwrap().clone();
                assert!(f.live.finish().is_empty());
                assert_eq!(*f.calls.lock().unwrap(), calls);
            }
            assert_eq!(outputs[0], outputs[1]);
        }
    }
}

#[test]
fn terminal_clips_before_mapping_discards_synthetic_only_and_rejects_overlap() {
    struct OnFlush {
        pcm: Vec<f32>,
        real_end: u64,
        overlap: bool,
        queue: VecDeque<(u64, Vec<f32>)>,
    }
    impl Segmenter for OnFlush {
        fn terminal_guard(&self) -> Option<usize> {
            Some(PINNED_GUARD)
        }
        fn accept(&mut self, pcm: &[f32]) {
            self.pcm.extend_from_slice(pcm);
        }
        fn flush(&mut self) {
            self.queue.push_back((0, self.pcm.clone()));
            self.queue.push_back((self.real_end + 1, vec![0.0; 512]));
            if self.overlap {
                self.queue.push_back((0, self.pcm.clone()));
            }
        }
        fn reset(&mut self) {
            self.pcm.clear();
            self.queue.clear();
        }
        fn drain_completed(&mut self, visit: &mut dyn FnMut(u64, &[f32])) {
            while let Some((start, pcm)) = self.queue.pop_front() {
                visit(start, &pcm);
            }
        }
    }
    for overlap in [false, true] {
        for sample in [0.0, 0.5] {
            let mut f = fixture(true);
            f.live.vad = Box::new(OnFlush {
                pcm: Vec::new(),
                real_end: 31,
                overlap,
                queue: VecDeque::new(),
            });
            push(&mut f.live, &[sample; 31], 16000);
            let progress = f.live.advance_boundary(BoundaryInput::SourceEnded);
            assert_eq!(
                progress,
                if overlap {
                    BoundaryProgress::TerminalUnsettled(BoundaryError::MappingUnavailable)
                } else {
                    BoundaryProgress::Ended
                }
            );
            assert_eq!(f.live.decode_complete(), !overlap);
            assert_eq!(
                f.live.terminal.certified_end,
                if overlap { None } else { Some(31) }
            );
            let attempts = f.attempts.lock().unwrap();
            assert_eq!(attempts.len(), 1);
            assert_eq!((attempts[0].start, attempts[0].end), (0, 31));
            assert_eq!(bits(&attempts[0].pcm), bits(&[sample; 31]));
        }
    }
}

#[test]
fn terminal_known_evicted_mapping_stays_metadata_but_source_holes_refuse() {
    let mut evicted = fixture(true);
    for _ in 0..300 {
        push(&mut evicted.live, &[0.0; 512], 16000);
        evicted.live.input_clock.end_epoch();
    }
    assert_eq!(
        evicted.live.input_clock.input_span(0, 300 * 512),
        Err(MappingError::EvictedHistory)
    );
    assert_eq!(
        evicted.live.advance_boundary(BoundaryInput::SourceEnded),
        BoundaryProgress::Ended
    );
    assert!(evicted.live.decode_complete());
    assert!(evicted.live.take_decode_report().is_complete());
    let mut hole = fixture(true);
    push(&mut hole.live, &[0.0; 512], 16000);
    hole.live.input_clock.admit(10).unwrap();
    push(&mut hole.live, &[0.0; 512], 16000);
    assert_eq!(
        hole.live.input_clock.input_span(0, 1024),
        Err(MappingError::Invalid)
    );
    assert_eq!(
        hole.live.advance_boundary(BoundaryInput::SourceEnded),
        BoundaryProgress::TerminalUnsettled(BoundaryError::MappingUnavailable)
    );
    assert_eq!(hole.live.terminal.padding, 0);
    assert!(!hole.live.decode_complete());
}

#[test]
fn terminal_legacy_signed_headroom_is_checked_before_any_synthetic_accept() {
    // Inject the scalar edge, without allocating or sending billions of samples to the fake/native.
    let mut f = fixture(true);
    f.live.boundary = None;
    let native = i32::MAX as u64 - 576 - PINNED_GUARD as u64 + 1;
    f.live.terminal.native_accepted = native;
    f.live.win.fed = native;
    let span = f.live.input_clock.admit(native as usize).unwrap();
    assert!(!f.live.input_clock.map_input(span, 16000, 0));
    f.live.finish();
    assert_eq!(f.live.input_clock.frames, native);
    assert_eq!(f.live.terminal.native_accepted, native);
    assert_eq!(f.live.terminal.padding, 0);
    assert!(!f.live.decode_complete());
    assert!(
        !f.calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| matches!(call, Call::Accept(_)))
    );
}

#[test]
fn terminal_resampler_failure_cannot_recertify_old_loss_or_redecode_old_pcm() {
    let mut f = fixture(true);
    f.live.boundary = None;
    push(&mut f.live, &[0.0; 8192], 16000);
    push(&mut f.live, &[0.5; 31], 16000);
    f.live.resampler_factory = Box::new(|_| None);
    f.live.push(&[0.5; 17], 44100);
    assert!(f.live.finish().is_empty());
    assert!(!f.live.decode_complete());
    assert_eq!(f.live.input_clock.frames, 8240);
    assert!(f.attempts.lock().unwrap().is_empty());
    assert!(
        f.live
            .take_decode_report()
            .outcomes()
            .iter()
            .any(|o| o.span.start_frame == 8223
                && o.span.end_frame == 8240
                && o.status == RegionStatus::Failed(DecodeErrorCategory::ResamplerUnavailable))
    );
}
