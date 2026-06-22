#!/usr/bin/env python3
"""Concatenated minimum-Permutation WER (cpWER) scorer for the bench harness.

cpWER measures speaker-attributed transcription quality: it concatenates each
speaker's words, finds the optimal reference<->hypothesis speaker assignment
(Hungarian algorithm), and computes WER under that assignment. It therefore
penalizes both word errors AND speaker confusion / wrong speaker counts.

Inputs:
  --ref-turns : reference turns JSON, shape ``[{"speaker": "...",
                "text": "..."}, ...]``. Turns with the same ``speaker`` value
                are concatenated into one reference stream for that speaker.
  --hyp       : a corti ``DiarizedTranscript`` JSON, shape
                ``{"segments": [{"speaker": {"kind": "me"|"other",
                "label": "..."}, "start": f64, "end": f64, "text": "..."},
                ...]}``. Each segment's speaker key is derived from
                ``kind`` + ``label`` so distinct speakers stay distinct.

All text (ref and hyp) is run through the frozen ``normalize()`` first.

Output JSON on stdout::
    {"cpwer", "ref_speakers", "hyp_speakers", "speaker_count_error"}

``speaker_count_error`` is ``hyp_speakers - ref_speakers`` (signed: positive =
over-segmentation / too many speakers, negative = merged/missed speakers).

meeteval API used (meeteval 0.4.3):
    from meeteval.wer import cp_word_error_rate
    er = cp_word_error_rate({spk: text, ...}, {spk: text, ...})
    er.error_rate, er.scored_speaker, er.missed_speaker, er.falarm_speaker
The dict-of-{speaker: concatenated_text} form is the documented entry point
(see cp_word_error_rate docstring examples).
"""

from __future__ import annotations

import argparse
import json
import sys
from collections import OrderedDict
from pathlib import Path

from meeteval.wer import cp_word_error_rate

try:
    from normalize import normalize
except ImportError:  # pragma: no cover - path shim for direct execution
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    from normalize import normalize


def load_ref_turns(path: Path) -> "OrderedDict[str, str]":
    """Collapse reference turns into ``{speaker: normalized_concatenated}``."""
    turns = json.loads(path.read_text(encoding="utf-8"))
    by_speaker: "OrderedDict[str, list[str]]" = OrderedDict()
    for turn in turns:
        spk = str(turn.get("speaker"))
        by_speaker.setdefault(spk, []).append(turn.get("text", ""))
    return OrderedDict(
        (spk, normalize(" ".join(parts))) for spk, parts in by_speaker.items()
    )


def _hyp_speaker_key(speaker: dict) -> str:
    """Stable per-speaker key from a DiarizedTranscript speaker object.

    Combines ``kind`` and ``label`` so that e.g. two distinct ``other``
    speakers with different labels are not merged.
    """
    if not isinstance(speaker, dict):
        return str(speaker)
    kind = speaker.get("kind", "")
    label = speaker.get("label", "")
    return f"{kind}:{label}"


def load_hyp_segments(path: Path) -> "OrderedDict[str, str]":
    """Collapse DiarizedTranscript segments into ``{speaker: normalized}``.

    Segments are concatenated per speaker in ascending (start, end) order.
    """
    data = json.loads(path.read_text(encoding="utf-8"))
    # Accept a bare DiarizedTranscript or the corti-bench envelope {"transcript": {...}}.
    data = data.get("transcript", data)
    segments = sorted(
        data.get("segments", []),
        key=lambda s: (s.get("start", 0.0), s.get("end", 0.0)),
    )
    by_speaker: "OrderedDict[str, list[str]]" = OrderedDict()
    for seg in segments:
        key = _hyp_speaker_key(seg.get("speaker", {}))
        by_speaker.setdefault(key, []).append(seg.get("text", ""))
    return OrderedDict(
        (spk, normalize(" ".join(parts))) for spk, parts in by_speaker.items()
    )


def score(ref: "OrderedDict[str, str]", hyp: "OrderedDict[str, str]") -> dict:
    # meeteval requires at least one speaker on each side. Guard empties so we
    # emit a sane result rather than crashing the harness.
    ref_in = dict(ref) if ref else {"__empty_ref__": ""}
    hyp_in = dict(hyp) if hyp else {"__empty_hyp__": ""}
    ref_speakers = len(ref)
    hyp_speakers = len(hyp)

    # cp_word_error_rate is O(N!) in the speaker permutation and meeteval hard-refuses pathological counts.
    # Catastrophic over-clustering (issue #18) produces dozens of "Them N" speakers — there the
    # speaker-count error IS the finding, so report it with cpwer=None rather than crashing the sweep.
    cpwer = None
    note = None
    try:
        cpwer = cp_word_error_rate(ref_in, hyp_in).error_rate
    except Exception as e:  # noqa: BLE001 — meeteval raises a bare RuntimeError on too many speakers
        note = f"cpWER uncomputable ({type(e).__name__}): hyp has {hyp_speakers} speakers vs {ref_speakers} ref"

    out = {
        "cpwer": cpwer,
        "ref_speakers": ref_speakers,
        "hyp_speakers": hyp_speakers,
        "speaker_count_error": hyp_speakers - ref_speakers,
    }
    if note:
        out["note"] = note
    return out


def _main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Compute cpWER (meeteval, concatenated min-permutation "
            "speaker-attributed WER) and speaker-count error between "
            "reference turns and a corti DiarizedTranscript."
        ),
    )
    parser.add_argument(
        "--ref-turns",
        required=True,
        type=Path,
        help='Reference turns JSON: [{"speaker": "...", "text": "..."}, ...]',
    )
    parser.add_argument(
        "--hyp",
        required=True,
        type=Path,
        help="Hypothesis corti DiarizedTranscript .json file.",
    )
    args = parser.parse_args(argv)

    ref = load_ref_turns(args.ref_turns)
    hyp = load_hyp_segments(args.hyp)

    result = score(ref, hyp)
    json.dump(result, sys.stdout, indent=2)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(_main(sys.argv[1:]))
