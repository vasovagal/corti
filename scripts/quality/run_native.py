#!/usr/bin/env python3
"""Run exact native checks against authored synthetic fixtures, never user audio."""
import argparse
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
)


def capture_suite_passed(output, returncode):
    summaries = re.findall(r"(?m)^test result: ([A-Za-z]+)\. (\d+) passed; (\d+) failed;.*$", output)
    return (returncode == 0 and bool(summaries)
            and all(status == "ok" and failed == "0" for status, _, failed in summaries)
            and all(re.search(r"(?m)^test [^\n]*::" + re.escape(name) + r" \.\.\. ok$", output)
                    for name in CAPTURE_REQUIRED_TESTS))


APP_PATH_REQUIRED_TESTS = (
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
        command = ["cargo", "test", "--locked", "-p", "corti-core", "-p", "corti-coreaudio",
                   "-p", "corti-capture", "-p", "corti-detect", "-p", "corti-app",
                   "-p", "corti-jobs", "-p", "corti-queue", "-p", "corti-transcribe",
                   "-p", "corti-transcribe-local", "-p", "corti-tap"]
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
                  "capture_rate": capture_rate, "app_path_source_sha256": app_path_sources}
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
