#!/usr/bin/env bash
# Wrap `corti-bench capture` in a signed .app and launch it via LaunchServices so macOS grants the
# audio-capture TCC permission to a stable BUNDLE IDENTITY (a loose CLI run from a shell is silently denied —
# the terminal becomes the responsible process and gets zero IO callbacks). Adapted from
# crates/corti-coreaudio/bundle-spike.sh. Phase 0 / Phase 4 of design/06-benchmark-harness.md.
#
# Usage:  bench/bundle-capture.sh [seconds] [out.wav] [extra corti-bench capture args...]
#   bench/bundle-capture.sh 5 /tmp/cap.wav                 # 5s global-tap + mic smoke capture
#   bench/bundle-capture.sh 300 /tmp/podcast.wav           # 5-min acoustic capture
# For a STABLE grant across rebuilds, make a self-signed cert once and export CORTI_SIGN_ID=its-name
# (ad-hoc signing changes the cdhash every build → macOS re-prompts and orphans the prior grant).
set -euo pipefail

secs="${1:-5}"
out="${2:-/tmp/corti-bench-capture.wav}"
shift || true; shift || true
extra=("$@")

root="$(cd "$(dirname "$0")/.." && pwd)"   # worktree root
bin="$root/target/aarch64-apple-darwin/release/corti-bench"
app="$root/bench/dist/corti-bench-capture.app"
macos="$app/Contents/MacOS"
venv_py="$root/bench/.venv/bin/python"

# Refuse to fight the tray for the mic/aggregate device.
if pgrep -f "Corti.app/Contents/MacOS/corti" >/dev/null 2>&1; then
  echo "✗ The Corti tray app is running. Quit it first (capture fights it for the mic):"
  echo "    osascript -e 'quit app \"Corti\"'   # or kill the menu-bar app"
  exit 1
fi

echo "building corti-bench (release)…"
( cd "$root" && cargo build -p corti-bench --release >/dev/null )

echo "assembling $app …"
rm -rf "$app"; mkdir -p "$macos"
cp "$bin" "$macos/corti-bench"

cat > "$app/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key><string>corti-bench</string>
    <key>CFBundleIdentifier</key><string>com.vasovagal.corti.bench</string>
    <key>CFBundleName</key><string>corti-bench-capture</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleShortVersionString</key><string>0.7.0</string>
    <key>LSMinimumSystemVersion</key><string>14.2</string>
    <key>LSBackgroundOnly</key><true/>
    <key>NSAudioCaptureUsageDescription</key>
    <string>corti-bench records system audio + mic to benchmark transcription and echo cancellation.</string>
    <key>NSMicrophoneUsageDescription</key>
    <string>corti-bench records the microphone to benchmark transcription and echo cancellation.</string>
</dict>
</plist>
PLIST

sign_id="${CORTI_SIGN_ID:--}"
echo "signing (identity: $sign_id)…"
codesign --force --sign "$sign_id" --timestamp=none "$app" >/dev/null
codesign --verify --verbose "$app"

echo
echo "launching capture via LaunchServices (${secs}s) — APPROVE the macOS prompt if it appears…"
rm -f "$out"
open "$app" --args capture --seconds "$secs" --out "$out" --no-lock ${extra[@]+"${extra[@]}"}

sleep "$(awk "BEGIN{print $secs + 5}")"
echo
if [[ -f "$out" ]]; then
  echo "✅ wrote $out"
  afinfo "$out" 2>/dev/null | grep -E "channels|sample rate|estimated duration" || true
  # Per-channel RMS (non-silence go/no-go).
  "$venv_py" - "$out" <<'PYEOF'
import sys, soundfile as sf, numpy as np
x, sr = sf.read(sys.argv[1], always_2d=True)
def rms(a): return float(np.sqrt(np.mean(a**2))) if a.size else 0.0
ch = {f"ch{i}": round(rms(x[:, i]), 6) for i in range(x.shape[1])}
print(f"sample_rate={sr} channels={x.shape[1]} frames={x.shape[0]} rms={ch}")
silent = [k for k, v in ch.items() if v < 1e-5]
print("⚠ SILENT channels:", silent if silent else "none — capture looks good")
PYEOF
else
  echo "✗ no WAV produced — the TCC prompt is probably pending. Approve it in"
  echo "  System Settings → Privacy & Security → (Microphone AND Screen & System Audio Recording),"
  echo "  enable 'corti-bench-capture', then re-run this script."
fi
