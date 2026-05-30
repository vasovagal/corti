//! Capture spike — the architecture go/no-go (ADR 0002).
//!
//! Taps all system output + the default mic through one aggregate device and writes a single multichannel
//! float WAV. The mic channel(s) come first, then the tap (system-audio) channel(s).
//!
//! It MUST be launched as a signed `.app` bundle via LaunchServices so it gets its own TCC identity and the
//! audio-capture permission prompt appears — a loose binary run from a shell is silently denied (the
//! terminal becomes the responsible process). Use the bundle script:
//!
//! ```sh
//! ./crates/corti-coreaudio/bundle-spike.sh 15 /tmp/corti-spike.wav
//! ```
//!
//! Diagnostics are written to `<out>.log` (because `open` detaches stdout). Then open the WAV in Audacity:
//! the first channel(s) should be *only your voice*, the later channel(s) *only the system audio*, in sync.

#[cfg(target_os = "macos")]
fn main() {
    let mut args = std::env::args().skip(1);
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(15);
    let out = args
        .next()
        .unwrap_or_else(|| "/tmp/corti-spike.wav".to_string());

    // Errors are already recorded to <out>.log by run_spike; print too (visible if run from a shell).
    if let Err(e) = corti_coreaudio::tap::run_spike(secs, std::path::Path::new(&out)) {
        eprintln!("spike error: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("corti is macOS-only");
}
