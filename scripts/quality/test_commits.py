"""Tests for commit coverage and GitHub diagnostic escaping."""
import unittest
from unittest.mock import patch

import commits


class CommitChecks(unittest.TestCase):
    def test_range_uses_merge_base_and_preserves_every_commit_in_order(self):
        with patch("commits.subprocess.check_output", side_effect=["ancestor\n", "one\ntwo\n"]) as run:
            self.assertEqual(commits.commit_range("base", "head"), ["one", "two"])
        self.assertEqual(run.call_args_list[1].args[0],
                         ["git", "rev-list", "--reverse", "ancestor..head"])

    def test_empty_matrix_is_failure(self):
        with patch("commits.subprocess.check_output", side_effect=["same\n", ""]):
            with self.assertRaises(ValueError):
                commits.commit_range("same", "same")

    def test_dispatch_can_report_an_empty_range_without_inventing_a_pr_commit(self):
        with patch("commits.subprocess.check_output", side_effect=["same\n", ""]):
            self.assertEqual(commits.commit_range("same", "same", allow_empty=True), [])

    def test_error_annotation_preserves_location_and_cannot_inject_command(self):
        error = {"level": "error", "message": "bad%\n::warning::injected",
                 "spans": [{"is_primary": True, "file_name": "src/a,b.rs",
                            "line_start": 7, "column_start": 3}]}
        self.assertEqual(commits.annotations(error), [
            "::error file=src/a%2Cb.rs,line=7,col=3::bad%25%0A::warning::injected"])
        self.assertEqual(commits.annotations({"level": "warning"}), [])


if __name__ == "__main__":
    unittest.main()
