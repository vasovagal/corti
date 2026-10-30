#!/usr/bin/env python3
"""Run exact native checks against authored synthetic fixtures, never user audio."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys


def single_test_passed(output, returncode):
    summaries = re.findall(r"(?m)^test result: ([A-Za-z]+)\. (\d+) passed; (\d+) failed;.*$", output)
    return returncode == 0 and summaries == [("ok", "1", "0")]


def named_tests_passed(output, returncode, required):
    """A feature-specific suite must execute its exact permanent inventory, without skips."""
    summaries = re.findall(
        r"(?m)^test result: ([A-Za-z]+)\. (\d+) passed; (\d+) failed; (\d+) ignored;.*$",
        output)
    executed = re.findall(r"(?m)^test [^\n]*::([^ :]+) \.\.\. ok$", output)
    return (bool(required) and len(required) == len(set(required)) and returncode == 0
            and summaries == [("ok", str(len(required)), "0", "0")]
            and sorted(executed) == sorted(required))


TAP_LIVE_REQUIRED_TESTS = (
    "live_preview_failed_partial_empty_and_metadata_keep_exact_survivors",
    "live_preview_panic_drains_known_report_and_keeps_other_channel_without_retry",
    "live_preview_alignment_preserves_both_prefixes_and_discontinuous_source_bindings",
    "live_preview_availability_masks_real_aec_tail_and_rejects_malformed_mask",
    "live_preview_persistence_retains_capture_wav_and_refuses_conflicts",
    "run_live_caller_preserves_words_wav_and_quality_and_rejects_incomplete",
    "live_preview_late_track_origin_and_absent_epoch_keep_clock_and_padding",
    "run_live_caller_capture_finish_error_preserves_known_history_without_promotion",
)
TAP_INBOX_REQUIRED_TESTS = (
    "inbox_cleanup_preserves_corrections_without_audio_evidence",
    "inbox_partial_decode_keeps_other_words_and_exact_loss_metadata",
    "inbox_missing_interval_mapping_remains_visible_as_incomplete",
    "inbox_complete_quality_keeps_existing_body_text",
    "inbox_epoch_jump_keeps_source_time_and_unknown_boundaries_explicit",
    "inbox_projection_bounds_metadata_but_body_keeps_every_retained_loss",
)
STREAM_REQUIRED_TESTS = (
    "stream_middle_failure_retains_exact_neighbors_spans_and_terminal_status",
    "stream_partial_and_tail_failures_preserve_available_words_without_retry",
    "stream_valid_empty_and_metadata_only_completion_remain_quiet",
    "stream_same_push_panic_preserves_pending_words_reports_and_known_loss",
    "stream_tail_panic_is_terminal_and_never_retries_the_backend",
    "stream_consumer_drop_stops_at_send_without_finish_or_extra_backend_calls",
    "stream_backlog_drops_are_visible_separately_from_admitted_decode_status",
    "stream_report_merge_preserves_metadata_failure_priority_and_saturation",
    "stream_unhandled_worker_panic_keeps_known_facts_and_reports_join_failure",
)


CAPTURE_REQUIRED_TESTS = (
    *TAP_LIVE_REQUIRED_TESTS,
    "failure_across_compressed_source_jump_splits_without_covering_the_jump",
    "floor_start_ceil_end_and_fractional_epoch_boundary_are_checked",
    "missing_and_omitted_epochs_and_arithmetic_overflow_remain_counted",
    "cap_preserves_the_failure_count_and_source_loss_is_unchanged",
    "successful_cap_never_hides_a_later_failure_or_invalidates_silence",
    "report_merge_and_saturated_failure_cap_cannot_restore_complete_success",
    "complete_and_empty_report_caps_retain_degradation_without_recovery",
    "failed_report_replacement_counts_displaced_success_and_preserves_failure",
    "merged_report_caps_and_drains_count_each_omission_once",
    "metadata_saturation_keeps_decode_success_but_quality_unknown",
    "report_and_received_mapping_omissions_accumulate_without_losing_real_failure",
    "cap_eviction_records_metadata_with_per_push_report_drains",
    "cap_evicted_long_success_keeps_words_and_counts_distinct_metadata_facts",
    "cap_evicted_failed_or_partial_region_still_has_processing_loss",
    "invalid_native_coordinates_never_decode_or_publish_fabricated_times",
    "evicted_prefix_proves_only_covered_ranges_and_counts_each_removal_once",
    "evicted_metadata_never_hides_retained_or_forgotten_source_and_output_holes",
    "evicted_prefix_does_not_cover_pre_epoch_positions_or_arithmetic_overflow",
    "batch_cap_metadata_accumulates_both_channels_without_processing_loss",
    "batch_metadata_cap_preserves_existing_source_and_decode_failure_facts",
    "successful_metadata_caps_reach_filed_and_no_note_without_decode_loss",
    "metadata_report_survives_same_call_panic_without_processing_loss",
    "late_first_received_epoch_settles_exactly_after_provisional_omission",
    "buffered_aec_tee_gaps_and_compressed_epochs_do_not_shift_later_failures",
    "resampler_failure_survives_recovery_rate_change_and_independent_track_clocks",
    "unmappable_gap_keeps_known_intersections_and_counts_missing_processing_loss",
    "mapping_and_failure_caps_coalesce_only_exact_adjacent_bindings",
    "window_not_filed_uses_logical_inputs_but_retained_losses_split_at_authority",
    "invalid_and_saturated_input_coordinates_never_panic_or_create_a_source_span",
    "additive_default_calls_existing_backend_once_without_upgrading_capture_quality",
    "additive_default_preserves_whole_file_errors_without_a_second_attempt",
    "inbox_partial_decode_keeps_other_words_and_exact_loss_metadata",
    "inbox_missing_interval_mapping_remains_visible_as_incomplete",
    "inbox_complete_quality_keeps_existing_body_text",
    "inbox_epoch_jump_keeps_source_time_and_unknown_boundaries_explicit",
    "inbox_projection_bounds_metadata_but_body_keeps_every_retained_loss",
    "failed_middle_region_keeps_exact_neighbors_and_flush_tail",
    "recognized_empty_and_partial_payload_remain_distinct",
    "resampler_failure_preserves_fractional_time_across_recovery_checkpoint_and_rate_change",
    "old_native_resampler_and_vad_tail_flush_before_failed_rate_switch",
    "fractional_skipped_duration_is_visible_immediately_without_a_later_clock_jump",
    "ggml_errors_are_content_free_and_keep_native_partial_words",
    "sherpa_missing_result_is_failure_but_valid_blank_is_complete",
    "batch_decode_loss_extends_the_capture_record_without_resolving_history",
    "batch_resampler_failure_without_capture_mapping_is_an_omitted_loss",
    "decode_failures_cross_push_checkpoint_and_finish_without_losing_survivors",
    "final_ledger_cap_does_not_make_durable_windows_partial",
    "partial_redo_decode_cannot_resolve_prior_or_current_processing_loss",
    "decode_fallback_runs_once_on_original_backend_and_replays_partial_batch",
    "decode_source_binding_survives_late_mic_tee_holes_and_aec_tail",
    "failed_checkpoint_drops_its_window_and_recovers",
    "fallback_after_later_finish_panic_retains_new_aec_tail_decode_loss",
    "batch_and_live_reports_use_original_source_frames",
    "unavailable_clock_cannot_emit_later_words_at_compressed_timestamps",
    "native_ceil_tail_maps_to_actual_input_and_cross_rate_boundaries",
    "bounded_epoch_history_cannot_invent_missing_input_mapping",
    "skipped_rational_carry_never_rounds_each_push",
    "mapping_omission_cap_records_degradation_without_inventing_decode_loss",
    "mapping_omission_invalid_inputs_and_clock_overflow_are_counted_once_per_run",
    "mapping_omission_counter_saturation_remains_unknown_without_claiming_decode_loss",
    "failed_aec_window_files_loss_for_delivered_tail_and_delayed_prefix",
    "queued_decode_report_survives_same_call_panic_at_every_channel_boundary",
    "production_recording_path_invokes_scripted_backend_once_and_keeps_partial_output",
    "fallback_without_finish_keeps_mapping_omissions_and_omits_clean_quality",
    "legacy_history_create_reopen_and_compare_and_swap_preserve_unknown_capture",
    "legacy_history_success_and_empty_results_never_promote_capture_storage",
    "legacy_history_rejects_observed_input_instead_of_rewriting_known_facts",
    "legacy_history_state_dependent_fields_and_bindings_are_strict",
    "legacy_history_initial_publication_cannot_clobber_collisions_or_interrupted_work",
    "legacy_history_concurrent_initial_and_later_publishers_preserve_the_winner",
    "legacy_history_publication_faults_restore_absence_or_exact_prior_bytes",
    "legacy_history_interrupted_updates_keep_private_history_and_registered_cleanup",
    "legacy_history_caps_reject_before_replacing_history",
    "capture_wire_frame_counts_remain_numbers_and_cannot_be_unknown",
    "delayed_mic_poll_keeps_critical_words_after_temporal_source_eviction",
    "temporal_source_eviction_still_allows_later_acoustic_echo_deletion",
    "rate_transition_with_invalid_sample_clock_reanchors_without_fabricated_loss",
    "reanchored_sample_clock_after_rate_transition_preserves_later_exact_gap",
    "invalid_sample_rate_transition_preserves_real_host_gap_and_offset",
    "same_rate_invalid_sample_clock_keeps_anchor_across_layout_change",
    "redo_retains_live_mismatch_while_retry_payload_exists",
    "redo_history_overlay_splits_live_overlap_but_never_recovers_new_batch_failure",
    "redo_history_overlay_stays_bounded_when_overlap_splits_at_the_span_cap",
    "concurrent_settlement_publishers_preserve_the_winner_and_retry_union",
    "settled_history_uses_existing_publication_and_rejects_stale_attempts",
    "settled_partial_history_survives_capture_finalization_without_promoting_storage",
    "redo_in_progress_retains_source_coverage_through_finalization",
    "redo_finalizing_retains_source_coverage_through_finalization",
    "storage_completeness_projection_preserves_intrinsic_quality",
    "local_redo_after_backend_refusal_recovers_processing_without_rewriting_source",
    "small_pcm_ring_retains_a_gap_b_c_identically_in_wav_and_tee",
    "restart_after_finalize_before_completion_preserves_exact_loss_and_large_epoch_mapping",
    "explicit_stop_authorizes_completion_but_dropped_intent_keeps_exact_terminal_prefix_in_progress",
    "native_notification_only_invalidates_until_control_side_refresh",
    "disabled_callback_admission_keeps_pcm_descriptors_and_accounting_unchanged",
    "exact_capture_quality_reaches_live_finish_before_the_completion_event",
    "offline_aec_masks_primed_gap_and_tail_without_masking_compressed_epoch_audio",
    "consume_and_finish_keep_availability_through_rolling_windows",
    "crash_before_checkpoint_preserves_backend_and_gates_the_callable_sequence",
    "first_live_fallback_retains_six_durable_batch_attempts_without_fresh_egress",
    "explicit_local_redo_after_done_cleanup_resolves_tee_loss_without_inventing_origin",
    "host_gap_with_invalid_sample_clock_preserves_later_source_time_and_unknown_loss",
    "full_capacity_quality_uses_labelled_projection_without_losing_authoritative_facts",
    "stranded_recording_retry_reloads_quality_without_a_queue_carrier",
    "settled_retry_quality_survives_long_retention_and_failed_artifact_deletion",
    "manual_rearm_crash_before_recording_reset_repairs_routing_without_losing_quality",
    "manual_rearm_never_restores_first_batch_hosted_authorization",
    "exhaustion_crash_retains_settled_quality_without_repeating_backend_work",
    "manual_rearm_of_exhausted_active_job_cannot_extend_its_attempt_budget",
    "final_attempt_checkpoint_crash_recovers_filing_without_an_extra_backend_call",
    "manual_retry_unreadable_checkpoint_cannot_rearm_known_aws_origin",
    "settled_mismatch_quality_survives_retry_rewrite_reopen_refusal_and_checkpoint",
    "supplemental_live_quality_never_certifies_missing_corrupt_or_unfinished_capture",
    "mismatched_chunks_keep_both_tracks_in_filter_order",
    "single_track_layouts_do_not_create_mismatches",
    "mismatch_uses_writer_position_after_a_tee_drop_and_unfilled_jump",
    "mismatch_warnings_report_the_leading_edge_and_each_hundred",
    "late_track_origins_and_absent_epochs_preserve_word_and_filter_clocks",
    "late_microphone_origin_shifts_acoustic_evidence_once",
    "mismatch_history_survives_final_filing_failure_later_consume_error_and_panic",
    "consumed_panic_waits_for_finish_when_the_start_epoch_tee_was_dropped",
    "note_less_live_fallback_retains_settled_quality_for_durable_batch",
    "mic_only_epoch_emits_absent_tap_and_bounded_positioned_chunks",
    "tee_position_after_a_drop_keeps_the_writer_clock",
    "recording_quality_merge_survives_duplicate_stale_claim_and_reopen_without_reset",
    "malformed_recording_quality_fails_before_mutation_without_payload_diagnostics",
    "failed_recording_quality_merge_and_manual_rearm_are_atomic_without_implicit_restart",
    "recording_reschedule_merges_new_quality_and_rejects_stale_claims",
    "manual_recording_rearm_rolls_back_both_authorities_and_reopens_together",
    "manual_recording_rearm_preserves_absent_origin_active_claims_and_backoff",
    "recording_retry_gc_preserves_owned_and_unknown_payloads_until_explicit_cleanup",
    "both_short_sides_and_empty_expected_tracks_keep_exact_missing_tail",
    "retained_positions_survive_tee_drops_unfilled_jumps_and_rate_rounding",
    "absent_conflicting_cross_epoch_or_arithmetic_mapping_remains_counted_loss",
    "cap_coalescing_warning_cadence_and_saturation_keep_all_loss_accounted",
    "good_start_epoch_keeps_a_later_mismatch_exact_before_finish",
    "observed_epoch_inventory_is_bounded_and_conflicts_remain_counted_loss",
    "provisional_processing_facts_merge_only_with_independent_capture_authority",
    "activity_publishes_open_run_before_finish_with_exact_degraded_stage_and_levels",
    "activity_actual_span_cap_counts_each_run_once_and_preserves_summary_before_finish",
    "activity_same_input_retry_keeps_omission_lower_bound_and_stages_remain_distinct",
    "activity_span_dropped_by_merge_cap_is_not_recounted_by_same_input_retry",
    "activity_distinct_scope_merges_preserve_summed_lower_bounds_and_generic_history",
    "activity_supplement_uses_authoritative_epochs_without_changing_source_authority",
    "activity_actual_epoch_cap_retains_suspected_facts_and_one_omission_per_run",
    "activity_projection_uses_observed_epoch_and_outward_source_rounding",
    "activity_legacy_unknown_availability_does_not_observe_numerical_fill",
    "activity_complete_zero_proof_conflicts_with_nonzero_and_never_erases_uncertainty",
    "activity_partial_union_never_certifies_absence_and_stage_bindings_do_not_pool",
    "activity_complete_retry_preserves_uncertainty_but_supplies_its_own_extent",
    "activity_conflicting_input_rate_or_extent_is_conservative_and_merge_is_associative",
    "activity_actual_record_serde_defaults_and_fixed_memory_reject_malformed_proofs",
    "activity_omission_saturation_does_not_change_source_completeness_or_recovery",
    "activity_invalid_proofs_reject_on_both_record_serde_paths_without_payload_diagnostics",
    "dead_tap_active_mic_opens_at_five_seconds_and_finishes_once",
    "both_quiet_is_not_suspected_and_levels_are_measured",
    "intermittent_keeps_global_evidence_and_partial_nonzero_closes_run",
    "quiet_zero_stretch_is_intermittent_without_suspicion",
    "short_zero_stretches_do_not_sum_into_sustained_silence",
    "unavailable_fill_is_unknown_and_a_later_observed_run_can_trigger",
    "invalid_and_short_tracks_never_become_silence_or_active_mic",
    "observed_tap_zero_streak_survives_nonfinite_mic",
    "observed_tap_zero_streak_survives_missing_mic",
    "mic_invalidity_breaks_suspicion_but_preserves_reference_history",
    "invalid_tap_missing_tap_and_unavailable_fill_still_break_zero_history",
    "empty_mask_matches_explicit_all_available_without_an_adapter_bitmap",
    "one_missing_tap_frame_cannot_be_padded_to_the_threshold",
    "activity_accepted_minus_sixty_dbfs_threshold_has_exact_f32_boundary_controls",
    "threshold_is_mean_square_not_peak_and_is_explicitly_uncalibrated",
    "waveform_partition_sweep_keeps_identical_levels_runs_and_facts",
    "rate_and_epoch_changes_restart_bins_without_bridging_source_jumps",
    "conflicting_or_reused_epoch_is_rejected_without_counting_samples",
    "final_partial_second_neither_opens_nor_extends_suspicion",
    "retained_holes_close_runs_in_constant_state_without_synthetic_seconds",
    "leading_or_trailing_holes_missing_verdict_and_empty_input_cannot_certify_absent",
    "structurally_absent_tracks_are_not_observed_zeros_and_never_suspected",
    "absent_layout_then_positive_tap_is_intermittent",
    "bounded_sink_counts_each_omitted_run_once_including_a_long_run",
    "external_epoch_binding_cap_rejects_once_per_run_without_growing_tracker",
    "replayed_positions_do_not_double_count_and_empty_pushes_do_not_reset",
    "counter_overflow_saturates_and_does_not_wrap_or_erase_positive_facts",
    "malformed_input_and_position_overflow_fail_without_admission",
    "observed_pcm_is_borrowed_and_extreme_finite_levels_stay_finite",
    "activity_schema_one_wire_round_trip_preserves_summary_only_observations",
    "activity_saturation_preserves_later_source_processing_and_generic_degraded_authority",
    "activity_stage_sum_saturates_only_diagnostic_authority_and_preserves_serde_history",
    "activity_summary_and_terminal_proof_survive_store_without_promoting_storage",
    "activity_summary_only_settlement_is_durable_and_cannot_regress_on_retry",
    "activity_is_pre_filter_and_keeps_raw_tee_and_retained_pcm_unchanged",
    "activity_open_run_persistence_precedes_dependent_pcm_and_failure_never_certifies",
    "offline_observation_requires_affirmative_matching_raw_processing_authority",
    "offline_conflicts_cannot_authorize_input_but_historical_uncertainty_can",
    "offline_tap_only_observation_preserves_not_applicable_and_creates_no_derivative",
    "offline_activity_uses_pre_aec_samples_without_changing_wav_bytes_or_retry_counts",
    "offline_activity_keeps_valid_tap_history_across_mic_only_source_loss",
    "offline_activity_legacy_input_remains_unknown_and_failed_output_keeps_observations",
    "every_aec_execution_arm_keeps_disposition_and_projects_only_its_input_scope",
    "live_activity_projection_requires_actual_execution_and_keeps_short_lookahead",
    "legacy_aec_disposition_survives_refresh_without_borrowing_new_capture_observations",
    "offline_observations_survive_backend_error_and_print_retention_is_read_only",
    "activity_live_observes_raw_input_across_windows_and_refreshes_owned_note",
    "activity_live_requires_matching_terminal_authority_and_never_observes_fill",
    "activity_live_epoch_cap_is_diagnostic_and_source_projection_stays_exact",
    "activity_live_execution_refresh_requires_actual_mic_input",
    "activity_live_unknown_authority_keeps_empty_asr_note_free",
    "trace_records_selected_phrase_winners_and_utf8_output_offsets",
    "actual_correct_and_incorrect_rust_hint_reconstruct_the_original_input",
    "actual_early_echo_context_keeps_exact_raw_and_carry_parents_and_seals_bad_or_excess_links",
    "actual_poll_echo_split_child_echo_diarization_and_cleanup_keep_one_asr_lineage",
    "lineage_shared_append_and_readback_reject_lost_duplicated_fabricated_ranges_or_protection",
    "lineage_full_unchanged_duplicate_rows_keep_distinct_owners_and_empty_derived_stages_work",
    "lineage_bad_caller_map_seals_evidence_and_keeps_borrowed_outputs_and_quality",
    "lineage_raw_echo_consumes_exact_decisions_context_and_cannot_restore_a_withheld_row",
    "lineage_new_owner_and_range_vectors_obey_identical_limits_on_readback",
    "actual_word_range_emitters_match_legacy_rows_and_cover_unicode_empty_words_and_trace_cap",
    "actual_filler_trace_preserves_utf8_preimages_and_stutter_positions",
    "actual_structural_passes_keep_explicit_duplicate_row_identity",
    "actual_echo_queries_each_input_once_and_keeps_every_candidate_decision",
    "actual_critical_echo_protection_and_fallback_identity_survive_readback",
    "actual_legacy_rule_runs_once_and_seals_missing_evidence_without_losing_output",
    "actual_early_echo_retains_withheld_inputs_and_source_ordinals",
    "actual_upstream_decision_omission_seals_evidence_but_preserves_known_input",
    "actual_protection_survives_sort_merge_and_skipped_filler_pass",
    "actual_carry_uses_caller_links_or_explicit_unattributed_snapshot",
    "production_preflight_rejects_small_wire_large_owner_expansion_before_decode",
    "actual_backchannel_deletion_keeps_its_preimage_and_caller_protection",
    "actual_trace_and_capture_caps_preserve_cleanup_output_and_mark_missing_evidence",
    "actual_early_echo_rejects_ambiguous_ordinals_instead_of_matching_identical_text",
    "actual_filler_position_corpus_covers_punctuation_unicode_and_repeated_bigrams",
    "actual_early_rows_assemble_for_cleanup_without_reminting_asr_or_copying_text",
    "actual_assembly_missing_or_omitted_parents_never_change_cleanup_output",
    "production_dense_source_round_trips_at_the_same_minimum_owned_limit",
    "review_invalid_carry_seals_the_missing_suffix_across_readback_and_later_windows",
    "review_source_and_assembly_errors_before_admission_remain_durable_omissions",
    "review_late_text_trace_error_keeps_prior_stages_and_seals_only_missing_suffix",
    "review_early_echo_errors_seal_source_and_decision_omissions",
    "review_cleanup_assembly_inherits_protection_on_append_and_read_in_both_link_forms",
    "review_cleanup_assembly_rejects_minted_rows_but_retains_genuinely_empty_assembly",
    "rereview_admitted_document_reloads_with_the_same_owned_limit",
    "rereview_valid_unicode_case_expansion_from_existing_cleanup_is_recordable",
    "rereview_nested_ownership_and_settled_quality_reload_at_identical_limits",
    "rereview_external_capacity_is_not_corruption_and_invalid_groups_are_not_omitted",
    "rereview_unicode_case_mapping_keeps_positions_and_rejects_non_case_rewrites",
    "review_multiple_retained_windows_preserve_ordered_provider_ancestry",
    "review_lexicon_rejects_no_rule_id_on_append_and_read",
    "review_echo_keep_rejects_untraced_rewrite_on_append_and_read",
    "review_sort_requires_every_moved_row_decision_on_append_and_read",
    "review_tiny_wire_record_cannot_retain_unbudgeted_string_capacity",
    "review_text_pass_reason_must_match_the_recorded_pass",
    "review_valid_sort_covers_moved_rows_and_preserves_unchanged_rows",
    "rereview_time_valid_sort_still_requires_each_moved_row_decision",
    "review_merge_has_exact_structural_preimages_and_keeps_unmerged_row",
    "review_protected_echo_and_acoustic_drop_keep_exact_row_identity",
    "review_backchannel_drop_cannot_resurrect_via_a_later_parent_reference",
    "review_omitted_parent_and_assembled_group_are_unavailable_not_reminted",
    "review_fillers_allow_positioned_case_and_whitespace_without_metadata_changes",
    "review_every_nested_owned_capacity_is_budgeted_even_when_not_serialized",
    "review_owned_budget_is_exact_across_growth_and_reserves_final_quality",
    "borrowed_row_index_and_budget_fallback_preserve_unordered_ids_and_missing_rows",
    "exact_subseconds_speaker_and_track_round_trip_without_markdown",
    "source_frame_derived_timing_corpus_round_trips_every_f64_bit",
    "finite_nonnegative_timing_bits_survive_json_without_a_custom_time_representation",
    "very_very_stutter_keeps_the_deleted_position_and_preimage",
    "filler_position_and_separate_case_repair_cover_the_actual_output",
    "fully_deleted_filler_row_remains_reconstructable_without_an_empty_output_clone",
    "early_echo_region_ordinal_is_not_a_stage_local_index_or_a_u32",
    "changed_cleanup_cannot_silently_omit_its_decision_or_position_trace",
    "identical_text_and_time_in_different_windows_never_join_invocations",
    "later_provider_and_prose_invocations_link_retained_rows_without_copying_asr",
    "local_input_indices_select_the_right_identical_row_and_bad_links_are_rejected",
    "exact_escaped_byte_boundary_keeps_or_omits_the_whole_linked_group",
    "group_cannot_retain_only_its_source_when_its_linked_decision_does_not_fit",
    "final_shared_quality_fits_reserved_room_after_content_is_full_and_truncated",
    "invalid_utf8_range_unreported_edit_and_nonfinite_time_are_rejected",
    "processed_wav_cannot_claim_to_be_the_exact_live_aec_input",
    "unknown_identity_is_explicit_and_never_fabricated",
    "malformed_truncation_counts_and_future_versions_are_rejected",
    "maximum_width_truncation_metadata_fits_its_reserved_room",
    "codec_errors_are_category_only_and_do_not_expose_authored_sentinel",
    "actual_owned_and_temporary_allocations_stay_within_documented_envelopes",
)


def capture_suite_passed(output, returncode):
    summaries = re.findall(r"(?m)^test result: ([A-Za-z]+)\. (\d+) passed; (\d+) failed;.*$", output)
    return (returncode == 0 and bool(summaries)
            and all(status == "ok" and failed == "0" for status, _, failed in summaries)
            and all(re.search(r"(?m)^test (?:[^ \n]*::)?" + re.escape(name) + r" \.\.\. ok$", output)
                    for name in CAPTURE_REQUIRED_TESTS))


APP_PATH_REQUIRED_TESTS = (
    "unfiled_boundary_recovers_once_and_retains_source_intervals_after_restart",
    "production_recording_path_invokes_scripted_backend_once_and_keeps_partial_output",
    "full_production_chain_counts_backend_and_replays_checkpoint_without_audio",
    "successful_live_quality_survives_completion_replay_and_publication_failure",
    "note_less_quality_uses_existing_batch_payload_without_fallback",
)
APP_PATH_SOURCE_MODULES = {
    "pipeline", "transcribe", "jobs", "config", "private_file", "checkpoint",
    "provenance", "postprocess_app", "live",
}


def app_path_receipt(output, returncode):
    """Require every actual orchestration check and the exact extracted-source inventory."""
    summaries = re.findall(
        r"(?m)^test result: ([A-Za-z]+)\. (\d+) passed; (\d+) failed; (\d+) ignored;.*$",
        output)
    executed = re.findall(r"(?m)^test [^\n]*::([^ :]+) \.\.\. ok$", output)
    prefix = "decode-quality source sha256: "
    receipts = [json.loads(line.removeprefix(prefix)) for line in output.splitlines()
                if line.startswith(prefix)]
    if (returncode != 0 or summaries != [("ok", str(len(APP_PATH_REQUIRED_TESTS)), "0", "0")]
            or sorted(executed) != sorted(APP_PATH_REQUIRED_TESTS)
            or len(receipts) != 1 or not isinstance(receipts[0], dict)
            or receipts[0].keys() != APP_PATH_SOURCE_MODULES
            or any(not isinstance(value, str) or not re.fullmatch(r"[0-9a-f]{64}", value)
                   for value in receipts[0].values())):
        raise RuntimeError("app decode orchestration did not verify all tests and source hashes")
    return receipts[0]


EVIDENCE_SOURCE_FILES = {
    *(f"app/src/{name}.rs" for name in APP_PATH_SOURCE_MODULES | {"cli", "evidence"}),
    "app/src/evidence/integration.rs", "app/src/evidence/live.rs", "app/src/evidence/tests.rs",
    "app/tests/decode_quality/run.py", "app/tests/decode_quality/boundaries.rs",
    "app/tests/decode_quality/full_path.rs", "app/tests/decode_quality/retention.rs",
    "app/tests/evidence/run.py", "app/tests/evidence/verify_receipt.py",
    "app/tests/evidence/test_receipt.py", "app/tests/evidence/live_harness.py",
    "app/tests/evidence/retention.rs", "app/tests/evidence/batch.rs",
    "app/tests/evidence/live.rs", "app/tests/evidence/live_optout.rs", "app/tests/evidence/tracing.rs",
    "app/tests/evidence/allocations.rs", "app/tests/evidence/hosted_boundary.rs",
    "app/tests/evidence/scheduling.rs", "crates/corti-transcribe/tests/evidence_allocations.rs",
    "crates/corti-coreaudio/src/capture.rs", "crates/corti-capture/src/lib.rs",
}


def evidence_receipt(output, returncode, root):
    """Reuse the harness's named-test authority and bind its receipt to this checkout."""
    spec = importlib.util.spec_from_file_location(
        "evidence_receipt", root / "app/tests/evidence/verify_receipt.py")
    validator = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(validator)
    validator.require_complete(output, returncode)
    prefix = "evidence-persistence source sha256: "
    receipts = [json.loads(line.removeprefix(prefix)) for line in output.splitlines()
                if line.startswith(prefix)]
    if (len(receipts) != 1 or not isinstance(receipts[0], dict)
            or receipts[0].keys() != EVIDENCE_SOURCE_FILES):
        raise RuntimeError("evidence source receipt is missing, duplicate or incomplete")
    current = {name: hashlib.sha256((root / name).read_bytes()).hexdigest()
               for name in EVIDENCE_SOURCE_FILES}
    if receipts[0] != current:
        raise RuntimeError("evidence source receipt does not match this checkout")
    return receipts[0]


