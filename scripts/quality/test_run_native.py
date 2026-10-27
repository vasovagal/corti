"""A native probe passes only when exactly its intended test executed."""
import unittest
import json

from run_native import CAPTURE_REQUIRED_TESTS, capture_rate_receipt, capture_suite_passed, single_test_passed


class NativeResultTests(unittest.TestCase):
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
