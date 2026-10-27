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


CAPTURE_REQUIRED_TESTS = (
    "delayed_mic_poll_keeps_critical_words_after_temporal_source_eviction",
    "temporal_source_eviction_still_allows_later_acoustic_echo_deletion",
    "rate_transition_with_invalid_sample_clock_reanchors_without_fabricated_loss",
    "reanchored_sample_clock_after_rate_transition_preserves_later_exact_gap",
    "invalid_sample_rate_transition_preserves_real_host_gap_and_offset",
    "same_rate_invalid_sample_clock_keeps_anchor_across_layout_change",
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
                   "-p", "corti-jobs", "-p", "corti-queue"]
        print("+ " + " ".join(command), flush=True)
        capture_tests = subprocess.run(command, env=env, text=True, stdout=subprocess.PIPE,
                                       stderr=subprocess.STDOUT)
        print(capture_tests.stdout, flush=True)
        (args.output_dir / "capture-quality-native.log").write_text(capture_tests.stdout)
        passed = capture_suite_passed(capture_tests.stdout, capture_tests.returncode)
        results.append({"check": "capture-quality-native-suites", "passed": passed})
        if not passed:
            raise RuntimeError("capture-quality native suites failed or required tests did not execute")
        run_test("ggml::tests::quality_probe_metal", "metal-probe", ("ggml",))
        metal = result_path.read_text().strip()
        if metal not in {"available", "unavailable"}:
            raise RuntimeError("Metal probe returned no recognized status")
        engines = ["sherpa"] + (["ggml"] if metal == "available" else [])
        for engine in engines:
            env["CORTI_VERIFY_ASR_ENGINE"] = engine
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
                  "capture_rate": capture_rate}
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
