#!/usr/bin/env python3
"""Run exact native checks against authored synthetic fixtures, never user audio."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys


def single_test_passed(output, returncode):
    summaries = re.findall(r"(?m)^test result: ([a-z]+)\. (\d+) passed; (\d+) failed;.*$", output)
    return returncode == 0 and summaries == [("ok", "1", "0")]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model-dir", type=Path, required=True)
    parser.add_argument("--fixtures", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    args.output_dir.mkdir(parents=True, exist_ok=True)
    fixture_set = json.loads(args.fixtures.read_text())
    fixtures = fixture_set["fixtures"]
    first = next(f for f in fixtures if f["language"] == "en" and f["sample_rate"] == 16000)
    result_path = args.output_dir / "metal.txt"
    env = dict(os.environ, CORTI_VERIFY_MODEL_DIR=str(args.model_dir.resolve()),
               CORTI_VERIFY_WAV=str(Path(first["path"]).resolve()),
               CORTI_VERIFY_METAL_RESULT=str(result_path.resolve()))
    results = []

    def run_test(name, label, features=()):
        command = ["cargo", "test", "--locked", "--lib", "-p", "corti-transcribe-local"]
        if features:
            command += ["--features", ",".join(features)]
        command += [name, "--", "--ignored", "--exact", "--nocapture"]
        print("+ " + " ".join(command), flush=True)
        result = subprocess.run(command, env=env, text=True, stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT)
        print(result.stdout, flush=True)
        (args.output_dir / f"{label}.log").write_text(result.stdout)
        passed = single_test_passed(result.stdout, result.returncode)
        results.append({"check": label, "passed": passed})
        if not passed:
            raise RuntimeError(f"native check failed or did not execute exactly one test: {label}")

    metal = "not probed"
    failed = False
    try:
        run_test("ggml::tests::quality_probe_metal", "metal-probe", ("ggml",))
        metal = result_path.read_text().strip()
        if metal not in {"available", "unavailable"}:
            raise RuntimeError("Metal probe returned no recognized status")
        engines = ["sherpa"] + (["ggml"] if metal == "available" else [])
        for engine in engines:
            env["CORTI_VERIFY_ASR_ENGINE"] = engine
            for fixture in fixtures:
                env["CORTI_VERIFY_WAV"] = str(Path(fixture["path"]).resolve())
                label = f"{engine}-{fixture['language']}-{fixture['sample_rate']}"
                features = ("ggml",) if engine == "ggml" else ()
                run_test("live::tests::live_equals_batch_over_chunking", label + "-chunking", features)
                if fixture["sample_rate"] != 16000:
                    run_test("live::tests::live_survives_sample_rate_switch", label + "-rate", features)
                if engine == "ggml":
                    run_test("live::tests::ggml_checkpoint_keeps_call_relative_timestamps",
                             label + "-checkpoint", features)
    except (OSError, ValueError, RuntimeError, StopIteration) as error:
        print(f"::error::{error}", flush=True)
        failed = True
    finally:
        report = {"metal": metal, "spanish_available": fixture_set["spanish_available"],
                  "results": results}
        (args.output_dir / "summary.json").write_text(json.dumps(report, indent=2) + "\n")
        summary = os.environ.get("GITHUB_STEP_SUMMARY")
        if summary:
            with open(summary, "a", encoding="utf-8") as output:
                output.write(f"Metal decode: {metal}; Spanish voice: "
                             f"{'available' if report['spanish_available'] else 'unavailable'}; "
                             "full voice list is in Generate synthetic speech logs.\n\n")
                voices = sorted({(f["language"], f["voice"], f["locale"]) for f in fixtures})
                for language, voice, locale in voices:
                    output.write(f"- Voice {language}: {voice} ({locale})\n")
                for result in results:
                    output.write(f"- {result['check']}: {'pass' if result['passed'] else 'FAIL'}\n")
    return int(failed)


if __name__ == "__main__":
    sys.exit(main())
