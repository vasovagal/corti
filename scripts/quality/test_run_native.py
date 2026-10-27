"""A native probe passes only when exactly its intended test executed."""
import unittest
import json

from run_native import (APP_PATH_REQUIRED_TESTS, APP_PATH_SOURCE_MODULES, CAPTURE_REQUIRED_TESTS,
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


if __name__ == "__main__":
    unittest.main()
