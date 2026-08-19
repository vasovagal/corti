#!/usr/bin/env python3
"""Capture Corti's real frontend and copy images into vasovagal.github.io."""

from __future__ import annotations

import argparse
from pathlib import Path
import shutil
import subprocess

SCREENSHOTS_DIR = Path(__file__).resolve().parent
CORTI_DIR = SCREENSHOTS_DIR.parent
DEFAULT_SITE = CORTI_DIR.parent / "vasovagal.github.io"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--site",
        type=Path,
        default=DEFAULT_SITE,
        help=f"vasovagal.github.io checkout (default: {DEFAULT_SITE})",
    )
    parser.add_argument(
        "--skip-capture",
        action="store_true",
        help="reuse the reviewed screenshots/output files",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    site = args.site.expanduser().resolve()
    destination = site / "assets" / "screenshots"
    if not (site / "index.html").is_file():
        raise RuntimeError(f"{site} is not a vasovagal.github.io checkout")

    if not args.skip_capture:
        subprocess.run(["npm", "run", "capture"], cwd=SCREENSHOTS_DIR, check=True)

    sources = sorted((SCREENSHOTS_DIR / "output").glob("*.png"))
    if not sources:
        raise RuntimeError("no screenshots found; run npm run capture first")

    destination.mkdir(parents=True, exist_ok=True)
    for source in sources:
        shutil.copy2(source, destination / source.name)

    subprocess.run(["git", "diff", "--check"], cwd=site, check=True)
    print(f"Copied {len(sources)} Corti screenshots into {destination}")
    print("Review and commit the Corti and website changes together.")


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        raise SystemExit(f"error: {error}") from error
