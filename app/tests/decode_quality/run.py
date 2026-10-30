#!/usr/bin/env python3
"""Run the synthetic app fallback contract against this checkout's production functions.

Requires Python 3.11+ and the workspace's already cached Cargo dependencies. No ASR model,
credentials, recording, Tauri application, or network connection is used. The same harness runs on
macOS CI and Linux; boundaries.rs identifies the replaced host/UI boundaries.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib


HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
SOURCES = {}
JOBS_FUNCTIONS = (
    "initial_batch_attempts",
    "original_backend",
    "retry_payload_with_origin",
    "with_settled_quality",
    "merge_durable_settled_quality",
    "merge_historical_quality",
    "complete_with_retained_quality",
)
PIPELINE_ITEMS = (
    "pub(crate) struct Ctx",
    "enum LiveResolution",
    "struct LiveFallback",
    "fn resolve_live(",
    "fn process_live_result(",
    "fn schedule_retry(",
    "fn retry_status(",
    "fn retry_owned_note(",
    "fn live_filed(",
    "pub(crate) fn complete_canonical_note(",
    "fn complete_canonical_note_with_quality(",
    "pub(crate) fn fail_with_note(",
    "fn close_out_note(",
    "struct OwnedNoteError",
    "impl std::fmt::Display for OwnedNoteError",
    "impl std::error::Error for OwnedNoteError",
    "pub(crate) fn recovery_note_from_error(",
    "fn prepare_aws_staging(",
    "pub(crate) fn cleanup_aws_staging(",
    "pub(crate) fn mark_refused_recovery_quality(",
    "fn file_refused_recovery(",
    "pub(crate) fn transcribe_and_file(",
    "fn with_filing_checkpoint<",
    "fn resume_filing_checkpoint(",
    "fn transcribe_without_checkpoint(",
    "fn settle_batch_decode_quality(",
    "pub(crate) fn file_checkpoint(",
    "fn rewrite_checkpoint_note(",
    "fn file_and_done(",
    "fn cleanup_completed_artifacts(",
)


def source(name):
    if name not in SOURCES:
        SOURCES[name] = (ROOT / "app" / "src" / f"{name}.rs").read_text()
    return SOURCES[name]


def item(text, marker):
    """Copy a named Rust item verbatim; missing/ambiguous markers fail the gate."""
    if text.count(marker) != 1:
        raise ValueError(f"expected one source item: {marker}")
    start = text.index(marker)
    end = text.index("{", start) + 1
    depth = 1
    while depth:
        depth += (text[end] == "{") - (text[end] == "}")
        end += 1
    return text[start:end] + "\n"


def production(name):
    return source(name).split("#[cfg(test)]\nmod tests", 1)[0]


def harness_source():
    # These no-backend build warnings are unrelated to the exercised cfg(test) backend. The actual
    # app's native feature build remains a separate gate; no production source is rewritten here.
    parts = ["#![allow(dead_code, irrefutable_let_patterns, unused_variables)]\n"]
    # Copy configuration types/defaults and pure cleanup/AEC projection. User config-file and
    # environment loading are outside this fixture and would require an unused TOML dependency.
    config = source("config")
    parts.append("mod config {\n" + config[:config.index("impl AppConfig {", config.index("fn finite_or("))])
    parts.extend(item(config, f"fn {name}(")
                 for name in ("default_embedding_model", "default_asr_engine", "default_backend"))
    parts.append("}\n")
    for name in ("private_file", "checkpoint", "provenance"):
        parts.append(f"mod {name} {{\n{production(name)}\n}}\n")
    boundaries = (HERE / "boundaries.rs").read_text().replace(
        "// @SETTLED_FINAL_TRANSCRIPT@",
        item(source("postprocess_app"), "pub(crate) struct SettledFinalTranscript"),
    )
    boundaries = boundaries.replace("// @LIVE_OUTCOME@",
                                    "#[derive(Debug)]\n" + item(source("live"), "pub enum LiveOutcome"))
    parts.append(boundaries)
    parts.append("mod transcribe {\n" + production("transcribe"))
    parts.append("#[cfg(test)] mod tests { use super::*; #[test]\n")
    parts.append(item(source("transcribe"),
                      "fn production_recording_path_invokes_scripted_backend_once_and_keeps_partial_output("))
    parts.append("}\n}\n")
    jobs = source("jobs")
    parts.append("mod jobs { use anyhow::Result; use std::path::Path; use corti_queue::Queue; "
                 "use crate::checkpoint::OwnedNote; "
                 'pub const RETRY_TRANSCRIPTION: &str = "retry_transcription";\n')
    start = jobs.index("pub(crate) const RETRY_MAX_ATTEMPTS:")
    parts.append(jobs[start:jobs.index(";", start) + 1] + "\n")
    parts.extend(item(jobs, f"pub(crate) fn {name}(") for name in JOBS_FUNCTIONS)
    parts.append("}\n")
    parts.append("""mod pipeline {
use anyhow::{Context, Result};
use std::{path::{Path, PathBuf}, sync::Arc};
use corti_queue::{Queue, JobUpdate, Job};
use chrono::Utc;
use corti_jobs::Backoff;
use std::time::Duration;
use corti_vagus::Vagus;
use tracing::{error, info, warn};
use crate::{tray, settings::SharedConfig, transcribe::Backend,
    checkpoint::{AwsStaging, FilingCheckpoint, OwnedNote, path_for as checkpoint_path}};
type AppHandle = ();
enum Stage { Transcribing, Filing, Idle }
fn set_stage(_: &AppHandle, _: Stage) {}
""")
    pipeline = source("pipeline")
    start = pipeline.index("const JOB_BACKOFF:")
    parts.append(pipeline[start:pipeline.index("};", start) + 2] + "\n")
    core_imports = [line for line in pipeline.splitlines() if line.startswith("use corti_core::")]
    if len(core_imports) != 1 or not core_imports[0].endswith(";"):
        raise ValueError("expected one complete production pipeline core import")
    parts.append(core_imports[0] + "\n")
    for marker in PIPELINE_ITEMS:
        if marker == "struct OwnedNoteError":
            parts.append("#[derive(Debug)]\n")
        parts.append(item(pipeline, marker))
    parts.append((HERE / "full_path.rs").read_text())
    parts.append((HERE / "retention.rs").read_text())
    parts.append("}\n")
    return "".join(parts)


def manifest():
    value = """[package]
name = "decode163-full-production"
version = "0.0.0"
edition = "2024"
[features]
aws = []
local = []
[dependencies]
anyhow = "1"
chrono = "0.4"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tracing = "0.1"
hound = "3.5"
libc = "0.2"
rusqlite = { version = "0.40", features = ["bundled"] }
"""
    for crate in ("core", "capture", "aec", "transcribe", "lexicon", "vagus",
                  "postprocess", "queue", "jobs"):
        path = (ROOT / "crates" / f"corti-{crate}").as_posix()
        value += f'corti-{crate} = {{ path = "{path}" }}\n'
    return value


def registry_locks(path):
    with path.open("rb") as stream:
        lock = tomllib.load(stream)
    return {(entry["name"], entry["version"], entry["source"], entry.get("checksum"))
            for entry in lock["package"] if "source" in entry}


def run(output, cargo_args, clippy):
    output.mkdir(parents=True, exist_ok=True)
    (output / "src").mkdir(exist_ok=True)
    (output / "src" / "lib.rs").write_text(harness_source())
    (output / "Cargo.toml").write_text(manifest())
    shutil.copyfile(ROOT / "Cargo.lock", output / "Cargo.lock")
    env = dict(os.environ)
    env.setdefault("CARGO_TARGET_DIR", str(ROOT / "target" / "decode-quality-app"))
    target = env.get("CARGO_BUILD_TARGET")
    if not target:
        version = subprocess.check_output(["rustc", "-vV"], env=env, text=True)
        target = next(line.removeprefix("host: ") for line in version.splitlines()
                      if line.startswith("host: "))
    # Cargo adds the temporary root package and prunes unused workspace members. It must retain every
    # registry package identity/checksum from the workspace lock before any test is compiled or run.
    subprocess.run(["cargo", "metadata", "--offline", "--format-version", "1",
                    "--filter-platform", target],
                   cwd=output, env=env, check=True, stdout=subprocess.DEVNULL)
    extra = registry_locks(output / "Cargo.lock") - registry_locks(ROOT / "Cargo.lock")
    if extra:
        raise RuntimeError(f"harness dependency drift from workspace Cargo.lock: {sorted(extra)}")
    hashes = {name: hashlib.sha256(source(name).encode()).hexdigest()
              for name in ("pipeline", "transcribe", "jobs", "config", "private_file",
                           "checkpoint", "provenance", "postprocess_app", "live")}
    print("decode-quality source sha256: " + json.dumps(hashes, sort_keys=True), flush=True)
    if clippy:
        subprocess.run(["cargo", "clippy", "--all-targets", "--locked", "--offline",
                        "--", "-D", "warnings"], cwd=output, env=env, check=True)
    subprocess.run(["cargo", "test", "--lib", "--locked", "--offline", *cargo_args],
                   cwd=output, env=env, check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", type=Path,
                        help="retain generated sources and lock for inspection")
    parser.add_argument("--clippy", action="store_true",
                        help="require strict Clippy checks before the tests")
    parser.add_argument("cargo_args", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    cargo_args = args.cargo_args[1:] if args.cargo_args[:1] == ["--"] else args.cargo_args
    if args.output_dir:
        run(args.output_dir.resolve(), cargo_args, args.clippy)
    else:
        with tempfile.TemporaryDirectory(prefix="corti-decode-quality-") as temporary:
            run(Path(temporary), cargo_args, args.clippy)


if __name__ == "__main__":
    main()
