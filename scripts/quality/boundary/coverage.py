"""Finite pre-model occurrence binding. Declared alternatives locate words; scoring stays raw."""
from functools import lru_cache
import math

from score import compare, tokens

MAX_WORDS = 512
MAX_TOKENS = 512
MAX_OCCURRENCES = 128
MAX_ALTERNATIVES = 8
MAX_SURFACE_TOKENS = 16


class CoverageError(ValueError):
    pass


def validated_words(words, source_frames, sample_rate):
    if (type(sample_rate) is not int or sample_rate <= 0
            or type(source_frames) is not int or source_frames <= 0
            or not isinstance(words, list) or not words or len(words) > MAX_WORDS):
        raise CoverageError("missing or invalid speech word inventory")
    previous = -1.0
    duration = source_frames / sample_rate
    for word in words:
        if (set(word) != {"text", "start", "end"}
                or not isinstance(word["text"], str) or not word["text"].strip()
                or any(type(word[key]) not in (int, float) or not math.isfinite(word[key])
                       for key in ("start", "end"))
                or not 0 <= word["start"] <= word["end"] <= duration
                or word["start"] < previous):
            raise CoverageError("invalid source word or timestamp")
        previous = word["start"]
    result = tokens(word["text"] for word in words)
    if not result or len(result) > MAX_TOKENS:
        raise CoverageError("speech token inventory exceeds bound")
    return result


def locate_key(token):
    """Locating only: ASR sentence-casing and sentence punctuation never change a critical value.

    Only a leading capital is folded, so unit case such as mM/mm stays distinct. Scoring still
    compares the candidate's raw tokens with the batch's raw tokens.
    """
    token = token.strip(".,!?;:")
    return token[:1].lower() + token[1:]


def bind_occurrences(words, occurrences, source_frames, sample_rate):
    """Bind all ordered IDs uniquely within PREDECLARED source windows.

    An alternative is a literal NFC token sequence, not a numeric equivalence rule. Locating ignores
    only sentence punctuation and a leading capital (see locate_key). A baseline may select a
    declared spelling; candidate scoring subsequently uses those exact raw
    tokens. No surface, order, window, or occurrence is derived from candidate output.
    """
    baseline = validated_words(words, source_frames, sample_rate)
    if (not isinstance(occurrences, list) or not occurrences
            or len(occurrences) > MAX_OCCURRENCES):
        raise CoverageError("missing or oversized critical occurrence inventory")
    seen, candidates = set(), []
    for occurrence in occurrences:
        if set(occurrence) != {"id", "authored", "alternatives", "source_window"}:
            raise CoverageError("invalid critical occurrence shape")
        identifier = occurrence["id"]
        if not isinstance(identifier, str) or not identifier or identifier in seen:
            raise CoverageError("duplicate or invalid critical occurrence identity")
        seen.add(identifier)
        authored = occurrence["authored"]
        alternatives = occurrence["alternatives"]
        window = occurrence["source_window"]
        if (not isinstance(authored, list) or not 1 <= len(authored) <= MAX_SURFACE_TOKENS
                or not isinstance(alternatives, list) or not 1 <= len(alternatives) <= MAX_ALTERNATIVES
                or not isinstance(window, list) or len(window) != 2
                or any(type(frame) is not int for frame in window)
                or not 0 <= window[0] < window[1] <= source_frames):
            raise CoverageError("invalid critical surface or source window")
        for surface in [authored, *alternatives]:
            if (not isinstance(surface, list) or not 1 <= len(surface) <= MAX_SURFACE_TOKENS
                    or any(not isinstance(piece, str) or not piece
                           or tuple(token.raw for token in tokens([piece])) != (piece,)
                           for piece in surface)):
                raise CoverageError("critical surfaces must be literal NFC single-token arrays")
        if len({tuple(surface) for surface in alternatives}) != len(alternatives):
            raise CoverageError("duplicate critical surface alternative")
        matches = set()
        for surface in alternatives:
            for start in range(len(baseline) - len(surface) + 1):
                indices = tuple(range(start, start + len(surface)))
                if (tuple(locate_key(baseline[index].raw) for index in indices)
                        != tuple(locate_key(piece) for piece in surface)):
                    continue
                # Fixed source windows include only the predeclared timing margin, never a match-
                # selected expansion. Multi-piece ASR words preserve their common native interval.
                if all(window[0] <= ((words[baseline[index].word_index]["start"]
                                     + words[baseline[index].word_index]["end"]) / 2) * sample_rate
                       < window[1] for index in indices):
                    matches.add(indices)
        if not matches:
            raise CoverageError("missing authored critical occurrence")
        candidates.append(tuple(sorted(matches)))

    @lru_cache(maxsize=None)
    def assignments(index, after):
        if index == len(candidates):
            return 1, ()
        count, selected = 0, None
        for indices in candidates[index]:
            if indices[0] < after:
                continue
            suffix_count, suffix = assignments(index + 1, indices[-1] + 1)
            if suffix_count:
                count = min(2, count + suffix_count)
                selected = (indices,) + suffix
            if count == 2:
                break
        return count, selected

    count, binding = assignments(0, 0)
    if count != 1:
        raise CoverageError("ambiguous or nonordered critical occurrence coverage")
    return [{"id": occurrence["id"], "indices": indices,
             "authored": occurrence["authored"],
             "baseline_surface": [baseline[index].raw for index in indices],
             "authored_lexical_difference": occurrence["authored"]
             != [baseline[index].raw for index in indices]}
            for occurrence, indices in zip(occurrences, binding)]


def compare_cell(reference, hypothesis, occurrences, source_frames, sample_rate, critical_lexemes):
    """One source track and one immutable matrix cell. Coverage failure never becomes PASS."""
    binding = bind_occurrences(reference, occurrences, source_frames, sample_rate)
    validated_words(hypothesis, source_frames, sample_rate)
    score = compare([word["text"] for word in reference], [word["text"] for word in hypothesis],
                    [(row["id"], row["indices"]) for row in binding],
                    critical_lexemes=critical_lexemes)
    return {"coverage": binding, "score": score,
            "passed": score["word_gate"] and score["critical_gate"]}
