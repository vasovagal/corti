#!/usr/bin/env python3
"""Segment-cleanup scorer for the bench harness (issue #149).

Scores what `corti_transcribe::segment::cleanup` is supposed to fix, on a whole transcript rather than on
words: cross-channel echo (#107), fragmented turns, and pure backchannel rows. Unlike `wer.py`/`cpwer.py`
this needs **no reference** — every metric is intrinsic to the transcript, so it can be run over real calls
that have no ground truth.

Inputs (both `--before` and `--after` accept either form; `.md` is detected by suffix, or forced with
`--markdown`):

  * a corti ``DiarizedTranscript`` JSON — ``{"segments": [{"speaker": {"kind": "me"|"other",
    "label": "..."}, "start": f64, "end": f64, "text": "..."}, ...]}`` — as written by
    ``corti-bench process --out`` (with or without ``--cleanup``) and ``corti-bench clean --out``;
  * a corti note (``**[mm:ss] Me:** …`` / ``**[h:mm:ss] Them 1:** …`` turns). A note has no segment end
    times, so each turn's end is approximated as ``min(next same-channel start, start + 20 s)`` — 20 s is
    the local backend's ``MAX_SPEECH_SECONDS`` VAD region cap, the longest a real segment can be.

Metrics, per transcript::

    n_segments             turns in the transcript
    echo_pairs_remaining   turns that still look like a copy of the other channel (see below)
    turns_le3              turns of three words or fewer (the fragmentation symptom)
    backchannel_turns      turns that are nothing but "yeah" / "okay" / "makes sense" / …
    content_tokens         total content-token occurrences (the denominator for retention)

and, when both ``--before`` and ``--after`` are given::

    content_retention      after / before content-token occurrences (target >= 0.97)
    dropped                turns present in before and gone from after
    orphan_drops           dropped turns whose content is NOT >= 70 % present in a kept neighbour
                           within 6 s — i.e. speech the cleanup actually lost. MUST be 0.
    backchannel_orphans    the same test for turns that are wholly a backchannel phrase ("I see.").
                           Not a loss — it is what the backchannel pass removes, and its words are
                           deliberately not stopwords — but reported so a wrong phrase list is visible.

The echo detector is deliberately **looser than the cleanup rule** (containment >= 0.5 vs. the shipping
0.7): a metric that used the rule's own threshold could only ever report zero. A turn counts once, no
matter how many sources it echoes.

The stopword / filler / backchannel vocabularies are mirrored from
``crates/corti-transcribe/src/segment.rs``; keep the two in lock-step. Python stdlib only.

Usage::

    cleanup.py --before transcript.json [--after cleaned.json]
    cleanup.py --before note.md --emit-json-before raw.json     # replay a note, keep the JSON
    cleanup.py --before raw.json --after cleaned.json --table   # human-readable summary on stderr
"""
import argparse
import json
import re
import sys

# --- vocabularies (mirror of segment.rs) -----------------------------------------------------------------

FILLERS = {
    "um", "umm", "uh", "uhh", "ah", "aah", "er", "erm", "hm", "hmm", "mm", "mmm", "mhm", "huh",
}

BACKCHANNELS = {
    "yeah", "yep", "yup", "yes", "okay", "ok", "sure", "right", "uh-huh", "mm-hmm", "uhhuh",
    "cool", "alright", "exactly", "absolutely", "totally", "definitely", "gotcha",
}

BACKCHANNEL_PHRASES = {
    "all right", "there we go", "go ahead", "got it", "makes sense", "that makes sense",
    "i see", "sounds good", "for sure", "of course", "no worries",
}

STOPWORDS = {
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "do", "for", "from", "had", "has",
    "have", "he", "her", "his", "i", "if", "in", "is", "it", "its", "just", "like", "me", "my",
    "no", "not", "of", "oh", "on", "or", "our", "out", "she", "so", "that", "the", "their", "them",
    "then", "there", "these", "they", "this", "to", "up", "was", "we", "well", "were", "with",
    "you", "your", "i'm", "it's", "that's", "we're", "you're", "don't", "we've", "i've",
}

NOISE = FILLERS | BACKCHANNELS | STOPWORDS

