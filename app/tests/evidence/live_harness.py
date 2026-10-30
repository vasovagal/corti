"""Actual live algorithms; only native CaptureChunk transport and host/UI constructors are absent.

The transport DTO/constant are copied verbatim from coreaudio, not implemented here. All echo,
boundary, window, writer, persistence, quality, and terminal functions are production source.
"""
from pathlib import Path


def live_source(runner):
    s = runner.source("live")
    item = runner.item
    parts = ["mod live { use std::{collections::VecDeque, path::PathBuf, sync::{Arc,mpsc::{Sender,Receiver}}};\n"
             "use anyhow::{Context,Result}; use tracing::{info,warn}; use corti_aec::StreamingAec;\n"
             "use corti_core::{DiarizedTranscript,MicTapMismatches as LiveMismatches,RecordingMeta,Speaker,TranscriptSegment};\n"
             "use corti_transcribe::segment::*; use corti_transcribe::decode::{DecodeQualityTracker,DecodeReport,InputSpan};\n"
             "use crate::{config::AppConfig,settings::SharedConfig,pipeline::PipelineMsg}; use std::thread::JoinHandle; pub struct LiveManager;\n"]
    start = s.index("use corti_transcribe::boundary::")
    parts.append(s[start:s.index(";", start)+1])
    capture = (runner.ROOT / "crates/corti-coreaudio/src/capture.rs").read_text()
    start = capture.index("pub const FILTER_FRAMES_PER_CHUNK:")
    parts.append("mod corti_capture {" + item(capture, "pub struct CaptureChunk") + capture[start:capture.index(";", start)+1] + "}\nuse corti_capture::CaptureChunk;\n")
    for name in ("MAX_DIARIZATION_AUDIO_BYTES", "MAX_BOUNDARY_EXTRA_AUDIO_BYTES", "MAX_BUFFERED_TRANSCRIPT_BYTES", "TEXT_CAP_HEADROOM", "MAX_WINDOW_AEC_BLOCKS", "MAX_FINAL_TRANSCRIPT_BYTES", "LIVE_AEC_LOOKAHEAD_SECONDS", "LIVE_HOLD_MAX_REGION_SECONDS", "LIVE_HOLD_MAX_CONTENT_TOKENS", "MAX_HELD_MIC_REGIONS", "MAX_LIVE_ECHO_SOURCES"):
        start = s.index("const " + name)
        parts.append(s[start:s.index(";", start)+1])
    parts.append(s[s.index("fn new_live_aec("):s.index("/// Owns live-session lifetimes.")])
    parts.append(s[s.index("struct RawEchoSource {"):s.index("// ----- Small seams")])
    for marker in ("struct DiscardReporter", "struct DiscardWork", "fn join_live_thread(", "fn reap_discard_work("):
        parts.append(item(s, marker))
    for marker in ("fn consume_with_panic_boundary(", "fn await_capture_verdict<", "trait TranscriptPublisher", "trait LiveChannel", "trait LiveDiarizer", "trait NoteFiler", "struct NoteWriter", "impl<F: NoteFiler> NoteWriter"):
        parts.append(item(s, marker))
    parts.append("#[cfg(test)] pub(crate) mod tests { use super::*; use corti_core::OwningApp; use std::cell::RefCell; use std::sync::mpsc::sync_channel;\n")
    for marker in ("fn word(", "fn meta(", "fn fixture_epoch(", "struct Scripted {", "impl Scripted {", "impl LiveChannel for Scripted", "impl LiveDiarizer for NoDiarizer", "struct ScriptedDiarizer", "impl LiveDiarizer for ScriptedDiarizer", "struct TempFiler", "impl TempFiler", "impl NoteFiler for TempFiler", "fn block("):
        parts.append(item(s, marker))
    parts.append("struct NoDiarizer; struct NoopPublisher; impl TranscriptPublisher for NoopPublisher { fn words(&self,_:Speaker,_:&[Word]) {} }\n")
    parts.append("#[derive(Default)] struct RecordingPublisher(RefCell<Vec<(String,Vec<Word>)>>);\n" + item(s, "impl TranscriptPublisher for RecordingPublisher"))
    for name in ("live_far_poll_protection_survives_pause_split_units", "live_mic_poll_protection_survives_pause_split_units", "live_raw_protection_survives_later_backchannel_and_filler_passes", "activity_live_unknown_authority_keeps_empty_asr_note_free"):
        parts.append("#[test]\n" + item(s, "fn " + name + "("))
    parts.append((Path(__file__).parent / "live.rs").read_text())
    parts.append((Path(__file__).parent / "live_optout.rs").read_text())
    parts.append("}\n}\n")
    value = "".join(parts)
    # Crash scheduling only, after the unchanged actual note rewrite has durably returned.
    rewritten = '            .context("rewriting the live note before final state publication")?;\n        self.provenance = provenance;'
    if value.count(rewritten) != 1:
        raise ValueError("expected one actual successful live rewrite seam")
    return value.replace(rewritten, '            .context("rewriting the live note before final state publication")?;\n        crate::evidence_scheduling::rewrite_complete();\n        self.provenance = provenance;', 1)
