//! Synchronized capture engine: a CoreAudio **process tap** (all system output, or one app's output)
//! combined with the **microphone** in a single **aggregate device**, pulled through one IO proc so both
//! ends share a clock. [`CaptureSession::start_tap_only`] is the listen-only variant — the tap drives the
//! clock by itself and the mic is never opened (no orange "mic in use" dot), used for webinars and talks.
//!
//! This is the reusable core proven by the spike (ADR 0002). The two pieces CoreAudio doesn't expose
//! through `coreaudio-sys` — `AudioHardwareCreateProcessTap`/`Destroy` and the `CATapDescription`
//! Objective-C class — are declared here directly.
//!
//! A [`CaptureSession`] starts capturing on construction and yields the buffered audio on [`stop`]. The
//! captured stream is interleaved with the **mic channel(s) first, then the tap channel(s)** — the
//! "me" / "them" split that downstream code (and offline echo cancellation) relies on.
//!
//! [`stop`]: CaptureSession::stop

use std::ffi::CStr;
use std::os::raw::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use core_foundation::array::CFArray;
use core_foundation::base::TCFType;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use coreaudio_sys as ca;
use objc2::msg_send;
use objc2::runtime::{AnyClass, AnyObject};
use objc2_foundation::{NSArray, NSNumber};

use crate::listener;
use crate::property;

// CoreAudio process-tap API (macOS 14.2+), absent from coreaudio-sys.
#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioHardwareCreateProcessTap(
        in_description: *mut c_void, // CATapDescription*
        out_tap_id: *mut ca::AudioObjectID,
    ) -> ca::OSStatus;
    fn AudioHardwareDestroyProcessTap(in_tap_id: ca::AudioObjectID) -> ca::OSStatus;
}

/// What system audio to tap alongside the mic.
#[derive(Debug, Clone)]
pub enum TapTarget {
    /// All system output (every app). Noisier — includes music, notifications, etc.
    Global,
    /// Only the given process's output (the meeting app). Falls back to [`TapTarget::Global`] at
    /// construction if the PID can't be translated to a Core Audio process object.
    Process(i32),
}

/// Audio captured by a [`CaptureSession`]: interleaved float frames, mic channel(s) first then tap.
#[derive(Debug, Clone)]
pub struct CapturedAudio {
    /// Interleaved samples: `[mic_0, …, mic_(m-1), tap_0, …, tap_(t-1)]` per frame.
    pub samples: Vec<f32>,
    /// Number of leading mic channels per frame.
    pub mic_channels: u32,
    /// Number of trailing tap (system-audio) channels per frame.
    pub tap_channels: u32,
    pub sample_rate: u32,
    /// How many times the IO proc fired (0 ⇒ no audio delivered; see SPIKE.md on TCC).
    pub callbacks: u32,
}

impl CapturedAudio {
    /// Total channels per frame.
    pub fn channels(&self) -> u32 {
        self.mic_channels + self.tap_channels
    }
    /// Frame count (samples / channels), 0 if no channels.
    pub fn frames(&self) -> usize {
        let ch = self.channels() as usize;
        if ch == 0 { 0 } else { self.samples.len() / ch }
    }
    /// The mic channels mixed down to mono (averaged).
    pub fn mic_mono(&self) -> Vec<f32> {
        self.downmix(0, self.mic_channels as usize)
    }
    /// The tap channels mixed down to mono (averaged) — the far-end "them" track.
    pub fn tap_mono(&self) -> Vec<f32> {
        self.downmix(self.mic_channels as usize, self.tap_channels as usize)
    }
    fn downmix(&self, offset: usize, count: usize) -> Vec<f32> {
        let ch = self.channels() as usize;
        if ch == 0 || count == 0 {
            return Vec::new();
        }
        self.samples
            .chunks_exact(ch)
            .map(|frame| frame[offset..offset + count].iter().sum::<f32>() / count as f32)
            .collect()
    }
}

/// An in-progress capture. Audio accumulates from the moment this is constructed until [`stop`].
///
/// [`stop`]: CaptureSession::stop
pub struct CaptureSession {
    agg_id: ca::AudioObjectID,
    tap_id: ca::AudioObjectID,
    proc_id: ca::AudioDeviceIOProcID,
    state: *mut Cap,
    sample_rate: u32,
}

// The raw pointers make this !Send by default; the session owns them for its whole lifetime and tears the
// IO proc down before freeing, so moving the handle between threads is sound.
unsafe impl Send for CaptureSession {}