# The echo window and the orphan-neighbour window are both the shipping `echo_window_seconds`.
WINDOW_S = 6.0
# Loose detector threshold: what still *looks* like an echo after cleanup ran at 0.7.
DETECT_CONTAINMENT = 0.5
# A dropped turn is accounted for if this much of it survives in a neighbour.
ORPHAN_CONTAINMENT = 0.7
# Longest VAD region the local backend can emit (MAX_SPEECH_SECONDS), used to bound a replayed turn.
MAX_TURN_S = 20.0

TOKEN_KEEP = re.compile(r"[^a-z\-']+")
TURN_RE = re.compile(r"^\*\*\[(?:(\d+):)?(\d+):(\d+)\]\s+(.+?):\*\*\s*(.*)$")


def normalize_token(raw):
    """Lowercase a raw token, keep letters plus word-internal - and ', drop the rest."""
    kept = TOKEN_KEEP.sub("", raw.lower()).strip("-'")
    return kept if any(c.isalpha() for c in kept) else ""


def tokens(text):
    """Normalized tokens, in order, punctuation stripped."""
    return [t for t in (normalize_token(w) for w in text.split()) if t]


def content(text):
    """The SET of content tokens: normalized tokens minus fillers, stopwords and backchannels."""
    return {t for t in tokens(text) if t not in NOISE}


def content_count(text):
    """Content-token OCCURRENCES — the retention denominator (a set would hide a lost repetition)."""
    return sum(1 for t in tokens(text) if t not in NOISE)


def is_backchannel(text):
    toks = tokens(text)
    if not toks:
        return False
    if " ".join(toks) in BACKCHANNEL_PHRASES:
        return True
    return all(t in BACKCHANNELS or t in FILLERS for t in toks)


def containment(subject, source):
    """Fraction of `subject`'s content tokens that also appear in `source`."""
    if not subject:
        return 0.0
    return len(subject & source) / len(subject)


# --- input ------------------------------------------------------------------------------------------------


def speaker_key(speaker):
    """`Me` is one channel; every `Other` label is the far-end channel (matching segment.rs)."""
    return "me" if speaker.get("kind") == "me" else "them"


def load_json(path):
    doc = json.load(open(path))
    # Accept either the bare DiarizedTranscript or corti-bench's stdout envelope.
    segs = doc["transcript"]["segments"] if "transcript" in doc else doc["segments"]
    return [
        {
            "channel": speaker_key(s["speaker"]),
            "label": s["speaker"].get("label") or "Me",
            "start": float(s["start"]),
            "end": float(s["end"]),
            "text": s["text"],
        }
        for s in segs
    ]


def load_markdown(path):
    """Replay a corti note's `**[mm:ss] Speaker:** text` turns.

    A note carries no end times, so each turn ends at the next turn from the SAME channel, capped at the
    20 s VAD region limit. That is an approximation, and it is why note replay is only used to compare a
    note against its own cleaned form, never against a WAV run.
    """
    out = []
    for line in open(path, encoding="utf-8"):
        m = TURN_RE.match(line.strip())
        if not m:
            continue
        h, mm, ss, speaker, text = m.groups()
        start = int(h or 0) * 3600 + int(mm) * 60 + int(ss)
        out.append(
            {
                "channel": "me" if speaker.strip() == "Me" else "them",
                "label": speaker.strip(),
                "start": float(start),
                "end": float(start) + MAX_TURN_S,
                "text": text.strip(),
            }
        )
    out.sort(key=lambda s: s["start"])
    for i, s in enumerate(out):
        nxt = next((o for o in out[i + 1:] if o["channel"] == s["channel"]), None)
        if nxt:
            s["end"] = min(s["end"], max(nxt["start"], s["start"]))
    return out


def load(path, markdown):
    return load_markdown(path) if (markdown or path.endswith(".md")) else load_json(path)


def to_transcript_json(segments):
    """Back to a DiarizedTranscript, so a replayed note can be fed to `corti-bench clean`."""
    return {
        "segments": [
            {
                "speaker": {"kind": "me"} if s["channel"] == "me" else {"kind": "other", "label": s["label"]},
                "start": s["start"],
                "end": s["end"],
                "text": s["text"],
            }
            for s in segments
        ]
    }


# --- metrics ----------------------------------------------------------------------------------------------


