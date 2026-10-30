"""Synthetic regressions for cleanup diagnostics; run with stdlib unittest discovery."""

import unittest

import cleanup


def segment(text, channel="me", start=1.0):
    return {"channel": channel, "label": channel, "start": start, "end": start + 1.0, "text": text}


class CriticalTokenTests(unittest.TestCase):
    def test_raw_signature_protects_numeric_units_before_grammar_and_fillers(self):
        cases = (
            ("The current is 5 A.", ["current", "is", "5", "A"]),
            ("The capacity is 5 Ah.", ["capacity", "is", "5", "Ah"]),
            ("The distance is 5 mm.", ["distance", "is", "5", "mm"]),
            ("The concentration is 5 mM.", ["concentration", "is", "5", "mM"]),
            ("The distance is 5 um.", ["distance", "is", "5", "um"]),
            ("mm 5.", ["mm", "5"]),
            ("A 5.", ["A", "5"]),
            ("a ５.", ["a", "５"]),
            ("a ².", ["a", "²"]),
            ("a Ⅳ.", ["a", "Ⅳ"]),
            # Rust char::is_numeric uses Unicode N categories, not every numeric ideograph.
            ("a 四.", ["四"]),
        )
        for text, expected in cases:
            with self.subTest(text=text):
                self.assertEqual(cleanup.critical_tokens(text), expected)

    def test_raw_signature_preserves_nonordinary_case_collisions(self):
        cases = (
            ("The marker is mM.", ["marker", "mM"]),
            ("The marker is Ah.", ["marker", "Ah"]),
            ("The marker is A.", ["marker", "A"]),
            ("AM gateway.", ["AM", "gateway"]),
        )
        for text, expected in cases:
            with self.subTest(text=text):
                self.assertEqual(cleanup.critical_tokens(text), expected)

    def test_raw_signature_keeps_marked_fillers_and_distinct_identifiers(self):
        cases = (
            ("The ticket is @um.", ["ticket", "@um"]),
            ("The ticket is #ah.", ["ticket", "#ah"]),
            ("Read @ready ready now.", ["Read", "@ready", "ready", "now"]),
            ("Use foo/bar foobar.", ["Use", "foo/bar", "foobar"]),
        )
        for text, expected in cases:
            with self.subTest(text=text):
                self.assertEqual(cleanup.critical_tokens(text), expected)
        self.assertNotEqual(cleanup.critical_tokens("@ready"), cleanup.critical_tokens("ready"))
        self.assertFalse(cleanup.is_backchannel("@um."))
        self.assertFalse(cleanup.is_backchannel("#ah."))

    def test_ordinary_grammar_equivalence_survives_without_erasing_fillers(self):
        self.assertEqual(cleanup.critical_tokens("The gateway is ready."), ["gateway", "ready"])
        self.assertEqual(cleanup.critical_tokens("Um, gateway ready!"), ["Um", "gateway", "ready"])
        self.assertEqual(cleanup.critical_tokens("gateway um ready."), ["gateway", "um", "ready"])

    def test_raw_signature_retains_ambiguous_units_without_numeric_neighbors(self):
        for text, expected in (
            ("Report the dimensions in mm today.", ["Report", "dimensions", "in", "mm", "today"]),
            ("Report the area in square um today.", ["Report", "area", "in", "square", "um", "today"]),
            ("Set thickness to fifteen mm.", ["Set", "thickness", "to", "fifteen", "mm"]),
        ):
            with self.subTest(text=text):
                self.assertEqual(cleanup.critical_tokens(text), expected)


