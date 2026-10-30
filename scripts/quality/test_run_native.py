"""A native probe passes only when exactly its intended test executed."""
import unittest
from unittest.mock import patch
from contextlib import redirect_stdout
import hashlib
import io
import json
from pathlib import Path
import runpy
import subprocess
import sys
import tempfile

from run_native import (APP_PATH_REQUIRED_TESTS, APP_PATH_SOURCE_MODULES, CAPTURE_REQUIRED_TESTS,
                        EVIDENCE_SOURCE_FILES, evidence_receipt, main,
                        TAP_LIVE_REQUIRED_TESTS, TAP_INBOX_REQUIRED_TESTS, STREAM_REQUIRED_TESTS, app_path_receipt,
                        capture_rate_receipt, capture_suite_passed, named_tests_passed, single_test_passed)


class NativeResultTests(unittest.TestCase):
    def test_feature_suites_require_exact_named_inventory_without_skips(self):
        for required in (TAP_LIVE_REQUIRED_TESTS, TAP_INBOX_REQUIRED_TESTS, STREAM_REQUIRED_TESTS):
            with self.subTest(required=required):
                tests = "\n".join(f"test fixture::tests::{name} ... ok" for name in required)
                summary = f"\ntest result: ok. {len(required)} passed; 0 failed; 0 ignored;\n"
                output = tests + summary
                self.assertTrue(named_tests_passed(output, 0, required))
                self.assertFalse(named_tests_passed(output, 1, required))
                self.assertFalse(named_tests_passed(output, 0, ()))
                self.assertFalse(named_tests_passed(output, 0, (*required, required[0])))
                bad = [output + summary, output + f"test fixture::tests::{required[0]} ... ok\n",
                       output.replace("0 ignored", "1 ignored"),
                       output.replace("0 failed", "1 failed"),
                       output.replace(f"{len(required)} passed", "0 passed"),
                       output.replace(required[0], "different_test")]
                bad += [output.replace(f"test fixture::tests::{name} ... ok", "")
                        for name in required]
                for candidate in bad:
                    self.assertFalse(named_tests_passed(candidate, 0, required))

    def test_app_path_receipt_requires_all_tests_and_exact_source_hash_inventory(self):
        hashes = {name: "a" * 64 for name in APP_PATH_SOURCE_MODULES}
        tests = "\n".join(f"test synthetic::tests::{name} ... ok"
                          for name in APP_PATH_REQUIRED_TESTS)
        count = len(APP_PATH_REQUIRED_TESTS)
        summary = (f"\ntest result: ok. {count} passed; 0 failed; 0 ignored; "
                   "0 measured; 0 filtered out;\n")
        receipt = "decode-quality source sha256: " + json.dumps(hashes) + "\n"
        output = receipt + tests + summary
        self.assertEqual(app_path_receipt(output, 0), hashes)
        bad_outputs = [
            tests + summary, output + receipt, output + summary,
            output.replace(APP_PATH_REQUIRED_TESTS[0], "unrelated_test"),
            output + f"test synthetic::tests::{APP_PATH_REQUIRED_TESTS[0]} ... ok\n",
            output.replace(f"{count} passed", "0 passed"),
            output.replace(f"{count} passed", "2 passed"),
            output.replace(f"{count} passed", f"{count + 10} passed"),
            output.replace("0 ignored", "1 ignored"),
            output.replace("0 failed", "1 failed"),
            receipt.replace("a" * 64, "bad-hash") + tests + summary,
            "decode-quality source sha256: {}\n" + tests + summary,
        ]
        for name in APP_PATH_REQUIRED_TESTS:
            bad_outputs.append(output.replace(f"test synthetic::tests::{name} ... ok", ""))
        for name in APP_PATH_SOURCE_MODULES:
            missing = {key: value for key, value in hashes.items() if key != name}
            bad_outputs.append("decode-quality source sha256: " + json.dumps(missing)
                               + "\n" + tests + summary)
        for candidate, status in [(output, 1)] + [(value, 0) for value in bad_outputs]:
            with self.subTest(status=status, output=candidate):
                with self.assertRaises(RuntimeError):
                    app_path_receipt(candidate, status)

    def test_capture_suite_requires_every_regression_and_successful_exit(self):
        output = "\n".join(f"test synthetic::tests::{name} ... ok" for name in CAPTURE_REQUIRED_TESTS)
        output += f"\ntest result: ok. {len(CAPTURE_REQUIRED_TESTS)} passed; 0 failed; 0 ignored\n"
        self.assertTrue(capture_suite_passed(output, 0))
        self.assertFalse(capture_suite_passed(output, 1))
        self.assertFalse(capture_suite_passed(output.replace(CAPTURE_REQUIRED_TESTS[0], "other"), 0))
        self.assertFalse(capture_suite_passed(output + "test result: FAILED. 0 passed; 1 failed;\n", 0))
        integration = "actual_owned_and_temporary_allocations_stay_within_documented_envelopes"
        mixed = output.replace(f"synthetic::tests::{integration}", integration)
        self.assertTrue(capture_suite_passed(mixed, 0))
        for name in CAPTURE_REQUIRED_TESTS:
            with self.subTest(missing=name):
                self.assertFalse(capture_suite_passed(mixed.replace(name, "other"), 0))
        for replacement in (f"prefix_{integration}", f"{integration}_suffix",
                            f"log {integration}", f"{integration} ... ignored"):
            with self.subTest(replacement=replacement):
                self.assertFalse(capture_suite_passed(mixed.replace(integration, replacement), 0))

    def test_exact_single_success(self):
        self.assertTrue(single_test_passed(
            "running 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored\n", 0))

    def test_missing_or_extra_tests_are_not_a_probe_pass(self):
        for count in (0, 11):
            with self.subTest(count=count):
                self.assertFalse(single_test_passed(
                    f"test result: ok. {count} passed; 0 failed; 0 ignored\n", 0))
        self.assertFalse(single_test_passed("noise: test result: ok. 1 passed; 0 failed;\n", 0))
        self.assertFalse(single_test_passed("test result: ok. 1 passed; 0 failed;\n" * 2, 0))

    def test_failure_or_nonzero_exit_cannot_pass(self):
        self.assertFalse(single_test_passed("test result: FAILED. 0 passed; 1 failed;\n", 101))
        self.assertFalse(single_test_passed("test result: ok. 1 passed; 0 failed;\n", 101))

    def test_capture_rate_receipt_requires_counts_and_semantic_verification(self):
        rows = [{"type": "throughput", "elapsed_ns": 7}] * 90 + [{
            "type": "verified", "measurements": 90, "exact_counts": True,
            "chunk_and_repeat_checksums_match": True}]
        encode = lambda values: "\n".join(json.dumps(row) for row in values)
        self.assertEqual(capture_rate_receipt(encode(rows) + "\n123 maximum resident set size\n", 0), rows)
        for values, status in [(rows[1:], 0), (rows + [rows[-1]], 0), (rows, 1),
                               (rows[:-1] + [dict(rows[-1], exact_counts=False)], 0)]:
            with self.subTest(status=status, length=len(values)):
                with self.assertRaises(RuntimeError):
                    capture_rate_receipt(encode(values), status)


class EvidenceGateTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.root = Path(__file__).resolve().parents[2]
        cls.expected = runpy.run_path(
            str(cls.root / "app/tests/evidence/verify_receipt.py"))["EXPECTED"]

    def setUp(self):
        self.hashes = {name: hashlib.sha256((self.root / name).read_bytes()).hexdigest()
                       for name in EVIDENCE_SOURCE_FILES}

    def receipt(self, hashes=None):
        return ("evidence-persistence source sha256: "
                + json.dumps(self.hashes if hashes is None else hashes) + "\n")

    @staticmethod
    def suite(names):
        return ("".join(f"test {name} ... ok\n" for name in names)
                + f"test result: ok. {len(names)} passed; 0 failed; 0 ignored; "
                  "0 measured; 0 filtered out; finished in 0.1s\n")

    def complete(self):
        return self.receipt() + self.suite(sorted(self.expected))

    def native_run(self, evidence=None, evidence_status=0, controls=None, metal="unavailable",
                   boundary="BOUNDARY_MATRIX PASS\n"):
        """Exercise real orchestration and receipt checks with synthetic subprocess results."""
        if evidence is None:
            evidence = self.complete()
        if controls is None:
            controls = subprocess.CompletedProcess([], 0, "..\nRan 2 tests in 0.001s\n\nOK\n")
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            fixtures = output / "fixtures.json"
            fixtures.write_text(json.dumps({"spanish_available": False, "fixtures": [{
                "language": "en", "sample_rate": 16000, "path": str(output / "synthetic.wav"),
                "voice": "Synthetic", "locale": "en_US"}]}))
            summary = output / "step-summary.md"
            commands = []

            def execute(command, **kwargs):
                commands.append(command)
                status = 0
                text = self.suite(["synthetic::probe"])
                if "scripts/quality/boundary/generate.py" in command:
                    text = ""
                elif "scripts/quality/boundary/run.py" in command:
                    text = boundary
                elif "app/tests/evidence/test_receipt.py" in command:
                    return controls
                elif "app/tests/evidence/run.py" in command:
                    if isinstance(evidence, OSError):
                        raise evidence
                    text, status = evidence, evidence_status
                elif "app/tests/decode_quality/run.py" in command:
                    text = ("decode-quality source sha256: "
                            + json.dumps({name: "a" * 64 for name in APP_PATH_SOURCE_MODULES})
                            + "\n" + self.suite([f"synthetic::{name}" for name in APP_PATH_REQUIRED_TESTS]))
                elif "corti-app" in command:
                    text = self.suite([f"synthetic::{name}" for name in CAPTURE_REQUIRED_TESTS])
                elif "corti-tap" in command and "test" in command:
                    names = TAP_LIVE_REQUIRED_TESTS if command[-1] == "live" else TAP_INBOX_REQUIRED_TESTS
                    text = self.suite([f"synthetic::{name}" for name in names])
                elif "live::stream::tests::" in command:
                    text = self.suite([f"synthetic::{name}" for name in STREAM_REQUIRED_TESTS])
                elif "ggml::tests::quality_probe_metal" in command:
                    Path(kwargs["env"]["CORTI_VERIFY_METAL_RESULT"]).write_text(metal)
                elif "resample::comparison::benchmark_production_against_borrowed_packets" in command:
                    text += '{"resample_comparison":{"synthetic":true}}\n'
                elif command[:2] == ["cargo", "build"]:
                    text = json.dumps({"reason": "compiler-artifact", "executable": "synthetic-bench",
                                       "target": {"name": "capture_rate_bench"}})
                elif command[0] == "/usr/bin/time":
                    rows = [{"type": "throughput"}] * 90 + [{"type": "verified", "measurements": 90,
                            "exact_counts": True, "chunk_and_repeat_checksums_match": True}]
                    text = "\n".join(json.dumps(row) for row in rows)
                return subprocess.CompletedProcess(command, status, text, "")

            argv = ["run_native.py", "--model-dir", str(output / "models"),
                    "--fixtures", str(fixtures), "--output-dir", str(output)]
            with patch.object(sys, "argv", argv), patch("run_native.Path.cwd", return_value=self.root), \
                    patch("run_native.subprocess.run", side_effect=execute), \
                    patch.dict("os.environ", {"GITHUB_STEP_SUMMARY": str(summary)}), \
                    redirect_stdout(io.StringIO()):
                status = main()
            artifacts = {path.name: path.read_text() for path in output.iterdir() if path.is_file()}
        return status, commands, json.loads(artifacts["summary.json"]), artifacts

    def test_evidence_receipt_requires_complete_current_source_inventory(self):
        output = self.complete()
        self.assertEqual(evidence_receipt(output, 0, self.root), self.hashes)
        first = sorted(self.hashes)[0]
        invalid = [dict(self.hashes, **{first: "0" * 64}), {},
                   {key: value for key, value in self.hashes.items() if key != first},
                   dict(self.hashes, unexpected="a" * 64)]
        for hashes in invalid:
            with self.subTest(hashes=hashes.keys()), self.assertRaises(RuntimeError):
                evidence_receipt(self.receipt(hashes) + self.suite(sorted(self.expected)), 0, self.root)
        for candidate in (output + self.receipt(), self.suite(sorted(self.expected)),
                          "evidence-persistence source sha256: []\n" + self.suite(sorted(self.expected))):
            with self.subTest(candidate=candidate[:80]), self.assertRaises(RuntimeError):
                evidence_receipt(candidate, 0, self.root)

    def test_evidence_runs_before_metal_and_keeps_receipts_in_job_artifacts(self):
        for metal in ("available", "unavailable"):
            with self.subTest(metal=metal):
                status, commands, report, artifacts = self.native_run(metal=metal)
                self.assertEqual(status, 0)
                self.assertEqual(report["metal"], metal)
                controls = next(i for i, cmd in enumerate(commands) if "app/tests/evidence/test_receipt.py" in cmd)
                evidence = next(i for i, cmd in enumerate(commands) if "app/tests/evidence/run.py" in cmd)
                probe = next(i for i, cmd in enumerate(commands) if "ggml::tests::quality_probe_metal" in cmd)
                self.assertLess(controls, evidence)
                self.assertLess(evidence, probe)
                self.assertIn("--clippy", commands[evidence])
                self.assertIn("--output-dir", commands[evidence])
                self.assertNotIn("--allocations", commands[evidence])
                self.assertNotIn("--", commands[evidence])
                self.assertEqual(report["evidence_source_sha256"], self.hashes)
                self.assertEqual(artifacts["evidence-app-orchestration.log"], self.complete())
                self.assertIn("Ran 2 tests", artifacts["evidence-receipt-controls.log"])
                for check in ("evidence-app-orchestration", "evidence-receipt-controls"):
                    self.assertIn({"check": check, "passed": True}, report["results"])
                    self.assertIn(f"- {check}: pass", artifacts["step-summary.md"])

    def test_native_gate_rejects_missing_ignored_failed_and_stale_evidence(self):
        output = self.complete()
        first = sorted(self.expected)[0]
        invalid = [output.replace(f"test {first} ... ok\n", ""),
                   output.replace(f"test {first} ... ok", f"test {first} ... ignored"),
                   output.replace(f"test {first} ... ok", f"test {first} ... FAILED"),
                   output.replace("0 ignored", "1 ignored"),
                   output.replace("0 failed", "1 failed"),
                   output.replace(next(iter(self.hashes.values())), "0" * 64), ""]
        for candidate, returncode in [(value, 0) for value in invalid] + [(output, 1)]:
            with self.subTest(candidate=candidate[:80], returncode=returncode):
                status, commands, report, artifacts = self.native_run(candidate, returncode)
                self.assertEqual(status, 1)
                self.assertEqual(report["evidence_source_sha256"], {})
                self.assertIn({"check": "evidence-app-orchestration", "passed": False}, report["results"])
                self.assertEqual(artifacts["evidence-app-orchestration.log"], candidate)
                self.assertIn("- evidence-app-orchestration: FAIL", artifacts["step-summary.md"])
                # A red stage no longer hides later evidence; the job still fails.
                self.assertIn({"check": "metal-probe", "passed": True}, report["results"])

    def test_native_gate_requires_both_receipt_controls_without_skips(self):
        for output, returncode in [("", 0), ("Ran 0 tests in 0s\nOK\n", 0),
                                   ("Ran 2 tests in 0s\nOK (skipped=1)\n", 0),
                                   ("Ran 2 tests in 0s\nOK\n", 1)]:
            with self.subTest(output=output, returncode=returncode):
                controls = subprocess.CompletedProcess([], returncode, output)
                status, commands, report, artifacts = self.native_run(controls=controls)
                self.assertEqual(status, 1)
                self.assertIn({"check": "evidence-receipt-controls", "passed": False}, report["results"])
                self.assertEqual(artifacts["evidence-receipt-controls.log"], output)
                self.assertFalse(any("app/tests/evidence/run.py" in cmd for cmd in commands))

    def test_red_boundary_matrix_still_runs_and_reports_every_later_stage(self):
        status, commands, report, artifacts = self.native_run(boundary="BOUNDARY_MATRIX FAIL\n")
        self.assertEqual(status, 1)
        self.assertIn({"check": "boundary-native-matrix", "passed": False}, report["results"])
        self.assertIn("- boundary-native-matrix: FAIL", artifacts["step-summary.md"])
        for check in ("capture-quality-native-suites", "live-word-stream-feature",
                      "decode-quality-app-orchestration", "evidence-app-orchestration",
                      "metal-probe", "capture-rate-throughput"):
            with self.subTest(check=check):
                self.assertIn({"check": check, "passed": True}, report["results"])

    def test_missing_evidence_command_keeps_failed_result_and_summary(self):
        status, commands, report, artifacts = self.native_run(evidence=FileNotFoundError("missing harness"))
        self.assertEqual(status, 1)
        self.assertIn({"check": "evidence-app-orchestration", "passed": False}, report["results"])
        self.assertIn("- evidence-app-orchestration: FAIL", artifacts["step-summary.md"])
        self.assertEqual(report["evidence_source_sha256"], {})

    def test_missing_current_source_cannot_accept_an_old_evidence_receipt(self):
        with patch("run_native.Path.read_bytes", side_effect=FileNotFoundError("missing source")):
            status, commands, report, artifacts = self.native_run()
        self.assertEqual(status, 1)
        self.assertIn({"check": "evidence-app-orchestration", "passed": False}, report["results"])
        self.assertEqual(report["evidence_source_sha256"], {})
        self.assertEqual(artifacts["evidence-app-orchestration.log"], self.complete())


if __name__ == "__main__":
    unittest.main()
