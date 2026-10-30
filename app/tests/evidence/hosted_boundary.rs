// Synthetic hosted-service boundary only. Production finish_session, acceptance ordering, note
// rewrite, evidence adapters, and persistence remain unmodified source in the harness.
thread_local! { static EVIDENCE_HOSTED_MODE: std::cell::Cell<u8> = const { std::cell::Cell::new(0) }; }
pub(crate) fn evidence_hosted_mode(mode: u8) {
    EVIDENCE_HOSTED_MODE.set(mode);
}
fn evidence_hosted_enabled() -> bool {
    matches!(EVIDENCE_HOSTED_MODE.get(), 1 | 2 | 4 | 5)
}
fn evidence_hosted_finalize(mut transcript: DiarizedTranscript) -> SettledFinalTranscript {
    let mode = EVIDENCE_HOSTED_MODE.get();
    assert_ne!(mode, 0, "unexpected hosted boundary");
    assert_eq!(transcript.segments.len(), if mode == 4 { 2 } else { 1 });
    assert_eq!(transcript.segments[0].text, "Original synthetic statement.");
    if mode == 4 {
        assert_eq!(transcript.segments[1].text, "Later synthetic statement.");
        assert!(transcript.segments[0].start < transcript.segments[1].start);
    }
    if matches!(mode, 1 | 2) {
        transcript.segments[0].text = "Selected synthetic hosted prose.".into();
    }
    SettledFinalTranscript {
        transcript,
        applied_postprocess: AppliedPostprocessProvenance::none(),
        source_transcript_fingerprint: None,
        call_ids: if mode == 5 {
            vec![CallId::new("synthetic-fallback-attempt").unwrap()]
        } else {
            Vec::new()
        },
        hosted_text_applied: matches!(mode, 1 | 2),
        fallback_code: None,
    }
}
