#!/usr/bin/env bash
# Idempotent re-derivation of the Planet Money ground-truth fixture corpus.
#
# Re-runs are cheap: existing HTML/MP3/WAV outputs are reused unless --force.
# Committed artifacts (manifest.json, *.reference.txt, *.turns.json) are
# regenerated deterministically from the raw HTML.
#
# Requires: curl, ffmpeg/ffprobe, python3, shasum.
#   npr.org blocks plain curl/WebFetch -> we send a Mozilla UA.
#
# Usage:
#   ./fetch.sh           # fetch missing, parse, build manifest
#   ./fetch.sh --force   # re-download HTML + MP3 even if present
set -euo pipefail
cd "$(dirname "$0")"

FORCE="${1:-}"
UA='Mozilla/5.0'
RSS='https://feeds.npr.org/510289/podcast.xml'
IDS=(nx-s1-5844617 nx-s1-5856509 nx-s1-5859441)
EXCERPT_START=120     # seconds; past the cold-open/intro, into dialogue
EXCERPT_LEN=300       # 5 minutes

need() { [[ "$FORCE" == "--force" || ! -s "$1" ]]; }

echo ">> RSS"
if need podcast.xml; then curl -sL -A "$UA" "$RSS" -o podcast.xml; fi

for id in "${IDS[@]}"; do
  echo ">> $id"

  # 1. transcript HTML (gitignored)
  if need "$id.html"; then
    curl -sL -A "$UA" "https://www.npr.org/transcripts/$id" -o "$id.html"
  fi

  # 2. mp3 enclosure: resolve from RSS, follow redirects (gitignored)
  if need "$id.mp3"; then
    enc=$(python3 - "$id" <<'PY'
import sys,re,html
sid=sys.argv[1]
rss=open('podcast.xml').read()
for it in re.split(r'<item>',rss):
    if sid in it:
        m=re.search(r'<enclosure[^>]*url="([^"]+)"',it)
        print(html.unescape(m.group(1))); break
PY
)
    final=$(curl -sL -A "$UA" -o "$id.mp3" -w '%{url_effective}' "$enc")
    echo "$final" > "$id.mp3.finalurl"
  fi

  # 3. clean reference + turns (committed) -- deterministic from HTML
  python3 parse_transcript.py "$id"

  # 4. excerpt + full WAV: 16 kHz mono 16-bit (gitignored)
  if need "$id.5min.wav"; then
    ffmpeg -y -loglevel error -ss "$EXCERPT_START" -t "$EXCERPT_LEN" \
      -i "$id.mp3" -ac 1 -ar 16000 -sample_fmt s16 "$id.5min.wav"
  fi
  if need "$id.full.wav"; then
    ffmpeg -y -loglevel error -i "$id.mp3" \
      -ac 1 -ar 16000 -sample_fmt s16 "$id.full.wav"
  fi
done

# 5. manifest.json (committed)
python3 - <<'PY'
import json,hashlib,subprocess
IDS=["nx-s1-5844617","nx-s1-5856509","nx-s1-5859441"]
TITLES={
 "nx-s1-5844617":"There's no business like dough business",
 "nx-s1-5856509":"It's my tree. Why can't I cut it down?",
 "nx-s1-5859441":"Can computer hackers get inside your mind?",
}
def canon(sid):
    # Stable simplecast URL (drop per-request session/redirect tokens).
    u=open(f"{sid}.mp3.finalurl").read().strip()
    return u.split("?")[0]
eps=[]
for sid in IDS:
    sha=hashlib.sha256(open(f"{sid}.mp3","rb").read()).hexdigest()
    dur=float(subprocess.check_output(["ffprobe","-v","error",
        "-show_entries","format=duration","-of","default=nw=1:nk=1",f"{sid}.mp3"]).strip())
    turns=json.load(open(f"{sid}.turns.json"))
    speakers=sorted({t["speaker"] for t in turns})
    wc=len(open(f"{sid}.reference.txt").read().split())
    eps.append({"id":sid,"title":TITLES[sid],"mp3_url":canon(sid),
        "mp3_sha256":sha,"duration_s":round(dur,3),
        "transcript_url":f"https://www.npr.org/transcripts/{sid}",
        "speaker_count":len(speakers),"speakers":speakers,
        "ref_word_count":wc,"turn_count":len(turns),
        "excerpt_start_s":120,"excerpt_len_s":300})
json.dump({"source":"NPR Planet Money","rss":"https://feeds.npr.org/510289/podcast.xml",
    "episodes":eps},open("manifest.json","w"),indent=2,ensure_ascii=False)
open("manifest.json","a").write("\n")
for e in eps:
    print(f"{e['id']}: speakers={e['speaker_count']} words={e['ref_word_count']} dur={e['duration_s']}s")
PY
echo ">> done"
