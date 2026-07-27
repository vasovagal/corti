#!/usr/bin/env bash
# Reject a release app that does not satisfy Corti's macOS/TCC bundle contract.
set -euo pipefail

fail() {
  echo "release bundle verification failed: $*" >&2
  exit 1
}

[[ $# -eq 1 ]] || fail "usage: $0 <Corti.app>"
APP=$1
INFO="$APP/Contents/Info.plist"
EXE="$APP/Contents/MacOS/corti"
[[ -d "$APP" ]] || fail "not an app bundle: $APP"
[[ -f "$INFO" ]] || fail "missing Info.plist"
[[ -f "$EXE" ]] || fail "missing executable: $EXE"

codesign --verify --deep --strict --verbose=2 "$APP"

plist_raw() {
  plutil -extract "$1" raw -o - "$2" 2>/dev/null
}

IDENTIFIER=$(plist_raw CFBundleIdentifier "$INFO")
[[ "$IDENTIFIER" == "com.vasovagal.corti" ]] || fail "unexpected bundle identifier: $IDENTIFIER"
for key in NSAudioCaptureUsageDescription NSMicrophoneUsageDescription; do
  value=$(plist_raw "$key" "$INFO")
  [[ -n "$value" ]] || fail "$key is missing or empty"
done
[[ "$(plist_raw LSMinimumSystemVersion "$INFO")" == "15.0" ]] || \
  fail "Info.plist minimum system version is not 15.0"

ARCHS=$(lipo -archs "$EXE")
[[ "$ARCHS" == "arm64" ]] || fail "expected arm64-only executable, got: $ARCHS"
MINOS=$(vtool -show-build "$EXE" | awk '$1 == "minos" { print $2; exit }')
[[ "$MINOS" == "15.0" ]] || fail "expected Mach-O minos 15.0, got: ${MINOS:-missing}"

ENTITLEMENTS=$(mktemp "${TMPDIR:-/tmp}/corti-entitlements.XXXXXX")
cleanup() {
  rm -f "$ENTITLEMENTS"
}
trap cleanup EXIT HUP INT TERM
# `:-` asks codesign for XML on current macOS; stderr carries only display diagnostics/deprecation text.
codesign -d --entitlements :- "$APP" >"$ENTITLEMENTS" 2>/dev/null
for key in com.apple.security.device.audio-input com.apple.security.device.microphone; do
  escaped_key=${key//./\\.}
  [[ "$(plist_raw "$escaped_key" "$ENTITLEMENTS")" == "true" ]] || fail "missing true entitlement: $key"
done

printf 'verified release bundle: %s\n' "$APP"
