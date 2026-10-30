#!/usr/bin/env python3
"""Generate and seal only the authored #155 corpus before any model is loaded."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import wave


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def run(command):
    result = subprocess.run(command, text=True, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, timeout=180, check=False)
    if result.returncode:
        raise ValueError("authored fixture tool failed")
    return result.stdout


# The standard macOS US voice. The alphabetically first voice is a novelty voice (Albert) whose
# speech the model mostly mishears, which would leave no usable batch reference.
PREFERRED_VOICE = ("Samantha", "en_US")


def voice_from_listing(listing):
    matches = re.findall(r"^\s*(.+?)\s+(en_[A-Za-z_]+)\s+#", listing, re.MULTILINE)
    if not matches:
        raise ValueError("no installed English synthetic voice")
    if PREFERRED_VOICE in matches:
        return PREFERRED_VOICE
    return min(matches, key=lambda item: (item[1] != "en_US", item))


def read_format(path, rate):
    with wave.open(str(path), "rb") as source:
        if (source.getnchannels(), source.getsampwidth(), source.getframerate(), source.getcomptype()) \
                != (1, 2, rate, "NONE"):
            raise ValueError("unexpected authored WAV format")
        frames = source.getnframes()
        if frames <= 0 or not any(any(block) for block in iter(lambda: source.readframes(4096), b"")):
            raise ValueError("empty authored speech WAV")
        return frames


def zeros(output, frames):
    while frames:
        count = min(frames, 4096)
        output.writeframesraw(bytes(count * 2))
        frames -= count


def copy_pcm(output, path):
    with wave.open(str(path), "rb") as source:
        for block in iter(lambda: source.readframes(4096), b""):
            output.writeframesraw(block)


def frame_count(milliseconds, rate):
    product = milliseconds * rate
    if type(milliseconds) is not int or milliseconds < 0 or product % 1000:
        raise ValueError("fixture time is not an exact source frame")
    return product // 1000


def validate_definition(definition):
    if definition["schema_version"] != 1:
        raise ValueError("unsupported authored fixture schema")
    sentences = {row["id"]: row for row in definition["sentences"]}
    if len(sentences) != len(definition["sentences"]):
        raise ValueError("duplicate authored sentence")
    for row in [*sentences.values(), *[r for r in definition["waveforms"] if "text" in r]]:
        authored = row["text"].split()
        cursor, identities = 0, set()
        for occurrence in row["critical"]:
            surface = occurrence["authored"]
            if occurrence["id"] in identities or not surface or not occurrence["alternatives"]:
                raise ValueError("invalid authored occurrence identity")
            identities.add(occurrence["id"])
            matches = [index for index in range(cursor, len(authored) - len(surface) + 1)
                       if authored[index:index + len(surface)] == surface]
            if not matches:
                raise ValueError("critical occurrence is absent or out of authored order")
            cursor = matches[0] + len(surface)
    matrix = definition["matrix"]
    if len(matrix) != 13 or len({row["id"] for row in matrix}) != len(matrix):
        raise ValueError("required boundary matrix is incomplete")
    for row in matrix:
        if (row["sample_rate"] not in definition["generation"]["sample_rates"]
                or row["waveform"] not in {item["id"] for item in definition["waveforms"]}
                or not 0 <= row["phase_ms"] < row["window_ms"]):
            raise ValueError("invalid required boundary cell")
        for value in (row["phase_ms"], row["window_ms"], *row.get("forced_source_ms", [])):
            frame_count(value, row["sample_rate"])
    if definition["tracks"] != [{"id": "mic", "prefix_silence_ms": 0},
                                {"id": "tap", "prefix_silence_ms": 400}]:
        raise ValueError("required source-track matrix changed")
    return sentences


def validate_seal(receipt, definition_path):
    """Rebuild authored windows and verify assembled PCM before loading any ASR model."""
    definition_path = Path(definition_path)
    definition = json.loads(definition_path.read_text())
    sentences = validate_definition(definition)
    generation = definition["generation"]
    if (receipt["definition"] != definition or receipt["definition_sha256"] != digest(definition_path)
            or receipt["matrix"] != definition["matrix"] or receipt["schema_version"] != 1
            or receipt["synthetic_only"] is not True or receipt["pre_inference_seal"] is not True):
        raise ValueError("fixture seal does not match the reviewed authored definition")
    sources = {name: digest(Path(__file__).parent / name)
               for name in ("generate.py", "coverage.py", "score.py", "matrix.py", "run.py")}
    if receipt["generator_sources"] != sources:
        raise ValueError("fixture seal does not bind every reviewed generator and validator")
    fixtures = {row["id"]: row for row in receipt["fixtures"]}
    expected_ids = {f"{waveform['id']}-{rate}-{track['id']}" for waveform in definition["waveforms"]
                    for rate in generation["sample_rates"] for track in definition["tracks"]}
    if len(fixtures) != len(receipt["fixtures"]) or set(fixtures) != expected_ids:
        raise ValueError("fixture seal omitted, duplicated or invented a source")

    def silent(reader, count):
        while count:
            n = min(count, 4096)
            data = reader.readframes(n)
            if len(data) != 2 * n or any(data):
                raise ValueError("authored digital pause differs from the frozen construction")
            count -= n

    for waveform in definition["waveforms"]:
        for rate in generation["sample_rates"]:
            for track in definition["tracks"]:
                identifier = f"{waveform['id']}-{rate}-{track['id']}"
                fixture = fixtures[identifier]
                path = Path(fixture["path"])
                if (fixture["waveform"] != waveform["id"] or fixture["track"] != track["id"]
                        or fixture["sample_rate"] != rate or digest(path) != fixture["sha256"]
                        or read_format(path, rate) != fixture["frames"]):
                    raise ValueError("authored source byte identity or format changed")
                prefix = frame_count(track["prefix_silence_ms"], rate)
                kind = "continuous_" if waveform["id"] == "continuous" else ""
                leading = frame_count(generation[kind + "leading_silence_ms"], rate)
                trailing = frame_count(generation[kind + "trailing_silence_ms"], rate)
                pause = frame_count(generation["between_utterance_silence_ms"], rate)
                order = waveform.get("sentence_order", [waveform["id"]])
                cursor, utterances, annotations = prefix + leading, [], []
                with wave.open(str(path), "rb") as source:
                    silent(source, cursor)
                    for number, sentence_id in enumerate(order):
                        row = sentences.get(sentence_id, waveform)
                        utterance = path.parent / f"utterance-{sentence_id}-{rate}.wav"
                        count = read_format(utterance, rate)
                        start, end = cursor, cursor + count
                        with wave.open(str(utterance), "rb") as original:
                            for block in iter(lambda: original.readframes(4096), b""):
                                if source.readframes(len(block) // 2) != block:
                                    raise ValueError("authored utterance PCM differs from sealed assembly")
                        utterances.append({"id": f"{number}:{sentence_id}", "start_frame": start,
                            "end_frame": end, "text": row["text"], "utterance_wav_sha256": digest(utterance)})
                        margin = frame_count(250, rate)
                        annotations.extend({**item, "id": f"{number}:{sentence_id}:{item['id']}",
                                            "source_window": [start - margin, end + margin]}
                                           for item in row["critical"])
                        cursor = end
                        if number + 1 != len(order):
                            silent(source, pause)
                            cursor += pause
                    silent(source, trailing)
                    cursor += trailing
                    if source.readframes(1):
                        raise ValueError("authored source has undeclared suffix PCM")
                if (fixture["utterances"] != utterances or fixture["occurrences"] != annotations
                        or fixture["frames"] != cursor
                        or cursor - prefix < waveform.get("minimum_duration_seconds", 0) * rate
                        or sum(row["end_frame"] - row["start_frame"] for row in utterances)
                        <= waveform.get("minimum_spoken_duration_seconds", 0) * rate):
                    raise ValueError("authored windows, alternatives or duration changed before inference")


def generate(definition_path, output_directory, tool=run):
    definition_path, output_directory = Path(definition_path), Path(output_directory)
    definition = json.loads(definition_path.read_text())
    sentences = validate_definition(definition)
    generation = definition["generation"]
    output_directory.mkdir(parents=True, exist_ok=True)
    voice, locale = voice_from_listing(tool(["say", "-v", "?"]))
    rows = [*sentences.values(), *[row for row in definition["waveforms"] if "text" in row]]
    audio = {}
    for row in rows:
        text = output_directory / (row["id"] + ".txt")
        text.write_text(row["text"] + "\n")
        aiff = output_directory / (row["id"] + ".aiff")
        tool(["say", "-v", voice, "-r", str(generation["words_per_minute"]),
              "-o", str(aiff), "-f", str(text)])
        for rate in generation["sample_rates"]:
            wav = output_directory / f"utterance-{row['id']}-{rate}.wav"
            tool(["afconvert", "-f", "WAVE", "-d", f"LEI16@{rate}", "-c", "1", str(aiff), str(wav)])
            audio[row["id"], rate] = (wav, read_format(wav, rate))
    fixtures = []
    for waveform in definition["waveforms"]:
        for rate in generation["sample_rates"]:
            order = waveform.get("sentence_order", [waveform["id"]])
            for track in definition["tracks"]:
                path = output_directory / f"{waveform['id']}-{rate}-{track['id']}.wav"
                prefix = frame_count(track["prefix_silence_ms"], rate)
                kind = "continuous_" if waveform["id"] == "continuous" else ""
                leading = frame_count(generation[kind + "leading_silence_ms"], rate)
                trailing = frame_count(generation[kind + "trailing_silence_ms"], rate)
                pause = frame_count(generation["between_utterance_silence_ms"], rate)
                annotations, utterances = [], []
                cursor = prefix + leading
                with wave.open(str(path), "wb") as output:
                    output.setparams((1, 2, rate, 0, "NONE", "not compressed"))
                    zeros(output, cursor)
                    for number, identifier in enumerate(order):
                        wav, count = audio[identifier, rate]
                        row = sentences.get(identifier, waveform)
                        start, end = cursor, cursor + count
                        copy_pcm(output, wav)
                        margin = frame_count(250, rate)
                        for annotation in row["critical"]:
                            annotations.append({**annotation,
                                                "id": f"{number}:{identifier}:{annotation['id']}",
                                                "source_window": [start - margin, end + margin]})
                        utterances.append({"id": f"{number}:{identifier}", "start_frame": start,
                                           "end_frame": end, "text": row["text"],
                                           "utterance_wav_sha256": digest(wav)})
                        cursor = end
                        if number + 1 != len(order):
                            zeros(output, pause)
                            cursor += pause
                    zeros(output, trailing)
                    cursor += trailing
                if cursor != read_format(path, rate):
                    raise ValueError("authored source count mismatch")
                if (cursor - prefix < waveform.get("minimum_duration_seconds", 0) * rate
                        or sum(row["end_frame"] - row["start_frame"] for row in utterances)
                        <= waveform.get("minimum_spoken_duration_seconds", 0) * rate):
                    raise ValueError("authored waveform does not meet frozen duration requirement")
                fixtures.append({"id": f"{waveform['id']}-{rate}-{track['id']}",
                                 "waveform": waveform["id"], "track": track["id"],
                                 "sample_rate": rate, "frames": cursor, "path": str(path.resolve()),
                                 "sha256": digest(path), "utterances": utterances,
                                 "occurrences": annotations})
    sources = {name: digest(Path(__file__).parent / name)
               for name in ("generate.py", "coverage.py", "score.py", "matrix.py", "run.py")}
    receipt = {"schema_version": 1, "synthetic_only": True, "pre_inference_seal": True,
               "definition_sha256": digest(definition_path), "definition": definition,
               "generator_sources": sources, "voice": voice, "locale": locale,
               "fixtures": fixtures, "matrix": definition["matrix"]}
    destination = output_directory / "boundary-fixtures.json"
    validate_seal(receipt, definition_path)
    destination.write_text(json.dumps(receipt, indent=2, sort_keys=True) + "\n")
    return destination


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--definition", type=Path, default=Path(__file__).with_name("fixtures.json"))
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    sealed = generate(args.definition, args.output_dir)
    print("BOUNDARY_FIXTURE_SEAL " + digest(sealed))
