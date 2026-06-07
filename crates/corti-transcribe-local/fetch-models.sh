#!/usr/bin/env bash
# Fetch the ONNX models for corti's local transcription backend from pinned sherpa-onnx releases:
#   - Parakeet-TDT-0.6B-v3 (int8) ASR        (~487 MB)
#   - pyannote speaker-segmentation 3.0       (~7 MB)
#   - 3D-Speaker speaker embedding (16 kHz)   (~40 MB)
#   - Silero VAD                              (~0.6 MB)
#
# Usage: fetch-models.sh [MODEL_DIR]      (default: ~/Library/Caches/corti/models)
# Models are CC-BY-4.0 (Parakeet, pyannote) / Apache-2.0 — see the crate NOTICE.
set -euo pipefail

DIR="${1:-$HOME/Library/Caches/corti/models}"
BASE="https://github.com/k2-fsa/sherpa-onnx/releases/download"

mkdir -p "$DIR"
cd "$DIR"
echo "Fetching local-transcription models into $DIR"

fetch_tar() { # url — download a .tar.bz2 and extract it here
  local url="$1" file
  file="$(basename "$url")"
  echo "→ $file"
  curl -fL --retry 3 -o "$file" "$url"
  tar xjf "$file"
  rm -f "$file"
}

fetch_file() { # url — download a bare file here
  local url="$1"
  echo "→ $(basename "$url")"
  curl -fL --retry 3 -O "$url"
}

fetch_tar  "$BASE/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2"
fetch_tar  "$BASE/speaker-segmentation-models/sherpa-onnx-pyannote-segmentation-3-0.tar.bz2"
fetch_file "$BASE/speaker-recongition-models/3dspeaker_speech_eres2net_base_sv_zh-cn_3dspeaker_16k.onnx"
fetch_file "$BASE/asr-models/silero_vad.onnx"

echo "Done. Set CORTI_LOCAL_MODEL_DIR=$DIR (or leave unset to use this default cache)."