impl CaptureSession {
    /// Begin capturing `target` + the default mic. Returns once IO has started.
    pub fn start(target: TapTarget) -> Result<Self> {
        Self::start_inner(target, true)
    }

    /// Begin capturing `target` **only** — the microphone is never opened. The aggregate wraps just the
    /// process tap, which drives the clock, so there is no orange "mic in use" dot and no microphone TCC
    /// prompt. The captured stream is all tap channels (`mic_channels == 0`).
    pub fn start_tap_only(target: TapTarget) -> Result<Self> {
        Self::start_inner(target, false)
    }

    /// Shared start path. `capture_mic` adds the default input device as the aggregate's clock-leading
    /// main sub-device; when false the mic device is never touched (tap-only).
    fn start_inner(target: TapTarget, capture_mic: bool) -> Result<Self> {
        // 1. Tap.
        let description = unsafe { make_tap_description(&target)? };
        let mut tap_id: ca::AudioObjectID = 0;
        let st = unsafe { AudioHardwareCreateProcessTap(description, &mut tap_id) };
        unsafe { release(description) };
        if st != 0 {
            bail!("AudioHardwareCreateProcessTap failed: OSStatus {st}");
        }
        let tap_guard = TapGuard(tap_id);

        // 2. UIDs for the aggregate. The mic device is opened (and read) only in mic+tap mode.
        let tap_uid = read_cfstring(tap_id, ca::kAudioTapPropertyUID)
            .context("reading tap UID")?
            .ok_or_else(|| anyhow!("tap has no UID"))?;
        let mic_uid = if capture_mic {
            let mic = listener::default_input_device()?;
            Some(
                read_cfstring(mic, ca::kAudioDevicePropertyDeviceUID)
                    .context("reading mic UID")?
                    .ok_or_else(|| anyhow!("mic has no UID"))?,
            )
        } else {
            None
        };

        // 3. Aggregate: mic (clock-leading sub-device) + the tap, or tap-only when `capture_mic` is false.
        let agg_id = create_aggregate(mic_uid.as_deref(), &tap_uid)?;
        let agg_guard = AggregateGuard(agg_id);

        let sample_rate = unsafe { aggregate_input_format(agg_id) }
            .map(|f| f.mSampleRate as u32)
            .unwrap_or(48_000);

        // 4. IO proc.
        let state = Box::into_raw(Box::new(Cap {
            samples: Mutex::new(Vec::new()),
            channels: AtomicU32::new(0),
            mic_channels: AtomicU32::new(0),
            callbacks: AtomicU32::new(0),
            has_mic: capture_mic,
        }));
        let mut proc_id: ca::AudioDeviceIOProcID = None;
        let st = unsafe {
            ca::AudioDeviceCreateIOProcID(agg_id, Some(io_proc), state as *mut c_void, &mut proc_id)
        };
        if st != 0 {
            drop(unsafe { Box::from_raw(state) });
            bail!("AudioDeviceCreateIOProcID failed: OSStatus {st}");
        }
        let st = unsafe { ca::AudioDeviceStart(agg_id, proc_id) };
        if st != 0 {
            unsafe { ca::AudioDeviceDestroyIOProcID(agg_id, proc_id) };
            drop(unsafe { Box::from_raw(state) });
            bail!("AudioDeviceStart failed: OSStatus {st}");
        }

        // Ownership transfers to the session; defuse the RAII guards.
        std::mem::forget(tap_guard);
        std::mem::forget(agg_guard);
        Ok(Self {
            agg_id,
            tap_id,
            proc_id,
            state,
            sample_rate,
        })
    }

    /// Stop capturing and return the buffered audio.
    pub fn stop(self) -> CapturedAudio {
        unsafe { ca::AudioDeviceStop(self.agg_id, self.proc_id) };
        unsafe { ca::AudioDeviceDestroyIOProcID(self.agg_id, self.proc_id) };
        // SAFETY: the proc is stopped and destroyed; no callback can touch the state now.
        let cap = unsafe { &*self.state };
        let samples = std::mem::take(&mut *cap.samples.lock().unwrap());
        let total = cap.channels.load(Ordering::Relaxed);
        let mic = cap.mic_channels.load(Ordering::Relaxed).min(total);
        let captured = CapturedAudio {
            samples,
            mic_channels: mic,
            tap_channels: total - mic,
            sample_rate: self.sample_rate,
            callbacks: cap.callbacks.load(Ordering::Relaxed),
        };
        // Tear down the rest. ManuallyDrop-style: do the work Drop would, then forget to avoid a double.
        unsafe {
            drop(Box::from_raw(self.state));
            ca::AudioHardwareDestroyAggregateDevice(self.agg_id);
            AudioHardwareDestroyProcessTap(self.tap_id);
        }
        std::mem::forget(self);
        captured
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        // Only runs if the session is dropped without `stop()` (e.g. an early `?`). `stop()` forgets self.
        unsafe {
            ca::AudioDeviceStop(self.agg_id, self.proc_id);
            ca::AudioDeviceDestroyIOProcID(self.agg_id, self.proc_id);
            drop(Box::from_raw(self.state));
            ca::AudioHardwareDestroyAggregateDevice(self.agg_id);
            AudioHardwareDestroyProcessTap(self.tap_id);
        }
    }
}

