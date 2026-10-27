"""A native probe passes only when exactly its intended test executed."""
import unittest

from run_native import single_test_passed


class NativeResultTests(unittest.TestCase):
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


if __name__ == "__main__":
    unittest.main()
