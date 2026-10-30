#!/usr/bin/env python3
"""Exercise actual app evidence/checkpoint/retention and batch functions without host APIs.

Reuses the decode-quality harness's production-item copier, locked dependency validation,
and explicit Tauri/hosted/native constructor boundaries. No alternate persistence exists here.
"""
import importlib.util
import hashlib
import json
import os
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("decode_quality_runner", HERE.parent / "decode_quality" / "run.py")
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)
original_source = runner.harness_source
original_manifest = runner.manifest
live_spec = importlib.util.spec_from_file_location("evidence_live_harness", HERE / "live_harness.py")
live_runner = importlib.util.module_from_spec(live_spec)
live_spec.loader.exec_module(live_runner)
receipt_spec = importlib.util.spec_from_file_location("evidence_receipt", HERE / "verify_receipt.py")
receipt = importlib.util.module_from_spec(receipt_spec)
receipt_spec.loader.exec_module(receipt)
ALLOCATIONS = "--allocations" in sys.argv
if ALLOCATIONS:
    sys.argv.remove("--allocations")


def harness_source():
    source = original_source()
    # Hosted service/transport remains a declared boundary. These scripted outcomes exercise the
    # actual live application's acceptance fence and durable selection without a provider call.
    source = source.replace("pub struct HostedHandle;", "pub struct HostedHandle;\n" + (HERE / "hosted_boundary.rs").read_text(), 1)
    source = source.replace("master_enabled: false,\n                    final_lane: Lane { enabled: false },", "master_enabled: evidence_hosted_enabled(),\n                    final_lane: Lane { enabled: evidence_hosted_enabled() },", 1)
    old = runner.item(source, "pub fn finalize(&self,")
    source = source.replace(old, "pub fn finalize(&self, _: &str, transcript: DiarizedTranscript, _: bool) -> SettledFinalTranscript { evidence_hosted_finalize(transcript) }\n", 1)
    old = runner.item(source, "pub fn mark_final_applied(&self,")
    source = source.replace(old, "pub fn mark_final_applied(&self, _: &[CallId]) -> Result<(), &str> { if EVIDENCE_HOSTED_MODE.get() == 2 { Err(\"synthetic application fence\") } else { Ok(()) } }\n", 1)
    source = source.replace(runner.item(source, "mod live {"), live_runner.live_source(runner), 1)
    source = source.replace("mod pipeline {\n", "mod pipeline {\npub enum PipelineMsg { LiveNoteCreated { meta: corti_core::RecordingMeta, note_path: std::path::PathBuf }, LiveDiscardCleanup { meta: corti_core::RecordingMeta, note_path: std::path::PathBuf, error: String } }\n", 1)
    source = source.replace("mod tray {\n", "mod tray {\n    pub fn emit_queue_changed(_: &(), _: &str) {}\n", 1)
    existing_retention = (HERE.parent / "decode_quality/retention.rs").read_text()
    if not existing_retention.rstrip().endswith("}"):
        raise ValueError("expected production-path test module")
    batch_tests = (HERE / "batch.rs").read_text()
    source = source.replace(existing_retention, existing_retention.rstrip()[:-1] + batch_tests + "\n}\n")
    evidence = runner.source("evidence").split("#[cfg(test)]\nmod tests;", 1)[0]
    # Scheduling only: the original store body and all filesystem operations remain unchanged.
    persist_entry = "pub(crate) fn persist(&self, audio: &Path) -> Status {\n        match self.store(audio)"
    if evidence.count(persist_entry) != 1:
        raise ValueError("expected one actual Session publication seam")
    evidence = evidence.replace(persist_entry, "pub(crate) fn persist(&self, audio: &Path) -> Status {\n        crate::evidence_scheduling::persist_entry();\n        match self.store(audio)", 1)
    integration = (runner.ROOT / "app/src/evidence/integration.rs").read_text()
    evidence = evidence.replace("pub(crate) mod integration;", "pub(crate) mod integration {\n" + integration + "\n}")
    live = (runner.ROOT / "app/src/evidence/live.rs").read_text()
    evidence = evidence.replace("pub(crate) mod live;", "pub(crate) mod live {\n" + live + "\n}")
    tests = (runner.ROOT / "app/src/evidence/tests.rs").read_text()
    if ALLOCATIONS:
        tests += (HERE / "allocations.rs").read_text()
        allocator = (runner.ROOT / "crates/corti-transcribe/tests/evidence_allocations.rs").read_text()
        allocator = allocator[allocator.index("struct CountAlloc;"):allocator.index("#[test]")].replace("fn measure<T>", "pub(crate) fn measure<T>")
        source += "mod evidence_allocation { use std::alloc::{GlobalAlloc,Layout,System}; use std::sync::atomic::{AtomicUsize,Ordering}; use std::time::Instant;\n" + allocator + "}\n"
    source += "\nmod evidence {\n" + evidence + "\n#[cfg(test)] mod tests {\n" + tests + "\n}\n}\n"
    source += "\n#[cfg(test)] mod evidence_tracing {\n" + (HERE / "tracing.rs").read_text() + "\n}\n"
    source += "\nmod evidence_scheduling {\n" + (HERE / "scheduling.rs").read_text() + "\n}\n"
    source += "\nmod evidence_export { use anyhow::Result; use chrono::Local;\n" + runner.item(runner.source("cli"), "fn render_note(") + "\npub(crate) fn render(body: &str, provenance: &corti_vagus::provenance::TranscriptProvenance) -> String { render_note(\"Synthetic export\",\"synthetic\",body,provenance,Local::now()).unwrap() } }\n"
    config_test = runner.item(runner.source("config"), "fn legacy_settings_enable_evidence_and_explicit_optout_round_trips(")
    source += "\n#[cfg(test)] mod evidence_config_tests { use crate::config::*; #[test]\n" + config_test + "}\n"
    jobs = runner.source("jobs")
    source += "\nmod evidence_retention { use anyhow::Result; use chrono::{DateTime, Local, Utc}; use std::path::{Path, PathBuf}; use corti_queue::Queue; use tracing::warn; use crate::pipeline::Ctx; const MIN_ROW_RETENTION_DAYS: i64 = 90; const FAILED_JOB_ROW_RETENTION_DAYS: i64 = 30; const RETRY_TRANSCRIPTION: &str = \"retry_transcription\"; const CLEANUP_AWS_STAGING: &str = \"cleanup_aws_staging\";\n"
    for name in ("sweep_expired", "sweep_recordings", "remove_recording_artifacts", "remove_disabled_evidence", "remove_stale_checkpoint_temps", "remove_paths", "enqueue_aws_cleanup", "cleanup_payload"):
        source += runner.item(jobs, "fn " + name + "(")
    source += runner.item(jobs, "pub(crate) fn enqueue_terminal_aws_cleanup(")
    source += "pub(crate) fn run_sweep(ctx: &mut Ctx) -> Result<()> { sweep_expired(ctx) }\npub(crate) fn optout_sweep(queue: &Queue) -> Result<usize> { remove_disabled_evidence(queue) }\n"
    source += (HERE / "retention.rs").read_text() + "\n}\n"
    hashes = {f"app/src/{name}.rs": hashlib.sha256(value.encode()).hexdigest()
              for name, value in runner.SOURCES.items()}
    for path in (HERE / "retention.rs", HERE / "batch.rs", runner.ROOT / "app/src/evidence/tests.rs",
                 Path(__file__), HERE.parent / "decode_quality/run.py",
                 HERE.parent / "decode_quality/boundaries.rs",
                 HERE.parent / "decode_quality/full_path.rs",
                 HERE.parent / "decode_quality/retention.rs",
                 runner.ROOT / "app/src/evidence/integration.rs", runner.ROOT / "app/src/evidence/live.rs",
                 HERE / "live_harness.py", HERE / "live.rs", HERE / "live_optout.rs", HERE / "tracing.rs", HERE / "allocations.rs", HERE / "hosted_boundary.rs", HERE / "verify_receipt.py", HERE / "test_receipt.py", HERE / "scheduling.rs",
                 runner.ROOT / "crates/corti-transcribe/tests/evidence_allocations.rs", runner.ROOT / "crates/corti-coreaudio/src/capture.rs", runner.ROOT / "crates/corti-capture/src/lib.rs"):
        hashes[str(path.relative_to(runner.ROOT))] = hashlib.sha256(path.read_bytes()).hexdigest()
    print("evidence-persistence source sha256: " + json.dumps(hashes, sort_keys=True), flush=True)
    return source