/// Shared capture buffer, written by the IO proc.
struct Cap {
    samples: Mutex<Vec<f32>>,
    channels: AtomicU32,
    mic_channels: AtomicU32,
    callbacks: AtomicU32,
    /// Whether the aggregate has a mic sub-device. Set once at construction, read-only in the proc. When
    /// false (tap-only), buffer 0 is the tap — not the mic — so the mic-channel count is 0.
    has_mic: bool,
}

/// IO proc: interleave every input buffer's channels into one frame-major float stream. In mic+tap mode the
/// aggregate presents one buffer per sub-stream (buffer 0 = mic, the rest = tap), so the first buffer's
/// channel count is the mic-channel count. In tap-only mode there is no mic sub-device, so the mic-channel
/// count is 0 and every channel is tap.
unsafe extern "C" fn io_proc(
    _device: ca::AudioObjectID,
    _now: *const ca::AudioTimeStamp,
    input: *const ca::AudioBufferList,
    _input_time: *const ca::AudioTimeStamp,
    _out: *mut ca::AudioBufferList,
    _out_time: *const ca::AudioTimeStamp,
    client: *mut c_void,
) -> ca::OSStatus {
    if input.is_null() || client.is_null() {
        return 0;
    }
    let cap = unsafe { &*(client as *const Cap) };
    cap.callbacks.fetch_add(1, Ordering::Relaxed);
    let list = unsafe { &*input };
    let nbuf = list.mNumberBuffers as usize;
    if nbuf == 0 {
        return 0;
    }
    let buffers = unsafe { std::slice::from_raw_parts(list.mBuffers.as_ptr(), nbuf) };

    let total_channels: u32 = buffers.iter().map(|b| b.mNumberChannels).sum();
    cap.channels.store(total_channels, Ordering::Relaxed);
    cap.mic_channels.store(
        if cap.has_mic {
            buffers[0].mNumberChannels
        } else {
            0
        },
        Ordering::Relaxed,
    );

    let ch0 = buffers[0].mNumberChannels.max(1) as usize;
    let frames = buffers[0].mDataByteSize as usize / (4 * ch0);

    let mut out = match cap.samples.lock() {
        Ok(g) => g,
        Err(_) => return 0,
    };
    for f in 0..frames {
        for b in buffers {
            let ch = b.mNumberChannels as usize;
            let n = b.mDataByteSize as usize / 4;
            let data = b.mData as *const f32;
            for c in 0..ch {
                let idx = f * ch + c;
                let v = if !data.is_null() && idx < n {
                    unsafe { *data.add(idx) }
                } else {
                    0.0
                };
                out.push(v);
            }
        }
    }
    0
}

/// Allocate a `CATapDescription` for the target. Global = a stereo tap of all processes (exclude none);
/// Process = a stereo mixdown of one process's output. Falls back to global if the PID can't be resolved.
///
/// # Safety
/// Returns a +1 object the caller must `release`.
unsafe fn make_tap_description(target: &TapTarget) -> Result<*mut c_void> {
    let cls = AnyClass::get(c"CATapDescription")
        .ok_or_else(|| anyhow!("CATapDescription unavailable (needs macOS 14.2+)"))?;
    let alloc: *mut AnyObject = unsafe { msg_send![cls, alloc] };

    let object_id = match target {
        TapTarget::Process(pid) => translate_pid(*pid).filter(|&o| o != 0),
        TapTarget::Global => None,
    };

    let desc: *mut AnyObject = match object_id {
        Some(obj) => {
            // initStereoMixdownOfProcesses: takes an NSArray<NSNumber> of process AudioObjectIDs.
            let num = NSNumber::new_u32(obj);
            let arr: objc2::rc::Retained<NSArray<NSNumber>> = NSArray::from_retained_slice(&[num]);
            unsafe { msg_send![alloc, initStereoMixdownOfProcesses: &*arr] }
        }
        None => {
            // Empty array = exclude nothing = whole system mix.
            let empty: objc2::rc::Retained<NSArray<AnyObject>> = NSArray::from_retained_slice(&[]);
            unsafe { msg_send![alloc, initStereoGlobalTapButExcludeProcesses: &*empty] }
        }
    };
    if desc.is_null() {
        bail!("CATapDescription init returned nil");
    }
    Ok(desc as *mut c_void)
}

