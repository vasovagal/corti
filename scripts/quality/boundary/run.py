#!/usr/bin/env python3
"""Execute the reviewed sealed #155 matrix; retain every raw run even when scoring fails."""
import argparse
from dataclasses import asdict, is_dataclass
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile

from coverage import CoverageError
from generate import digest, frame_count, validate_definition, validate_seal
from matrix import diagnose, evaluate

NATIVE_SOURCE_FILES = (
    "Cargo.lock", "crates/corti-transcribe/src/boundary.rs", "crates/corti-transcribe/src/decode.rs",
    "crates/corti-transcribe-local/src/lib.rs", "crates/corti-transcribe-local/src/live.rs",
    "crates/corti-transcribe-local/src/boundary.rs", "crates/corti-transcribe-local/src/input_clock.rs",
    "crates/corti-transcribe-local/src/terminal.rs",
    "crates/corti-transcribe-local/src/engine.rs", "crates/corti-transcribe-local/src/asr.rs",
    "crates/corti-transcribe-local/src/resample.rs", "crates/corti-transcribe-local/src/models.rs",
    "crates/corti-transcribe-local/src/native_boundary_matrix.rs",
    "scripts/quality/run_native.py",
)


def sha_stream(source):
    result = hashlib.sha256()
    for block in iter(lambda: source.read(1024 * 1024), b""):
        result.update(block)
    return result.hexdigest()


def verify_models(repo, cache, model_dir):
    spec = importlib.util.spec_from_file_location("quality_native", repo / "scripts/quality/native.py")
    native = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(native)
    catalog = native.catalog_from_source(native.MODEL_SOURCE.read_text())
    result = {}
    for artifact in catalog["artifacts"]:
        if artifact["id"] not in ("parakeet", "vad"):
            continue
        archive = cache / (artifact["sha256"] + ".artifact")
        native.verify_artifact(archive, artifact)
        expected = {}
        if artifact["kind"] == "file":
            expected[artifact["install_rel"]] = artifact["sha256"]
        else:
            with tarfile.open(archive, "r|bz2") as bundle:
                for member in bundle:
                    if member.name in artifact["required_paths"]:
                        if not member.isfile() or member.name in expected:
                            raise CoverageError("invalid required native model member")
                        source = bundle.extractfile(member)
                        if source is None:
                            raise CoverageError("unreadable required native model member")
                        with source:
                            expected[member.name] = sha_stream(source)
        if set(expected) != set(artifact["required_paths"]):
            raise CoverageError("missing required native model member")
        for relative, expected_hash in expected.items():
            installed = native.safe_destination(model_dir, relative)
            if installed.is_symlink() or digest(installed) != expected_hash:
                raise CoverageError("installed native model does not match verified archive")
        result[artifact["id"]] = {"artifact_sha256": artifact["sha256"], "installed_sha256": expected}
    if set(result) != {"parakeet", "vad"}:
        raise CoverageError("native model identity inventory is incomplete")
    return result


def make_plan(seal, directory):
    definition = seal["definition"]
    validate_definition(definition)
    expected = {f"{cell['waveform']}-{cell['sample_rate']}-{track}"
                for cell in definition["matrix"] for track in ("mic", "tap")}
    fixtures = {row["id"]: row for row in seal["fixtures"]}
    if not expected <= set(fixtures) or len(fixtures) != len(seal["fixtures"]):
        raise CoverageError("sealed source fixture inventory mismatch")
    lines = []
    for identifier in sorted(expected):
        fixture = fixtures[identifier]
        path = Path(fixture["path"]).resolve()
        if any(character in str(path) for character in "\t\r\n") or digest(path) != fixture["sha256"]:
            raise CoverageError("sealed source fixture changed before inference")
        lines.append(f"F\t{identifier}\t{fixture['sample_rate']}\t{fixture['frames']}\t{path}")
    for cell in definition["matrix"]:
        rate = cell["sample_rate"]
        forced = ",".join(str(frame_count(value, rate)) for value in cell.get("forced_source_ms", []))
        fields = ("C", cell["id"], cell["waveform"], rate, frame_count(cell["window_ms"], rate),
                  frame_count(cell["phase_ms"], rate), cell["mode"],
                  cell.get("native_epoch_limit_samples", 2**31 - 1 - 512), forced)
        lines.append("\t".join(map(str, fields)))
    path = directory / "native-plan.tsv"
    path.write_text("\n".join(lines) + "\n")
    return path, expected


