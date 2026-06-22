#!/usr/bin/env python3
"""ERLE / echo-cancellation quality scorer for the bench harness.

Measures how well an acoustic echo canceller (AEC) suppressed the far-end
(loudspeaker) signal that leaked into the microphone.

  ERLE (Echo Return Loss Enhancement), in dB::
        ERLE = 10 * log10( sum(mic^2) / sum(cleaned^2) )
  computed over ECHO-ONLY regions (far-end active, near-end silent). Higher
  is better: more echo energy removed.

Inputs:
  --mic   : microphone capture WAV (echo + optional near-end speech).
  --far   : far-end reference WAV (what was played to the loudspeaker).
  --out   : AEC-cleaned output WAV (what the canceller produced).
  --near  : OPTIONAL clean near-end WAV (the local talker only, no echo).

Echo-only region selection:
  * If --near is given, echo-only frames are those where the near-end signal
    is ~silent (below an energy gate). Over those frames only near-side
    contribution is echo, so ERLE there is meaningful. Additionally we report
    NEAR-END PRESERVATION over near-active frames: segmental SNR of the
    cleaned output relative to the reference near-end, plus their Pearson
    correlation. A good AEC removes echo WITHOUT chewing up near-end speech.
  * If --near is NOT given, we assume the whole signal is echo-only and
    compute ERLE over the entire (overlapping, length-matched) signal.

Output is a single JSON object on stdout.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

import numpy as np
import soundfile as sf

# Frame size for energy gating / segmental SNR (20 ms at 16 kHz = 320 samples;
# we compute per-file from the sample rate).
_FRAME_MS = 20.0
# Near-end "silence" gate: a frame is considered near-silent if its RMS is
# this many dB below the near signal's peak-frame RMS.
_SILENCE_GATE_DB = 30.0


def _read_mono(path: Path) -> tuple[np.ndarray, int]:
    data, sr = sf.read(str(path), always_2d=False)
    if data.ndim > 1:
        data = data.mean(axis=1)
    return data.astype(np.float64), sr


def _frame_rms(sig: np.ndarray, frame: int) -> np.ndarray:
    """Per-frame RMS over non-overlapping frames covering ``sig``."""
    n = len(sig) // frame
    if n == 0:
        return np.array([np.sqrt(np.mean(sig**2))]) if len(sig) else np.array([0.0])
    trimmed = sig[: n * frame].reshape(n, frame)
    return np.sqrt(np.mean(trimmed**2, axis=1))


# Sentinel for "infinite" ERLE (cleaned is exactly silent). We avoid emitting
# float('inf') because that is not valid strict JSON; 200 dB is far beyond any
# physically meaningful suppression and is safe for plotting/aggregation.
_ERLE_INF_DB = 200.0


def _erle_db(mic: np.ndarray, cleaned: np.ndarray) -> float:
    mic_e = float(np.sum(mic**2))
    cln_e = float(np.sum(cleaned**2))
    if cln_e <= 0.0:
        # Cleaned is pure silence: effectively infinite suppression. Report a
        # large finite sentinel so output stays strict-JSON valid.
        return _ERLE_INF_DB if mic_e > 0.0 else 0.0
    if mic_e <= 0.0:
        return 0.0
    return 10.0 * np.log10(mic_e / cln_e)


def score(
    mic_path: Path,
    far_path: Path,
    out_path: Path,
    near_path: Path | None,
) -> dict:
    mic, sr = _read_mono(mic_path)
    far, _ = _read_mono(far_path)
    cleaned, _ = _read_mono(out_path)

    # Length-match mic/cleaned (and near) to the shortest common length.
    frame = max(1, int(sr * _FRAME_MS / 1000.0))

    result: dict = {"sample_rate": sr}

    if near_path is not None:
        near, _ = _read_mono(near_path)
        n = min(len(mic), len(cleaned), len(near))
        mic, cleaned, near = mic[:n], cleaned[:n], near[:n]

        near_rms = _frame_rms(near, frame)
        peak = float(np.max(near_rms)) if near_rms.size else 0.0
        gate = peak * (10.0 ** (-_SILENCE_GATE_DB / 20.0))

        # Per-frame masks aligned to the framed signals.
        nf = len(near_rms)
        silent_frames = near_rms <= gate
        active_frames = ~silent_frames

        # Expand frame masks to sample masks.
        def _expand(mask: np.ndarray) -> np.ndarray:
            full = np.repeat(mask, frame)
            out = np.zeros(n, dtype=bool)
            out[: len(full)] = full[:n]
            return out

        echo_only = _expand(silent_frames)
        near_active = _expand(active_frames)

        # ERLE over echo-only (near-silent) frames.
        if echo_only.any():
            erle = _erle_db(mic[echo_only], cleaned[echo_only])
        else:
            # No near-silent frames; fall back to whole-signal ERLE.
            erle = _erle_db(mic, cleaned)
        result["erle_db"] = erle
        result["echo_only_frames"] = int(silent_frames.sum())
        result["near_active_frames"] = int(active_frames.sum())

        # Near-end preservation over near-active frames: segmental SNR of
        # cleaned vs reference near-end, and Pearson correlation.
        if near_active.any():
            c = cleaned[near_active]
            r = near[near_active]
            # Segmental SNR (dB): mean over frames of 10log10(ref^2 / err^2).
            m = (len(c) // frame) * frame
            seg_snrs = []
            for i in range(0, m, frame):
                rr = r[i : i + frame]
                ee = rr - c[i : i + frame]
                rp = float(np.sum(rr**2))
                ep = float(np.sum(ee**2))
                if rp > 0.0 and ep > 0.0:
                    seg_snrs.append(10.0 * np.log10(rp / ep))
            result["near_seg_snr_db"] = (
                float(np.mean(seg_snrs)) if seg_snrs else None
            )
            if np.std(c) > 0 and np.std(r) > 0:
                result["near_correlation"] = float(np.corrcoef(c, r)[0, 1])
            else:
                result["near_correlation"] = None
        else:
            result["near_seg_snr_db"] = None
            result["near_correlation"] = None
    else:
        # Whole signal treated as echo-only.
        n = min(len(mic), len(cleaned))
        mic, cleaned = mic[:n], cleaned[:n]
        result["erle_db"] = _erle_db(mic, cleaned)
        result["echo_only_frames"] = None
        result["near_active_frames"] = None
        result["near_seg_snr_db"] = None
        result["near_correlation"] = None

    return result


def _main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Compute dB-ERLE (echo suppression) and, with --near, near-end "
            "preservation for an AEC-cleaned signal."
        ),
    )
    parser.add_argument("--mic", required=True, type=Path, help="Microphone WAV.")
    parser.add_argument("--far", required=True, type=Path, help="Far-end reference WAV.")
    parser.add_argument("--out", required=True, type=Path, help="AEC-cleaned output WAV.")
    parser.add_argument(
        "--near",
        type=Path,
        default=None,
        help="Optional clean near-end WAV for echo-only gating + preservation.",
    )
    args = parser.parse_args(argv)

    result = score(args.mic, args.far, args.out, args.near)
    json.dump(result, sys.stdout, indent=2)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(_main(sys.argv[1:]))