/// Translate a PID to its Core Audio process object, or `None` if the process has no audio object.
fn translate_pid(pid: i32) -> Option<ca::AudioObjectID> {
    let addr = property::global(ca::kAudioHardwarePropertyTranslatePIDToProcessObject);
    let mut obj: ca::AudioObjectID = 0;
    let mut size = std::mem::size_of::<ca::AudioObjectID>() as u32;
    // The translate property takes the PID as qualifier data; pid_t is i32 on macOS.
    let st = unsafe {
        ca::AudioObjectGetPropertyData(
            ca::kAudioObjectSystemObject,
            &addr,
            std::mem::size_of::<i32>() as u32,
            &pid as *const i32 as *const c_void,
            &mut size,
            &mut obj as *mut _ as *mut c_void,
        )
    };
    (st == 0 && obj != 0).then_some(obj)
}

/// `[obj release]`.
unsafe fn release(obj: *mut c_void) {
    if !obj.is_null() {
        let _: () = unsafe { msg_send![obj as *mut AnyObject, release] };
    }
}

/// Read a `CFStringRef`-typed property as a Rust `String`.
fn read_cfstring(obj: ca::AudioObjectID, selector: u32) -> Result<Option<String>> {
    let addr = property::global(selector);
    let cfref: CFStringRef = unsafe { property::get(obj, &addr)? };
    if cfref.is_null() {
        return Ok(None);
    }
    Ok(Some(
        unsafe { CFString::wrap_under_create_rule(cfref) }.to_string(),
    ))
}

/// Hands out a unique suffix per aggregate created in this process.
static AGG_SEQ: AtomicU32 = AtomicU32::new(0);

