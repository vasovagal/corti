#!/usr/bin/env bash
# Download, authenticate, and extract the exact sherpa-onnx native archive used by release builds.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")" && pwd -P)
MANIFEST="$ROOT/checksums/sherpa-onnx-1.13.2-osx-arm64-static-lib.sha256"
RELEASE_BASE="https://github.com/k2-fsa/sherpa-onnx/releases/download/v1.13.2"

usage() {
  echo "usage: $0 <destination-directory>" >&2
  exit 2
}

[[ $# -eq 1 ]] || usage
DEST=$1
ARCHIVE_NAME=$(awk 'NF == 2 { print $2 }' "$MANIFEST")
EXPECTED_SHA256=$(awk 'NF == 2 { print $1 }' "$MANIFEST")
[[ -n "$ARCHIVE_NAME" && -n "$EXPECTED_SHA256" ]] || {
  echo "invalid checksum manifest: $MANIFEST" >&2
  exit 1
}

DEST_PARENT=$(dirname "$DEST")
mkdir -p "$DEST_PARENT"
DEST_PARENT=$(cd "$DEST_PARENT" && pwd -P)
DEST="$DEST_PARENT/$(basename "$DEST")"
WORK=$(mktemp -d "$DEST_PARENT/.corti-sherpa-onnx.XXXXXX")
cleanup() {
  rm -rf "$WORK"
}
trap cleanup EXIT HUP INT TERM

ARCHIVE="$WORK/$ARCHIVE_NAME"
if [[ -n "${SHERPA_ONNX_ARCHIVE_PATH:-}" ]]; then
  cp "$SHERPA_ONNX_ARCHIVE_PATH" "$ARCHIVE"
else
  curl --fail --location --retry 3 --output "$ARCHIVE" "$RELEASE_BASE/$ARCHIVE_NAME"
fi

ACTUAL_SHA256=$(shasum -a 256 "$ARCHIVE" | awk '{ print $1 }')
if [[ "$ACTUAL_SHA256" != "$EXPECTED_SHA256" ]]; then
  echo "sherpa-onnx archive checksum mismatch" >&2
  echo "  archive:  $ARCHIVE_NAME" >&2
  echo "  expected: $EXPECTED_SHA256" >&2
  echo "  actual:   $ACTUAL_SHA256" >&2
  exit 1
fi

echo "$EXPECTED_SHA256  $ARCHIVE_NAME" | (cd "$WORK" && shasum -a 256 --check -) >/dev/null

tar -xjf "$ARCHIVE" -C "$WORK"
EXTRACTED="$WORK/${ARCHIVE_NAME%.tar.bz2}"
[[ -d "$EXTRACTED/lib" ]] || {
  echo "verified archive did not contain expected lib directory: $EXTRACTED/lib" >&2
  exit 1
}
for library in libsherpa-onnx-c-api.a libonnxruntime.a; do
  [[ -f "$EXTRACTED/lib/$library" ]] || {
    echo "verified archive is missing $library" >&2
    exit 1
  }
done

PROMOTE="$DEST.tmp-$$"
rm -rf "$PROMOTE"
mv "$EXTRACTED" "$PROMOTE"
rm -rf "$DEST"
mv "$PROMOTE" "$DEST"
printf '%s\n' "$DEST/lib"
