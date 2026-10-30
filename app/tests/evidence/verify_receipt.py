"""Require the complete named actual-source suite; scoped probes are explicitly separate."""
import re

EXPECTED = {
    "evidence::tests::capture_optout_discovers_only_owned_names_and_preserves_unrelated_targets",
    "evidence::tests::directory_sync_failure_reports_error_after_complete_atomic_replacement",
    "evidence::tests::failed_atomic_publication_preserves_old_bytes_and_cleans_registered_temps",
    "evidence::tests::identity_documents_are_recoverable_and_exclude_secrets_paths_and_quality",
    "evidence::tests::merged_maximum_quality_and_activity_fit_reserved_capacity_after_text_truncation",
    "evidence::tests::optout_removes_evidence_and_temps_but_never_audio_or_quality",
    "evidence::tests::optout_unlinks_known_file_when_directory_listing_is_denied_and_reports_partial_failure",
    "evidence::tests::private_atomic_windows_keep_precise_asr_and_bind_generation",
    "evidence::tests::readback_rejects_rebound_identity_documents_and_keeps_fixed_error_categories",
    "evidence::tests::truncated_evidence_is_durable_and_cannot_fail_filing",
    "evidence_config_tests::legacy_settings_enable_evidence_and_explicit_optout_round_trips",
    "evidence_retention::tests::actual_sweep_optout_and_deletion_cover_active_and_aws_owned_rows",
    "live::tests::activity_live_unknown_authority_keeps_empty_asr_note_free",
    "live::tests::actual_live_aec_consume_and_finish_record_track_specific_short_lookahead",
    "live::tests::actual_live_discard_and_post_panic_reaper_remove_optional_text_and_temps",
    "live::tests::actual_live_early_echo_context_matches_pruned_classifier_inventory",
    "live::tests::actual_live_early_echo_context_survives_post_assessment_pruning",
    "live::tests::actual_live_early_echo_immediate_publication_has_no_comparison_context",
    "live::tests::actual_live_early_echo_missing_lineage_and_cap_preserve_output",
    "live::tests::actual_live_early_echo_split_poll_bounds_context_capacity_and_preserves_every_word",
    "live::tests::actual_live_evidence_cap_never_drops_words_or_stops_later_windows",
    "live::tests::actual_live_evidence_preserves_critical_raw_protection_through_diarization",
    "live::tests::actual_live_finalization_records_only_selected_hosted_prose",
    "live::tests::actual_live_flush_keeps_one_asr_per_input_and_persists_windows_before_note_failure",
    "live::tests::actual_live_optout_releases_text_and_never_recreates_removed_evidence",
    "live::tests::actual_live_pre_note_optout_sweep_removes_capture_owned_evidence",
    "live::tests::actual_live_selected_and_fallback_prose_precede_note_rewrite_crash",
    "live::tests::actual_live_withheld_early_echo_retains_original_ordinal_and_explicit_context",
    "live::tests::live_far_poll_protection_survives_pause_split_units",
    "live::tests::live_mic_poll_protection_survives_pause_split_units",
    "live::tests::live_raw_protection_survives_later_backchannel_and_filler_passes",
    "pipeline::retention_tests::actual_batch_live_and_persistence_diagnostics_omit_content_under_failures",
    "pipeline::retention_tests::actual_batch_optout_and_evidence_write_failure_never_block_filing",
    "pipeline::retention_tests::actual_batch_optout_cannot_complete_between_enabled_check_and_publication",
    "pipeline::retention_tests::actual_batch_pending_note_reuses_byte_identical_asr_without_backend_or_audio",
    "pipeline::retention_tests::actual_batch_precheckpoint_crash_retries_and_atomically_replaces_one_asr_generation",
    "pipeline::retention_tests::actual_pending_note_survives_optional_corruption_and_optout_without_another_asr",
    "pipeline::retention_tests::actual_readonly_transcription_and_export_exclude_evidence_and_clean_redo_records_processed_input",
    "pipeline::retention_tests::actual_sweep_removes_opted_out_evidence_before_aws_job_enqueue_failure",
    "pipeline::retention_tests::note_less_quality_uses_existing_batch_payload_without_fallback",
    "pipeline::retention_tests::successful_live_quality_survives_completion_replay_and_publication_failure",
    "pipeline::retention_tests::unfiled_boundary_recovers_once_and_retains_source_intervals_after_restart",
    "pipeline::tests::full_production_chain_counts_backend_and_replays_checkpoint_without_audio",
    "transcribe::tests::production_recording_path_invokes_scripted_backend_once_and_keeps_partial_output",
}


def require_complete(output, returncode):
    rows = re.findall(r"^test (\S+) \.\.\. (\S+)$", output, re.MULTILINE)
    summaries = re.findall(r"^test result: (.+)$", output, re.MULTILINE)
    expected_summary = f"ok. {len(EXPECTED)} passed; 0 failed; 0 ignored; 0 measured; 0 filtered out;"
    if (returncode != 0 or len(rows) != len(EXPECTED)
            or {name for name, status in rows if status == "ok"} != EXPECTED
            or len(summaries) != 1 or not summaries[0].startswith(expected_summary)):
        raise RuntimeError("incomplete evidence actual-source test receipt")
