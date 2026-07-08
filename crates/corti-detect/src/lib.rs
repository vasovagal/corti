//! Mic-in-use start/stop + attribution **state machine** for corti.
//!
//! Watches the mic-in-use signal (`corti_coreaudio::MicMonitor`), debounces transitions, attributes the
//! owning app + PID (`corti_coreaudio::mic_owner`), and drives a `corti_capture::Recorder`, emitting a
//! [`DetectorEvent`] when a recording starts or finishes. See `design/01-corti-detect.md` for the full
//! design (state machine, debounce/coalesce, thread model).
//!
//! The timing logic lives in the platform-independent [`machine`] module (unit-tested without any HAL);
//! the macOS [`Detector`] wires it to the CoreAudio listeners on a worker thread. The HAL callback runs
//! on a CoreAudio thread (guardrail 9), so the worker — not the callback — owns the `Recorder` and calls
//! `on_event`.

use std::path::PathBuf;
use std::time::Duration;

use corti_core::RecordingMeta;

mod machine;

#[cfg(target_os = "macos")]
mod platform;

#[cfg(target_os = "macos")]
pub use platform::{Detector, LiveHook};

/// Debounce window: a rising edge must persist this long before a recording starts (drops notification
/// chirps and brief device reacquisitions).
pub const DEBOUNCE: Duration = Duration::from_millis(1500);
/// Falling-edge confirm window: a mic-off must persist this long before a recording stops. It doubles as
/// the gap-coalesce window — a mic blip shorter than this is absorbed into the ongoing recording, so one
/// recording spans a brief mid-call drop. (There is no separate falling-edge debounce.)
pub const COALESCE: Duration = Duration::from_secs(2);
/// Recordings whose mic-open span is shorter than this are discarded as accidental blips.
pub const MIN_RECORDING: Duration = Duration::from_secs(3);
/// While recording, corti's own capture aggregate pins the device-level mic-in-use signal true, so the
/// falling edge (call ended) can't be observed via the HAL listener. The worker instead polls process
/// attribution at this interval to notice when every other app has released the mic. (See
/// `design/LESSONS.md` trap 6.)
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Events emitted by the [`Detector`] as the mic comes and goes.
#[derive(Debug, Clone)]
pub enum DetectorEvent {
    /// A recording just started; `meta.ended_at` is `None`.
    RecordingStarted { meta: RecordingMeta },
    /// A recording finished and its 2-track WAV is on disk at `audio_path`; `meta.ended_at` is set.
    RecordingFinished {
        meta: RecordingMeta,
        audio_path: PathBuf,
    },
    /// A started recording was discarded below the keep threshold (too short): no WAV was kept. Lets a
    /// consumer that reacted to `RecordingStarted` unwind its in-flight state instead of sitting on it.
    RecordingDiscarded { meta: RecordingMeta },
    /// A non-fatal error (capture failed to start/finish, or a device rebind failed). The detector keeps
    /// running.
    Error(String),
}
