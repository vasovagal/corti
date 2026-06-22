#!/usr/bin/env python3
"""Parse an NPR Planet Money transcript HTML into clean reference text + turns JSON.

Usage: parse_transcript.py <story-id>

Reads <id>.html (raw NPR transcript page) from the cwd and writes:
  <id>.reference.txt  -- all turns concatenated as running text (WER target)
  <id>.turns.json     -- ordered [{"speaker": "<id>", "text": "..."}] (cpWER target)

Parsing rules (see README.md):
  * Body = the <div class="transcript storytext"> ... up to the first
    <p class="disclaimer"> (Copyright / Accuracy footer is dropped).
  * The injected "Sponsor Message" / "Become an NPR sponsor" block lives AFTER
    the disclaimer, so isolating up to the disclaimer also drops it.
  * Bracketed cues ([THEME MUSIC], [LAUGHTER], [AUDIO PLAYBACK], [END PLAYBACK],
    etc.) are removed.
  * Turns are delimited only by inline ALL-CAPS speaker labels. A label is a run
    of CAPS letters/space/.'- ending in a colon at a turn boundary. Full-name
    first mentions ("ALEXI HOROWITZ-GHAZI:") are merged with later last-name-only
    mentions ("HOROWITZ-GHAZI:") into one speaker id (the last-name token).
  * The sentence-final-CAPS-word-glued-to-next-label trap ("OK.  BOND:",
    "NPR.  GOLDMAN:") is handled with a stop-list: a leading "<STOPWORD>. " is
    peeled off the candidate label and folded back into the prior turn's text.
"""
import json
import re
import sys
import html as htmlmod

# Sentence-final ALL-CAPS tokens that can glue onto the next speaker label.
GLUE_STOPWORDS = {"OK", "NPR", "ISIS", "NSA", "AI", "FBI", "CIA", "US", "USA", "UK", "EU"}


def extract_body(raw: str) -> str:
    i = raw.find('class="transcript storytext"')
    if i < 0:
        raise SystemExit("transcript storytext div not found")
    seg = raw[i:]
    # Skip past the leading icn-story-transcript bold wrapper.
    start = seg.find("</b>\n    </b>")
    start = seg.find("</b>", start + 8) + 4
    end = seg.find('<p class="disclaimer">')
    if end < 0:
        raise SystemExit("disclaimer footer not found (cannot bound body)")
    return seg[start:end]


def to_text(body: str) -> str:
    # NPR uses literal <p><p> as paragraph separators inside the transcript.
    t = re.sub(r"</?p[^>]*>", " ", body)
    t = re.sub(r"<[^>]+>", " ", t)          # any stray tags
    t = htmlmod.unescape(t)
    # Drop bracketed stage cues: [THEME MUSIC], [LAUGHTER], [AUDIO PLAYBACK], ...
    t = re.sub(r"\[[^\]]*\]", " ", t)
    return t


# A candidate speaker label: 2..40 chars of CAPS letters / digits / space . ' -
# immediately followed by a colon. We require at least one letter.
LABEL_RE = re.compile(r"([A-Z][A-Z0-9 .'\-]{1,40}?):")


def split_turns(text: str):
    """Return list of (raw_label, turn_text) in order."""
    matches = list(LABEL_RE.finditer(text))
    turns = []
    for k, m in enumerate(matches):
        label = m.group(1).strip()
        seg_start = m.end()
        seg_end = matches[k + 1].start() if k + 1 < len(matches) else len(text)
        body = text[seg_start:seg_end]
        turns.append([label, body])
    return turns


def peel_glue(label: str):
    """If label is '<STOPWORD>.  REALLABEL', return (prefix_to_reappend, real_label).

    e.g. 'OK.  BOND' -> ('OK.', 'BOND'); 'NPR.  GOLDMAN' -> ('NPR.', 'GOLDMAN').
    Returns ('', label) when no glue prefix.
    """
    m = re.match(r"^([A-Z]+)\.\s+(.+)$", label)
    if m and m.group(1) in GLUE_STOPWORDS:
        return m.group(1) + ".", m.group(2).strip()
    return "", label


def last_name_id(label: str) -> str:
    """Canonical speaker id = last whitespace-delimited token of the label."""
    return label.split()[-1]


def parse(story_id: str):
    raw = open(f"{story_id}.html").read()
    text = to_text(extract_body(raw))
    raw_turns = split_turns(text)

    # Peel glued sentence-final words back into the previous turn.
    cleaned = []
    for label, body in raw_turns:
        prefix, real = peel_glue(label)
        if prefix and cleaned:
            cleaned[-1][1] = cleaned[-1][1].rstrip() + " " + prefix
        cleaned.append([real, body])

    # Build alias map: any full-name label whose last token equals a short label
    # collapses to that last token. We key everything by last-name token, which
    # naturally merges "ALEX GOLDMAN" and "GOLDMAN".
    turns = []
    for label, body in cleaned:
        spk = last_name_id(label)
        txt = re.sub(r"\s+", " ", body).strip()
        if not txt:
            continue
        if turns and turns[-1]["speaker"] == spk:
            turns[-1]["text"] += " " + txt          # merge consecutive same-speaker
        else:
            turns.append({"speaker": spk, "text": txt})

    reference = " ".join(t["text"] for t in turns)
    reference = re.sub(r"\s+", " ", reference).strip()

    with open(f"{story_id}.reference.txt", "w") as f:
        f.write(reference + "\n")
    with open(f"{story_id}.turns.json", "w") as f:
        json.dump(turns, f, ensure_ascii=False, indent=2)
        f.write("\n")

    speakers = sorted({t["speaker"] for t in turns})
    word_count = len(reference.split())
    return speakers, word_count, len(turns)


if __name__ == "__main__":
    sid = sys.argv[1]
    spk, wc, nt = parse(sid)
    print(f"{sid}: speakers={len(spk)} {spk}  turns={nt}  words={wc}")
