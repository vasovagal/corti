import copy
import json
from pathlib import Path
import tempfile
import unittest
import wave

from generate import digest, frame_count, generate, validate_definition, validate_seal, voice_from_listing


class DefinitionTests(unittest.TestCase):
    def setUp(self):
        self.definition = json.loads(Path(__file__).with_name("fixtures.json").read_text())

    def test_frozen_authored_occurrences_and_complete_matrix_validate(self):
        self.assertEqual(set(validate_definition(self.definition)), {"release", "budget", "measure", "repeat"})
        self.assertEqual(len(self.definition["matrix"]), 13)

    def test_missing_duplicated_or_wrong_source_matrix_refuses(self):
        changed = copy.deepcopy(self.definition)
        changed["matrix"].pop()
        with self.assertRaises(ValueError):
            validate_definition(changed)
        changed = copy.deepcopy(self.definition)
        changed["matrix"][-1] = changed["matrix"][0]
        with self.assertRaises(ValueError):
            validate_definition(changed)
        changed = copy.deepcopy(self.definition)
        changed["tracks"][1]["prefix_silence_ms"] = 0
        with self.assertRaises(ValueError):
            validate_definition(changed)

    def test_authored_occurrences_cannot_be_deleted_or_reordered(self):
        for replacement in (["Grace"], ["one", "thousand", "five", "hundred"]):
            changed = copy.deepcopy(self.definition)
            changed["sentences"][0]["critical"][0]["authored"] = replacement
            with self.subTest(replacement=replacement), self.assertRaises(ValueError):
                validate_definition(changed)

    def test_voice_choice_is_input_order_independent_and_never_downloads(self):
        lines = ["Zoe en_GB # Sample", "Zoe en_US # Sample", "Alex en_US # Sample"]
        self.assertEqual(voice_from_listing("\n".join(lines)), ("Alex", "en_US"))
        self.assertEqual(voice_from_listing("\n".join(reversed(lines))), ("Alex", "en_US"))
        with self.assertRaises(ValueError):
            voice_from_listing("Jorge es_ES # Sample")

    def test_fractional_source_frame_is_refused_instead_of_rounded(self):
        self.assertEqual(frame_count(6250, 44100), 275625)
        with self.assertRaises(ValueError):
            frame_count(1, 44100)

    def test_generation_seals_all_tracks_windows_and_bytes_before_any_inference(self):
        commands = []

        def fake_tool(command):
            commands.append(command)
            if command == ["say", "-v", "?"]:
                return "Alex en_US # Sample\n"
            if command[0] == "say":
                return ""
            self.assertEqual(command[0], "afconvert")
            rate = int(command[command.index("-d") + 1].split("@")[1])
            seconds = 25 if "continuous" in command[-1] else 8
            with wave.open(command[-1], "wb") as output:
                output.setparams((1, 2, rate, 0, "NONE", "not compressed"))
                remaining = seconds * rate
                while remaining:
                    count = min(4096, remaining)
                    output.writeframesraw(b"\x01\x00" * count)
                    remaining -= count
            return ""

        with tempfile.TemporaryDirectory(prefix="corti155-fixture-mock-") as directory:
            sealed = generate(Path(__file__).with_name("fixtures.json"), directory, fake_tool)
            receipt = json.loads(sealed.read_text())
            self.assertTrue(receipt["pre_inference_seal"])
            self.assertEqual(len(receipt["fixtures"]), 12)
            self.assertEqual(len(receipt["matrix"]), 13)
            self.assertEqual(receipt["definition_sha256"], digest(Path(__file__).with_name("fixtures.json")))
            self.assertEqual(set(receipt["generator_sources"]), {"generate.py", "coverage.py", "score.py", "matrix.py", "run.py"})
            for mutation in ("window", "alternative", "missing", "extra", "source"):
                changed = copy.deepcopy(receipt)
                if mutation == "window":
                    changed["fixtures"][0]["occurrences"][0]["source_window"][0] += 1
                elif mutation == "alternative":
                    changed["fixtures"][0]["occurrences"][0]["alternatives"].append(["unreviewed"])
                elif mutation == "missing":
                    changed["fixtures"].pop()
                elif mutation == "extra":
                    changed["fixtures"].append(changed["fixtures"][0])
                else:
                    changed["generator_sources"].pop("run.py")
                with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                    validate_seal(changed, Path(__file__).with_name("fixtures.json"))
            self.assertEqual(sum(command[0] == "say" and "-o" in command for command in commands), 5)
            self.assertEqual(sum(command[0] == "afconvert" for command in commands), 15)
            for fixture in receipt["fixtures"]:
                self.assertEqual(fixture["sha256"], digest(fixture["path"]))
                self.assertEqual(len(fixture["occurrences"]), 39 if fixture["waveform"] == "paused" else 13)
                for occurrence in fixture["occurrences"]:
                    self.assertLessEqual(0, occurrence["source_window"][0])
                    self.assertLessEqual(occurrence["source_window"][1], fixture["frames"])
            for waveform in ("paused", "continuous"):
                for rate in (16000, 44100, 48000):
                    mic = next(row for row in receipt["fixtures"]
                               if row["id"] == f"{waveform}-{rate}-mic")
                    tap = next(row for row in receipt["fixtures"]
                               if row["id"] == f"{waveform}-{rate}-tap")
                    self.assertEqual(tap["frames"] - mic["frames"], frame_count(400, rate))
                    self.assertEqual(tap["occurrences"][0]["source_window"][0]
                                     - mic["occurrences"][0]["source_window"][0], frame_count(400, rate))


if __name__ == "__main__":
    unittest.main()
