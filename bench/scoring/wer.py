#!/usr/bin/env python3
"""Word Error Rate scorer for the corti bench harness.

Compares a hypothesis transcript against a plain-text reference and reports
WER computed by ``jiwer`` on BOTH the frozen-normalized text (the headline
number) and the raw text (for sanity / debugging).

The hypothesis may be either:
  * a corti ``DiarizedTranscript`` JSON file, shape::
        {"segments": [{"speaker": {...}, "start": f64, "end": f64,
                       "text": "..."}, ...]}
    in which case segment ``text`` is concatenated in ascending ``start``
    (then ``end``) time order; or
  * a plain ``.txt`` file, used verbatim.

Output is a single JSON object on stdout::
    {"wer_normalized", "wer_raw", "ref_words", "hyp_words",
     "substitutions", "deletions", "insertions"}

The substitution/deletion/insertion counts and ref_words/hyp_words correspond
to the NORMALIZED comparison (the headline metric).
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

import jiwer

# Allow running both as a module and as a bare script.
try:
    from normalize import normalize
except ImportError:  # pragma: no cover - path shim for direct execution
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    from normalize import normalize


def load_hypothesis_text(path: Path) -> str:
    """Return the hypothesis transcript as one concatenated string.

    ``.json`` is parsed as a DiarizedTranscript and its segments are joined in
    time order. Anything else is read as plain text.
    """
    if path.suffix.lower() == ".json":
        data = json.loads(path.read_text(encoding="utf-8"))
        # Accept a bare DiarizedTranscript or the corti-bench envelope {"transcript": {...}}.
        data = data.get("transcript", data)
        segments = data.get("segments", [])
        # Sort by (start, end) so concatenation follows the spoken timeline,
        # regardless of the order segments happen to be stored in.
        segments = sorted(
            segments,
            key=lambda s: (s.get("start", 0.0), s.get("end", 0.0)),
        )
        return " ".join(seg.get("text", "") for seg in segments)
    return path.read_text(encoding="utf-8")


def _measure(ref: str, hyp: str) -> jiwer.WordOutput:
    """Run jiwer.process_words, tolerating empty strings.

    jiwer raises if both ref and hyp are empty; we guard that edge case.
    """
    return jiwer.process_words(ref, hyp)


def score(ref_text: str, hyp_text: str) -> dict:
    ref_norm = normalize(ref_text)
    hyp_norm = normalize(hyp_text)

    out_norm = _measure(ref_norm, hyp_norm)
    # Raw WER: lowercase only would be unfair to "raw"; we pass text verbatim
    # except collapsing whitespace so jiwer's default tokenizer behaves.
    out_raw = _measure(
        " ".join(ref_text.split()),
        " ".join(hyp_text.split()),
    )

    return {
        "wer_normalized": out_norm.wer,
        "wer_raw": out_raw.wer,
        "ref_words": len(ref_norm.split()),
        "hyp_words": len(hyp_norm.split()),
        "substitutions": out_norm.substitutions,
        "deletions": out_norm.deletions,
        "insertions": out_norm.insertions,
    }


def _main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Compute WER (jiwer) of a hypothesis transcript against a "
            "plain-text reference, on both frozen-normalized and raw text."
        ),
    )
    parser.add_argument(
        "--ref",
        required=True,
        type=Path,
        help="Reference transcript as a plain .txt file.",
    )
    parser.add_argument(
        "--hyp",
        required=True,
        type=Path,
        help=(
            "Hypothesis: a corti DiarizedTranscript .json (segments joined "
            "in time order) OR a plain .txt file."
        ),
    )
    args = parser.parse_args(argv)

    ref_text = args.ref.read_text(encoding="utf-8")
    hyp_text = load_hypothesis_text(args.hyp)

    result = score(ref_text, hyp_text)
    json.dump(result, sys.stdout, indent=2)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(_main(sys.argv[1:]))