class RetentionTests(unittest.TestCase):
    def assert_lost_correction(self, source, correction):
        for source_channel, dropped_channel in (("them", "me"), ("me", "them")):
            with self.subTest(source_channel=source_channel, source=source, correction=correction):
                kept = segment(source, source_channel, start=0.0)
                dropped = segment(correction, dropped_channel)
                report = cleanup.compare([kept, dropped], [kept])
                self.assertEqual(report["dropped"], 1)
                self.assertEqual(report["orphan_drops"], 1)
                self.assertEqual(report["backchannel_orphans"], 0)
                self.assertEqual(report["orphans"][0]["channel"], dropped_channel)

    def test_numeric_backchannels_cannot_hide_a_lost_number(self):
        for source, correction in (
            ("Okay, 1500.", "Okay, 15000."),
            ("I see 1500.", "I see 15000."),
            ("Okay.", "Okay, 15000."),
            ("I see.", "I see 15000."),
        ):
            self.assert_lost_correction(source, correction)

    def test_backchannel_metric_uses_raw_safe_production_classification(self):
        critical = (
            "Okay, 15000.", "I see 15000.", "Okay, 1.2.4.", "I see #15000.",
            "Okay1.", "@Okay.", "#Okay.", "Okay mM.", "Okay Ah.", "Okay, 15 mm.",
            "Got IT.", "I SEE.", "Go Ahead.", "OkAy.",
        )
        ordinary = (
            "Okay.", "OKAY!", "Um, okay.", "Ah, okay.", "I see.", "Sounds good.",
            "Got it.", "Go ahead.",
        )
        for text in critical:
            with self.subTest(text=text):
                self.assertFalse(cleanup.is_backchannel(text))
        for text in ordinary:
            with self.subTest(text=text):
                self.assertTrue(cleanup.is_backchannel(text))
        rows = [segment(text, start=float(i)) for i, text in enumerate(critical + ordinary)]
        self.assertEqual(cleanup.score(rows)["backchannel_turns"], len(ordinary))

    def test_case_corrections_cannot_hide_behind_backchannel_exemptions(self):
        for source, correction in (
            ("Okay.", "OKAY."),
            ("OKAY.", "Okay."),
            ("Got it.", "Got IT."),
            ("Got IT.", "Got it."),
            ("I see.", "I SEE."),
            ("I SEE.", "I see."),
            ("Go ahead.", "Go Ahead."),
            ("Go Ahead.", "Go ahead."),
        ):
            self.assert_lost_correction(source, correction)

    def test_backchannel_classifier_preserves_unmarked_medial_fillers(self):
        for text in ("Okay mm.", "Okay um.", "Yeah hmm."):
            with self.subTest(text=text):
                self.assertFalse(cleanup.is_backchannel(text))
        for text in ("Um okay.", "Okay, um.", "Okay um, okay."):
            with self.subTest(text=text):
                self.assertTrue(cleanup.is_backchannel(text))
        self.assert_lost_correction("Okay mm.", "Okay um.")
        self.assert_lost_correction("Okay um.", "Okay mm.")

    def test_backchannel_normalization_cannot_hide_versions_or_identifiers(self):
        for source, correction in (
            ("Okay, 1.2.3.", "Okay, 1.2.4."),
            ("I see 1.2.3.", "I see 1.2.4."),
            ("Okay, #1500.", "Okay, #15000."),
            ("I see #1500.", "I see #15000."),
            ("Okay.", "Okay1."),
            ("Okay.", "@Okay."),
            ("Okay.", "#Okay."),
        ):
            self.assert_lost_correction(source, correction)

    def test_numeric_unit_changes_and_omissions_are_orphaned(self):
        for source, correction in (
            ("Set current to 5 A.", "Set current to 5 Ah."),
            ("Set distance to 5 mm.", "Set distance to 5 mM."),
            ("Set distance to 5.", "Set distance to 5 mm."),
            ("Okay, 5 A.", "Okay, 5 Ah."),
            ("Okay, 5 mm.", "Okay, 5 mM."),
            ("Okay, 5.", "Okay, 5 mm."),
            ("I see 5 mm.", "I see 5 mM."),
            ("Okay.", "Okay mM."),
            ("Okay.", "Okay Ah."),
        ):
            self.assert_lost_correction(source, correction)

    def test_ambiguous_unit_changes_without_adjacent_digits_are_orphaned(self):
        for source, correction in (
            ("Report the dimensions in mm today.", "Report the dimensions in um today."),
            ("Report the area in square um today.", "Report the area in square mm today."),
            ("Set thickness to fifteen mm.", "Set thickness to fifteen um."),
        ):
            self.assert_lost_correction(source, correction)

    def test_raw_corrections_survive_alphabetic_content_containment(self):
        for source, correction in (
            ("Set budget to 1500 today.", "Set budget to 15000 today."),
            ("Set version to 1.2.3 today.", "Set version to 1.2.4 today."),
            ("Set ticket to ID-15 today.", "Set ticket to ID-16 today."),
            ("Send package to Nora today.", "Send package to Noraa today."),
            ("Send package to Nora today.", "Do not send package to Nora today."),
        ):
            self.assert_lost_correction(source, correction)

    def test_ordinary_backchannels_keep_legacy_exemptions(self):
        kept = segment("The gateway is ready today.", "them", start=0.0)
        for text, phrase_orphans in (
            ("Okay.", 0),
            ("OKAY!", 0),
            ("Um, okay.", 0),
            ("Ah, okay.", 0),
            ("Mm-hmm.", 0),
            ("I see.", 1),
            ("Got it.", 1),
            ("Go ahead.", 1),
            ("Of course.", 1),
            ("Sounds good.", 1),
        ):
            with self.subTest(text=text):
                dropped = segment(text)
                report = cleanup.compare([kept, dropped], [kept])
                self.assertEqual(report["dropped"], 1)
                self.assertEqual(report["orphan_drops"], 0)
                self.assertEqual(report["backchannel_orphans"], phrase_orphans)

    def test_preserved_critical_values_in_nearby_merges_are_accounted_for(self):
        for text in ("Okay, 15000.", "I see 1.2.4.", "Okay, #15000.", "Set distance to 5 mm."):
            with self.subTest(text=text):
                dropped = segment(text)
                merged = segment("Earlier words. " + text + " Later words.", "them", start=0.0)
                report = cleanup.compare([merged, dropped], [merged])
                self.assertEqual(report["dropped"], 1)
                self.assertEqual(report["orphan_drops"], 0)
                self.assertEqual(report["backchannel_orphans"], 0)

    def test_same_channel_case_difference_does_not_override_backchannel_exemption(self):
        earlier = segment("Okay.", "me", start=0.0)
        other = segment("The gateway is ready today.", "them", start=1.5)
        other["end"] = 5.0
        dropped = segment("OKAY.", "me", start=2.0)
        report = cleanup.compare([earlier, other, dropped], [earlier, other])
        self.assertEqual(report["dropped"], 1)
        self.assertEqual(report["orphan_drops"], 0)
        self.assertEqual(report["backchannel_orphans"], 0)

    def test_same_channel_merged_fragment_still_accounts_for_critical_values(self):
        for text in ("Okay, 15000.", "Set distance to 5 mm."):
            with self.subTest(text=text):
                merged = segment("Earlier words. " + text + " Later words.", "me", start=0.0)
                dropped = segment(text, "me", start=2.0)
                report = cleanup.compare([merged, dropped], [merged])
                self.assertEqual(report["dropped"], 1)
                self.assertEqual(report["orphan_drops"], 0)
                self.assertEqual(report["backchannel_orphans"], 0)

    def test_critical_values_must_survive_in_order_and_within_the_window(self):
        dropped = segment("I see 12 34.")
        for kept in (segment("I see 34 12.", "them", start=0.0),
                     segment("I see 12 34.", "them", start=20.0)):
            with self.subTest(kept=kept):
                report = cleanup.compare([kept, dropped], [kept])
                self.assertEqual(report["orphan_drops"], 1)
                self.assertEqual(report["backchannel_orphans"], 0)


if __name__ == "__main__":
    unittest.main()
