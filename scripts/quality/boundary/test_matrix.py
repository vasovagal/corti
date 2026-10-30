import copy
import hashlib
import json
from pathlib import Path
import unittest

from coverage import CoverageError
from matrix import (ACCEPTED_GUARD, FORCED_REPLAY_PREROLL, critical_crossings, diagnose, evaluate,
                    expected_runtime, replay_origin, unique_index)

RUNTIME_IDENTITY = {"synthetic_runner": "known"}


def synthetic_complete_run():
    definition = json.loads(Path(__file__).with_name("fixtures.json").read_text())
    fixtures, baselines = {}, {}
    for cell in definition["matrix"]:
        for track in ("mic", "tap"):
            identifier = f"{cell['waveform']}-{cell['sample_rate']}-{track}"
            shift = 0.4 if track == "tap" else 0
            values = [("Nora", 3, 4.5), ("Ada", 5.5, 6.7), ("not", 8, 10), ("1500", 13, 15)]
            words = [{"text": text, "start": start + shift, "end": end + shift}
                     for text, start, end in values]
            rate = cell["sample_rate"]
            annotations = [{"id": kind, "authored": [text], "alternatives": [[text]],
                            "source_window": [0, 100 * rate]}
                           for kind, (text, _, _) in zip(("name-nora", "name-ada", "negation", "amount"), values)]
            fixtures[identifier] = {"id": identifier, "sha256": "fixture-" + identifier,
                                    "sample_rate": rate, "frames": 100 * rate, "occurrences": annotations}
            baselines[identifier] = {"id": identifier, "pcm_sha256": "fixture-" + identifier,
                                     "batch_words": words, "uncut_live_words": copy.deepcopy(words),
                                     "decode_complete": True}
    for identifier, baseline in baselines.items():
        fixture = fixtures[identifier]
        native = {}
        for role in ("batch", "uncut"):
            inputs, position, index = [], 0, 0
            while position < fixture["frames"]:
                count = fixture["frames"] if role == "batch" else definition["input_chunks"][index % len(definition["input_chunks"])]
                end = min(position + count, fixture["frames"])
                inputs.append({"start": position, "end": end, "sample_rate": fixture["sample_rate"]})
                position, index = end, index + 1
            native[role] = {"words": copy.deepcopy(baseline["batch_words"]), "decode_complete": True,
                "pcm_exact_and_once": True, "omitted_failures": 0, "omitted_metadata": 0,
                "counter_saturated": False, "inputs": inputs,
                "ownership": [{"start": 0, "end": fixture["frames"], "eof": True}],
                "quality": [{"start": 0, "end": fixture["frames"], "status": "Complete"}],
                "attempts": [{"start": 0, "end": 1600000, "pcm_fnv64": "0000000000000000"}]}
        baseline["native"] = native
    cells = []
    for spec in definition["matrix"]:
        rate = spec["sample_rate"]
        runtime = expected_runtime(spec, definition["input_chunks"])
        boundaries, inputs = [], []
        received, step, origin, due, pending, explicit = 0, 0, 0, runtime["window_frames"] - runtime["phase_frames"], None, 0
        retained = 0
        fixed = [value * rate // 1000 for value in spec.get("forced_source_ms", [])]
        while received < 100 * rate:
            accepted = (received * 16000 // rate) // 512 * 512
            remaining = runtime["epoch_limit"] - (accepted - origin)
            if pending and received >= pending["cut_frame"] + rate * 2 // 5:
                pending["settled_received_frame"] = received
                pending["settled_canonical_accepted"] = [accepted] * 2
                boundaries.append(pending)
                origin = retained = replay_origin(
                    retained, (pending["cut_frame"] * 16000 + rate - 1) // rate, FORCED_REPLAY_PREROLL)
                pending = None
                explicit += 1
                continue
            select_explicit = spec["mode"] == "explicit-forced-probe" and explicit < len(fixed) and received == fixed[explicit]
            select_natural = spec["mode"] != "explicit-forced-probe" and received >= due + 2 * rate
            if not pending and (select_explicit or select_natural):
                cut = fixed[explicit] if select_explicit else due + rate
                maintenance = remaining <= runtime["maintenance_trigger"]
                event = {"kind": "explicit" if select_explicit else "natural",
                         "nominal_due_frame": runtime["window_frames"] if select_explicit else due,
                         "cut_frame": cut, "selected_received_frame": received,
                         "settled_received_frame": received, "maintenance_reset": maintenance,
                         "epoch_remaining": [remaining] * 2, "canonical_accepted": [accepted] * 2,
                         "epoch_origins": [origin] * 2, "settled_canonical_accepted": [accepted] * 2}
                if select_explicit:
                    pending = event
                else:
                    boundaries.append(event)
                    due = cut + runtime["window_frames"]
                    if maintenance:
                        origin = retained = replay_origin(retained, accepted - ACCEPTED_GUARD, 0)
                    else:
                        retained = max(accepted - ACCEPTED_GUARD, 0)
                continue
            count = min(definition["input_chunks"][step % len(definition["input_chunks"])], 100 * rate - received)
            if pending:
                count = min(count, pending["cut_frame"] + runtime["settlement_source_reserve"] - received)
            elif spec["mode"] == "explicit-forced-probe":
                if explicit < len(fixed):
                    count = min(count, fixed[explicit] - received)
            else:
                count = min(count, (due if received < due else due + 22 * rate) - received)
            assert count > 0
            inputs.append({"start": received, "end": received + count, "sample_rate": rate})
            received += count
            step += 1
        tracks = {}
        for track in ("mic", "tap"):
            identifier = f"{spec['waveform']}-{rate}-{track}"
            tracks[track] = {"fixture_id": identifier, "pcm_sha256": "fixture-" + identifier,
                             "words": copy.deepcopy(baselines[identifier]["batch_words"]),
                             "decode_complete": True, "pcm_exact_and_once": True,
                             "omitted_failures": 0, "omitted_metadata": 0, "counter_saturated": False,
                             "inputs": copy.deepcopy(inputs),
                             "quality": [{"start": 0, "end": 100 * rate, "status": "Complete"}],
                             "attempts": [{"start": 0, "end": 100 * 16000, "pcm_fnv64": "0000000000000000"}]}
            endpoints = [0, *[event["cut_frame"] for event in boundaries], 100 * rate]
            tracks[track]["ownership"] = [{"start": start, "end": end, "eof": index == len(endpoints) - 2}
                                          for index, (start, end) in enumerate(zip(endpoints, endpoints[1:]))]
        cells.append({"id": spec["id"], "runtime": runtime, "boundaries": boundaries, "tracks": tracks})
    seal = {"definition": definition, "definition_sha256": "synthetic-definition",
            "pre_inference_seal": True, "synthetic_only": True, "fixtures": list(fixtures.values())}
    seal_digest = hashlib.sha256((json.dumps(seal, indent=2, sort_keys=True) + "\n").encode()).hexdigest()
    result = {"definition_sha256": "synthetic-definition", "fixture_seal_sha256": seal_digest,
              "engine": "sherpa", "verified_models": {"vad": "pinned", "asr": "pinned"},
              "runtime_identity": copy.deepcopy(RUNTIME_IDENTITY), "input_chunks": definition["input_chunks"],
              "baselines": list(baselines.values()), "cells": cells}
    return seal, result


class MatrixTests(unittest.TestCase):
    def test_complete_matrix_scores_every_track_and_its_actual_baseline(self):
        seal, result = synthetic_complete_run()
        scored = evaluate(seal, result, result["verified_models"], RUNTIME_IDENTITY)
        self.assertTrue(scored["passed"])
        self.assertEqual(len(scored["cells"]), 26)
        self.assertEqual(len(scored["baseline_scores"]), 10)

    def forced_cell(self, result):
        return next(cell for cell in result["cells"] if cell["id"] == "continuous-forced-fence")

    def scored_forced(self, seal, result):
        scored = evaluate(seal, result, result["verified_models"], RUNTIME_IDENTITY)
        return scored, [row for row in scored["cells"] if row["id"] == "continuous-forced-fence"]

    def test_forced_seam_damage_is_reported_degraded_not_silently_passed(self):
        seal, result = synthetic_complete_run()
        # "Ada" (5.5-6.7 s) straddles the forced cut at 6.25 s.
        self.forced_cell(result)["tracks"]["mic"]["words"][1]["text"] = "Ad"
        scored, rows = self.scored_forced(seal, result)
        mic = next(row for row in rows if row["track"] == "mic")
        self.assertTrue(mic["passed"])
        self.assertIn("name-ada", mic["forced_gate"]["degraded_critical"])
        self.assertTrue(scored["passed"])
        # The same damage at a natural cut still fails the strict gate.
        result["cells"][0]["tracks"]["mic"]["words"][1]["text"] = "Ad"
        self.assertFalse(evaluate(seal, result, result["verified_models"], RUNTIME_IDENTITY)["passed"])

    def test_forced_cell_still_fails_on_loss_away_from_its_seams(self):
        seal, result = synthetic_complete_run()
        del self.forced_cell(result)["tracks"]["mic"]["words"][0]  # "Nora", 3-4.5 s
        scored, rows = self.scored_forced(seal, result)
        mic = next(row for row in rows if row["track"] == "mic")
        self.assertFalse(mic["passed"])
        self.assertEqual(mic["forced_gate"]["lost_critical"], ["name-nora"])
        self.assertFalse(scored["passed"])

    def test_forced_seam_edits_are_capped(self):
        seal, result = synthetic_complete_run()
        words = self.forced_cell(result)["tracks"]["mic"]["words"]
        words[1:1] = [{"text": "um", "start": 5.5, "end": 5.5}] * 5
        scored, rows = self.scored_forced(seal, result)
        self.assertFalse(next(row for row in rows if row["track"] == "mic")["passed"])

    def test_forced_gate_away_from_seams_allows_only_formatting_equivalents(self):
        from coverage import compare_cell
        from matrix import forced_gate

        def words(text):
            return [{"text": word, "start": index, "end": index + 0.5}
                    for index, word in enumerate(text.split())]

        reference = words("Nora has 1,500 euros and not more today")
        annotations = [{"id": "amount", "authored": ["1,500", "euros"],
                        "alternatives": [["1,500", "euros"]], "source_window": [0, 10000]},
                       {"id": "negation", "authored": ["not"], "alternatives": [["not"]],
                        "source_window": [0, 10000]}]
        cuts = [50.0]  # far from every word

        def gate(text):
            score = compare_cell(reference, words(text), annotations, 10000, 1000, ("Nora",))
            return forced_gate(score, reference, words(text), cuts)

        self.assertTrue(gate("Nora has 1500€ and not more today")["passed"])
        self.assertTrue(gate("Nora has 1,500 Euros, and not more today")["passed"])
        self.assertEqual(gate("Nora has 1,600 euros and not more today")["changed_text"],
                         {"amount": "1,500 euros -> 1,600 euros"})
        for changed in ("Nora has 1,500 euros and dot more today",   # negation flipped
                        "Nora has 1,600 euros and not more today",   # value changed
                        "Nora has 1,500 euros and not never more today",  # negator introduced
                        "Nora has 1,500 euros and not more 7 today"):  # digit introduced
            with self.subTest(changed=changed):
                self.assertFalse(gate(changed)["passed"])

    def test_formatting_key_folds_unit_abbreviation_but_not_unit_case(self):
        from matrix import formatting_key
        self.assertEqual(formatting_key("50mm."), formatting_key("50 millimeters"))
        self.assertNotEqual(formatting_key("50 mM"), formatting_key("50 millimeters"))
        self.assertNotEqual(formatting_key("15,000 euros"), formatting_key("1500 euros"))

    def test_forced_cell_fails_on_a_negation_flip_away_from_its_seams(self):
        seal, result = synthetic_complete_run()
        self.forced_cell(result)["tracks"]["mic"]["words"][2]["text"] = "dot"  # "not", 8-10 s
        scored, rows = self.scored_forced(seal, result)
        self.assertFalse(next(row for row in rows if row["track"] == "mic")["passed"])

    def test_one_failed_cell_cannot_be_averaged_into_success(self):
        seal, result = synthetic_complete_run()
        result["cells"][0]["tracks"]["mic"]["words"][3]["text"] = "15000"
        scored = evaluate(seal, result, result["verified_models"], RUNTIME_IDENTITY)
        self.assertFalse(scored["passed"])
        self.assertEqual(sum(not row["passed"] for row in scored["cells"]), 1)

    def test_incomplete_decode_or_missing_track_cannot_report_success(self):
        seal, result = synthetic_complete_run()
        result["cells"][0]["tracks"]["mic"]["decode_complete"] = False
        self.assertFalse(evaluate(seal, result, result["verified_models"], RUNTIME_IDENTITY)["passed"])
        result["cells"][0]["tracks"].pop("tap")
        with self.assertRaises(CoverageError):
            evaluate(seal, result, result["verified_models"], RUNTIME_IDENTITY)

    def test_required_forced_maintenance_and_natural_modes_cannot_be_skipped(self):
        for cell_id in ("paused-16-phase0", "continuous-forced-fence", "paused-maintenance"):
            seal, result = synthetic_complete_run()
            cell = next(row for row in result["cells"] if row["id"] == cell_id)
            for event in cell["boundaries"]:
                event["kind"] = "text_cap"
                event["maintenance_reset"] = False
            with self.subTest(cell=cell_id), self.assertRaises(CoverageError):
                evaluate(seal, result, result["verified_models"], RUNTIME_IDENTITY)

    def test_missing_critical_crossing_is_a_coverage_failure_not_a_good_score(self):
        seal, result = synthetic_complete_run()
        for cell in result["cells"]:
            if cell["id"].startswith("paused-") and cell["id"] != "paused-maintenance":
                for event in cell["boundaries"]:
                    event["nominal_due_frame"] += 20 * seal["definition"]["matrix"][
                        next(index for index, spec in enumerate(seal["definition"]["matrix"])
                             if spec["id"] == cell["id"])]["sample_rate"]
                    event["cut_frame"] = event["nominal_due_frame"] + 1
                    event["settled_received_frame"] = event["cut_frame"] + 1
        with self.assertRaises(CoverageError):
            evaluate(seal, result, result["verified_models"], RUNTIME_IDENTITY)

    def test_fixture_or_model_identity_mismatch_refuses(self):
        for field in ("definition_sha256", "fixture_seal_sha256"):
            seal, result = synthetic_complete_run()
            result[field] = "wrong"
            with self.subTest(field=field), self.assertRaises(CoverageError):
                evaluate(seal, result, {"vad": "pinned", "asr": "pinned"}, RUNTIME_IDENTITY)
        seal, result = synthetic_complete_run()
        with self.assertRaises(CoverageError):
            evaluate(seal, result, {"vad": "different", "asr": "pinned"}, RUNTIME_IDENTITY)

    def test_missing_extra_duplicate_or_zero_cells_never_pass(self):
        for rows in ([], [{"id": "a"}], [{"id": "a"}, {"id": "a"}],
                     [{"id": "a"}, {"id": "b"}, {"id": "c"}]):
            with self.subTest(rows=rows), self.assertRaises(CoverageError):
                unique_index(rows, {"a", "b"}, "cell")
        self.assertEqual(set(unique_index([{"id": "a"}, {"id": "b"}], {"a", "b"}, "cell")), {"a", "b"})

    def test_boundary_crossing_uses_source_span_not_word_presence(self):
        words = [{"text": "one thousand", "start": 1.0, "end": 2.0},
                 {"text": "five hundred", "start": 2.0, "end": 3.0}]
        binding = [{"id": "0:budget:amount", "indices": (0, 1, 2, 3)}]
        self.assertEqual(critical_crossings(binding, words, 2500, 1000),
                         [{"id": "0:budget:amount", "category": "number"}])
        for frame in (0, 1000, 3000, 4000):
            with self.subTest(frame=frame):
                self.assertEqual(critical_crossings(binding, words, frame, 1000), [])

    def test_ignored_phase_shortened_deadline_and_unrequested_reset_refuse(self):
        for mutation in ("phase", "deadline", "reset", "settlement"):
            seal, result = synthetic_complete_run()
            event = result["cells"][1]["boundaries"][0]
            if mutation == "phase":
                event["nominal_due_frame"] += 1600
            elif mutation == "deadline":
                event["kind"] = "deadline"
            elif mutation == "reset":
                event["maintenance_reset"] = True
            else:
                result["cells"][1]["boundaries"][1]["settled_received_frame"] = 0
            with self.subTest(mutation=mutation), self.assertRaises(CoverageError):
                evaluate(seal, result, result["verified_models"], RUNTIME_IDENTITY)

    def test_boundary_local_diagnostics_reuse_whole_run_alignment(self):
        seal, result = synthetic_complete_run()
        scored = evaluate(seal, result, result["verified_models"], RUNTIME_IDENTITY)
        row = scored["cells"][0]
        for local in row["boundary_local"]:
            for step in local["alignment"]:
                self.assertIn(step, row["score"]["score"]["alignment"])
                self.assertIn(step[1], local["reference_token_indices"])

    def test_missing_history_post_eof_and_unbounded_settlement_refuse(self):
        for mutation in ("missing", "post_eof", "unbounded", "ownership", "admission", "rate", "chunks", "identity"):
            seal, result = synthetic_complete_run()
            cell = next(row for row in result["cells"] if row["id"] == "continuous-phase0")
            if mutation == "missing":
                cell["boundaries"] = cell["boundaries"][:1]
            elif mutation == "post_eof":
                cell["boundaries"][-1]["cut_frame"] = 101 * 16000
                cell["boundaries"][-1]["settled_received_frame"] = 102 * 16000
            elif mutation == "unbounded":
                cell["boundaries"][0]["settled_received_frame"] += 1000000000
            elif mutation == "ownership":
                cell["tracks"]["mic"]["ownership"].pop()
            elif mutation == "admission":
                cell["tracks"]["tap"]["inputs"].pop(1)
            elif mutation == "rate":
                cell["runtime"]["sample_rate"] = 8000
            elif mutation == "chunks":
                result["input_chunks"] = [1]
            else:
                result["runtime_identity"] = {"synthetic_runner": "wrong"}
            with self.subTest(mutation=mutation), self.assertRaises(CoverageError):
                evaluate(seal, result, result["verified_models"], RUNTIME_IDENTITY)

    def test_frozen_budget_values_protect_each_euro_unit(self):
        from coverage import compare_cell
        definition = json.loads(Path(__file__).with_name("fixtures.json").read_text())
        budget = next(row for row in definition["sentences"] if row["id"] == "budget")
        words = [{"text": text, "start": index, "end": index + 0.8}
                 for index, text in enumerate(budget["text"].split())]
        annotations = [{**row, "source_window": [0, 20 * 16000]} for row in budget["critical"]]
        lexemes = definition["comparison"]["extra_critical_lexemes"]
        for unit in ("euros", "euros."):
            for replacement in (None, "dollars"):
                candidate = copy.deepcopy(words)
                index = next(index for index, word in enumerate(candidate) if word["text"] == unit)
                if replacement is None:
                    candidate.pop(index)
                else:
                    candidate[index]["text"] = replacement
                with self.subTest(unit=unit, replacement=replacement):
                    self.assertFalse(compare_cell(words, candidate, annotations, 20 * 16000, 16000, lexemes)["score"]["critical_gate"])

    def test_actual_chunk_cycle_cannot_be_replaced_by_contiguous_fixed_pushes(self):
        seal, result = synthetic_complete_run()
        track = result["cells"][0]["tracks"]["mic"]
        track["inputs"] = [{"start": start, "end": min(start + 4096, 1600000), "sample_rate": 16000}
                           for start in range(0, 1600000, 4096)]
        with self.assertRaisesRegex(CoverageError, "actual native pushes"):
            evaluate(seal, result, result["verified_models"], RUNTIME_IDENTITY)

    def test_attempts_require_exact_typed_source_outcomes(self):
        for mutation in ("missing", "duplicate", "failed", "unbound", "unknown"):
            seal, result = synthetic_complete_run()
            track = result["cells"][0]["tracks"]["mic"]
            if mutation == "missing":
                track["quality"] = []
            elif mutation == "duplicate":
                track["quality"] *= 2
            elif mutation == "failed":
                track["quality"][0]["status"] = "Failed(NoResult)"
            elif mutation == "unbound":
                track["quality"][0]["end"] = 1
            else:
                track["unreviewed_proof"] = True
            with self.subTest(mutation=mutation), self.assertRaises(CoverageError):
                evaluate(seal, result, result["verified_models"], RUNTIME_IDENTITY)

    def test_maintenance_headroom_is_bound_to_real_admission_and_reset_origin(self):
        for mutation in ("early", "origin", "accepted", "selection"):
            seal, result = synthetic_complete_run()
            cell = next(row for row in result["cells"] if row["id"] == "paused-maintenance")
            event = cell["boundaries"][0]
            if mutation == "early":
                event["maintenance_reset"] = True
                event["epoch_remaining"] = [cell["runtime"]["maintenance_trigger"]] * 2
            elif mutation == "origin":
                event["epoch_origins"] = [1, 1]
            elif mutation == "accepted":
                event["canonical_accepted"] = [1600001, 1600001]
            else:
                event["selected_received_frame"] += 1
            with self.subTest(mutation=mutation), self.assertRaises(CoverageError):
                evaluate(seal, result, result["verified_models"], RUNTIME_IDENTITY)

    def test_batch_and_uncut_controls_require_their_own_native_evidence(self):
        for role in ("batch", "uncut"):
            seal, result = synthetic_complete_run()
            result["baselines"][0]["native"][role]["quality"] = []
            with self.subTest(role=role), self.assertRaises(CoverageError):
                evaluate(seal, result, result["verified_models"], RUNTIME_IDENTITY)


    def test_failure_report_names_unbound_occurrence_and_cut_context(self):
        seal, result = synthetic_complete_run()
        baseline = result["baselines"][0]
        for words in (baseline["batch_words"], baseline["uncut_live_words"],
                      baseline["native"]["batch"]["words"], baseline["native"]["uncut"]["words"]):
            words[1]["text"] = "Eda"
        with self.assertRaises(CoverageError):
            evaluate(seal, result, result["verified_models"], RUNTIME_IDENTITY)
        report = "\n".join(diagnose(seal, result))
        self.assertIn(f"baseline {baseline['id']}: unbound name-ada", report)
        self.assertIn("Eda", report)
        forced = next(cell for cell in result["cells"] if cell["id"] == "continuous-forced-fence")
        forced["tracks"]["tap"]["words"][1]["text"] = "Ad"
        report = "\n".join(diagnose(seal, result))
        self.assertIn("cell continuous-forced-fence/tap", report)
        self.assertIn("explicit cut", report)
        self.assertIn("hyp: Nora Ad", report)

    def test_failure_report_never_raises_on_malformed_evidence(self):
        seal, result = synthetic_complete_run()
        del result["cells"][0]["tracks"]
        result["baselines"][1]["batch_words"] = None
        self.assertTrue(any("unreadable" in line for line in diagnose(seal, result)))


if __name__ == "__main__":
    unittest.main()
