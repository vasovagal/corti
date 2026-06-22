#!/usr/bin/env python3
"""FROZEN WER text normalizer for the corti bench harness.

This module is LOAD-BEARING. Every scorer (wer.py, cpwer.py) routes both
reference and hypothesis text through ``normalize()`` so that WER / cpWER
numbers are comparable across runs and across tools. Once this file is
"frozen" it MUST NOT change: altering normalization silently shifts every
historical score and makes regressions/improvements impossible to compare.

Design principle: ERR TOWARD MINIMAL TRANSFORMATION. We only fold away
differences that are (a) purely orthographic and (b) trivially safe — i.e.
no reasonable ASR/reference disagreement on the *spoken words* is being
masked. When in doubt, we do NOT normalize.

The normalizer is deliberately ASCII/English-oriented; the corti use case is
English clinical conversation transcripts.

================================================================================
NORMALIZATION RULES (applied in this exact order)
================================================================================

1. UNICODE PUNCTUATION FOLDING
   Curly quotes / apostrophes (U+2018 U+2019 U+201B) -> ASCII apostrophe (').
   Curly double quotes (U+201C U+201D) -> ASCII double quote (").
   En/em dashes (U+2013 U+2014) and the minus sign (U+2212) -> ASCII hyphen-minus (-).
   Rationale: these are typography variants of the same characters; the spoken
   words are identical. No lexical content is changed.

2. LOWERCASE
   ``str.lower()``. Casing is never a spoken-word distinction in English ASR.

3. REMOVE BRACKETED NON-LEXICAL CUES
   Delete any ``[...]`` or ``(...)`` span that contains NO digits and whose
   inner text is non-lexical stage direction such as ``[LAUGHTER]``,
   ``[THEME MUSIC]``, ``(coughs)``, ``[inaudible]``. These mark sounds, not
   spoken words, and appear inconsistently between references and hypotheses.
   IMPORTANT SAFETY LIMIT: we only strip brackets whose contents are a single
   line of letters/spaces (a "cue"). Brackets containing digits or sentence
   punctuation are left for the punctuation-stripping step, so we never eat
   real numeric/lexical content that merely happened to be parenthesised.

4. STRIP PUNCTUATION (except intra-word apostrophes and hyphens)
   Remove all punctuation characters. EXCEPTIONS, preserved only when they sit
   *between two word characters*:
     - apostrophe  : keeps contractions/possessives intact ("don't", "patient's")
     - hyphen      : keeps compounds intact ("co-pay", "follow-up")
   A leading/trailing or standalone apostrophe/hyphen (quotes, dashes used as
   separators) is removed. This means "don't" stays "don't" but 'quoted'
   becomes quoted and "well - yeah" becomes "well yeah".

5. NUMBER / SPELLED-FORM MAPPING  (TINY, FIXED, TRIVIALLY-SAFE SET ONLY)
   We do NOT do general number<->word conversion: "20" vs "twenty" is a real
   ASR distinction we must keep visible. We only fold a handful of forms that
   are pure spelling variants of the identical spoken token:
     - "ok"      -> "okay"      (same word, two spellings)
     - "alright" -> "all right" is NOT applied (debated spelling; left as-is)
   The mapping table ``_WORD_MAP`` below is the COMPLETE, FROZEN list. Adding
   entries here is a normalization change and is forbidden after freeze.

6. COLLAPSE WHITESPACE
   Any run of whitespace -> single ASCII space; strip leading/trailing.

================================================================================
Anything not listed above is intentionally left untouched.
================================================================================
"""

from __future__ import annotations

import argparse
import re
import sys
import unicodedata

# ---------------------------------------------------------------------------
# Rule 1: unicode punctuation folding table
# ---------------------------------------------------------------------------
_UNICODE_FOLD = {
    "‘": "'",   # left single quote
    "’": "'",   # right single quote / curly apostrophe
    "‛": "'",   # single high-reversed-9 quote
    "“": '"',   # left double quote
    "”": '"',   # right double quote
    "–": "-",   # en dash
    "—": "-",   # em dash
    "−": "-",   # minus sign
    " ": " ",   # non-breaking space
}

# ---------------------------------------------------------------------------
# Rule 3: bracketed non-lexical cue detector.
# Matches [...] or (...) whose inner content is letters/spaces only (no digits,
# no nested brackets). These are stage directions like [LAUGHTER], (coughs).
# ---------------------------------------------------------------------------
_BRACKET_CUE = re.compile(r"[\[(][a-zA-Z][a-zA-Z \t]*[\])]")

# ---------------------------------------------------------------------------
# Rule 4: punctuation handling.
# We keep ' and - as candidates, then prune the ones that are NOT intra-word.
# Everything else in this class is deleted outright.
# ---------------------------------------------------------------------------
# All punctuation except apostrophe and hyphen -> removed.
_PUNCT_DROP = re.compile(r"[^\w\s'\-]", re.UNICODE)
# An apostrophe or hyphen that is NOT flanked by word chars on both sides.
# (Underscore is a \w char; that's fine, it's never in transcripts.)
_LONE_APOS_HYPHEN = re.compile(r"(?<![^\W_])['\-]|['\-](?![^\W_])")

# ---------------------------------------------------------------------------
# Rule 5: FROZEN spelled-form map. COMPLETE LIST. Do not extend after freeze.
# ---------------------------------------------------------------------------
_WORD_MAP = {
    "ok": "okay",
}


def _strip_accents(text: str) -> str:
    """NFC normalize then fold our explicit unicode punctuation table.

    We do NOT strip diacritics from letters (that could change a real word);
    we only NFC-compose and then map the specific punctuation code points in
    ``_UNICODE_FOLD``.
    """
    text = unicodedata.normalize("NFC", text)
    return "".join(_UNICODE_FOLD.get(ch, ch) for ch in text)


def normalize(text: str) -> str:
    """Return the frozen WER-normalized form of ``text``.

    See the module docstring for the exact, ordered rule set. This function is
    the single public entry point and its output must remain stable.
    """
    if text is None:
        return ""

    # Rule 1: fold unicode punctuation variants to ASCII.
    text = _strip_accents(text)

    # Rule 2: lowercase.
    text = text.lower()

    # Rule 3: remove bracketed non-lexical cues (run repeatedly for adjacency).
    while True:
        new = _BRACKET_CUE.sub(" ", text)
        if new == text:
            break
        text = new

    # Rule 4a: drop all punctuation except apostrophe/hyphen.
    text = _PUNCT_DROP.sub(" ", text)
    # Rule 4b: drop apostrophes/hyphens that are not intra-word.
    text = _LONE_APOS_HYPHEN.sub(" ", text)

    # Rule 6 (partial): split on whitespace for per-token mapping.
    tokens = text.split()

    # Rule 5: trivially-safe spelled-form mapping.
    tokens = [_WORD_MAP.get(tok, tok) for tok in tokens]

    # Rule 6: collapse whitespace (join with single spaces).
    return " ".join(tokens)


def _main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Frozen WER text normalizer for the corti bench harness. "
            "Reads text from --text or stdin and prints the normalized form."
        ),
    )
    parser.add_argument(
        "--text",
        help="Text to normalize. If omitted, read from stdin.",
    )
    args = parser.parse_args(argv)

    raw = args.text if args.text is not None else sys.stdin.read()
    sys.stdout.write(normalize(raw) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(_main(sys.argv[1:]))
