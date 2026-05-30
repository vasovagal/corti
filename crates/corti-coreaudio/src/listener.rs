//! The mic-in-use trigger.
//!
//! `kAudioDevicePropertyDeviceIsRunningSomewhere` on the default input device is true whenever *any*
//! process has an active input IO on that device. Its rising edge is "a call started"; the falling edge is
//! "the call ended". We register a HAL property listener and re-read the value inside the callback.
//!
//! The callback is delivered on a CoreAudio-managed thread, so the supplied closure must be `Send` and must
//! not block. (Re-binding to a new device when the user switches input — AirPods, etc. — is handled one
//! level up in `corti-detect`, which can drop and recreate the monitor on a default-device change.)

use std::os::raw::c_void;

use anyhow::{Result, bail};
use coreaudio_sys as ca;

use crate::property;

/// The current default input device, or an error if there is none.
pub fn default_input_device() -> Result<ca::AudioObjectID> {
    let addr = property::global(ca::kAudioHardwarePropertyDefaultInputDevice);
    let id: ca::AudioObjectID = unsafe { property::get(ca::kAudioObjectSystemObject, &addr)? };
    if id == 0 {
        bail!("no default input device");
    }
    Ok(id)
}

/// Whether `device` currently has input IO running somewhere on the system.
pub fn is_running(device: ca::AudioObjectID) -> Result<bool> {
    let addr = property::global(ca::kAudioDevicePropertyDeviceIsRunningSomewhere);
    let v: u32 = unsafe { property::get(device, &addr)? };
    Ok(v != 0)
}

struct State {
    on_change: Box<dyn Fn(bool) + Send>,
}

/// Watches the default input device and invokes a callback on every mic-in-use transition.
///
/// Dropping it removes the HAL listener and frees the callback.
pub struct MicMonitor {
    device: ca::AudioObjectID,
    addr: ca::AudioObjectPropertyAddress,
    state: *mut State,
}

// The raw `*mut State` makes this `!Send`; the boxed closure is `Send` and we own the box for the
// monitor's whole lifetime, removing the listener before freeing it, so this is sound.
unsafe impl Send for MicMonitor {}

impl MicMonitor {
    /// Start watching the current default input device. `on_change(true)` fires when the mic starts being
    /// used, `on_change(false)` when it stops.
    pub fn new(on_change: impl Fn(bool) + Send + 'static) -> Result<Self> {
        let device = default_input_device()?;
        let addr = property::global(ca::kAudioDevicePropertyDeviceIsRunningSomewhere);
        let state = Box::into_raw(Box::new(State {
            on_change: Box::new(on_change),
        }));
        let st = unsafe {
            ca::AudioObjectAddPropertyListener(
                device,
                &addr,
                Some(trampoline),
                state as *mut c_void,
            )
        };
        if st != 0 {
            drop(unsafe { Box::from_raw(state) });
            bail!("AudioObjectAddPropertyListener failed: OSStatus {st}");
        }
        Ok(Self {
            device,
            addr,
            state,
        })
    }

    /// The device this monitor is bound to.
    pub fn device(&self) -> ca::AudioObjectID {
        self.device
    }

    /// Read the current mic-in-use state directly.
    pub fn current(&self) -> Result<bool> {
        is_running(self.device)
    }
}

impl Drop for MicMonitor {
    fn drop(&mut self) {
        unsafe {
            ca::AudioObjectRemovePropertyListener(
                self.device,
                &self.addr,
                Some(trampoline),
                self.state as *mut c_void,
            );
            // No callbacks can arrive after the remove above returns, so freeing is safe.
            drop(Box::from_raw(self.state));
        }
    }
}

/// HAL listener proc: re-read the property and forward the boolean to the closure.
unsafe extern "C" fn trampoline(
    obj: ca::AudioObjectID,
    _n: u32,
    _addrs: *const ca::AudioObjectPropertyAddress,
    client: *mut c_void,
) -> ca::OSStatus {
    if client.is_null() {
        return 0;
    }
    let state = unsafe { &*(client as *const State) };
    if let Ok(running) = is_running(obj) {
        (state.on_change)(running);
    }
    0
}