def score(segments):
    toks = [content(s["text"]) for s in segments]
    echo_pairs = 0
    for i, s in enumerate(segments):
        if len(toks[i]) < 3:
            continue
        for j in range(i):
            o = segments[j]
            if o["channel"] == s["channel"]:
                continue
            if not (o["start"] <= s["start"] <= o["end"] + WINDOW_S):
                continue
            if containment(toks[i], toks[j]) >= DETECT_CONTAINMENT:
                echo_pairs += 1
                break
    return {
        "n_segments": len(segments),
        "echo_pairs_remaining": echo_pairs,
        "turns_le3": sum(1 for s in segments if len(s["text"].split()) <= 3),
        "backchannel_turns": sum(1 for s in segments if is_backchannel(s["text"])),
        "content_tokens": sum(content_count(s["text"]) for s in segments),
    }


def compare(before, after):
    """Retention and orphan drops: what the cleanup removed, and whether any of it was real speech."""
    kept = {(s["channel"], round(s["start"], 3)) for s in after}
    dropped = [s for s in before if (s["channel"], round(s["start"], 3)) not in kept]

    orphans, backchannel_orphans = [], 0
    for d in dropped:
        d_tok = content(d["text"])
        if not d_tok:
            continue  # nothing to lose: a filler or single-word backchannel row
        best = 0.0
        for k in after:
            if k["start"] > d["end"] + WINDOW_S or k["end"] < d["start"] - WINDOW_S:
                continue
            best = max(best, containment(d_tok, content(k["text"])))
        if best >= ORPHAN_CONTAINMENT:
            continue
        # A whole-phrase backchannel ("I see.", "Of course.", "Sounds good.") is not lost content — it is
        # what the backchannel pass exists to remove, and `backchannel_turns` already counts it. Its words
        # are not in the stopword set (they carry meaning elsewhere), so it would otherwise look orphaned.
        # Counted separately rather than hidden, because a spike here would mean the phrase list is wrong.
        if is_backchannel(d["text"]):
            backchannel_orphans += 1
            continue
        orphans.append({"start": d["start"], "channel": d["channel"], "best_containment": round(best, 3)})

    before_tokens = sum(content_count(s["text"]) for s in before)
    after_tokens = sum(content_count(s["text"]) for s in after)
    return {
        "dropped": len(dropped),
        "orphan_drops": len(orphans),
        "backchannel_orphans": backchannel_orphans,
        "orphans": orphans[:20],
        "content_retention": round(after_tokens / before_tokens, 4) if before_tokens else 1.0,
    }


def table(report):
    b, a = report["before"], report.get("after")
    rows = ["n_segments", "echo_pairs_remaining", "turns_le3", "backchannel_turns", "content_tokens"]
    width = max(len(r) for r in rows)
    print(f"{'metric':{width}}  {'before':>8}  {'after':>8}", file=sys.stderr)
    for r in rows:
        after = f"{a[r]:>8}" if a else " " * 8
        print(f"{r:{width}}  {b[r]:>8}  {after}", file=sys.stderr)
    if "delta" in report:
        d = report["delta"]
        print(
            f"\ndropped={d['dropped']}  orphan_drops={d['orphan_drops']}  "
            f"backchannel_orphans={d['backchannel_orphans']}  "
            f"content_retention={d['content_retention']}",
            file=sys.stderr,
        )


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--before", required=True, help="DiarizedTranscript JSON or corti note (.md)")
    ap.add_argument("--after", help="the same transcript after cleanup")
    ap.add_argument("--markdown", action="store_true", help="force note parsing for both inputs")
    ap.add_argument("--emit-json-before", help="write --before as DiarizedTranscript JSON (note replay)")
    ap.add_argument("--emit-json-after", help="write --after as DiarizedTranscript JSON")
    ap.add_argument("--label", default="", help="tag echoed back in the JSON report")
    ap.add_argument("--table", action="store_true", help="also print a human summary on stderr")
    a = ap.parse_args()

    before = load(a.before, a.markdown)
    report = {"label": a.label, "before_path": a.before, "before": score(before)}
    if a.emit_json_before:
        json.dump(to_transcript_json(before), open(a.emit_json_before, "w"))

    if a.after:
        after = load(a.after, a.markdown)
        report["after_path"] = a.after
        report["after"] = score(after)
        report["delta"] = compare(before, after)
        if a.emit_json_after:
            json.dump(to_transcript_json(after), open(a.emit_json_after, "w"))

    print(json.dumps(report))
    if a.table:
        table(report)


if __name__ == "__main__":
    main()
