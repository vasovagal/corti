import copy
from pathlib import Path
import tempfile
import unittest

from coverage import CoverageError
from matrix import evaluate
from run import parse_native
from test_matrix import RUNTIME_IDENTITY, synthetic_complete_run


def native_rows(result):
    lines = []

    def words(role, identifier, track, row):
        prefix = f"{role}\t{identifier}\t{track}"
        for item in row.get("inputs", []):
            lines.append(f"I\t{prefix}\t{item['start']}\t{item['end']}\t{item['sample_rate']}")
        for item in row.get("ownership", []):
            lines.append(f"O\t{prefix}\t{item['start']}\t{item['end']}\t{str(item['eof']).lower()}")
        for word in row["words"]:
            lines.append(f"W\t{prefix}\t{word['start']}\t{word['end']}\t{word['text'].encode().hex()}")
        lines.append(f"A\t{prefix}\t0\t1600000\t0000000000000000")
        for item in row.get("quality", [{"start": 0, "end": 1, "status": "Complete"}]):
            lines.append(f"Q\t{prefix}\t{item['start']}\t{item['end']}\t{item['status']}")
        lines.append(f"S\t{prefix}\t{str(row['decode_complete']).lower()}\ttrue\t0\t0\tfalse")

    for baseline in result["baselines"]:
        for role, field in (("batch", "batch_words"), ("uncut", "uncut_live_words")):
            words(role, baseline["id"], "-", baseline["native"][role])
    for cell in result["cells"]:
        runtime = cell["runtime"]
        values = [runtime[key] for key in ("sample_rate", "window_frames", "phase_frames", "epoch_limit",
            "settlement_source_reserve", "settlement_accept", "next_accept", "maintenance_trigger", "minimum_epoch_samples")]
        values += [",".join(map(str, runtime["input_chunks"])), runtime["vad_threshold"], runtime["vad_min_silence"]]
        lines.append("\t".join(map(str, ["R", cell["id"], *values])))
        for event in cell["boundaries"]:
            values = [event[key] for key in ("kind", "nominal_due_frame", "cut_frame", "settled_received_frame")]
            values += [str(event["maintenance_reset"]).lower(), *event["epoch_remaining"]]
            values += [event["selected_received_frame"], *event["canonical_accepted"],
                       *event["epoch_origins"], *event["settled_canonical_accepted"]]
            lines.append("\t".join(map(str, ["B", cell["id"], *values])))
        for track, row in cell["tracks"].items():
            words("cell", cell["id"], track, row)
    return lines


class RunnerTests(unittest.TestCase):
    def parse(self, lines, seal, result):
        required = {row["id"] for row in result["baselines"]}
        with tempfile.TemporaryDirectory(prefix="corti155-run-parser-") as directory:
            path = Path(directory) / "raw.tsv"
            path.write_text("\n".join(lines) + "\n")
            return parse_native(path, seal, result["verified_models"], required, RUNTIME_IDENTITY)

    def test_native_receipt_roundtrip_binds_all_controls_cells_and_tracks(self):
        seal, result = synthetic_complete_run()
        parsed = self.parse(native_rows(result), seal, result)
        scored = evaluate(seal, parsed, result["verified_models"], RUNTIME_IDENTITY)
        self.assertTrue(scored["passed"])
        self.assertEqual(len(parsed["cells"]), 13)
        self.assertEqual(len(parsed["baselines"]), 10)

    def test_missing_duplicate_failed_or_unbound_native_run_refuses(self):
        seal, result = synthetic_complete_run()
        original = native_rows(result)
        for mutation in ("missing", "duplicate", "error", "unbound", "overlap", "metadata"):
            lines = original.copy()
            if mutation == "missing":
                lines = [line for line in lines if not line.startswith("R\tpaused-16-phase0\t")]
            elif mutation == "duplicate":
                lines.append(next(line for line in lines if line.startswith("S\t")))
            elif mutation == "error":
                lines.append("E\tcontinuous-phase0")
            elif mutation == "unbound":
                lines[0] = lines[0].replace("\tbatch\t", "\tunknown\t")
            elif mutation == "overlap":
                index = next(index for index, line in enumerate(lines) if line.startswith("A\t"))
                lines.insert(index, lines[index])
            else:
                index = next(index for index, line in enumerate(lines) if line.startswith("S\t"))
                lines[index] = lines[index][:-len("0\tfalse")] + "1\tfalse"
            with self.subTest(mutation=mutation), self.assertRaises(CoverageError):
                self.parse(lines, seal, result)

    def test_native_receipt_does_not_relabel_a_different_rate_or_chunk_cycle(self):
        seal, result = synthetic_complete_run()
        for field, replacement in (("sample_rate", 8000), ("input_chunks", [1]), ("vad_threshold", 0.99)):
            changed = copy.deepcopy(result)
            changed["cells"][0]["runtime"][field] = replacement
            parsed = self.parse(native_rows(changed), seal, changed)
            with self.subTest(field=field), self.assertRaises(CoverageError):
                evaluate(seal, parsed, result["verified_models"], RUNTIME_IDENTITY)


if __name__ == "__main__":
    unittest.main()