/// Build the aggregate-device description dict and create the device.
///
/// `mic_uid = Some(uid)` adds the mic as the clock-leading main sub-device (mic + tap). `None` builds a
/// **tap-only** aggregate: only the process tap is in the device, so it is the sole input stream and drives
/// the clock — the mic is never opened (no orange "mic in use" dot, no microphone TCC prompt). The keys that
/// pull in a sub-device (`MainSubDevice`/`SubDeviceList`) are simply omitted in that case.
fn create_aggregate(mic_uid: Option<&str>, tap_uid: &str) -> Result<ca::AudioObjectID> {
    let tap_uid = CFString::new(tap_uid);

    let tap_entry = CFDictionary::from_CFType_pairs(&[
        (key(ca::kAudioSubTapUIDKey).as_CFType(), tap_uid.as_CFType()),
        (
            key(ca::kAudioSubTapDriftCompensationKey).as_CFType(),
            CFNumber::from(1i32).as_CFType(),
        ),
    ]);
    let tap_list = CFArray::from_CFTypes(&[tap_entry]);

    // A fresh UID per aggregate avoids colliding with a sibling (the detector and webinar capture share one
    // process, so a bare PID would not be unique) or a stale aggregate left by a crashed run.
    let uid = CFString::new(&format!(
        "com.vasovagal.corti.agg.{}.{}",
        std::process::id(),
        AGG_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let name = CFString::new("corti-capture");

    let mut pairs = vec![
        (
            key(ca::kAudioAggregateDeviceNameKey).as_CFType(),
            name.as_CFType(),
        ),
        (
            key(ca::kAudioAggregateDeviceUIDKey).as_CFType(),
            uid.as_CFType(),
        ),
        (
            key(ca::kAudioAggregateDeviceIsPrivateKey).as_CFType(),
            CFNumber::from(1i32).as_CFType(),
        ),
        (
            key(ca::kAudioAggregateDeviceTapListKey).as_CFType(),
            tap_list.as_CFType(),
        ),
        (
            key(ca::kAudioAggregateDeviceTapAutoStartKey).as_CFType(),
            CFNumber::from(1i32).as_CFType(),
        ),
    ];

    // Mic+tap mode only: add the mic as the clock-leading main sub-device. (`as_CFType()` retains, so the
    // CFString/CFArray locals may drop at the end of this block — the dict keeps its own references.)
    let mic_uid = mic_uid.map(CFString::new);
    if let Some(mic_uid) = &mic_uid {
        let sub_dev = CFDictionary::from_CFType_pairs(&[(
            key(ca::kAudioSubDeviceUIDKey).as_CFType(),
            mic_uid.as_CFType(),
        )]);
        let sub_list = CFArray::from_CFTypes(&[sub_dev]);
        pairs.push((
            key(ca::kAudioAggregateDeviceMainSubDeviceKey).as_CFType(),
            mic_uid.as_CFType(),
        ));
        pairs.push((
            key(ca::kAudioAggregateDeviceSubDeviceListKey).as_CFType(),
            sub_list.as_CFType(),
        ));
    }

    let desc = CFDictionary::from_CFType_pairs(&pairs);

    let mut agg_id: ca::AudioObjectID = 0;
    let st = unsafe {
        ca::AudioHardwareCreateAggregateDevice(desc.as_concrete_TypeRef() as _, &mut agg_id)
    };
    if st != 0 {
        bail!("AudioHardwareCreateAggregateDevice failed: OSStatus {st}");
    }
    Ok(agg_id)
}

/// The aggregate device's input stream format. A 0 channel count signals the aggregate failed to assemble
/// its inputs — a construction problem, not a permission one.
///
/// # Safety
/// Reads a HAL property; `agg_id` must be a valid aggregate device.
pub(crate) unsafe fn aggregate_input_format(
    agg_id: ca::AudioObjectID,
) -> Result<ca::AudioStreamBasicDescription> {
    let addr = property::address(
        ca::kAudioDevicePropertyStreamFormat,
        ca::kAudioObjectPropertyScopeInput,
        ca::kAudioObjectPropertyElementMain,
    );
    unsafe { property::get(agg_id, &addr) }
}

/// Make a `CFString` from one of coreaudio-sys's `b"key\0"` constants.
fn key(bytes: &[u8]) -> CFString {
    let s = CStr::from_bytes_with_nul(bytes)
        .ok()
        .and_then(|c| c.to_str().ok())
        .unwrap_or_default();
    CFString::new(s)
}

/// RAII: destroy the process tap on drop (used only during `start` error paths).
struct TapGuard(ca::AudioObjectID);
impl Drop for TapGuard {
    fn drop(&mut self) {
        unsafe { AudioHardwareDestroyProcessTap(self.0) };
    }
}

/// RAII: destroy the aggregate device on drop (used only during `start` error paths).
struct AggregateGuard(ca::AudioObjectID);
impl Drop for AggregateGuard {
    fn drop(&mut self) {
        unsafe { ca::AudioHardwareDestroyAggregateDevice(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::CapturedAudio;

    /// A tap-only capture (no mic sub-device) reports `mic_channels == 0`, so the whole interleaved stream
    /// is the tap: `mic_mono()` is empty and `tap_mono()` averages every channel. This is the split
    /// `start_tap_only` + `write_tap_only_wav` rely on.
    #[test]
    fn tap_only_split_treats_all_channels_as_tap() {
        // Two stereo-tap frames, no mic channel: [tapL, tapR] per frame. Values are exact in f32.
        let audio = CapturedAudio {
            samples: vec![0.25, 0.75, 0.5, 1.0],
            mic_channels: 0,
            tap_channels: 2,
            sample_rate: 48_000,
            callbacks: 1,
        };
        assert_eq!(audio.channels(), 2);
        assert_eq!(audio.frames(), 2);
        assert!(
            audio.mic_mono().is_empty(),
            "no mic channel → empty me track"
        );
        assert_eq!(audio.tap_mono(), vec![0.5, 0.75]);
    }

    /// The mic+tap split keeps "me" (leading mic channel) and "them" (trailing tap channels) separate.
    #[test]
    fn mic_plus_tap_split_keeps_me_and_them_separate() {
        // Two frames of [mic, tapL, tapR].
        let audio = CapturedAudio {
            samples: vec![1.0, 0.25, 0.75, 0.0, 0.5, 1.0],
            mic_channels: 1,
            tap_channels: 2,
            sample_rate: 48_000,
            callbacks: 1,
        };
        assert_eq!(audio.mic_mono(), vec![1.0, 0.0]);
        assert_eq!(audio.tap_mono(), vec![0.5, 0.75]);
    }
}