def manifest():
    return original_manifest().replace('serde_json = "1"', 'serde_json = { version = "1", features = ["raw_value"] }') + '\nring = "0.17"\ntoml = "0.8"\ntracing-subscriber = { version = "0.3", default-features = false, features = ["registry", "std"] }\n'


runner.harness_source = harness_source
runner.manifest = manifest
original_run = runner.run


def run(output, cargo_args, clippy):
    # Keep the existing exact source/manifest/registry-lock/build procedure. Capture only its Cargo
    # test output so ignored/missing/extra/duplicate cases cannot turn a partial run into this gate.
    execute = runner.subprocess.run
    def checked(command, **kwargs):
        if command[:2] != ["cargo", "test"]:
            return execute(command, **kwargs)
        kwargs["check"] = False
        # Actual opt-out/retention functions scan the capture root even for an empty queue. Never
        # let a synthetic test inherit the owner's root, including an explicit caller override.
        environment = kwargs.get("env")
        kwargs["env"] = dict(os.environ if environment is None else environment)
        with tempfile.TemporaryDirectory(prefix="corti-evidence-capture-") as capture_root:
            kwargs["env"]["CORTI_RECORDINGS_DIR"] = capture_root
            result = execute(command, **kwargs, capture_output=True, text=True)
        print(result.stdout, end="", flush=True)
        print(result.stderr, end="", file=sys.stderr, flush=True)
        result.check_returncode()
        if cargo_args:
            print("evidence scoped probe: not the complete named suite", flush=True)
        else:
            receipt.require_complete(result.stdout, result.returncode)
            print("evidence complete named suite verified", flush=True)
        return result
    runner.subprocess.run = checked
    try:
        original_run(output, cargo_args, clippy)
    finally:
        runner.subprocess.run = execute


runner.run = run

if __name__ == "__main__":
    runner.main()
