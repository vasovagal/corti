//! The capture **spike** binary's logic — the architecture go/no-go (ADR 0002), now a thin driver over the
//! reusable [`crate::capture`] engine.
//!
//! `run_spike` captures all system output (a global tap) plus the default mic for a fixed window and streams
//! a single multichannel 16-bit WAV to disk: the mic channel(s) first, then the tap channel(s). Open it (or
//! split it with the `splitwav` example) to confirm "me" and "them" land on separate channels and stay in
//! sync.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};

use crate::capture::{CaptureSession, OutputLayout, TapTarget};

/// Accumulates diagnostics to stdout AND a sibling `.log` file. The log file is essential because the spike
/// is launched via `open` (LaunchServices) — the launch path that gives the bundle its own TCC identity —
/// and `open` detaches stdout.
struct Log {
    lines: Vec<String>,
    path: PathBuf,
}

impl Log {
    fn new(out: &Path) -> Self {
        Self {
            lines: Vec::new(),
            path: out.with_extension("log"),
        }
    }
    fn line(&mut self, s: impl Into<String>) {
        let s = s.into();
        println!("{s}");
        self.lines.push(s);
    }
    fn flush(&self) {
        let _ = std::fs::write(&self.path, self.lines.join("\n") + "\n");
    }
}

/// Run the capture spike: tap system output + mic for `secs`, write a multichannel float WAV to `out`.
/// Diagnostics go to stdout and to `<out>.log` so an `open`-launched run is still inspectable.
pub fn run_spike(secs: u64, out: &Path) -> Result<PathBuf> {
    let mut log = Log::new(out);
    log.line(format!("corti capture spike — {secs}s → {}", out.display()));
    let res = capture(secs, out, &mut log);
    if let Err(e) = &res {
        log.line(format!("ERROR: {e:#}"));
    }
    log.flush();
    res
}

fn capture(secs: u64, out: &Path, log: &mut Log) -> Result<PathBuf> {
    // The spike keeps every captured channel (AllChannels) so each can be auditioned via `splitwav`; the
    // session streams it straight to disk.
    let session = CaptureSession::start_recording(
        TapTarget::Global,
        out.to_path_buf(),
        OutputLayout::AllChannels,
    )?;
    log.line("capture session started (global tap + mic)");
    std::thread::sleep(Duration::from_secs(secs));
    let handle = session.stop()?;

    log.line(format!(
        "io proc fired {} times; mic {} ch + tap {} ch = {} ch @ {} Hz; {} frames",
        handle.callbacks,
        handle.mic_channels,
        handle.tap_channels,
        handle.channels(),
        handle.sample_rate,
        handle.frames
    ));
    if handle.callbacks == 0 {
        bail!(
            "the IO proc never fired — likely the TCC audio-capture permission: launch the bundle via \
             LaunchServices (`open corti-spike.app --args …`), NOT a loose binary from a shell. See SPIKE.md."
        );
    }
    if handle.frames == 0 {
        bail!("IO proc fired but produced no samples — a format/layout issue");
    }
    if handle.dropped_samples > 0 {
        log.line(format!(
            "WARNING: dropped {} samples (ring overflow — disk too slow)",
            handle.dropped_samples
        ));
    }

    log.line(format!("wrote {}", out.display()));
    Ok(out.to_path_buf())
}
