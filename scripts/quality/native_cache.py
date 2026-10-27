#!/usr/bin/env python3
"""Repair retained Sherpa build outputs whose downloaded static libraries were pruned.

rust-cache keeps Cargo build-script outputs but prunes sherpa-onnx-prebuilt,
which sherpa-onnx-sys 1.13.2 stores outside OUT_DIR. Read the retained linker
obligations rather than duplicating the dependency's native library list.
This repair supports the repository's aarch64-apple-darwin static build only.
"""

import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys


REPO = Path(__file__).resolve().parents[2]
TARGET = "aarch64-apple-darwin"
PACKAGE = "sherpa-onnx-sys"
ARCHIVE = re.compile(r"sherpa-onnx-v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?-osx-arm64-static-lib")


class CacheRepairError(Exception):
    """An unsupported cache layout that must not be removed automatically."""


def checked_path(root, path):
    """Require a lexical descendant with no symlinks between root and path."""
    try:
        relative = path.relative_to(root)
    except ValueError as error:
        raise CacheRepairError("cache path is outside its Cargo directory") from error
    if ".." in relative.parts or root.is_symlink():
        raise CacheRepairError("unsafe Cargo cache path")
    current = root
    for part in relative.parts:
        current /= part
        if current.is_symlink():
            raise CacheRepairError("refusing symlink in Cargo cache path")
    return path


def cargo_directories(repo, env):
    result = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps", "--locked", "--offline"],
        cwd=repo, env=env, check=True, capture_output=True, text=True,
    )
    metadata = json.loads(result.stdout)
    directories = []
    for key in ("target_directory", "build_directory"):
        value = metadata.get(key, metadata.get("target_directory"))
        if not isinstance(value, str) or not Path(value).is_absolute() or ".." in Path(value).parts:
            raise CacheRepairError("Cargo metadata did not provide an absolute cache directory")
        path = Path(value)
        # This action owns only checkout-local build caches, never a developer's
        # external/shared CARGO_TARGET_DIR or a symlink into another checkout.
        checked_path(repo, path)
        if path == repo:
            raise CacheRepairError("Cargo cache directory must be below the checkout")
        directories.append(path)
    return directories


def retained_outputs(build_dir):
    for prefix in (build_dir, build_dir / TARGET):
        for profile in ("debug", "release"):
            directory = checked_path(build_dir, prefix / profile / "build")
            if not directory.is_dir():
                continue
            for package in sorted(directory.glob(PACKAGE + "-*")):
                if not re.fullmatch(PACKAGE + r"-[0-9a-f]{16}", package.name):
                    continue
                output = checked_path(build_dir, package / "output")
                if output.is_file():
                    yield output


def native_obligations(output, target_dir):
    searches = set()
    libraries = set()
    for line in output.read_text(encoding="utf-8").splitlines():
        search = re.fullmatch(r"cargo::?rustc-link-search=native=(.+)", line)
        if search:
            searches.add(search[1])
        library = re.fullmatch(r"cargo::?rustc-link-lib=(static.*)", line)
        if library:
            name = re.fullmatch(r"static=([A-Za-z0-9_][A-Za-z0-9_.+-]*)", library[1])
            if not name:
                raise CacheRepairError("unsupported Sherpa static link directive")
            libraries.add(name[1])
    if not searches and not libraries:
        return None  # For example, the dependency's DOCS_RS output has no linker obligations.
    if len(searches) != 1 or not libraries:
        raise CacheRepairError("unsupported Sherpa static linker output")
    value = next(iter(searches))
    lib_dir = Path(value)
    if not lib_dir.is_absolute() or ".." in lib_dir.parts or "\\" in value:
        raise CacheRepairError("malformed Sherpa native library path")
    native_root = target_dir / "sherpa-onnx-prebuilt"
    try:
        relative = lib_dir.relative_to(native_root)
    except ValueError:
        # An earlier SHERPA_ONNX_LIB_DIR override is not a downloaded cache.
        # Cargo tracks that environment variable and reruns the build when it changes.
        return None
    if len(relative.parts) != 2 or relative.parts[1] != "lib" or not ARCHIVE.fullmatch(relative.parts[0]):
        raise CacheRepairError("unsupported Sherpa native extraction path")
    checked_path(target_dir, lib_dir)
    return lib_dir, libraries


def repair_cache(repo=REPO, env=None):
    repo = Path(repo).resolve()
    env = dict(os.environ if env is None else env)
    if "SHERPA_ONNX_LIB_DIR" in env:
        print("Sherpa native cache: explicit library directory; no repair needed.")
        return 0
    target_dir, build_dir = cargo_directories(repo, env)
    broken = set()
    # Validate the entire plan before deleting anything or invoking cargo clean.
    for output in retained_outputs(build_dir):
        obligations = native_obligations(output, target_dir)
        if obligations is None:
            continue
        lib_dir, libraries = obligations
        for name in libraries:
            library = checked_path(target_dir, lib_dir / f"lib{name}.a")
            if not library.is_file() or library.stat().st_size == 0:
                broken.add(lib_dir.parent)
    if not broken:
        print("Sherpa native cache: no incomplete retained extraction.")
        return 0
    for extraction in sorted(broken):
        checked_path(target_dir, extraction)
        if extraction.exists() and not extraction.is_dir():
            raise CacheRepairError("Sherpa extraction is not a directory")
    for extraction in sorted(broken):
        if extraction.exists():
            # An existing incomplete lib/ makes the dependency skip extraction.
            # Keep the sibling .tar.bz2 for its normal build script to reuse.
            shutil.rmtree(extraction)
        print(f"Sherpa native cache: invalidating incomplete {extraction.name}.", flush=True)
    # Both profiles share the native extraction. Cargo package clean defaults to
    # dev only; it clears this package's host/target build scripts and fingerprints.
    for profile in ("dev", "release"):
        subprocess.run(
            ["cargo", "clean", "--locked", "-p", PACKAGE,
             "--target", TARGET, "--target-dir", str(target_dir), "--profile", profile],
            cwd=repo, env=env, check=True,
        )
    return len(broken)


def main():
    try:
        repair_cache()
    except (CacheRepairError, OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"::error::Sherpa native cache repair failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