def boolean(value):
    if value not in {"true", "false"}:
        raise CoverageError("invalid native boolean")
    return value == "true"


def parse_native(path, seal, expected_models, required_fixtures, runtime_identity):
    runs, boundaries, runtime, errors = {}, {}, {}, []
    cell_specs = {row["id"]: row for row in seal["definition"]["matrix"]}
    fixtures = {row["id"]: row for row in seal["fixtures"]}
    for line in path.read_text().splitlines():
        parts = line.split("\t")
        if parts[0] == "R" and len(parts) == 14:
            identifier = parts[1]
            if identifier not in cell_specs or identifier in runtime:
                raise CoverageError("missing or duplicate native runtime identity")
            runtime[identifier] = dict(zip(("sample_rate", "window_frames", "phase_frames", "epoch_limit",
                "settlement_source_reserve", "settlement_accept", "next_accept", "maintenance_trigger",
                "minimum_epoch_samples"), map(int, parts[2:11])))
            runtime[identifier].update(input_chunks=list(map(int, parts[11].split(","))),
                                       vad_threshold=float(parts[12]), vad_min_silence=float(parts[13]))
            continue
        if parts[0] == "B" and len(parts) == 16:
            (_, identifier, kind, due, cut, settled, maintenance, mic_remaining, tap_remaining,
             selected, mic_accepted, tap_accepted, mic_origin, tap_origin, mic_settled, tap_settled) = parts
            if identifier not in cell_specs:
                raise CoverageError("unexpected native boundary cell")
            boundaries.setdefault(identifier, []).append({"kind": kind, "nominal_due_frame": int(due),
                "cut_frame": int(cut), "settled_received_frame": int(settled),
                "maintenance_reset": boolean(maintenance),
                "epoch_remaining": [int(mic_remaining), int(tap_remaining)],
                "selected_received_frame": int(selected),
                "canonical_accepted": [int(mic_accepted), int(tap_accepted)],
                "epoch_origins": [int(mic_origin), int(tap_origin)],
                "settled_canonical_accepted": [int(mic_settled), int(tap_settled)]})
            continue
        if parts[0] == "E" and len(parts) == 2:
            errors.append(parts[1])
            continue
        if len(parts) < 4 or parts[0] not in {"W", "A", "Q", "S", "I", "O"}:
            raise CoverageError("invalid native evidence row")
        kind, role, identifier, track = parts[:4]
        if not ((role in {"batch", "uncut"} and identifier in required_fixtures and track == "-")
                or (role == "cell" and identifier in cell_specs and track in {"mic", "tap"})):
            raise CoverageError("unbound native source run")
        row = runs.setdefault((role, identifier, track), {"words": [], "attempts": [], "quality": [],
                                                         "inputs": [], "ownership": []})
        if "decode_complete" in row:
            raise CoverageError("native evidence after terminal summary")
        if kind == "I" and len(parts) == 7:
            row["inputs"].append(dict(zip(("start", "end", "sample_rate"), map(int, parts[4:7]))))
        elif kind == "O" and len(parts) == 7:
            row["ownership"].append({"start": int(parts[4]), "end": int(parts[5]), "eof": boolean(parts[6])})
        elif kind == "W" and len(parts) == 7:
            row["words"].append({"start": float(parts[4]), "end": float(parts[5]),
                                 "text": bytes.fromhex(parts[6]).decode("utf-8")})
        elif kind == "A" and len(parts) == 7:
            start, end = map(int, parts[4:6])
            if start < 0 or start >= end or (row["attempts"] and start < row["attempts"][-1]["end"]):
                raise CoverageError("native canonical PCM was attempted twice or out of order")
            row["attempts"].append({"start": start, "end": end, "pcm_fnv64": parts[6]})
        elif kind == "Q" and len(parts) == 7:
            row["quality"].append({"start": int(parts[4]), "end": int(parts[5]), "status": parts[6]})
        elif kind == "S" and len(parts) == 9:
            row.update(decode_complete=boolean(parts[4]), pcm_exact_and_once=boolean(parts[5]),
                       omitted_failures=int(parts[6]), omitted_metadata=int(parts[7]),
                       counter_saturated=boolean(parts[8]))
            if (not row["pcm_exact_and_once"] or row["omitted_failures"] or row["omitted_metadata"]
                    or row["counter_saturated"] or not row["attempts"]):
                raise CoverageError("native source ownership or evidence is incomplete")
        else:
            raise CoverageError("invalid native evidence field count")
        if (any(len(row[key]) > 512 for key in ("words", "attempts", "quality", "ownership"))
                or len(row["inputs"]) > 16384):
            raise CoverageError("native evidence exceeds frozen bounds")
    expected_keys = {(role, identifier, "-") for role in ("batch", "uncut") for identifier in required_fixtures}
    expected_keys |= {("cell", identifier, track) for identifier in cell_specs for track in ("mic", "tap")}
    if (set(runs) != expected_keys or set(runtime) != set(cell_specs)
            or any("decode_complete" not in row for row in runs.values()) or errors):
        raise CoverageError("native matrix did not complete every required run")
    baselines = []
    for identifier in sorted(required_fixtures):
        batch, uncut = (runs[role, identifier, "-"] for role in ("batch", "uncut"))
        baselines.append({"id": identifier, "pcm_sha256": fixtures[identifier]["sha256"],
                          "batch_words": batch["words"], "uncut_live_words": uncut["words"],
                          "decode_complete": batch["decode_complete"] and uncut["decode_complete"],
                          "native": {"batch": batch, "uncut": uncut}})
    cells = []
    for identifier, spec in cell_specs.items():
        tracks = {}
        for track in ("mic", "tap"):
            fixture_id = f"{spec['waveform']}-{spec['sample_rate']}-{track}"
            tracks[track] = {**runs["cell", identifier, track], "fixture_id": fixture_id,
                              "pcm_sha256": fixtures[fixture_id]["sha256"]}
        cells.append({"id": identifier, "runtime": runtime[identifier], "tracks": tracks,
                      "boundaries": boundaries.get(identifier, [])})
    seal_sha = hashlib.sha256((json.dumps(seal, indent=2, sort_keys=True) + "\n").encode()).hexdigest()
    return {"engine": "sherpa", "verified_models": expected_models, "definition_sha256": seal["definition_sha256"],
            "fixture_seal_sha256": seal_sha, "runtime_identity": runtime_identity,
            "input_chunks": seal["definition"]["input_chunks"], "baselines": baselines, "cells": cells}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--seal", type=Path, required=True)
    parser.add_argument("--model-dir", type=Path, required=True)
    parser.add_argument("--cache-dir", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    args.output_dir.mkdir(parents=True, exist_ok=True)
    seal = json.loads(args.seal.read_text())
    validate_seal(seal, Path(__file__).with_name("fixtures.json"))
    if seal["definition"] != json.loads(Path(__file__).with_name("fixtures.json").read_text()):
        raise CoverageError("sealed definition differs from reviewed source")
    if seal["definition_sha256"] != digest(Path(__file__).with_name("fixtures.json")):
        raise CoverageError("reviewed definition hash mismatch")
    for name, expected in seal["generator_sources"].items():
        if digest(Path(__file__).with_name(name)) != expected:
            raise CoverageError("reviewed generator or scorer changed after sealing")
    plan_path, expected = make_plan(seal, args.output_dir)
    models = verify_models(args.repo, args.cache_dir, args.model_dir)
    command = ["cargo", "test", "--locked", "-p", "corti-transcribe-local", "--lib", "--no-run", "--message-format=json"]
    build = subprocess.run(command, cwd=args.repo, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    (args.output_dir / "native-matrix-build.log").write_text(build.stdout + build.stderr)
    executables = [Path(row["executable"]) for line in build.stdout.splitlines()
                   if line.startswith("{") and (row := json.loads(line)).get("reason") == "compiler-artifact"
                   and row.get("executable") and row.get("target", {}).get("name") == "corti_transcribe_local"
                   and row.get("profile", {}).get("test")]
    if build.returncode or len(executables) != 1:
        raise CoverageError("native matrix executable was not built exactly once")
    executable = executables[0].resolve()
    toolchain = subprocess.run(["rustc", "--version", "--verbose"], cwd=args.repo, text=True,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True).stdout
    execution_seal = {"fixture_seal_sha256": digest(args.seal), "plan_sha256": digest(plan_path),
                      "verified_models": models, "runner_sha256": digest(__file__),
                      "executable_sha256": digest(executable), "toolchain": toolchain,
                      "native_sources_sha256": {path: digest(args.repo / path) for path in NATIVE_SOURCE_FILES}}
    execution_path = args.output_dir / "execution-seal.json"
    execution_path.write_text(json.dumps(execution_seal, indent=2, sort_keys=True) + "\n")
    print("BOUNDARY_EXECUTION_SEAL " + digest(execution_path), flush=True)
    print(execution_path.read_text(), flush=True)
    native_output = args.output_dir / "native-raw.tsv"
    env = dict(os.environ, CORTI_BOUNDARY_PLAN=str(plan_path.resolve()),
               CORTI_BOUNDARY_OUTPUT=str(native_output.resolve()), CORTI_VERIFY_MODEL_DIR=str(args.model_dir.resolve()))
    command = ["/usr/bin/time", "-l", str(executable), "live::native_boundary_matrix::native_boundary_frozen_matrix",
               "--ignored", "--exact", "--nocapture"]
    process = subprocess.run(command, cwd=args.repo, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    (args.output_dir / "native-matrix.log").write_text(process.stdout)
    print(process.stdout, flush=True)
    for fixture in seal["fixtures"]:
        if digest(fixture["path"]) != fixture["sha256"]:
            raise CoverageError("sealed waveform changed during native inference")
    if digest(executable) != execution_seal["executable_sha256"]:
        raise CoverageError("sealed native executable changed during inference")
    for relative, expected_hash in execution_seal["native_sources_sha256"].items():
        if digest(args.repo / relative) != expected_hash:
            raise CoverageError("sealed source changed during native inference")
    if process.returncode or re.findall(r"(?m)^test result: ok\. (\d+) passed; 0 failed;", process.stdout) != ["1"]:
        raise CoverageError("native boundary matrix did not execute successfully")
    result = parse_native(native_output, seal, models, expected, execution_seal)
    (args.output_dir / "native-results.json").write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    try:
        scored = evaluate(seal, result, models, execution_seal)
    except CoverageError:
        print("\n".join(["BOUNDARY_DIAGNOSTIC", *diagnose(seal, result)]), flush=True)
        raise
    if not scored["passed"]:
        print("\n".join(["BOUNDARY_DIAGNOSTIC", *diagnose(seal, result)]), flush=True)
    (args.output_dir / "scores.json").write_text(json.dumps(scored, indent=2, sort_keys=True,
        default=lambda value: asdict(value) if is_dataclass(value) else value) + "\n")
    for row in scored["declared_differences"]:  # stay visible on every run until #168 removes them
        print("BOUNDARY_DECLARED_DIFFERENCE " + json.dumps(row, sort_keys=True), flush=True)
    print("BOUNDARY_MATRIX " + ("PASS" if scored["passed"] else "FAIL"), flush=True)
    return 0 if scored["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
