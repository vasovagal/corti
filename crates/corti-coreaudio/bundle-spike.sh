#!/usr/bin/env bash
# Build the spike, wrap it in a .app bundle, sign it, and launch it via LaunchServices.
#
# WHY a bundle + `open`: macOS TCC anchors the audio-capture permission to a *bundle identity* reached
# through LaunchServices. A loose CLI binary — even with an embedded Info.plist — run from a shell makes the
# *terminal* the responsible process and is silently denied (no prompt, zero IO callbacks). Process-tap
# tools (AudioCap) ship as .app bundles for exactly this reason; this also mirrors the eventual Tauri app.
#
# Usage:  ./bundle-spike.sh [seconds] [out.wav]      # defaults: 15  /tmp/corti-spike.wav
set -euo pipefail

secs="${1:-15}"
out="${2:-/tmp/corti-spike.wav}"

cd "$(dirname "$0")/../.."          # workspace root
crate_dir="crates/corti-coreaudio"
app="$crate_dir/dist/corti-spike.app"
macos="$app/Contents/MacOS"

echo "building spike (release)…"
cargo build -p corti-coreaudio --bin spike --release

echo "assembling $app …"
rm -rf "$app"
mkdir -p "$macos"
cp target/release/spike "$macos/corti-spike"

cat > "$app/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key><string>corti-spike</string>
    <key>CFBundleIdentifier</key><string>com.vasovagal.corti.spike</string>
    <key>CFBundleName</key><string>corti-spike</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleShortVersionString</key><string>0.1.0</string>
    <key>LSMinimumSystemVersion</key><string>14.2</string>
    <key>LSBackgroundOnly</key><true/>
    <key>NSAudioCaptureUsageDescription</key>
    <string>corti records meeting audio to generate transcript notes.</string>
    <key>NSMicrophoneUsageDescription</key>
    <string>corti records your microphone during meetings to generate transcript notes.</string>
</dict>
</plist>
PLIST

# Ad-hoc sign the bundle. NOTE: ad-hoc cdhash changes every rebuild, so macOS re-prompts and the prior grant
# is orphaned. For repeated runs, create a stable self-signed cert once and set CORTI_SIGN_ID to its name:
#   CORTI_SIGN_ID="corti-dev"  ./bundle-spike.sh
sign_id="${CORTI_SIGN_ID:--}"
echo "signing (identity: $sign_id)…"
codesign --force --sign "$sign_id" --timestamp=none "$app"
codesign --verify --verbose "$app"

echo
echo "launching via LaunchServices (this is what makes the TCC prompt appear)…"
rm -f "$out" "${out%.wav}.log"
open "$app" --args "$secs" "$out"

echo "recording ${secs}s — TALK and PLAY SOME AUDIO now."
echo "Approve the macOS prompt if it appears, then re-run this script (the first attempt is usually denied)."
# Wait for the run to finish (a little longer than the capture window) and surface the log.
sleep "$((secs + 4))"
log="${out%.wav}.log"
echo
if [[ -f "$log" ]]; then
    echo "── ${log} ──"; cat "$log"
else
    echo "no log at $log — the app may not have launched; check Console.app for crashes."
fi
echo
if [[ -f "$out" ]]; then
    echo "✅ wrote $out"
    echo "   inspect:  afinfo \"$out\"   or   open -a Audacity \"$out\""
else
    echo "✗ no WAV produced — see the log above (likely the TCC prompt; approve it and re-run)."
fi
