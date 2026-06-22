#!/usr/bin/env python3
"""Build the SYNTHETIC double-talk AEC fixture for the corti bench harness.

Produces a ground-truthed acoustic-echo fixture so the AEC sweep can measure
both echo cancellation (dB-ERLE) and near-end preservation against a perfectly
known clean near-end (the macOS `say` script is the ground-truth transcript).

Recipe (after the Microsoft AEC-Challenge synthetic pipeline):
  * NEAR-END  : clean TTS speech (known text). Uncorrelated with far-end.
  * FAR-END   : an unrelated NPR segment (different content from near-end).
  * ROOM IR   : synthetic plausible impulse response (direct path + early
                reflections + exponentially-decaying late reverb tail).
  * NONLINEAR : loudspeaker nonlinearity modeled as tanh(drive * far) before
                the echo path convolution (speaker overdrive / clipping).
  * MIC       : mic = near + echo + noise,  echo = conv(tanh(drive*far), ir)*atten
  * ECHO-ONLY : mic_echo = echo + small_noise (near is silence) — pure ERLE.

EVERYTHING is 48 kHz mono float32 (corti captures at 48 kHz; AEC filter_len=4096
≈ 85 ms is tuned for 48 kHz). 2-track outputs use corti's channel order:
ch0 = mic ("me"), ch1 = far-end tap ("them").

Use --ir <wav> to swap in a REAL captured room impulse response later; by
default the script synthesizes the placeholder IR described above.
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

import numpy as np
import soundfile as sf
from scipy.signal import fftconvolve

SR = 48_000  # everything at 48 kHz
HERE = Path(__file__).resolve().parent

# ----------------------------------------------------------------------------
# Fixture parameters (also documented in README.md). Deterministic via SEED.
# ----------------------------------------------------------------------------
SEED = 1234

# Far-end source: an NPR full episode DIFFERENT in content from the near-end.
# Near-end is TTS; far-end below is the "trees" episode (5856509).
FAR_SRC = HERE.parent / "nx-s1-5856509.full.wav"
FAR_START_S = 700.0  # ffmpeg -ss
FAR_DUR_S = 180.0     # ffmpeg -t

# TTS near-end voice.
SAY_VOICE = "Samantha"

# Synthetic room IR params.
IR_LEN_S = 0.250          # total IR length (s)
IR_DIRECT_DELAY_S = 0.004 # bulk/direct-path delay (s) ~4 ms
IR_N_EARLY = 8            # number of early reflections
IR_EARLY_MAX_S = 0.030   # early reflections fall within ~30 ms
IR_RT60_S = 0.200        # late-reverb RT60 ≈ 200 ms

# Loudspeaker nonlinearity + echo path.
DRIVE = 1.5              # tanh drive (speaker overdrive)
TARGET_ERL_DB = 3.0      # desired Echo Return Loss: 10log10(near_pow/echo_pow)
                         # ERL ≈ 0..+6 dB is a realistic speaker-bleed level.
SNR_DB = 35.0            # gaussian mic noise SNR vs near (double-talk mic)
ECHOONLY_NOISE_DB = 45.0 # very low noise floor on the echo-only mic


def _run(cmd: list[str]) -> None:
    subprocess.run(cmd, check=True, capture_output=True)


def _ffmpeg() -> str:
    for c in ("/opt/homebrew/bin/ffmpeg", "ffmpeg"):
        if shutil.which(c) or Path(c).exists():
            return c
    raise SystemExit("ffmpeg not found")


def _write_wav(path: Path, data: np.ndarray, sr: int = SR) -> None:
    """Write float32 WAV. data shape (n,) mono or (n, ch) multi-track."""
    path.parent.mkdir(parents=True, exist_ok=True)
    sf.write(str(path), data.astype(np.float32), sr, subtype="FLOAT")


def _read_mono(path: Path) -> np.ndarray:
    data, sr = sf.read(str(path), always_2d=False)
    if data.ndim > 1:
        data = data.mean(axis=1)
    if sr != SR:
        raise SystemExit(f"{path}: expected {SR} Hz, got {sr}")
    return data.astype(np.float64)


def build_near(tmp: Path) -> np.ndarray:
    """Synthesize clean near-end TTS at 48 kHz mono f32."""
    script = (HERE / "near.reference.txt").read_text(encoding="utf-8")
    aiff = tmp / "near.aiff"
    _run(["say", "-v", SAY_VOICE, "-o", str(aiff), script])
    # Convert to 48 kHz mono float WAV via ffmpeg.
    near_wav = HERE / "near.wav"
    _run([
        _ffmpeg(), "-y", "-i", str(aiff),
        "-ac", "1", "-ar", str(SR), "-c:a", "pcm_f32le",
        str(near_wav),
    ])
    return _read_mono(near_wav)


def build_far(tmp: Path) -> np.ndarray:
    """Extract an unrelated NPR segment, resample to 48 kHz mono f32."""
    if not FAR_SRC.exists():
        raise SystemExit(f"far source missing: {FAR_SRC}")
    far_wav = HERE / "far.wav"
    _run([
        _ffmpeg(), "-y", "-ss", str(FAR_START_S), "-t", str(FAR_DUR_S),
        "-i", str(FAR_SRC),
        "-ac", "1", "-ar", str(SR), "-c:a", "pcm_f32le",
        str(far_wav),
    ])
    return _read_mono(far_wav)


def synth_ir(rng: np.random.Generator) -> np.ndarray:
    """Synthesize a plausible 48 kHz room impulse response.

    direct path at IR_DIRECT_DELAY_S + early reflections (decaying, random taps
    within IR_EARLY_MAX_S) + exponentially-decaying late reverb tail
    (RT60 = IR_RT60_S). Peak-normalized.
    """
    n = int(IR_LEN_S * SR)
    ir = np.zeros(n, dtype=np.float64)

    # Direct path.
    d0 = int(IR_DIRECT_DELAY_S * SR)
    ir[d0] = 1.0

    # Early reflections: random taps within ~30 ms, decaying with delay.
    early_max = int(IR_EARLY_MAX_S * SR)
    for _ in range(IR_N_EARLY):
        t = d0 + int(rng.integers(1, max(2, early_max - d0)))
        if t >= n:
            continue
        decay = np.exp(-3.0 * (t - d0) / max(1, early_max))  # gentle falloff
        amp = decay * rng.uniform(0.2, 0.6) * rng.choice([-1.0, 1.0])
        ir[t] += amp

    # Late-reverb tail: gaussian noise * exponential envelope, starting after
    # the early region. RT60 = time for envelope to drop 60 dB.
    t = np.arange(n, dtype=np.float64)
    # tau such that exp(-t/tau) hits -60 dB at RT60: tau = RT60 / (6.908)
    tau = IR_RT60_S * SR / 6.908
    env = np.exp(-t / tau)
    tail_start = d0 + early_max
    tail = rng.standard_normal(n) * env
    tail[:tail_start] = 0.0
    tail *= 0.15  # late tail is quieter than direct/early
    ir += tail

    # Peak-normalize.
    peak = np.max(np.abs(ir))
    if peak > 0:
        ir = ir / peak
    return ir


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--ir", type=Path, default=None,
        help="Real captured room IR WAV (48 kHz mono). Default: synthesize one.",
    )
    args = ap.parse_args(argv)

    rng = np.random.default_rng(SEED)
    tmp = Path(tempfile.mkdtemp(prefix="synth_aec_"))

    print("[1/6] near-end TTS ...", file=sys.stderr)
    near = build_near(tmp)
    print("[2/6] far-end NPR ...", file=sys.stderr)
    far = build_far(tmp)

    # Trim near/far to the same (min) length.
    n = min(len(near), len(far))
    near, far = near[:n], far[:n]

    print("[3/6] room IR ...", file=sys.stderr)
    if args.ir is not None:
        ir = _read_mono(args.ir)
        ir_note = f"real IR from {args.ir}"
    else:
        ir = synth_ir(rng)
        _write_wav(HERE / "room_ir" / "synthetic_ir.wav", ir)
        ir_note = "synthetic placeholder IR"

    print("[4/6] loudspeaker nonlinearity + echo path ...", file=sys.stderr)
    # Loudspeaker nonlinearity, then echo path convolution.
    spk = np.tanh(DRIVE * far)
    echo_full = fftconvolve(spk, ir, mode="full")[:n]

    # Choose atten so the achieved ERL ~ TARGET_ERL_DB:
    #   ERL = 10log10(near_pow / echo_pow). We scale echo to hit it.
    near_pow = float(np.mean(near**2))
    echo_pow = float(np.mean(echo_full**2))
    if echo_pow <= 0:
        raise SystemExit("echo power is zero — IR/far broken")
    # desired echo_pow = near_pow / 10^(ERL/10); atten^2 scales echo_pow.
    desired_echo_pow = near_pow / (10.0 ** (TARGET_ERL_DB / 10.0))
    atten = float(np.sqrt(desired_echo_pow / echo_pow))
    echo = echo_full * atten

    achieved_erl_db = 10.0 * np.log10(near_pow / float(np.mean(echo**2)))

    # Double-talk mic: mic = near + echo + noise (gaussian, SNR_DB vs near).
    noise_pow = near_pow / (10.0 ** (SNR_DB / 10.0))
    noise = rng.standard_normal(n) * np.sqrt(noise_pow)
    mic = near + echo + noise

    # Echo-only mic: near is silence. echo + very-low noise floor.
    eo_noise_pow = float(np.mean(echo**2)) / (10.0 ** (ECHOONLY_NOISE_DB / 10.0))
    eo_noise = rng.standard_normal(n) * np.sqrt(eo_noise_pow)
    mic_echo = echo + eo_noise

    # Guard against clipping: scale everything by a common factor if mic peaks
    # above 1.0 (keep relative levels / ERL intact).
    peak = max(np.max(np.abs(mic)), np.max(np.abs(mic_echo)), np.max(np.abs(far)))
    if peak > 0.999:
        g = 0.999 / peak
        near, far, echo, mic, mic_echo = (
            near * g, far * g, echo * g, mic * g, mic_echo * g,
        )

    print("[5/6] writing component + 2-track WAVs ...", file=sys.stderr)
    # Components for scoring.
    _write_wav(HERE / "near.wav", near)   # overwrite the scaled near
    _write_wav(HERE / "far.wav", far)
    _write_wav(HERE / "echo.wav", echo)

    # 2-track double-talk: ch0 = mic, ch1 = far (corti mic/tap order).
    _write_wav(HERE / "doubletalk.wav", np.stack([mic, far], axis=1))
    # 2-track echo-only: ch0 = mic_echo, ch1 = far.
    _write_wav(HERE / "echoonly.wav", np.stack([mic_echo, far], axis=1))

    print("[6/6] done.", file=sys.stderr)
    dur_s = n / SR
    print(
        f"\nSYNTHETIC AEC FIXTURE ({ir_note})\n"
        f"  sample_rate     : {SR} Hz, mono f32 components / 2-track mixes\n"
        f"  duration        : {dur_s:.1f} s ({n} samples)\n"
        f"  drive (tanh)    : {DRIVE}\n"
        f"  atten           : {atten:.5f}\n"
        f"  direct delay    : {IR_DIRECT_DELAY_S*1000:.1f} ms\n"
        f"  RT60            : {IR_RT60_S*1000:.0f} ms\n"
        f"  mic noise SNR   : {SNR_DB:.0f} dB (vs near)\n"
        f"  target ERL      : {TARGET_ERL_DB:.1f} dB\n"
        f"  ACHIEVED ERL    : {achieved_erl_db:.2f} dB\n"
        f"  channel order   : ch0 = mic, ch1 = far\n",
        file=sys.stderr,
    )
    # Machine-readable line for callers.
    print(f"ACHIEVED_ERL_DB={achieved_erl_db:.3f} ATTEN={atten:.6f}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
