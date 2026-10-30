"""Validate the complete frozen native matrix; no missing cell or coverage error is a pass."""
import hashlib
import json
import math
import re
import struct

from coverage import CoverageError, bind_occurrences, compare_cell
from generate import frame_count
from score import compare, tokens


ACCEPTED_GUARD = 5727
VAD_WINDOW = 512
FORCED_REPLAY_PREROLL = 16000
REPLAY_CONTEXT = FORCED_REPLAY_PREROLL + VAD_WINDOW - 1


def replay_origin(retained, owned_start, preroll):
    """The native reset replay: on the VAD window grid, never before retained PCM."""
    return max(retained, max(owned_start - preroll, 0) // VAD_WINDOW * VAD_WINDOW)


def expected_runtime(spec, chunks):
    """Pinned integer/f32 reserve calculation for this no-AEC native matrix, checked against Rust."""
    rate = spec["sample_rate"]
    ceil = lambda numerator, denominator: (numerator + denominator - 1) // denominator
    f32 = lambda value: struct.unpack("f", struct.pack("f", value))[0]
    ticks = math.lcm(rate, 16000)
    cutoff = f32(f32(f32(0.99) * 0.5) * min(rate, 16000))
    width = f32(6.0 / (2.0 * cutoff))
    lag_ticks = math.floor(f32(width * f32(ticks)))
    source_lag, canonical_lag = ceil(lag_ticks, ticks // rate), ceil(lag_ticks, ticks // 16000)
    reserve = ceil(6238 * rate, 16000) + 511 + source_lag + 4096
    settlement = ceil((reserve + 511) * 16000, rate) + ceil(511 * 16000, rate) + canonical_lag + 511
    burst = ceil((4096 + 511) * 16000, rate) + 511
    window = frame_count(spec["window_ms"], rate)
    owned = ceil((window + 22 * rate) * 16000, rate)
    trigger = owned + settlement + burst
    return {"sample_rate": rate, "window_frames": window,
            "phase_frames": frame_count(spec["phase_ms"], rate),
            "epoch_limit": spec.get("native_epoch_limit_samples", 2**31 - 1 - 512),
            "settlement_source_reserve": reserve, "settlement_accept": settlement,
            "next_accept": burst, "maintenance_trigger": trigger,
            "minimum_epoch_samples": trigger + settlement + REPLAY_CONTEXT + 1, "input_chunks": chunks,
            "vad_threshold": 0.5, "vad_min_silence": 1.0}


def validate_admission_cycle(cell, spec, fixtures, chunks):
    """Reproduce every push from the frozen cycle and ordinary/settlement admission bounds."""
    runtime, events = cell["runtime"], cell["boundaries"]
    lengths = {track: fixtures[row["fixture_id"]]["frames"] for track, row in cell["tracks"].items()}
    total, received, index, completed = max(lengths.values()), 0, 0, 0
    expected = {track: [] for track in lengths}
    visited = {0}
    while received < total:
        while completed < len(events) and events[completed]["settled_received_frame"] <= received:
            completed += 1
        next_event = events[completed] if completed < len(events) else None
        count = min(chunks[index % len(chunks)], total - received)
        if next_event and next_event["selected_received_frame"] <= received:
            count = min(count, next_event["cut_frame"] + runtime["settlement_source_reserve"] - received)
        elif spec["mode"] == "explicit-forced-probe":
            if next_event:
                count = min(count, next_event["cut_frame"] - received)
        else:
            due = (events[completed - 1]["cut_frame"] + runtime["window_frames"] if completed
                   else runtime["window_frames"] - runtime["phase_frames"])
            count = min(count, (due if received < due else due + 22 * spec["sample_rate"]) - received)
        if count <= 0:
            raise CoverageError("native admission continued past an unrecorded boundary")
        for track, frames in lengths.items():
            if received < frames:
                expected[track].append({"start": received, "end": min(received + count, frames),
                                        "sample_rate": spec["sample_rate"]})
        received += count
        visited.add(received)
        index += 1
        if index > 16384:
            raise CoverageError("native admission cycle exceeded evidence bound")
    if any(cell["tracks"][track]["inputs"] != rows for track, rows in expected.items()):
        raise CoverageError("actual native pushes differ from the frozen chunk cycle and admission bounds")
    if any(event[key] not in visited for event in events
           for key in ("selected_received_frame", "settled_received_frame")):
        raise CoverageError("native boundary transition did not occur at an actual admission endpoint")


def validate_track_evidence(actual, fixture, events):
    """Require exhaustive source delivery and filing ownership, including the terminal remainder."""
    frames, rate = fixture["frames"], fixture["sample_rate"]
    inputs = actual["inputs"]
    if not isinstance(inputs, list) or not inputs or len(inputs) > 16384:
        raise CoverageError("missing or oversized native admission evidence")
    cursor = 0
    for row in inputs:
        if (set(row) != {"start", "end", "sample_rate"}
                or any(type(row[key]) is not int for key in row)
                or row["start"] != cursor or not cursor < row["end"] <= min(cursor + 4096, frames)
                or row["sample_rate"] != rate):
            raise CoverageError("native input changed rate, skipped, duplicated or exceeded admission")
        cursor = row["end"]
    if cursor != frames:
        raise CoverageError("native input did not reach source EOF")
    expected, cursor = [], 0
    for event in events:
        end = min(event["cut_frame"], frames)
        if end > cursor:
            expected.append({"start": cursor, "end": end, "eof": False})
        cursor = end
    if cursor < frames:
        expected.append({"start": cursor, "end": frames, "eof": True})
    elif expected:
        expected[-1]["eof"] = True
    if actual["ownership"] != expected or not expected or expected[-1]["eof"] is not True:
        raise CoverageError("native filing ownership does not partition source through explicit EOF")
    validate_canonical_evidence(actual, fixture)


def validate_canonical_evidence(actual, fixture):
    frames, rate = fixture["frames"], fixture["sample_rate"]
    fields = {"words", "attempts", "quality", "inputs", "ownership", "decode_complete",
              "pcm_exact_and_once", "omitted_failures", "omitted_metadata", "counter_saturated"}
    if (set(actual) not in (fields, fields | {"fixture_id", "pcm_sha256"})
            or type(actual["decode_complete"]) is not bool
            or type(actual["counter_saturated"]) is not bool
            or any(type(actual[key]) is not int or actual[key] < 0 for key in ("omitted_failures", "omitted_metadata"))):
        raise CoverageError("unknown or malformed native proof fields")
    if actual["pcm_exact_and_once"] is not True or actual["omitted_failures"] or actual["omitted_metadata"] or actual["counter_saturated"]:
        raise CoverageError("native canonical PCM or report coverage is incomplete")
    cursor = 0
    for attempt in actual["attempts"]:
        if (set(attempt) != {"start", "end", "pcm_fnv64"}
                or type(attempt["start"]) is not int or type(attempt["end"]) is not int
                or not cursor <= attempt["start"] < attempt["end"] <= (frames * 16000 + rate - 1) // rate
                or not isinstance(attempt["pcm_fnv64"], str) or re.fullmatch(r"[0-9a-f]{16}", attempt["pcm_fnv64"]) is None):
            raise CoverageError("native canonical attempts overlap or exceed actual source")
        cursor = attempt["end"]
    if not actual["attempts"] or len(actual["attempts"]) > 512:
        raise CoverageError("missing or excessive canonical attempt evidence")
    quality = actual["quality"]
    if len(quality) != len(actual["attempts"]):
        raise CoverageError("native attempts lack one typed outcome each")
    for outcome, attempt in zip(quality, actual["attempts"]):
        if (set(outcome) != {"start", "end", "status"}
                or type(outcome["start"]) is not int or type(outcome["end"]) is not int
                or not 0 <= outcome["start"] < outcome["end"] <= frames
                or outcome["status"] not in {"Complete", "RecognizedEmpty"}
                or abs(outcome["start"] * 16000 - attempt["start"] * rate) > 16000
                or abs(outcome["end"] * 16000 - attempt["end"] * rate) > 16000):
            raise CoverageError("native typed outcome is failed, unbound or outside source ownership")


def category(identifier):
    tail = identifier.rsplit(":", 1)[-1]
    if "name" in tail:
        return "name"
    if "negation" in tail:
        return "negation"
    if tail.startswith(("amount", "contrast", "version")):
        return "number"
    return "repeat"


def critical_crossings(coverage, words, frame, rate):
    result = []
    # Coverage indices address whitespace token pieces. Recover the original ASR word identity.
    from score import tokens
    token_rows = tokens(word["text"] for word in words)
    for occurrence in coverage:
        first = words[token_rows[occurrence["indices"][0]].word_index]["start"]
        last = words[token_rows[occurrence["indices"][-1]].word_index]["end"]
        if first * rate < frame < last * rate:
            result.append({"id": occurrence["id"], "category": category(occurrence["id"])})
    return result


# A forced or deadline cut splits a speech region without overlap re-decoding. The word straddling
# the cut is decoded in two parts, and each part loses context, so number formatting can differ
# elsewhere in the region. The contract for such cells is no silent loss and no changed value, not
# equality with the uncut batch:
SEAM_SECONDS = 1.0  # damage at a marked forced cut stays within this distance of it
SEAM_EDIT_LIMIT = 4  # edit operations per forced cut and track
OUTSIDE_EDIT_PERCENT = 5  # the ordinary word gate, away from seams and formatting-only spans


def formatting_key(text):
    """Equal only for formatting variants: case, sentence punctuation, digit grouping, euro sign."""
    text = re.sub(r"(\d) ?mm\b", r"\1 millimeters", text)  # case-sensitive: mM is a different unit
    text = re.sub(r"(?<=\d),(?=\d{3})", "", text.lower())
    text = re.sub(r"€(\d+)", r"\1 euros", text)
    text = re.sub(r"(\d+)€", r"\1 euros", text)
    return " ".join(piece.strip(".,!?;:") for piece in text.split())


def forced_gate(score, reference_words, hypothesis_words, cuts):
    """Gate for a cell with forced cuts (seconds); natural-only cells keep the strict gate."""
    from score import tokens
    result = score["score"]
    reference = tokens(word["text"] for word in reference_words)
    hypothesis = tokens(word["text"] for word in hypothesis_words)
    path = result["alignment"]

    def midpoint(words, token):
        word = words[token.word_index]
        return (word["start"] + word["end"]) / 2

    def seam(step):
        _, r, h = step
        time = (midpoint(reference_words, reference[r]) if r is not None
                else midpoint(hypothesis_words, hypothesis[h]))
        return next((cut for cut in cuts if abs(time - cut) <= SEAM_SECONDS), None)

    exempt, degraded, lost, changed, formatting, changed_text = set(), [], [], [], [], {}
    for row in score["coverage"]:
        if result["critical_occurrences"][row["id"]]:
            continue
        steps = [index for index, (_, r, _) in enumerate(path) if r in row["indices"]]
        span = range(steps[0], steps[-1] + 1)
        if any(seam(path[index]) is not None for index in span):
            degraded.append(row["id"])
        elif all(path[index][0] == "delete" for index in steps):
            lost.append(row["id"])
        elif (formatting_key(" ".join(reference[index].raw for index in row["indices"]))
              == formatting_key(" ".join(hypothesis[path[index][2]].raw for index in span
                                         if path[index][2] is not None))):
            formatting.append(row["id"])
            exempt.update(span)
        else:
            changed.append(row["id"])
            changed_text[row["id"]] = (
                " ".join(reference[index].raw for index in row["indices"]) + " -> "
                + " ".join(hypothesis[path[index][2]].raw for index in span if path[index][2] is not None))
    seam_edits, outside_edits = {cut: 0 for cut in cuts}, 0
    for index, step in enumerate(path):
        if step[0] == "equal":
            continue
        cut = seam(step)
        if cut is not None:
            seam_edits[cut] += 1
        elif index not in exempt:
            outside_edits += 1
    introduced = [h for index, (_, _, h) in enumerate(path)
                  if h in result["introduced_critical_indices"] and index not in exempt
                  and seam(path[index]) is None]
    return {"seam_edits": {str(cut): count for cut, count in seam_edits.items()},
            "outside_edits": outside_edits, "degraded_critical": degraded,
            "lost_critical": lost, "changed_critical": changed, "changed_text": changed_text,
            "formatting_only_critical": formatting, "introduced_critical": introduced,
            "passed": (all(count <= SEAM_EDIT_LIMIT for count in seam_edits.values())
                       and 100 * outside_edits <= OUTSIDE_EDIT_PERCENT * len(reference)
                       and not lost and not changed and not introduced)}


def unique_index(rows, expected, label):
    if not isinstance(rows, list) or any(not isinstance(row, dict) or "id" not in row for row in rows):
        raise CoverageError("invalid " + label + " inventory")
    actual = {row["id"]: row for row in rows}
    if len(actual) != len(rows) or set(actual) != set(expected):
        raise CoverageError("missing, extra or duplicated " + label)
    return actual


def boundary_local(score, words, frame, rate):
    """Inspect the original whole-run alignment; do not realign or relax the global gate."""
    from score import tokens
    baseline = tokens(word["text"] for word in words)
    indices = {index for index, token in enumerate(baseline)
               if words[token.word_index]["start"] * rate < frame + rate
               and words[token.word_index]["end"] * rate > frame - rate}
    return {"cut_frame": frame,
            "reference_token_indices": sorted(indices),
            "alignment": [row for row in score["score"]["alignment"] if row[1] in indices],
            "critical_occurrences": {row["id"]: score["score"]["critical_occurrences"][row["id"]]
                                     for row in score["coverage"] if set(row["indices"]) & indices}}


def diagnose(seal, result, context_seconds=1.5):
    """Read-only CI log report for a failed run. It never raises and never affects acceptance."""
    lines = []
    try:
        fixtures = {row["id"]: row for row in seal["fixtures"]}
        specs = {row["id"]: row for row in seal["definition"]["matrix"]}
        baselines = {row["id"]: row for row in result["baselines"]}
    except Exception as error:
        return [f"diagnostic: unreadable seal or result ({error!r})"]

    def text(words, low, high):
        return " ".join(word["text"] for word in words if word["end"] > low and word["start"] < high)

    for identifier, baseline in sorted(baselines.items()):
        try:
            fixture, words = fixtures[identifier], baseline["batch_words"]
            rate = fixture["sample_rate"]
            for occurrence in fixture["occurrences"]:
                try:
                    bind_occurrences(words, [occurrence], fixture["frames"], rate)
                except CoverageError as error:
                    if "ambiguous" in str(error):
                        continue  # repeated phrases bind only jointly with their siblings
                    low, high = (frame / rate for frame in occurrence["source_window"])
                    lines.append(f"baseline {identifier}: unbound {occurrence['id']} ({error}); "
                                 f"alternatives {occurrence['alternatives']}; window {low:.2f}-{high:.2f}s; "
                                 f"batch: {text(words, low - 0.5, high + 0.5)}")
        except Exception as error:
            lines.append(f"baseline {identifier}: unreadable ({error!r})")
    lexemes = set(seal["definition"]["comparison"]["extra_critical_lexemes"])
    for fixture in fixtures.values():
        for occurrence in fixture["occurrences"]:
            lexemes.update(token for alternative in occurrence["alternatives"] for token in alternative)
    for identifier in ("paused-16000-mic", "paused-16000-tap"):
        try:
            fixture, words = fixtures[identifier], baselines[identifier]["batch_words"]
            bound = bind_occurrences(words, fixture["occurrences"], fixture["frames"], fixture["sample_rate"])
            reference = tokens(word["text"] for word in words)
            lines.append(f"critical times {identifier}: " + " ".join(
                f"{category(row['id'])}:{words[reference[row['indices'][0]].word_index]['start']:.2f}"
                f"-{words[reference[row['indices'][-1]].word_index]['end']:.2f}" for row in bound))
        except Exception as error:
            lines.append(f"critical times {identifier}: unavailable ({error!r})")
    for cell in result["cells"]:
        try:
            spec = specs[cell["id"]]
            for track, actual in sorted(cell["tracks"].items()):
                fixture_id = f"{spec['waveform']}-{spec['sample_rate']}-{track}"
                reference, hypothesis = baselines[fixture_id]["batch_words"], actual["words"]
                score = compare([word["text"] for word in reference], [word["text"] for word in hypothesis], [])
                lines.append(f"cell {cell['id']}/{track}: {score['edits']} edits / {score['denominator']} "
                             f"tokens {score['counts']}; decode_complete={actual['decode_complete']}")
                try:
                    fixture = fixtures[fixture_id]
                    scored = compare_cell(reference, hypothesis, fixture["occurrences"], fixture["frames"],
                                          fixture["sample_rate"], lexemes)
                    cuts = [event["cut_frame"] / spec["sample_rate"] for event in cell["boundaries"]
                            if event["kind"] in {"deadline", "explicit"}]
                    if cuts:
                        gate = forced_gate(scored, reference, hypothesis, cuts)
                        lines.append(f"  verdict forced_gate passed={gate['passed']} " + json.dumps(
                            {key: value for key, value in gate.items() if key != "passed"}, sort_keys=True))
                    else:
                        lines.append(f"  verdict strict passed={scored['passed']}")
                    crossed = sorted({row["category"] for event in cell["boundaries"]
                                      for row in critical_crossings(scored["coverage"], reference,
                                                                    event["nominal_due_frame"], spec["sample_rate"])})
                    lines.append(f"  nominal crossings {crossed}")
                except Exception as error:
                    lines.append(f"  verdict unavailable ({error!r})")
                batch_attempts = baselines[fixture_id]["native"]["batch"]["attempts"]

                def spans(attempts, low, high):
                    return " ".join(f"{row['start'] / 16000:.3f}-{row['end'] / 16000:.3f}" for row in attempts
                                    if row["end"] > low * 16000 and row["start"] < high * 16000)

                for event in cell["boundaries"]:
                    at = event["cut_frame"] / spec["sample_rate"]
                    low, high = at - context_seconds, at + context_seconds
                    lines.append(f"  {event['kind']} cut {at:.3f}s ref: {text(reference, low, high)}")
                    lines.append(f"  {event['kind']} cut {at:.3f}s hyp: {text(hypothesis, low, high)}")
                    lines.append(f"  {event['kind']} cut {at:.3f}s regions ref: {spans(batch_attempts, at - 3, at + 3)}"
                                 f" | hyp: {spans(actual['attempts'], at - 3, at + 3)}")
        except Exception as error:
            lines.append(f"cell {cell.get('id', '?') if isinstance(cell, dict) else '?'}: unreadable ({error!r})")
    return lines


def evaluate(seal, result, expected_models, expected_runtime_identity):
    definition = seal["definition"]
    sealed_digest = hashlib.sha256((json.dumps(seal, indent=2, sort_keys=True) + "\n").encode()).hexdigest()
    if (result["definition_sha256"] != seal["definition_sha256"]
            or result["fixture_seal_sha256"] != sealed_digest
            or not expected_models or result["verified_models"] != expected_models
            or not expected_runtime_identity or result["runtime_identity"] != expected_runtime_identity
            or result["input_chunks"] != definition["input_chunks"]
            or result["engine"] != "sherpa" or not seal["pre_inference_seal"]
            or not seal["synthetic_only"]):
        raise CoverageError("unbound native fixture identity")
    specs = {row["id"]: row for row in definition["matrix"]}
    fixtures = {row["id"]: row for row in seal["fixtures"]}
    required_fixtures = {f"{cell['waveform']}-{cell['sample_rate']}-{track}"
                         for cell in specs.values() for track in ("mic", "tap")}
    baselines = unique_index(result["baselines"], required_fixtures, "baseline")
    cells = unique_index(result["cells"], specs, "matrix cell")
    lexemes = set(definition["comparison"]["extra_critical_lexemes"])
    for fixture in fixtures.values():
        for occurrence in fixture["occurrences"]:
            lexemes.update(token for alternative in occurrence["alternatives"] for token in alternative)
    baseline_scores = {}
    for identifier, baseline in baselines.items():
        fixture = fixtures[identifier]
        if (baseline["pcm_sha256"] != fixture["sha256"] or baseline["decode_complete"] is not True):
            raise CoverageError("incomplete or wrongly bound baseline")
        native = baseline["native"]
        if set(native) != {"batch", "uncut"}:
            raise CoverageError("missing native batch or uncut control evidence")
        for role, actual in native.items():
            validate_canonical_evidence(actual, fixture)
            expected_inputs, position, index = [], 0, 0
            while position < fixture["frames"]:
                count = fixture["frames"] if role == "batch" else definition["input_chunks"][index % len(definition["input_chunks"]) ]
                end = min(position + count, fixture["frames"])
                expected_inputs.append({"start": position, "end": end, "sample_rate": fixture["sample_rate"]})
                position, index = end, index + 1
            if (actual["inputs"] != expected_inputs or actual["ownership"] != [{"start": 0, "end": fixture["frames"], "eof": True}]
                    or actual["decode_complete"] is not True
                    or actual["words"] != baseline["batch_words" if role == "batch" else "uncut_live_words"]):
                raise CoverageError("native control input, terminal ownership or words are unbound")
        baseline_scores[identifier] = compare_cell(
            baseline["batch_words"], baseline["uncut_live_words"], fixture["occurrences"],
            fixture["frames"], fixture["sample_rate"], lexemes)
    scores, short_crossings = [], {"mic": set(), "tap": set()}
    for identifier, spec in specs.items():
        cell = cells[identifier]
        runtime = expected_runtime(spec, definition["input_chunks"])
        if (cell["runtime"] != runtime or runtime["epoch_limit"] < runtime["minimum_epoch_samples"]
                or set(cell) != {"id", "runtime", "tracks", "boundaries"}):
            raise CoverageError("native run changed its frozen rate, configuration, phase or capacity")
        if set(cell["tracks"]) != {"mic", "tap"}:
            raise CoverageError("required source track missing")
        events = cell["boundaries"]
        if not isinstance(events, list) or not events or len(events) > 256:
            raise CoverageError("missing or excessive boundary inventory")
        source_end = max(fixtures[f"{spec['waveform']}-{spec['sample_rate']}-{track}"]["frames"]
                         for track in ("mic", "tap"))
        previous = -1
        epoch_origins = [0, 0]
        retained = [0, 0]
        nominal_due = frame_count(spec["window_ms"] - spec["phase_ms"], spec["sample_rate"])
        last_settlement = -1
        for event in events:
            if (set(event) != {"kind", "nominal_due_frame", "cut_frame", "selected_received_frame", "settled_received_frame", "maintenance_reset", "epoch_remaining", "canonical_accepted", "epoch_origins", "settled_canonical_accepted"}
                    or event["kind"] not in {"natural", "deadline", "explicit"}
                    or type(event["maintenance_reset"]) is not bool
                    or any(type(event[key]) is not int or event[key] < 0 for key in
                           ("nominal_due_frame", "cut_frame", "selected_received_frame", "settled_received_frame"))
                    or not previous < event["cut_frame"] <= event["selected_received_frame"] <= event["settled_received_frame"] <= source_end):
                raise CoverageError("invalid or nonmonotonic boundary event")
            remaining = event["epoch_remaining"]
            if (not isinstance(remaining, list) or len(remaining) != 2
                    or any(type(value) is not int or not 0 <= value <= runtime["epoch_limit"] for value in remaining)
                    or event["maintenance_reset"] != (min(remaining) <= runtime["maintenance_trigger"])):
                raise CoverageError("maintenance reset does not match actual native headroom")
            if event["epoch_origins"] != epoch_origins:
                raise CoverageError("native epoch origin changed without its selected reset")
            for key in ("canonical_accepted", "epoch_origins", "settled_canonical_accepted"):
                if (not isinstance(event[key], list) or len(event[key]) != 2
                        or any(type(value) is not int or value < 0 for value in event[key])):
                    raise CoverageError("invalid native canonical epoch evidence")
            for index, track in enumerate(("mic", "tap")):
                frames = fixtures[f"{spec['waveform']}-{spec['sample_rate']}-{track}"]["frames"]
                accepted = event["canonical_accepted"][index]
                settled = event["settled_canonical_accepted"][index]
                maximum = lambda key: (min(frames, event[key]) * 16000 + spec["sample_rate"] - 1) // spec["sample_rate"]
                if (not max(epoch_origins[index], maximum("selected_received_frame") - runtime["settlement_accept"]) <= accepted <= maximum("selected_received_frame")
                        or not accepted <= settled <= maximum("settled_received_frame")
                        or remaining[index] != runtime["epoch_limit"] - (accepted - epoch_origins[index])):
                    raise CoverageError("native headroom is not bound to actual admitted canonical PCM")
                if event["kind"] in {"deadline", "explicit"}:
                    cut = (event["cut_frame"] * 16000 + spec["sample_rate"] - 1) // spec["sample_rate"]
                    epoch_origins[index] = retained[index] = replay_origin(
                        retained[index], cut, FORCED_REPLAY_PREROLL)
                elif event["maintenance_reset"]:
                    epoch_origins[index] = retained[index] = replay_origin(
                        retained[index], accepted - ACCEPTED_GUARD, 0)
                else:
                    retained[index] = max(accepted - ACCEPTED_GUARD, 0)
            if (event["kind"] == "natural" and (event["selected_received_frame"] != event["settled_received_frame"]
                    or event["canonical_accepted"] != event["settled_canonical_accepted"])):
                raise CoverageError("natural settlement admitted input after its immutable selection")
            if event["kind"] != "natural" and event["selected_received_frame"] != event["cut_frame"]:
                raise CoverageError("forced boundary was not latched at its exact source fence")
            latest = (event["cut_frame"] if event["kind"] == "explicit" else
                      event["nominal_due_frame"] + 22 * spec["sample_rate"])
            if event["settled_received_frame"] > latest + runtime["settlement_source_reserve"]:
                raise CoverageError("native settlement exceeded its proved source reserve")
            if event["settled_received_frame"] < last_settlement:
                raise CoverageError("settlement source time went backwards")
            if spec["mode"] != "explicit-forced-probe":
                if event["nominal_due_frame"] != nominal_due or event["kind"] == "explicit":
                    raise CoverageError("native run changed the frozen nominal window or phase")
                nominal_due = event["cut_frame"] + frame_count(spec["window_ms"], spec["sample_rate"])
            elif event["kind"] != "explicit" or event["maintenance_reset"]:
                raise CoverageError("explicit probe changed its frozen cut mode")
            if event["kind"] in ("natural", "deadline") and not (
                    event["nominal_due_frame"] <= event["cut_frame"]
                    <= event["nominal_due_frame"] + 22 * spec["sample_rate"]):
                raise CoverageError("source ownership exceeds frozen deferral")
            if (event["kind"] == "deadline" and event["cut_frame"]
                    != event["nominal_due_frame"] + 22 * spec["sample_rate"]):
                raise CoverageError("forced deadline did not exhaust the frozen deferral")
            if spec["mode"] != "maintenance" and event["maintenance_reset"]:
                raise CoverageError("unexpected maintenance changed an ordinary control")
            previous = event["cut_frame"]
            last_settlement = event["settled_received_frame"]
        if spec["mode"] != "explicit-forced-probe" and source_end > nominal_due + 22 * spec["sample_rate"]:
            raise CoverageError("native boundary history omitted a due window before EOF")
        if (spec["waveform"] == "paused" and spec["mode"] == "ordinary"
                and not any(event["kind"] == "natural" for event in events)):
            raise CoverageError("required natural boundary was not exercised")
        if spec["mode"] == "maintenance" and not any(event["maintenance_reset"] for event in events):
            raise CoverageError("required proactive maintenance was not exercised")
        if spec["mode"] == "explicit-forced-probe":
            expected = [value * spec["sample_rate"] // 1000 for value in spec["forced_source_ms"]]
            if [event["cut_frame"] for event in events if event["kind"] == "explicit"] != expected:
                raise CoverageError("required fixed forced fences were not exercised")
        validate_admission_cycle(cell, spec, fixtures, definition["input_chunks"])
        for track, actual in cell["tracks"].items():
            fixture_id = f"{spec['waveform']}-{spec['sample_rate']}-{track}"
            fixture, baseline = fixtures[fixture_id], baselines[fixture_id]
            if actual["fixture_id"] != fixture_id or actual["pcm_sha256"] != fixture["sha256"]:
                raise CoverageError("candidate source track identity mismatch")
            if type(actual["decode_complete"]) is not bool:
                raise CoverageError("invalid candidate completion status")
            validate_track_evidence(actual, fixture, events)
            score = compare_cell(baseline["batch_words"], actual["words"], fixture["occurrences"],
                                 fixture["frames"], fixture["sample_rate"], lexemes)
            nominal = [row for event in events for row in critical_crossings(
                score["coverage"], baseline["batch_words"], event["nominal_due_frame"], spec["sample_rate"])]
            forced = [row for event in events if event["kind"] == "explicit"
                      for row in critical_crossings(score["coverage"], baseline["batch_words"],
                                                    event["cut_frame"], spec["sample_rate"])]
            if spec["waveform"] == "paused" and spec["window_ms"] == 4000 and spec["mode"] == "ordinary":
                short_crossings[track].update(row["category"] for row in nominal)
            if spec["mode"] == "explicit-forced-probe" and not forced:
                raise CoverageError("fixed forced cell did not cross an annotated critical occurrence")
            cuts = [event["cut_frame"] / spec["sample_rate"] for event in events
                     if event["kind"] in {"deadline", "explicit"}]
            gate = forced_gate(score, baseline["batch_words"], actual["words"], cuts) if cuts else None
            scores.append({"id": identifier, "track": track, "score": score, "forced_gate": gate,
                           "nominal_critical_crossings": nominal, "forced_critical_crossings": forced,
                           "boundary_local": [boundary_local(score, baseline["batch_words"],
                                                             event["cut_frame"], spec["sample_rate"])
                                              for event in events],
                           "decode_complete": actual["decode_complete"],
                           "passed": (gate["passed"] if gate else score["passed"])
                           and actual["decode_complete"]})
    if any(not {"name", "negation", "number"} <= categories for categories in short_crossings.values()):
        raise CoverageError("short phase matrix missed required critical crossing categories")
    return {"baseline_scores": baseline_scores, "cells": scores,
            "passed": all(row["passed"] for row in scores)
            and all(row["passed"] for row in baseline_scores.values())}
