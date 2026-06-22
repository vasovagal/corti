#!/usr/bin/env python3
"""Measure the real speaker->room->mic impulse response.

`gen`  — write an exponential sine sweep (ESS) WAV to play through the speakers.
`deconv` — given a 2-track capture of that sweep (ch0 = mic, ch1 = system tap = the exact played
           reference), recover the room IR by regularized spectral division IR = ifft(fft(mic)/fft(ref)).
           Using the TAP as the reference (not the original file) cancels playback resampling/latency and
           any output-chain coloring, so the IR is purely speaker->air->mic.

This real IR feeds bench/fixtures/synthetic/build_synthetic.py --ir <ir.wav> so the synthetic double-talk
fixture matches THIS room. Phase 2 of design/06-benchmark-harness.md.

  room_ir.py gen --out sweep.wav [--secs 10] [--sr 48000] [--f0 30] [--f1 20000] [--pad 1.0]
  room_ir.py deconv --capture cap.wav --out ir.wav [--ir-secs 0.5] [--reg 1e-3]
"""
import argparse
import numpy as np
import soundfile as sf


def gen(a):
    sr = a.sr
    t = np.arange(int(a.secs * sr)) / sr
    # Farina exponential sine sweep.
    w0, w1 = 2 * np.pi * a.f0, 2 * np.pi * a.f1
    K = a.secs * w0 / np.log(w1 / w0)
    L = a.secs / np.log(w1 / w0)
    sweep = np.sin(K * (np.exp(t / L) - 1.0))
    # Short fade in/out + trailing pad so the room tail is captured.
    fade = int(0.02 * sr)
    sweep[:fade] *= np.linspace(0, 1, fade)
    sweep[-fade:] *= np.linspace(1, 0, fade)
    pad = np.zeros(int(a.pad * sr))
    out = np.concatenate([sweep, pad]).astype(np.float32) * 0.8
    sf.write(a.out, out, sr, subtype="FLOAT")
    print(f"wrote {a.out}: {len(out)/sr:.1f}s ESS {a.f0}->{a.f1} Hz @ {sr}")


def deconv(a):
    x, sr = sf.read(a.capture, always_2d=True)
    if x.shape[1] < 2:
        raise SystemExit("capture must be 2-track (ch0=mic, ch1=tap reference)")
    mic = x[:, 0].astype(np.float64)
    ref = x[:, 1].astype(np.float64)
    n = 1 << int(np.ceil(np.log2(len(mic))))
    M = np.fft.rfft(mic, n)
    R = np.fft.rfft(ref, n)
    # Regularized inverse: avoids blow-up where the reference has little energy.
    reg = a.reg * np.max(np.abs(R) ** 2)
    H = (M * np.conj(R)) / (np.abs(R) ** 2 + reg)
    ir_full = np.fft.irfft(H, n)
    # Keep from the main peak for ir_secs.
    peak = int(np.argmax(np.abs(ir_full)))
    length = int(a.ir_secs * sr)
    start = max(0, peak - int(0.001 * sr))  # a touch of pre-ringing
    ir = ir_full[start : start + length]
    if np.max(np.abs(ir)) > 0:
        ir = ir / np.max(np.abs(ir))
    sf.write(a.out, ir.astype(np.float32), sr, subtype="FLOAT")
    # Crude RT60-ish: time for the energy decay curve to drop 60 dB.
    energy = np.cumsum(ir[::-1] ** 2)[::-1]
    edc = 10 * np.log10(energy / np.max(energy) + 1e-12)
    rt = np.argmax(edc < -60)
    print(f"wrote {a.out}: IR {len(ir)/sr*1000:.0f} ms @ {sr}, peak@{peak/sr*1000:.1f}ms, "
          f"EDC-60 ~{(rt/sr*1000) if rt else float('nan'):.0f}ms")


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    sub = ap.add_subparsers(dest="cmd", required=True)
    g = sub.add_parser("gen")
    g.add_argument("--out", required=True)
    g.add_argument("--secs", type=float, default=10.0)
    g.add_argument("--sr", type=int, default=48000)
    g.add_argument("--f0", type=float, default=30.0)
    g.add_argument("--f1", type=float, default=20000.0)
    g.add_argument("--pad", type=float, default=1.0)
    g.set_defaults(fn=gen)
    d = sub.add_parser("deconv")
    d.add_argument("--capture", required=True)
    d.add_argument("--out", required=True)
    d.add_argument("--ir-secs", type=float, default=0.5)
    d.add_argument("--reg", type=float, default=1e-3)
    d.set_defaults(fn=deconv)
    a = ap.parse_args()
    a.fn(a)


if __name__ == "__main__":
    main()
