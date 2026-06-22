# Ground-truth fixture corpus — NPR Planet Money

Three Planet Money episodes with human-edited NPR transcripts, used as ground
truth for ASR (WER) and speaker-attributed (cpWER) scoring in the bench harness.

| story id        | title                                      | speakers | ref words | turns | mp3 duration |
|-----------------|--------------------------------------------|---------:|----------:|------:|-------------:|
| nx-s1-5844617   | There's no business like dough business    |        7 |     4 777 |   106 |   1880.5 s   |
| nx-s1-5856509   | It's my tree. Why can't I cut it down?     |        6 |     4 509 |   123 |   1753.8 s   |
| nx-s1-5859441   | Can computer hackers get inside your mind? |        5 |     4 529 |   148 |   1998.1 s   |

Source: NPR Planet Money podcast feed `https://feeds.npr.org/510289/podcast.xml`.
Transcript pages: `https://www.npr.org/transcripts/<id>`.

## Files

**Committed** (small, deterministic):
- `manifest.json` — per-episode id / title / mp3_url / mp3_sha256 / duration_s /
  transcript_url / speaker_count / speakers / ref_word_count / turn_count /
  excerpt window.
- `<id>.reference.txt` — clean running text, all turns concatenated. **WER target.**
- `<id>.turns.json` — ordered `[{"speaker": "<id>", "text": "..."}]`. **cpWER target.**
- `<id>.5min.reference.txt` — pointer note (there is no clean 5-min reference; see below).
- `parse_transcript.py` — HTML → reference.txt + turns.json (deterministic).
- `fetch.sh` — idempotent re-derivation of the whole corpus.
- `.gitignore` — excludes the heavy / re-derivable artifacts below.

**Gitignored** (heavy, re-derivable via `./fetch.sh`):
- `<id>.html` — raw NPR transcript page.
- `<id>.mp3` — podcast audio (128 kbps), `<id>.mp3.finalurl` — resolved CDN URL.
- `podcast.xml` — the RSS feed snapshot.
- `<id>.5min.wav` — 5-minute excerpt, 16 kHz mono 16-bit PCM (`[120s, 420s)`).
- `<id>.full.wav` — full episode, 16 kHz mono 16-bit PCM.

Audio integrity is pinned by `mp3_sha256` in `manifest.json`. NPR inserts dynamic
ads, so the *downloaded* mp3 duration (≈1880 s) is longer than the RSS `d=` hint
(≈1643 s); `duration_s` reflects the actual downloaded file.

## Scoring targets — read before benchmarking

The NPR transcript is **full-episode and untimed**. It cannot be reliably sliced
to the `[120s, 420s)` window of the 5-minute excerpt. Therefore:

- **`*.5min.wav` → fast SWEEP iteration only.** Use these for *relative* ranking
  of parameter configurations (cheap, ~5 min of audio). Do **not** read absolute
  WER off a 5-min excerpt against the full reference — the denominators don't
  match. If you need an absolute 5-min number, hand-align a window of
  `reference.txt` once and cache it.
- **`*.full.wav` → absolute WER / cpWER.** Score the full-episode transcription
  against `<id>.reference.txt` (running text) and `<id>.turns.json`
  (per-speaker turns). This is the primary scoring target.

The 5-min excerpts start at **120 s** (past the cold open / theme, into dialogue).

## Parsing notes (see `parse_transcript.py`)

NPR transcript bodies are delimited only by inline ALL-CAPS speaker labels and
literal `<p><p>` separators. The parser:

1. Isolates the `<div class="transcript storytext">` body and cuts at the first
   `<p class="disclaimer">`. This drops the `Copyright © … NPR.` footer, the
   "Accuracy and availability…" disclaimer, and the injected
   "Sponsor Message" / "Become an NPR sponsor" block (all of which live *after*
   the disclaimer).
2. Strips bracketed cues: `[THEME MUSIC]`, `[LAUGHTER]`,
   `[AUDIO PLAYBACK]` / `[END PLAYBACK]`, etc.
3. Splits turns on a speaker-label regex (`^CAPS…:`), then **merges full-name →
   last-name aliases** into one speaker id keyed by the last-name token — e.g.
   `ALEXI HOROWITZ-GHAZI:` (first mention) and later `HOROWITZ-GHAZI:` both map to
   `HOROWITZ-GHAZI`; `ALEX GOLDMAN:` → `GOLDMAN`.
4. Handles the **sentence-final-CAPS-word-glued-to-next-label trap**: a real
   label can be preceded by a sentence-final caps word, e.g. `OK.  BOND:`,
   `NPR.  GOLDMAN:`, `ISIS.  FOUNTAIN:`. A stop-list (`OK, NPR, ISIS, NSA, AI,
   FBI, …`) peels the leading `<WORD>. ` back into the previous turn so the
   speaker id is just `BOND` / `GOLDMAN` / `FOUNTAIN`.
5. Merges consecutive same-speaker turns and normalizes whitespace.

`ANNOUNCER` (the cold-open station ident) is kept as a distinct speaker.

## Re-deriving

```sh
./fetch.sh            # fetch missing HTML/MP3, parse, extract WAVs, build manifest
./fetch.sh --force    # also re-download HTML + MP3
```

Requires `curl`, `ffmpeg`/`ffprobe`, `python3`, `shasum`. `npr.org` blocks plain
curl/WebFetch, so `fetch.sh` sends a `Mozilla/5.0` User-Agent.
