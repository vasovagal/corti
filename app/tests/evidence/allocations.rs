// Included inside evidence::tests only by --allocations. The caller keeps its input, original ledger,
// and encoded bytes alive during actual Session::load so their simultaneous ownership is visible.
#[test]
fn actual_maximum_envelope_allocation_probe() {
    let (dir, meta) = fixture("maximum-allocation");
    for (label, rows, text_bytes) in [
        (
            "near_wire_cap",
            2,
            (LEDGER_CAP - 256 * 1024 - 4096 - 32768) / 2,
        ),
        ("maximum_rows", 8192, 128),
    ] {
        let mut session = Session::begin("recording", &meta).unwrap();
        let caller: Vec<_> = (0..rows)
            .map(|index| TranscriptSegment {
                speaker: Speaker::Me,
                start: index as f64,
                end: index as f64 + 0.5,
                text: "x".repeat(text_bytes),
            })
            .collect();
        let caller_bytes = caller.capacity() * std::mem::size_of::<TranscriptSegment>()
            + caller.iter().map(|row| row.text.capacity()).sum::<usize>();
        // The portable per-invocation transient bound is 8 MiB. Two real windows approach the total
        // wire ceiling without weakening that independent admission rule.
        for (window, input) in caller.chunks(if rows == 2 { 1 } else { rows }).enumerate() {
            let identity = session.identity();
            let result = session
                .ledger_mut()
                .capture(
                    RecordingIdentity {
                        window: window as u64,
                        kind: StageKind::TrueAsr,
                        identity,
                        signal: SignalIdentity {
                            source: SignalSource::OfflineInput,
                            aec: AecOrigin::None,
                            source_rate_hz: Some(16_000),
                            lookahead_frames: None,
                        },
                    },
                    input,
                    &[],
                    &[],
                )
                .unwrap();
            assert_eq!(
                result.retained,
                corti_transcribe::evidence::Append::Retained
            );
        }
        let (status, encode_peak, encode_us) =
            crate::evidence_allocation::measure(|| session.persist(&meta.audio_path));
        assert!(matches!(status, Status::Available { .. }));
        let encoded = std::fs::read(path_for(&meta.audio_path)).unwrap();
        assert!(encoded.len() <= MAX_FILE_BYTES);
        let (loaded, read_peak, read_us) = crate::evidence_allocation::measure(|| {
            Session::load("recording", &meta, &meta.audio_path, status).unwrap()
        });
        assert_eq!(
            loaded
                .ledger()
                .groups()
                .iter()
                .map(|group| group.rows.len())
                .sum::<usize>(),
            rows
        );
        assert_eq!(loaded.ledger().groups()[0].rows[0].segment, caller[0]);
        assert!(loaded.ledger().owned_bytes() <= DEFAULT_OWNED_CAP);
        assert!(encode_peak <= MAX_FILE_BYTES + 1024 * 1024);
        assert!(read_peak <= 16 * DEFAULT_OWNED_CAP + 5 * MAX_FILE_BYTES);
        println!(
            "app-envelope-alloc label={label} caller_bytes={caller_bytes} retained_ledger={} encoded_capacity={} wire_bytes={} encode_additional_peak={encode_peak} encode_us={encode_us} read_additional_peak={read_peak} read_us={read_us} parsed_ledger={} simultaneous_payload_bound={}",
            session.ledger().owned_bytes(),
            encoded.capacity(),
            encoded.len(),
            loaded.ledger().owned_bytes(),
            caller_bytes + session.ledger().owned_bytes() + encoded.capacity() + read_peak
        );
        std::hint::black_box((&caller, &session, &encoded, &loaded));
    }
    std::fs::remove_dir_all(dir).unwrap();
}