def capture_rate_receipt(output, returncode):
    rows = [json.loads(line) for line in output.splitlines() if line.startswith('{')]
    measurements = [row for row in rows if row.get("type") == "throughput"]
    verified = [row for row in rows if row.get("type") == "verified"]
    if (returncode != 0 or len(measurements) != 90 or len(verified) != 1
            or verified[0].get("measurements") != 90
            or verified[0].get("exact_counts") is not True
            or verified[0].get("chunk_and_repeat_checksums_match") is not True):
        raise RuntimeError("capture-rate benchmark did not verify all 90 measurements")
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model-dir", type=Path, required=True)
    parser.add_argument("--cache-dir", type=Path,
                        help="verified model archive directory; defaults to the native job sibling cache")
    parser.add_argument("--fixtures", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    args.output_dir.mkdir(parents=True, exist_ok=True)
    fixture_set = json.loads(args.fixtures.read_text())
    fixtures = fixture_set["fixtures"]
    first = next(f for f in fixtures if f["language"] == "en" and f["sample_rate"] == 16000)
    result_path = args.output_dir / "metal.txt"
    env = dict(os.environ, CORTI_VERIFY_MODEL_DIR=str(args.model_dir.resolve()),
               CORTI_VERIFY_WAV=str(Path(first["path"]).resolve()),
               CORTI_VERIFY_METAL_RESULT=str(result_path.resolve()))
    results = []
    resample_comparison = []
    capture_rate = []
    app_path_sources = {}
    evidence_sources = {}

    def run_test(name, label, features=(), release=False):
        command = ["cargo", "test", "--locked", "--lib", "-p", "corti-transcribe-local"]
        if release:
            command.append("--release")
        if features:
            command += ["--features", ",".join(features)]
        command += [name, "--", "--ignored", "--exact", "--nocapture"]
        print("+ " + " ".join(command), flush=True)
        result = subprocess.run(command, env=env, text=True, stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT)
        print(result.stdout, flush=True)
        (args.output_dir / f"{label}.log").write_text(result.stdout)
        passed = single_test_passed(result.stdout, result.returncode)
        results.append({"check": label, "passed": passed})
        if not passed:
            raise RuntimeError(f"native check failed or did not execute exactly one test: {label}")
        return result.stdout

    metal = "not probed"
    failed = False
    try:
        boundary_output = args.output_dir / "boundary"
        boundary_fixtures = boundary_output / "fixtures"
        cache = args.cache_dir or args.model_dir.parent / "quality-model-archives"
        for label, command in (
            ("boundary-generate", [sys.executable, "scripts/quality/boundary/generate.py",
                "--output-dir", str(boundary_fixtures)]),
            ("boundary-native-matrix", [sys.executable, "scripts/quality/boundary/run.py",
                "--repo", str(Path.cwd()), "--seal", str(boundary_fixtures / "boundary-fixtures.json"),
                "--model-dir", str(args.model_dir), "--cache-dir", str(cache),
                "--output-dir", str(boundary_output)]),
        ):
            print("+ " + " ".join(command), flush=True)
            boundary = subprocess.run(command, env=env, text=True, stdout=subprocess.PIPE,
                                      stderr=subprocess.STDOUT)
            print(boundary.stdout, flush=True)
            (args.output_dir / f"{label}.log").write_text(boundary.stdout)
            passed = boundary.returncode == 0
            if label == "boundary-native-matrix":
                passed = passed and re.findall(r"(?m)^BOUNDARY_MATRIX (PASS|FAIL)$", boundary.stdout) == ["PASS"]
            results.append({"check": label, "passed": passed})
            if not passed:
                raise RuntimeError(f"authored boundary fixture or all-run word gate failed: {label}")
        command = ["cargo", "test", "--locked", "-p", "corti-core", "-p", "corti-coreaudio",
                   "-p", "corti-capture", "-p", "corti-detect", "-p", "corti-app",
                   "-p", "corti-jobs", "-p", "corti-queue", "-p", "corti-transcribe",
                   "-p", "corti-lexicon", "-p", "corti-transcribe-local", "-p", "corti-tap"]
        print("+ " + " ".join(command), flush=True)
        capture_tests = subprocess.run(command, env=env, text=True, stdout=subprocess.PIPE,
                                       stderr=subprocess.STDOUT)
        print(capture_tests.stdout, flush=True)
        (args.output_dir / "capture-quality-native.log").write_text(capture_tests.stdout)
        passed = capture_suite_passed(capture_tests.stdout, capture_tests.returncode)
        results.append({"check": "capture-quality-native-suites", "passed": passed})
        if not passed:
            raise RuntimeError("capture-quality native suites failed or required tests did not execute")
        for feature, required in (("live", TAP_LIVE_REQUIRED_TESTS),
                                  ("inbox-local", TAP_INBOX_REQUIRED_TESTS)):
            label = f"tap-{feature}-only"
            for verb in ("clippy", "test"):
                command = ["cargo", verb, "--locked", "-p", "corti-tap", "--bin", "corti-tap",
                           "--no-default-features", "--features", feature]
                if verb == "clippy":
                    command += ["--tests", "--", "-D", "warnings"]
                print("+ " + " ".join(command), flush=True)
                check = subprocess.run(command, env=env, text=True, stdout=subprocess.PIPE,
                                       stderr=subprocess.STDOUT)
                print(check.stdout, flush=True)
                (args.output_dir / f"{label}-{verb}.log").write_text(check.stdout)
                passed = (check.returncode == 0 if verb == "clippy"
                          else named_tests_passed(check.stdout, check.returncode, required))
                results.append({"check": f"{label}-{verb}", "passed": passed})
                if not passed:
                    raise RuntimeError(f"feature-specific check failed or did not execute its inventory: {label}-{verb}")
        command = ["cargo", "test", "--locked", "--lib", "-p", "corti-transcribe-local",
                   "--features", "stream", "live::stream::tests::"]
        print("+ " + " ".join(command), flush=True)
        stream_tests = subprocess.run(command, env=env, text=True, stdout=subprocess.PIPE,
                                      stderr=subprocess.STDOUT)
        print(stream_tests.stdout, flush=True)
        (args.output_dir / "live-word-stream-feature.log").write_text(stream_tests.stdout)
        passed = named_tests_passed(stream_tests.stdout, stream_tests.returncode, STREAM_REQUIRED_TESTS)
        results.append({"check": "live-word-stream-feature", "passed": passed})
        if not passed:
            raise RuntimeError("stream feature tests failed or did not execute their exact inventory")
        command = [sys.executable, "app/tests/decode_quality/run.py", "--clippy"]
        print("+ " + " ".join(command), flush=True)
        app_path = subprocess.run(command, env=env, text=True, stdout=subprocess.PIPE,
                                  stderr=subprocess.STDOUT)
        print(app_path.stdout, flush=True)
        (args.output_dir / "decode-quality-app-path.log").write_text(app_path.stdout)
        app_path_sources = app_path_receipt(app_path.stdout, app_path.returncode)
        results.append({"check": "decode-quality-app-orchestration", "passed": True})
        for label, command in (
            ("evidence-receipt-controls", [sys.executable, "app/tests/evidence/test_receipt.py"]),
            ("evidence-app-orchestration", [sys.executable, "app/tests/evidence/run.py", "--clippy",
                "--output-dir", str(args.output_dir / "evidence-harness")]),
        ):
            # Evidence persistence is required even when the later Metal probe is unavailable.
            check = {"check": label, "passed": False}
            results.append(check)
            print("+ " + " ".join(command), flush=True)
            evidence = subprocess.run(command, env=env, text=True, stdout=subprocess.PIPE,
                                      stderr=subprocess.STDOUT)
            print(evidence.stdout, flush=True)
            (args.output_dir / f"{label}.log").write_text(evidence.stdout)
            if label == "evidence-app-orchestration":
                evidence_sources = evidence_receipt(evidence.stdout, evidence.returncode, Path.cwd())
            elif (evidence.returncode != 0
                  or re.findall(r"(?m)^Ran (\d+) tests? in .+$", evidence.stdout) != ["2"]
                  or re.findall(r"(?m)^OK.*$", evidence.stdout) != ["OK"]):
                raise RuntimeError("evidence receipt controls failed or did not execute both tests")
            check["passed"] = True
        run_test("ggml::tests::quality_probe_metal", "metal-probe", ("ggml",))
        metal = result_path.read_text().strip()
        if metal not in {"available", "unavailable"}:
            raise RuntimeError("Metal probe returned no recognized status")
        engines = ["sherpa"] + (["ggml"] if metal == "available" else [])
        for engine in engines:
            env["CORTI_VERIFY_ASR_ENGINE"] = engine
            # The wrapper fails only the middle of three real Silero regions. The selected real
            # decoder still supplies the exact first/third words and timestamps on this fixture.
            env["CORTI_VERIFY_WAV"] = str(Path(first["path"]).resolve())
            features = ("ggml",) if engine == "ggml" else ()
            run_test("live::tests::native_middle_region_failure_preserves_exact_neighbors",
                     f"{engine}-middle-region-failure", features)
            for fixture in fixtures:
                env["CORTI_VERIFY_WAV"] = str(Path(fixture["path"]).resolve())
                label = f"{engine}-{fixture['language']}-{fixture['sample_rate']}"
                features = ("ggml",) if engine == "ggml" else ()
                run_test("live::tests::live_equals_batch_over_chunking", label + "-chunking", features)
                if fixture["sample_rate"] != 16000:
                    run_test("live::tests::live_survives_sample_rate_switch", label + "-rate", features)
                if engine == "ggml":
                    run_test("live::tests::ggml_checkpoint_keeps_call_relative_timestamps",
                             label + "-checkpoint", features)
        comparison_output = run_test(
            "resample::comparison::benchmark_production_against_borrowed_packets",
            "resampler-buffering", release=True,
        )
        for line in comparison_output.splitlines():
            start = line.find('{"resample_comparison":')
            if start >= 0:
                resample_comparison.append(json.loads(line[start:])["resample_comparison"])
        if not resample_comparison:
            raise RuntimeError("resampler comparison produced no measurements")
        # Compile before timing. Only the authored, model-free benchmark process is measured by time;
        # its JSON rows record paired order and variance inputs, with no machine-speed pass threshold.
        build = subprocess.run(
            ["cargo", "build", "--locked", "-p", "corti-core", "--release", "--example",
             "capture_rate_bench", "--message-format=json"], env=env, text=True,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        print(build.stderr, flush=True)
        (args.output_dir / "capture-rate-build.log").write_text(build.stdout + build.stderr)
        executable = next((row.get("executable") for line in build.stdout.splitlines()
                           if line.startswith('{') and (row := json.loads(line)).get("reason") == "compiler-artifact"
                           and row.get("target", {}).get("name") == "capture_rate_bench"
                           and row.get("executable")), None)
        if build.returncode != 0 or not executable:
            raise RuntimeError("capture-rate benchmark compilation failed")
        measured = subprocess.run(["/usr/bin/time", "-l", executable, "--quick"], env=env,
                                  text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        print(measured.stdout, flush=True)
        (args.output_dir / "capture-rate.log").write_text(measured.stdout)
        capture_rate = capture_rate_receipt(measured.stdout, measured.returncode)
        results.append({"check": "capture-rate-throughput", "passed": True})
    except (OSError, ValueError, RuntimeError, StopIteration) as error:
        print(f"::error::{error}", flush=True)
        failed = True
    finally:
        report = {"metal": metal, "spanish_available": fixture_set["spanish_available"],
                  "results": results, "resample_comparison": resample_comparison,
                  "capture_rate": capture_rate, "app_path_source_sha256": app_path_sources,
                  "evidence_source_sha256": evidence_sources}
        (args.output_dir / "summary.json").write_text(json.dumps(report, indent=2) + "\n")
        summary = os.environ.get("GITHUB_STEP_SUMMARY")
        if summary:
            with open(summary, "a", encoding="utf-8") as output:
                output.write(f"Metal decode: {metal}; Spanish voice: "
                             f"{'available' if report['spanish_available'] else 'unavailable'}; "
                             "full voice list is in Generate synthetic speech logs.\n\n")
                voices = sorted({(f["language"], f["voice"], f["locale"]) for f in fixtures})
                for language, voice, locale in voices:
                    output.write(f"- Voice {language}: {voice} ({locale})\n")
                for result in results:
                    output.write(f"- {result['check']}: {'pass' if result['passed'] else 'FAIL'}\n")
                if resample_comparison:
                    output.write("\nOptimized-build resampler comparison (model-free; "
                                 "native allocations and RSS are not measured):\n\n```json\n")
                    for row in resample_comparison:
                        output.write(json.dumps(row, separators=(",", ":")) + "\n")
                    output.write("```\n")
                if capture_rate:
                    output.write("\nCapture-rate normalizer: 90 verified synthetic measurements; "
                                 "release build, unchanged settings, no speed threshold. "
                                 "JSON rows and benchmark-only macOS memory/time receipts are in "
                                 "capture-rate.log and the job log. HAL, AEC and disk are excluded.\n\n```json\n")
                    for row in capture_rate:
                        output.write(json.dumps(row, separators=(",", ":")) + "\n")
                    output.write("```\n")
    return int(failed)


if __name__ == "__main__":
    sys.exit(main())
