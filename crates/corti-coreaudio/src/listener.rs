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

use anyhow::{Result, anyhow, bail};
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

/// The current default output device, or an error if there is none. The starting point for
/// [`tap_only_clock_device`], which the tap-only (no-mic) path uses as its aggregate clock anchor.
pub fn default_output_device() -> Result<ca::AudioObjectID> {
    let addr = property::global(ca::kAudioHardwarePropertyDefaultOutputDevice);
    let id: ca::AudioObjectID = unsafe { property::get(ca::kAudioObjectSystemObject, &addr)? };
    if id == 0 {
        bail!("no default output device");
    }
    Ok(id)
}

/// The clock anchor for a tap-only (no-mic) aggregate: an **output** device that exposes **no input
/// streams**. Clocking the aggregate off such a device gives it a tick without ever opening an input — so
/// no orange "mic in use" dot, no microphone TCC prompt, and no stray input channels folded into the tap.
///
/// The default output device is preferred when it is itself input-less (built-in speakers). When the default
/// output *also* has inputs — AirPods, USB headsets/interfaces, BlackHole/Loopback are one device with both
/// scopes — using it as the clock would pull its mic into the capture and may light the mic indicator (issue
/// #4). In that case we fall back to another output-only device, preferring the built-in output.
///
/// Errors if no input-less output device exists at all, rather than silently clocking off an input-capable
/// device: failing the capture is better than breaking the no-mic guarantee.
pub fn tap_only_clock_device() -> Result<ca::AudioObjectID> {
    let default_output = default_output_device()?;
    // Treat an unreadable input scope as "has input" so we never risk an input-capable clock.
    if !has_streams(default_output, ca::kAudioObjectPropertyScopeInput).unwrap_or(true) {
        return Ok(default_output);
    }

    // Default output has inputs; find an output-only device instead, preferring the built-in output.
    let mut fallback: Option<ca::AudioObjectID> = None;
    for device in all_devices()? {
        if device == default_output {
            continue;
        }
        // Must be an output, must have no input.
        if !has_streams(device, ca::kAudioObjectPropertyScopeOutput).unwrap_or(false) {
            continue;
        }
        if has_streams(device, ca::kAudioObjectPropertyScopeInput).unwrap_or(true) {
            continue;
        }
        if is_built_in(device).unwrap_or(false) {
            return Ok(device); // built-in output is the safest, most stable clock
        }
        fallback.get_or_insert(device);
    }
    fallback.ok_or_else(|| {
        anyhow!("no input-less output device available to clock a tap-only (no-mic) aggregate")
    })
}

/// Whether `device` exposes any streams on `scope` (`kAudioObjectPropertyScopeInput` /
/// `…Output`). An output-only device has zero input streams; AirPods/USB interfaces have both.
fn has_streams(device: ca::AudioObjectID, scope: u32) -> Result<bool> {
    let addr = property::address(
        ca::kAudioDevicePropertyStreams,
        scope,
        ca::kAudioObjectPropertyElementMain,
    );
    let streams: Vec<ca::AudioObjectID> = unsafe { property::get_vec(device, &addr)? };
    Ok(!streams.is_empty())
}

/// Whether `device`'s transport is the built-in audio (internal speakers/mic), as opposed to USB,
/// Bluetooth, virtual, etc.
fn is_built_in(device: ca::AudioObjectID) -> Result<bool> {
    let addr = property::global(ca::kAudioDevicePropertyTransportType);
    let transport: u32 = unsafe { property::get(device, &addr)? };
    Ok(transport == ca::kAudioDeviceTransportTypeBuiltIn)
}

/// Every audio device known to the system.
fn all_devices() -> Result<Vec<ca::AudioObjectID>> {
    let addr = property::global(ca::kAudioHardwarePropertyDevices);
    unsafe { property::get_vec(ca::kAudioObjectSystemObject, &addr) }
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

struct DeviceState {
    on_change: Box<dyn Fn() + Send>,
}

/// Watches for changes to the system **default input device** and invokes a callback on each change.
///
/// [`MicMonitor`] is bound to one device id, so when the user switches input mid-session (AirPods
/// connect, headset unplugged) it stops seeing transitions on the device that's now active. `corti-detect`
/// uses this monitor to drop and recreate its `MicMonitor` on the new default device.
///
/// Dropping it removes the HAL listener and frees the callback.
pub struct DefaultInputDeviceMonitor {
    addr: ca::AudioObjectPropertyAddress,
    state: *mut DeviceState,
}

// As with `MicMonitor`, the raw `*mut DeviceState` makes this `!Send`; the boxed closure is `Send` and we
// own the box for the monitor's whole lifetime, removing the listener before freeing it, so this is sound.
unsafe impl Send for DefaultInputDeviceMonitor {}

impl DefaultInputDeviceMonitor {
    /// Start watching for default-input-device changes. `on_change()` fires whenever the system default
    /// input device changes.
    pub fn new(on_change: impl Fn() + Send + 'static) -> Result<Self> {
        let addr = property::global(ca::kAudioHardwarePropertyDefaultInputDevice);
        let state = Box::into_raw(Box::new(DeviceState {
            on_change: Box::new(on_change),
        }));
        let st = unsafe {
            ca::AudioObjectAddPropertyListener(
                ca::kAudioObjectSystemObject,
                &addr,
                Some(device_trampoline),
                state as *mut c_void,
            )
        };
        if st != 0 {
            drop(unsafe { Box::from_raw(state) });
            bail!("AudioObjectAddPropertyListener(default input device) failed: OSStatus {st}");
        }
        Ok(Self { addr, state })
    }
}

impl Drop for DefaultInputDeviceMonitor {
    fn drop(&mut self) {
        unsafe {
            ca::AudioObjectRemovePropertyListener(
                ca::kAudioObjectSystemObject,
                &self.addr,
                Some(device_trampoline),
                self.state as *mut c_void,
            );
            // No callbacks can arrive after the remove above returns, so freeing is safe.
            drop(Box::from_raw(self.state));
        }
    }
}

/// HAL listener proc for default-device changes: forward to the closure (the value is re-read by the
/// caller after it rebinds, so the proc itself carries no payload).
unsafe extern "C" fn device_trampoline(
    _obj: ca::AudioObjectID,
    _n: u32,
    _addrs: *const ca::AudioObjectPropertyAddress,
    client: *mut c_void,
) -> ca::OSStatus {
    if client.is_null() {
        return 0;
    }
    let state = unsafe { &*(client as *const DeviceState) };
    (state.on_change)();
    0
}
