//! Mic-in-use start/stop + attribution **state machine** for corti — **shell, not yet implemented**.
//!
//! Watches the mic-in-use signal (`corti_coreaudio::MicMonitor`), debounces transitions, attributes the
//! owning app + PID (`corti_coreaudio::mic_owner`), and drives a `corti_capture::Recorder`, emitting a
//! [`DetectorEvent`] when a recording starts/finishes. Full design (state machine, debounce, thread model)
//! in `design/01-corti-detect.md`.
//!
//! The HAL callback runs on a CoreAudio thread (guardrail 9), so the real implementation must hop to a
//! worker thread before touching capture — never start a recording from the callback.

use std::path::PathBuf;

use corti_core::RecordingMeta;

/// Debounce window: a transition must persist this long to count (drops notification chirps).
pub const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(1500);
/// Recordings shorter than this are discarded as accidental mic blips.
pub const MIN_RECORDING: std::time::Duration = std::time::Duration::from_secs(3);

/// Events emitted by the [`Detector`] as the mic comes and goes.
#[derive(Debug, Clone)]
pub enum DetectorEvent {
    RecordingStarted {
        meta: RecordingMeta,
    },
    RecordingFinished {
        meta: RecordingMeta,
        audio_path: PathBuf,
    },
    Error(String),
}

#[cfg(target_os = "macos")]
pub use platform::Detector;

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use anyhow::Result;

    /// Watches the mic and turns confirmed on/off transitions into recordings. **Shell.**
    pub struct Detector {
        _monitor: corti_coreaudio::MicMonitor,
    }

    impl Detector {
        /// Begin watching. `on_event` is invoked off the HAL callback thread.
        ///
        /// Shell: wires up the `MicMonitor` but does not yet implement the debounce state machine or drive
        /// the `Recorder` — see `design/01-corti-detect.md`.
        pub fn start(_on_event: impl Fn(DetectorEvent) + Send + 'static) -> Result<Self> {
            let monitor = corti_coreaudio::MicMonitor::new(|_running| {
                // TODO(design/01-corti-detect.md): channel → worker → debounce → Recorder start/finish.
            })?;
            Ok(Self { _monitor: monitor })
        }
    }
}
