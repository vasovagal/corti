//! Owned CoreAudio bindings for corti: thin, safe wrappers over the macOS HAL.
//!
//! This crate is the foundation for three things, all of which live in CoreAudio HAL:
//!   - **mic-in-use detection** — `kAudioDevicePropertyDeviceIsRunningSomewhere` listener (the trigger);
//!   - **per-process attribution** — the `kAudioHardwarePropertyProcessObjectList` family;
//!   - **synchronized capture** — `AudioHardwareCreateProcessTap` + an aggregate device.
//!
//! It is Apple-Silicon / latest-macOS only by design (see the workspace README). The modules are added as
//! each milestone lands; the spike binary (`src/bin/spike.rs`) validates the capture bet first.
//!
//! There is intentionally no cross-platform fallback: on non-macOS targets this crate compiles to nothing
//! useful, because corti only ships for `aarch64-apple-darwin`.

#![cfg(target_os = "macos")]

mod property;

pub mod capture;
pub mod listener;
pub mod process;
pub mod tap;

pub use capture::{CaptureSession, CapturedAudio, TapTarget};
pub use listener::{DefaultInputDeviceMonitor, MicMonitor, default_input_device, is_running};
pub use process::{MicOwner, mic_owner, other_app_holds_input};
