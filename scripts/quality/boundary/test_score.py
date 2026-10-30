import unittest

from score import align, compare, tokens


class ScoringTests(unittest.TestCase):
    def test_exact_integer_five_percent_boundary(self):
        for size, expected in ((19, False), (20, True)):
            result = compare(["word"] * size, ["changed"] + ["word"] * (size - 1), [])
            self.assertEqual(result["word_gate"], expected)
            self.assertEqual((result["edits"], result["denominator"]), (1, size))

    def test_nfc_whitespace_and_word_piece_mapping_only(self):
        result = tokens(["Cafe\u0301  .5", "-5 15mm mM V1.2 don't"])
        self.assertEqual([t.raw for t in result], ["Café", ".5", "-5", "15mm", "mM", "V1.2", "don't"])
        self.assertEqual([(t.word_index, t.piece_index) for t in result], [(0, 0), (0, 1), (1, 0), (1, 1), (1, 2), (1, 3), (1, 4)])

    def test_critical_case_survives_lowercase_word_comparison(self):
        result = compare(["15", "mM"], ["15", "mm"], [("dose", (0, 1))])
        self.assertTrue(result["word_gate"])
        self.assertFalse(result["critical_gate"])

    def test_sign_decimal_identifier_and_contraction_changes_remain_visible(self):
        for old, new in (("-5", "5"), (".5", "5"), ("V1.2", "V12"), ("don't", "do"), ("15mm", "15")):
            result = compare([old], [new], [("critical", (0,))])
            self.assertFalse(result["word_gate"])
            self.assertFalse(result["critical_gate"])

    def test_occurrences_and_multiplicity_are_not_sets(self):
        result = compare(["Ada", "waits", "Ada"], ["Ada", "waits"], [("first", (0,)), ("second", (2,))])
        self.assertEqual(result["critical_occurrences"], {"first": True, "second": False})

    def test_inserted_critical_repetition_and_number_fail(self):
        for hypothesis in (["Ada", "Ada"], ["Ada", "1500"], ["Ada", "not"]):
            result = compare(["Ada"], hypothesis, [("person", (0,))])
            self.assertFalse(result["critical_gate"])
            self.assertTrue(result["inserted_critical_indices"])

    def test_multi_token_critical_span_cannot_be_split(self):
        result = compare(["twenty", "one"], ["twenty", "only", "one"], [("number", (0, 1))])
        self.assertFalse(result["critical_gate"])

    def test_empty_baseline_does_not_certify_speech(self):
        self.assertFalse(compare([], [], [])["word_gate"])
        self.assertFalse(compare([], [], [])["critical_gate"])
        self.assertTrue(compare([], [], [], speech_required=False)["word_gate"])
        self.assertFalse(compare([], ["speech"], [], speech_required=False)["word_gate"])

    def test_invalid_annotation_is_refused(self):
        for occurrences in ([("x", ())], [("x", (2,))], [("x", (0, 2))], [("x", (0,)), ("x", (1,))]):
            with self.assertRaises(ValueError):
                compare(["one", "two"], ["one", "two"], occurrences)

    def test_alignment_tie_break_is_deterministic(self):
        self.assertEqual(align(["a", "b"], ["b", "a"]),
                         (2, (("substitute", 0, 0), ("substitute", 1, 1))))

    def test_critical_hypothesis_substitution_is_not_ordinary(self):
        reference = ["Ada"] + ["ordinary"] * 19
        for replacement in ("15000", "not", "V2.0"):
            with self.subTest(replacement=replacement):
                hypothesis = reference.copy()
                hypothesis[10] = replacement
                result = compare(reference, hypothesis, [("person", (0,))])
                self.assertTrue(result["word_gate"])
                self.assertFalse(result["critical_gate"])

    def test_punctuated_and_typographic_negators_remain_critical(self):
        reference = ["Ada"] + ["ordinary"] * 19
        for insertion in ("not,", "never.", "don’t"):
            with self.subTest(insertion=insertion):
                result = compare(reference, reference + [insertion], [("person", (0,))])
                self.assertTrue(result["word_gate"])
                self.assertFalse(result["critical_gate"])

    def test_frozen_fixture_lexemes_cover_names_units_and_spelled_numbers(self):
        reference = ["Ada"] + ["ordinary"] * 19
        for replacement in ("Grace", "mM", "fifty"):
            hypothesis = reference.copy()
            hypothesis[10] = replacement
            result = compare(reference, hypothesis, [("person", (0,))],
                             critical_lexemes=("Grace", "mM", "fifty"))
            self.assertTrue(result["word_gate"])
            self.assertFalse(result["critical_gate"])

    def test_noncritical_five_percent_substitution_remains_permitted(self):
        reference = ["Ada"] + ["ordinary"] * 19
        hypothesis = reference.copy()
        hypothesis[10] = "changed"
        result = compare(reference, hypothesis, [("person", (0,))],
                         critical_lexemes=("Grace", "mM", "fifty"))
        self.assertTrue(result["word_gate"])
        self.assertTrue(result["critical_gate"])

    def test_raw_critical_lexeme_changes_cannot_hide_in_equal_normalized_endpoints(self):
        for old, new, declared in (("us", "US", "US"), ("grace", "Grace", "Grace"),
                                   ("mm", "mM", "mM"), ("mM", "mm", "mM")):
            with self.subTest(old=old, new=new):
                result = compare(["Ada", old], ["Ada", new], [("person", (0,))],
                                 critical_lexemes=(declared,))
                self.assertTrue(result["word_gate"])
                self.assertEqual(result["edits"], 0)
                self.assertFalse(result["critical_gate"])

    def test_ordinary_raw_case_change_preserves_case_insensitive_word_tolerance(self):
        result = compare(["Ada", "ordinary"], ["Ada", "Ordinary"], [("person", (0,))],
                         critical_lexemes=("US", "Grace", "mM"))
        self.assertTrue(result["word_gate"])
        self.assertTrue(result["critical_gate"])


if __name__ == "__main__":
    unittest.main()
