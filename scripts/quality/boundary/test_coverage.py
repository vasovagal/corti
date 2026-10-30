import copy
import unittest

from coverage import CoverageError, bind_occurrences, compare_cell


def words(text):
    return [{"text": word, "start": index, "end": index + 0.5}
            for index, word in enumerate(text.split())]


def occurrence(identifier, authored, alternatives=None, window=(0, 10000)):
    return {"id": identifier, "authored": authored,
            "alternatives": alternatives or [authored], "source_window": list(window)}


class CoverageTests(unittest.TestCase):
    def test_literal_number_alternative_binds_without_normalizing_candidate(self):
        reference = words("Nora has 1,500 euros")
        annotations = [occurrence("amount", ["one", "thousand", "five", "hundred"],
                                  [["one", "thousand", "five", "hundred"], ["1,500"], ["1500"]])]
        result = compare_cell(reference, reference, annotations, 10000, 1000, ("Nora",))
        self.assertTrue(result["passed"])
        self.assertEqual(result["coverage"][0]["baseline_surface"], ["1,500"])
        self.assertTrue(result["coverage"][0]["authored_lexical_difference"])
        for changed in ("1500", "15000", "fifteen thousand"):
            with self.subTest(changed=changed):
                result = compare_cell(reference, words("Nora has " + changed + " euros"),
                                      annotations, 10000, 1000, ("Nora",))
                self.assertFalse(result["score"]["critical_gate"])

    def test_genuine_repeat_occurrences_are_ordered_and_not_deduplicated(self):
        reference = words("Please restart Please restart")
        annotations = [occurrence("first", ["Please", "restart"]),
                       occurrence("second", ["Please", "restart"])]
        result = compare_cell(reference, reference, annotations, 10000, 1000, ())
        self.assertEqual([row["indices"] for row in result["coverage"]], [(0, 1), (2, 3)])
        self.assertTrue(result["passed"])
        self.assertFalse(compare_cell(reference, words("Please restart"), annotations,
                                      10000, 1000, ())["score"]["critical_gate"])

    def test_extra_or_missing_baseline_repeat_refuses_ambiguous_coverage(self):
        annotations = [occurrence("first", ["Ada"]), occurrence("second", ["Ada"])]
        for text in ("Ada", "Ada Ada Ada"):
            with self.subTest(text=text), self.assertRaises(CoverageError):
                bind_occurrences(words(text), annotations, 10000, 1000)

    def test_source_windows_bind_tracks_without_borrowing_neighbor_occurrence(self):
        reference = words("Ada checks Ada")
        annotation = occurrence("later", ["Ada"], window=(2000, 3000))
        self.assertEqual(bind_occurrences(reference, [annotation], 10000, 1000)[0]["indices"], (2,))
        annotation["source_window"] = [3000, 4000]
        with self.assertRaises(CoverageError):
            bind_occurrences(reference, [annotation], 10000, 1000)

    def test_unlisted_unit_version_and_polarity_changes_cannot_bind(self):
        for text, expected in (("15 mM", ["15", "mm"]), ("version 2.0", ["version", "1.5"]),
                               ("do deploy", ["not"]), ("15000", ["1500"])):
            with self.subTest(text=text), self.assertRaises(CoverageError):
                bind_occurrences(words(text), [occurrence("value", expected)], 10000, 1000)

    def test_missing_empty_or_duplicate_annotation_inventory_refuses(self):
        row = occurrence("name", ["Ada"])
        for annotations in ([], [row, row], [occurrence("absent", ["Grace"])]):
            with self.subTest(annotations=annotations), self.assertRaises(CoverageError):
                bind_occurrences(words("Ada"), annotations, 10000, 1000)

    def test_invalid_or_nonmonotonic_native_times_refuse(self):
        for value in (float("nan"), float("inf"), -1, 11):
            reference = words("Ada")
            reference[0]["start"] = value
            with self.subTest(value=value), self.assertRaises(CoverageError):
                bind_occurrences(reference, [occurrence("name", ["Ada"])], 10000, 1000)
        reference = words("Ada checks")
        reference.reverse()
        with self.assertRaises(CoverageError):
            bind_occurrences(reference, [occurrence("name", ["Ada"])], 10000, 1000)

    def test_multi_piece_native_word_keeps_original_word_identity(self):
        reference = [{"text": "one thousand", "start": 1, "end": 2}]
        bound = bind_occurrences(reference, [occurrence("amount", ["one", "thousand"])], 10000, 1000)
        self.assertEqual(bound[0]["indices"], (0, 1))

    def test_sentence_case_and_punctuation_locate_but_raw_tokens_stay_critical(self):
        reference = words("Never replace 50 millimeters! Not 1,500 euros.")
        annotations = [occurrence("negation", ["never"]),
                       occurrence("unit", ["50", "millimeters."]),
                       occurrence("polarity", ["not"]),
                       occurrence("amount", ["1,500", "euros"])]
        result = compare_cell(reference, reference, annotations, 10000, 1000, ())
        self.assertTrue(result["passed"])
        self.assertEqual(result["coverage"][0]["baseline_surface"], ["Never"])
        # The candidate must still reproduce the batch's raw tokens exactly.
        changed = compare_cell(reference, words("never replace 50 millimeters! Not 1,500 euros."),
                               annotations, 10000, 1000, ())
        self.assertFalse(changed["score"]["critical_gate"])
        with self.assertRaises(CoverageError):
            bind_occurrences(words("NEVER"), [occurrence("negation", ["never"])], 10000, 1000)

    def test_raw_case_difference_remains_critical_after_literal_binding(self):
        reference = words("15 mM")
        result = compare_cell(reference, words("15 mm"),
                              [occurrence("amount-unit", ["15", "mM"])], 10000, 1000, ("mm", "mM"))
        self.assertFalse(result["score"]["critical_gate"])

    def test_surface_arrays_are_not_a_hidden_normalizer(self):
        row = occurrence("name", ["Ada"])
        for alternatives in ([[" Ada"]], [["Ada Ada"]], [["Ada"], ["Ada"]], [["e\u0301"]]):
            changed = copy.deepcopy(row)
            changed["alternatives"] = alternatives
            with self.subTest(alternatives=alternatives), self.assertRaises(CoverageError):
                bind_occurrences(words("Ada"), [changed], 10000, 1000)


if __name__ == "__main__":
    unittest.main()
