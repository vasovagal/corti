#!/usr/bin/env python3
"""Extract the reference span that matches a clip's transcript.

NPR transcripts are full-episode and untimed, and the mp3s carry dynamically-inserted ads that are NOT in
the transcript. To score a short excerpt (e.g. a 5-minute sweep clip) we therefore can't slice the reference
by time. Instead we transcribe the clip ONCE with a baseline config, then align that hypothesis to the full
reference word-sequence and emit the contiguous reference span it covers. That span becomes the clip's frozen
reference (`<id>.5min.reference.txt`) which every sweep config is then scored against.

Method: normalize both sides with the frozen `normalize.normalize`, align normalized word lists with
`difflib.SequenceMatcher` (autojunk off), take the reference span from the first to the last matching block,
and emit the corresponding RAW reference words (so downstream WER re-normalizes consistently). Reports a
`coverage` = matched_words / hyp_words: low coverage means the window isn't really in the reference (e.g. it's
mostly an inserted ad) and the extracted span should not be trusted as an absolute-WER reference.

Usage:
  align_reference.py --ref <full.reference.txt> --hyp <clip.hyp.json|clip.txt> [--out <span.txt>]
Prints a JSON summary to stdout; writes the raw reference span to --out (or stdout if omitted with --print).
"""
import argparse
import difflib
import json
import sys

from normalize import normalize


def hyp_text(path: str) -> str:
    if path.endswith(".json"):
        d = json.load(open(path))
        d = d.get("transcript", d)
        segs = sorted(d.get("segments", []), key=lambda s: (s.get("start", 0.0), s.get("end", 0.0)))
        return " ".join(s.get("text", "") for s in segs)
    return open(path).read()


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--ref", required=True, help="full-episode reference .txt")
    ap.add_argument("--hyp", required=True, help="clip hypothesis (.json DiarizedTranscript or .txt)")
    ap.add_argument("--out", help="write the extracted raw reference span here")
    ap.add_argument("--print", action="store_true", help="also print the span text to stderr")
    a = ap.parse_args()

    ref_raw = open(a.ref).read().split()
    # Per-token normalization, keeping a map back to raw token indices. A raw token may normalize to '' (a
    # bracketed cue) — dropped — or, rarely, to several words — each mapped back to the same raw token.
    norm_list, raw_idx = [], []
    for i, w in enumerate(ref_raw):
        for sub in normalize(w).split():
            norm_list.append(sub)
            raw_idx.append(i)

    hyp_norm = normalize(hyp_text(a.hyp)).split()
    sm = difflib.SequenceMatcher(None, norm_list, hyp_norm, autojunk=False)
    blocks = [b for b in sm.get_matching_blocks() if b.size > 0]
    if not blocks:
        print(json.dumps({"error": "no alignment between hypothesis and reference"}))
        return 2

    # The hypothesis is a CONTIGUOUS window of the reference, so the correct reference span is ~len(hyp)
    # words long at a roughly constant offset (ref_index - hyp_index). Anchor on the largest matching block
    # (a real, unambiguous run of words inside the window — not a stray common word), estimate the window
    # offset, keep only blocks consistent with it (guards against scattered "the"/"and" matches elsewhere in
    # the episode), and take the span from the first to the last consistent match.
    anchor = max(blocks, key=lambda b: b.size)
    offset = anchor.a - anchor.b  # reference index aligned to hyp index 0
    win_start = max(0, offset)
    win_end = min(len(norm_list), win_start + len(hyp_norm))
    margin = max(50, len(hyp_norm) // 5)  # tolerate local drift / dropped words
    consistent = [b for b in blocks if win_start - margin <= b.a < win_end + margin]
    if not consistent:
        consistent = [anchor]

    start_ref = consistent[0].a
    end_ref = consistent[-1].a + consistent[-1].size  # exclusive in normalized space
    matched = sum(b.size for b in consistent)
    raw_start = raw_idx[start_ref]
    raw_end = raw_idx[end_ref - 1]
    span_raw = " ".join(ref_raw[raw_start : raw_end + 1])

    if a.out:
        open(a.out, "w").write(span_raw + "\n")
    if a.print:
        print(span_raw, file=sys.stderr)

    print(json.dumps({
        "ref_words_total": len(norm_list),
        "hyp_words": len(hyp_norm),
        "matched_words": matched,
        "coverage": round(matched / max(1, len(hyp_norm)), 4),
        "ref_span_words": raw_end - raw_start + 1,
        "ref_span_raw_range": [raw_start, raw_end],
        "out": a.out,
    }))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
