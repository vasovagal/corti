//! Live manual check of the detector: starts a [`corti_detect::Detector`] and prints every
//! [`corti_detect::DetectorEvent`].
//!
//! ```sh
//! cargo run -p corti-detect --example watch
//! ```
//!
//! Join a Slack huddle / Zoom call and you should see `RecordingStarted` with the attributed app after
//! ~1.5 s (the debounce); leave and you'll see `RecordingFinished`. Toggle the mic quickly to watch the
//! chirp get debounced, and reconnect within ~2 s to watch a gap get coalesced.
//!
//! Run from a bare terminal, `finish()` will emit an `Error` instead: capture needs the macOS
//! **audio-capture** TCC permission, granted only to a signed `.app` launched via LaunchServices (see
//! `design/LESSONS.md` §1). Detection, debounce, and attribution are observable regardless; the full
//! capture path is validated through the Tauri bundle (`design/05-app-tauri.md`).

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use std::sync::mpsc;

    use corti_detect::{Detector, DetectorEvent};

    let (tx, rx) = mpsc::channel();
    // Hold the detector for the whole run; dropping it stops the worker and removes the HAL listeners.
    let _detector = Detector::start(move |event| {
        let _ = tx.send(event);
    })?;

    println!("watching the mic — join/leave a call; Ctrl-C to quit");
    for event in rx {
        match event {
            DetectorEvent::RecordingStarted { meta } => {
                println!(
                    "▶ started: {} → {}",
                    meta.owning_app.name,
                    meta.audio_path.display()
                );
            }
            DetectorEvent::RecordingFinished { meta, audio_path } => {
                let ended = meta
                    .ended_at
                    .map(|t| t.format("%H:%M:%S").to_string())
                    .unwrap_or_default();
                println!(
                    "⏹ finished: {} ({}–{}) → {}",
                    meta.owning_app.name,
                    meta.started_at.format("%H:%M:%S"),
                    ended,
                    audio_path.display()
                );
            }
            DetectorEvent::RecordingDiscarded { meta } => {
                println!("✗ discarded (too short): {}", meta.owning_app.name);
            }
            DetectorEvent::Error(e) => println!("⚠ error: {e}"),
        }
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("corti-detect is macOS-only");
}
