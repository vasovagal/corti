"""Pre-model #155 comparison kernel; no fixture, annotation, or native acceptance claim.

Frozen normalization proposal: NFC, whitespace split, Unicode lowercase; retain punctuation.
Critical occurrences refer to token positions in a separately validated batch annotation manifest.
The caller must reject missing/unalignable authored annotations before invoking this kernel.
"""
from dataclasses import dataclass
import unicodedata


@dataclass(frozen=True)
class Token:
    raw: str
    normalized: str
    word_index: int
    piece_index: int


def tokens(words):
    return tuple(Token(piece, piece.lower(), word_index, piece_index)
                 for word_index, word in enumerate(words)
                 for piece_index, piece in enumerate(unicodedata.normalize("NFC", word).split()))


def align(reference, hypothesis):
    """Unit costs; equal-cost traceback chooses diagonal, deletion, then insertion."""
    n, m = len(reference), len(hypothesis)
    table = [[0] * (m + 1) for _ in range(n + 1)]
    for i in range(n + 1):
        table[i][0] = i
    for j in range(m + 1):
        table[0][j] = j
    for i in range(1, n + 1):
        for j in range(1, m + 1):
            table[i][j] = min(table[i - 1][j - 1] + (reference[i - 1] != hypothesis[j - 1]),
                              table[i - 1][j] + 1, table[i][j - 1] + 1)
    path, i, j = [], n, m
    while i or j:
        if i and j and table[i][j] == table[i - 1][j - 1] + (reference[i - 1] != hypothesis[j - 1]):
            path.append(("equal" if reference[i - 1] == hypothesis[j - 1] else "substitute", i - 1, j - 1))
            i, j = i - 1, j - 1
        elif i and table[i][j] == table[i - 1][j] + 1:
            path.append(("delete", i - 1, None))
            i -= 1
        else:
            path.append(("insert", None, j - 1))
            j -= 1
    return table[n][m], tuple(reversed(path))


def compare(reference_words, hypothesis_words, occurrences, *, speech_required=True,
            critical_lexemes=()):
    """Compare one cell/track; never average cells or certify an empty speech baseline.

    occurrences is a sequence of (unique occurrence ID, contiguous baseline token indices).
    Exact raw NFC values protect case, signs, units and identifiers independently of lowercase WER.
    The hypothesis-side edit check covers insertions AND substitutions: any introduced digit-bearing
    token, negator, annotated critical value, or predeclared fixture lexeme prevents acceptance.
    Fixture lexemes must be frozen before model execution and cover its names, units and spelled
    numbers. This finite inventory is not a universal semantic classifier for arbitrary new entities.
    No number-word conversion, contraction expansion, punctuation stripping or repeat collapse.
    """
    reference, hypothesis = tokens(reference_words), tokens(hypothesis_words)
    identifiers, protected = set(), set()
    frozen_occurrences = []
    for identifier, indices in occurrences:
        indices = tuple(indices)
        if (not identifier or identifier in identifiers or not indices
                or indices != tuple(range(indices[0], indices[0] + len(indices)))
                or indices[0] < 0 or indices[-1] >= len(reference)):
            raise ValueError("invalid or uncovered critical occurrence")
        identifiers.add(identifier)
        protected.update(indices)
        frozen_occurrences.append((identifier, indices))
    distance, path = align([t.normalized for t in reference], [t.normalized for t in hypothesis])
    by_reference = {r: (operation, h) for operation, r, h in path if r is not None}
    critical = {}
    for identifier, indices in frozen_occurrences:
        matched = [by_reference[index] for index in indices]
        critical[identifier] = (all(operation == "equal" and reference[index].raw == hypothesis[h].raw
                                    for index, (operation, h) in zip(indices, matched))
                                and all(matched[k + 1][1] == matched[k][1] + 1
                                        for k in range(len(matched) - 1)))
    values = {reference[index].normalized for index in protected}
    values.update(token.normalized for token in tokens(critical_lexemes))
    negators = {"no", "not", "never", "cannot", "can't", "don't", "doesn't", "isn't", "won't"}

    def introduced_critical(token):
        # Classification only: raw tokens and WER retain every punctuation character. The exact
        # sentence-decoration table and curly-apostrophe spelling are fixed before native use.
        lexical = token.normalized.strip('.,!?;:"()[]{}“”‘’').replace("’", "'")
        return (token.normalized in values or lexical in negators
                or any(character.isdigit() for character in token.raw))

    introduced = tuple(h for operation, _, h in path
                       if operation in ("insert", "substitute") and introduced_critical(hypothesis[h]))
    inserted_critical = tuple(h for operation, _, h in path
                              if operation == "insert" and h in introduced)
    raw_critical_changes = tuple(h for operation, r, h in path
                                 if operation == "equal" and reference[r].raw != hypothesis[h].raw
                                 and (reference[r].normalized in values
                                      or hypothesis[h].normalized in values))
    baseline_covered = bool(reference) or not speech_required
    counts = {name: sum(operation == name for operation, _, _ in path)
              for name in ("insert", "delete", "substitute")}
    return {"reference": reference, "hypothesis": hypothesis, "alignment": path,
            "edits": distance, "denominator": len(reference), "counts": counts,
            "word_gate": baseline_covered and (100 * distance <= 5 * len(reference)),
            "critical_occurrences": critical, "inserted_critical_indices": inserted_critical,
            "introduced_critical_indices": introduced,
            "raw_critical_change_indices": raw_critical_changes,
            "critical_gate": (baseline_covered and all(critical.values())
                              and not introduced and not raw_critical_changes)}
