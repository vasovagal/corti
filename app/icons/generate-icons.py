#!/usr/bin/env python3
"""Generate corti's app + tray icons with zero third-party deps (stdlib zlib/struct only).

Why hand-rolled: keeps the repo buildable offline with no Pillow/ImageMagick, and gives precise control over
the menu-bar *template* icons (monochrome black + alpha mask, which macOS tints for light/dark mode).

Outputs (into this dir):
  app-source.png  1024x1024  base image -> feed to `cargo tauri icon` for the bundle icon set
  tray-idle.png   44x44      menu-bar template icon, idle (ring)            [include_image! at build time]
  tray-rec.png    44x44      menu-bar template icon, recording (filled dot) [blinks against idle]

Run:
  python3 app/icons/generate-icons.py
  cargo tauri icon app/icons/app-source.png -o app/icons   # generates icon.icns / icon.png / *.png set
"""

import math
import os
import struct
import zlib


def write_png(path, w, h, rgba):
    def chunk(typ, data):
        return (
            struct.pack(">I", len(data))
            + typ
            + data
            + struct.pack(">I", zlib.crc32(typ + data) & 0xFFFFFFFF)
        )

    raw = bytearray()
    stride = w * 4
    for y in range(h):
        raw.append(0)  # filter byte 0 (None) per scanline
        raw.extend(rgba[y * stride : (y + 1) * stride])
    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n")
        f.write(chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0)))  # 8-bit RGBA
        f.write(chunk(b"IDAT", zlib.compress(bytes(raw), 9)))
        f.write(chunk(b"IEND", b""))


def render(size, ss, shade):
    """shade(nx, ny) -> (r, g, b, a) for normalized coords in [0,1], box-downsampled by `ss` (antialias)."""
    big = size * ss
    acc = [[0, 0, 0, 0] for _ in range(size * size)]
    for by in range(big):
        ny = (by + 0.5) / big
        row = (by // ss) * size
        for bx in range(big):
            nx = (bx + 0.5) / big
            r, g, b, a = shade(nx, ny)
            p = acc[row + bx // ss]
            p[0] += r
            p[1] += g
            p[2] += b
            p[3] += a
    out = bytearray(size * size * 4)
    n = ss * ss
    for idx, p in enumerate(acc):
        o = idx * 4
        out[o] = p[0] // n
        out[o + 1] = p[1] // n
        out[o + 2] = p[2] // n
        out[o + 3] = p[3] // n
    return out


def in_rounded_rect(nx, ny, margin, radius):
    """Signed-distance test for a rounded rect centered at (0.5,0.5) with the given margin + corner radius."""
    hx = 0.5 - margin
    hy = 0.5 - margin
    qx = abs(nx - 0.5) - (hx - radius)
    qy = abs(ny - 0.5) - (hy - radius)
    ax, ay = max(qx, 0.0), max(qy, 0.0)
    dist = math.hypot(ax, ay) + min(max(qx, qy), 0.0) - radius
    return dist <= 0.0


TEAL = (21, 163, 148)


def app_shade(nx, ny):
    if not in_rounded_rect(nx, ny, 0.06, 0.16):
        return (0, 0, 0, 0)
    r = math.hypot(nx - 0.5, ny - 0.5)
    # White ring + center dot (a stylized cochlea/ear) over the teal field.
    if 0.30 <= r <= 0.40 or r <= 0.14:
        return (255, 255, 255, 255)
    return (TEAL[0], TEAL[1], TEAL[2], 255)


def tray_dot(nx, ny):  # recording: filled dot
    r = math.hypot(nx - 0.5, ny - 0.5)
    return (0, 0, 0, 255) if r <= 0.32 else (0, 0, 0, 0)


def tray_ring(nx, ny):  # idle: ring outline
    r = math.hypot(nx - 0.5, ny - 0.5)
    return (0, 0, 0, 255) if 0.30 <= r <= 0.44 else (0, 0, 0, 0)


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    # App icon crisp at 1024 (cargo tauri icon downsamples to the rest with proper filtering).
    write_png(os.path.join(here, "app-source.png"), 1024, 1024, render(1024, 1, app_shade))
    # Tray icons supersampled (smooth small edges) — black + alpha = macOS template image.
    write_png(os.path.join(here, "tray-idle.png"), 44, 44, render(44, 4, tray_ring))
    write_png(os.path.join(here, "tray-rec.png"), 44, 44, render(44, 4, tray_dot))
    print("wrote app-source.png, tray-idle.png, tray-rec.png")


if __name__ == "__main__":
    main()
