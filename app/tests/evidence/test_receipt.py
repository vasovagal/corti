import contextlib
import importlib.util
import io
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock
from verify_receipt import EXPECTED, require_complete


class ReceiptTests(unittest.TestCase):
    def test_full_receipt_accepts_exact_complete_suite(self):
        require_complete(self.complete(), 0)
        RunnerIsolationChecks().check_capture_root_without_mutating_parent_or_caller()

    def test_missing_duplicate_extra_ignored_failed_and_empty_are_rejected(self):
        output = self.complete()
        first = sorted(EXPECTED)[0]
        for candidate in ["", output.replace(f"test {first} ... ok\n", ""),
                          output + f"test {first} ... ok\n", output + "test unlisted ... ok\n",
                          output.replace(f"test {first} ... ok", f"test {first} ... ignored"),
                          output.replace(f"test {first} ... ok", f"test {first} ... FAILED"),
                          output.replace("0 filtered out", "1 filtered out")]:
            with self.subTest(candidate=candidate[:80]), self.assertRaises(RuntimeError):
                require_complete(candidate, 0)
        with self.assertRaises(RuntimeError):
            require_complete(output, 1)
        RunnerIsolationChecks().check_cleanup_and_restoration_on_failed_test()

    @staticmethod
    def complete():
        return "".join(f"test {name} ... ok\n" for name in sorted(EXPECTED)) + f"test result: ok. {len(EXPECTED)} passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.1s\n"


class RunnerIsolationChecks(unittest.TestCase):
    @staticmethod
    def load_runner():
        spec = importlib.util.spec_from_file_location(
            "evidence_isolation_runner", Path(__file__).with_name("run.py"))
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return module

    def check_capture_root_without_mutating_parent_or_caller(self):
        module = self.load_runner()
        with tempfile.TemporaryDirectory(prefix="corti-invented-owner-") as owner:
            sentinel = Path(owner) / "20261030-010203-synthetic.evidence.json"
            sentinel.write_text("invented evidence")
            roots = []
            with mock.patch.dict(os.environ, {"CORTI_RECORDINGS_DIR": owner}):
                for supplied in (True, False):
                    caller = {"CORTI_RECORDINGS_DIR": owner, "RUNNER_SENTINEL": "preserved"}
                    original = caller.copy()

                    def execute(command, **kwargs):
                        if command[:2] != ["cargo", "test"]:
                            self.assertIs(kwargs["env"], caller)
                            self.assertEqual(kwargs["env"], original)
                            return subprocess.CompletedProcess(command, 0)
                        environment = kwargs["env"]
                        root = Path(environment["CORTI_RECORDINGS_DIR"])
                        self.assertNotEqual(root, Path(owner))
                        self.assertTrue(root.is_dir())
                        self.assertEqual(list(root.iterdir()), [])
                        if supplied:
                            self.assertEqual(environment["RUNNER_SENTINEL"], "preserved")
                        roots.append(root)
                        (root / "synthetic-test-artifact").write_text("disposable")
                        return subprocess.CompletedProcess(command, 0, ReceiptTests.complete(), "")

                    def drive(*_):
                        for action in ("metadata", "clippy"):
                            module.runner.subprocess.run(["cargo", action], env=caller)
                        options = {"env": caller} if supplied else {}
                        module.runner.subprocess.run(["cargo", "test", "--lib"], **options)

                    with mock.patch.object(module, "original_run", drive), \
                            mock.patch.object(module.runner.subprocess, "run", execute), \
                            contextlib.redirect_stdout(io.StringIO()):
                        module.run(Path(owner) / "unused-harness", [], False)
                        self.assertIs(module.runner.subprocess.run, execute)
                    self.assertEqual(caller, original)
                    self.assertEqual(os.environ["CORTI_RECORDINGS_DIR"], owner)
                    self.assertFalse(roots[-1].exists())
            self.assertEqual(len(set(roots)), 2)
            self.assertEqual(sentinel.read_text(), "invented evidence")

    def check_cleanup_and_restoration_on_failed_test(self):
        module = self.load_runner()
        roots = []

        def execute(command, **kwargs):
            root = Path(kwargs["env"]["CORTI_RECORDINGS_DIR"])
            self.assertTrue(root.is_dir())
            roots.append(root)
            (root / "synthetic-failed-test-artifact").write_text("disposable")
            if launch_failure:
                raise OSError("synthetic launch failure")
            return subprocess.CompletedProcess(command, 1, "", "synthetic failure")

        def drive(*_):
            module.runner.subprocess.run(["cargo", "test", "--lib"], env={})

        for launch_failure in (False, True):
            with mock.patch.object(module, "original_run", drive), \
                    mock.patch.object(module.runner.subprocess, "run", execute), \
                    contextlib.redirect_stderr(io.StringIO()):
                error = OSError if launch_failure else subprocess.CalledProcessError
                with self.assertRaises(error):
                    module.run(Path("unused-harness"), [], False)
                self.assertIs(module.runner.subprocess.run, execute)
            self.assertFalse(roots[-1].exists())
        self.assertEqual(len(roots), 2)


if __name__ == "__main__":
    unittest.main()
