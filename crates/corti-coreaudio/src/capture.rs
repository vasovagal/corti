//! Synchronized capture engine: a CoreAudio **process tap** (all system output, or one app's output)
//! combined with the **microphone** in a single **aggregate device**, pulled through one IO proc so both
//! ends share a clock. [`CaptureSession::start_tap_only_recording`] is the listen-only variant — the
//! aggregate is clocked off an input-less **output** device (so the mic is never opened, no orange "mic in
//! use" dot) while the tap supplies the audio, used for webinars and talks.
//!
//! This is the reusable core proven by the spike (ADR 0002). The two pieces CoreAudio doesn't expose
//! through `coreaudio-sys` — `AudioHardwareCreateProcessTap`/`Destroy` and the `CATapDescription`
//! Objective-C class — are declared here directly.
//!
//! **Streaming, bounded-RAM capture (ADR 0005).** The IO proc never accumulates the recording in memory: it
//! pushes interleaved frames into a pre-allocated, wait-free SPSC ring (lock-free, allocation-free — what
//! guardrail 9 requires), and a dedicated writer thread drains the ring and encodes the WAV **incrementally**
//! to disk. Peak RAM is `O(ring capacity)`, independent of call length, so an all-day recording no longer
//! grows without bound. The captured stream is interleaved with the **mic channel(s) first, then the tap
//! channel(s)** — the "me" / "them" split that downstream code (and echo cancellation) relies on.
//!
//! **In-flight mic filtering (#74).** The writer thread can run an optional [`CaptureFilter`] over the
//! downmixed mic/tap blocks before they are encoded, so normal output is already echo-cancelled. A filter
//! failure is explicitly marked and fails open to raw rather than losing the call. The filter lives in `corti-capture` (over
//! `corti_aec::StreamingAec`); this crate only owns the trait and the writer-thread plumbing.

use std::collections::VecDeque;
use std::ffi::CStr;
use std::os::raw::c_void;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::thread::JoinHandle;

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

#[path = "capture_format.rs"]
mod capture_format;
#[path = "capture_transport.rs"]
mod capture_transport;
#[path = "capture_microphone.rs"]
mod ordered_microphone;
#[path = "capture_writer.rs"]
mod ordered_writer;
use capture_format::FormatObserver;
use capture_transport::{Cap, io_proc};
#[cfg(test)]
use corti_core::capture_accounting::CaptureDescriptor;
use corti_core::capture_accounting::HAL_DESCRIPTOR_CAPACITY;

// CoreAudio process-tap API (macOS 14.2+), absent from coreaudio-sys.
#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioHardwareCreateProcessTap(
        in_description: *mut c_void, // CATapDescription*
        out_tap_id: *mut ca::AudioObjectID,
    ) -> ca::OSStatus;
    fn AudioHardwareDestroyProcessTap(in_tap_id: ca::AudioObjectID) -> ca::OSStatus;
}

/// How long the ring buffers before the writer must catch up, and the channel headroom used to size it.
///
/// Default is **30 s** at up to 8 channels (48 kHz) ≈ 46 MB — a deliberately generous safety margin
/// against a transient disk stall (ADR 0007: "ring defaulted bigger/safer"), still bounded regardless
/// of recording length. Override at runtime with `CORTI_CAPTURE_RING_SECS` (an integer count of seconds),
/// clamped to `[RING_SECONDS_MIN, RING_SECONDS_MAX]`. The cap of 300 s bounds worst-case RAM at
/// 300 s × 8 ch × 48 kHz × 4 bytes ≈ 460 MB; the floor of 5 s keeps enough slack to be useful.
/// Unset / empty / unparseable values fall back to the default.
const RING_SECONDS_DEFAULT: usize = 30;
const RING_SECONDS_MIN: usize = 5;
const RING_SECONDS_MAX: usize = 300;
const RING_MAX_CHANNELS: usize = 8;

/// Name of the env var that overrides the ring duration in seconds.
const RING_SECONDS_ENV: &str = "CORTI_CAPTURE_RING_SECS";

/// Frames per [`CaptureTee`] chunk (~85 ms at 48 kHz). Small enough for low live latency, large enough that
/// per-chunk channel + downmix overhead is negligible.
const TEE_FRAMES_PER_CHUNK: usize = 4096;

/// A chunk of **downmixed** capture handed to an optional live consumer: mono mic ("me") and mono tap
/// ("them"), the same number of frames, at the capture sample rate. `mic` is empty for a tap-only capture.
/// Delivered off the writer thread over the bounded [`CaptureTee`] channel.
#[derive(Debug, Clone)]
pub struct CaptureChunk {
    /// Immutable writer mapping for this chunk. Tee drops do not compress this position, and the
    /// writer flushes at epoch boundaries so layout and source-rate changes cannot share a chunk.
    pub epoch: corti_core::CaptureEpoch,
    pub retained_start_frame: u64,
    pub mic: Vec<f32>,
    pub tap: Vec<f32>,
    /// Frame-aligned source unavailability from the ordered capture descriptors. Empty means every
    /// frame is available; otherwise this has exactly `max(mic.len(), tap.len())` entries. The writer
    /// emits at most 4096 frames per chunk, so the bitmap stays bounded independently of call length.
    /// Consumers must reapply it after any stateful filter, including its delayed tail.
    pub unavailable: Vec<bool>,
}

/// A **bounded, lossy** tee of the downmixed capture stream, for live consumers (AEC / transcription).
///
/// The writer thread `try_send`s ~`TEE_FRAMES_PER_CHUNK`-frame chunks and **never blocks**: if the consumer
/// falls behind (channel full) or hangs up, the chunk is dropped and counted — the on-disk WAV is unaffected.
/// The tee is strictly additive; with no tee attached the writer path is byte-identical to before (ADR 0009).
/// Construct from the `SyncSender` half of a `sync_channel` (the caller keeps the `Receiver`); size the
/// channel for the live latency you can tolerate.
pub struct CaptureTee {
    tx: SyncSender<CaptureChunk>,
    dropped: Arc<AtomicU64>,
}

impl CaptureTee {
    /// Wrap a bounded sender as a lossy tee.
    pub fn new(tx: SyncSender<CaptureChunk>) -> Self {
        Self {
            tx,
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    /// A handle to the live-updating dropped-chunk counter. Clone it **before** starting capture to observe
    /// drops mid-recording; the final total is also on [`RecordingHandle::tee_dropped_chunks`].
    pub fn dropped_counter(&self) -> Arc<AtomicU64> {
        self.dropped.clone()
    }
}

/// Frames staged before each [`CaptureFilter::push`] (~85 ms at 48 kHz). Public so a caller can drive the
/// same filter with the same block sizes the writer thread uses (the batch/in-flight parity test does).
pub const FILTER_FRAMES_PER_CHUNK: usize = 4096;

/// A streaming filter applied to the **mic** ("me") track on the writer thread, before encoding (#74).
///
/// The writer hands it downmixed mono mic + mono tap blocks as they come off the ring and writes the
/// cleaned samples it returns paired with the untouched tap, so normal output is clean and peak RAM stays
/// `O(block + filter state + lookahead)` end-to-end. Contract failure is marked and fails open to raw. The implementation lives in `corti-capture`
/// (over `corti_aec::StreamingAec`) — declared as a trait here so the HAL crate carries no DSP dependency
/// and the writer can be tested with a trivial stand-in.
///
/// **Contract.** `push` may return a different number of samples than it was handed (an adaptive filter
/// with a warm-up window emits nothing, then a burst, then ~input-length); only the total over every
/// `push` plus `finish` must equal the total pushed. Both run on the writer thread and must never block:
/// a stall shows up as ring overflow (`RecordingHandle::dropped_samples`), never as backpressure on the
/// real-time IO proc.
pub trait CaptureFilter: Send {
    /// Maximum number of pushed samples the filter may legitimately withhold. The writer enforces this
    /// declaration (and a 35-second hard ceiling) so a buggy implementation cannot turn capture back into
    /// an `O(call length)` buffer.
    fn max_output_lag_samples(&self) -> usize;
    /// Filter one block of mono mic against the mono far-end reference. `mic.len() == far.len()`.
    fn push(&mut self, mic: &[f32], far: &[f32]) -> Vec<f32>;
    /// Flush trailing state at end of stream. Consumes the filter (boxed, so `Self: Sized` isn't needed).
    fn finish(self: Box<Self>) -> Vec<f32>;
}

/// Builds a [`CaptureFilter`] once the capture sample rate is known. Invoked **on the writer thread**
/// after the first callback publishes the channel layout, so filter setup (FFT planning, buffers) never
/// delays `AudioDeviceStart`.
pub type CaptureFilterFactory = Box<dyn FnOnce(u32) -> Box<dyn CaptureFilter> + Send>;

/// Optional extras for a capture session: a bounded live tee and/or an in-flight mic filter. Neither
/// changes the channel layout; `CaptureOptions::default()` is exactly the plain streaming writer.
#[derive(Default)]
pub struct CaptureOptions {
    /// Bounded, lossy tee of the **raw** downmix for live consumers (ADR 0009). Unaffected by `filter`.
    pub tee: Option<CaptureTee>,
    /// In-flight mic filter (#74). Only honored for [`OutputLayout::TwoTrack`] captures that actually
    /// carry a mic channel; ignored (with a warning) otherwise.
    pub filter: Option<CaptureFilterFactory>,
}

impl CaptureOptions {
    /// Attach a bounded, lossy live tee.
    pub fn with_tee(mut self, tee: CaptureTee) -> Self {
        self.tee = Some(tee);
        self
    }

    /// Filter the mic in-flight on the writer thread.
    pub fn with_filter(mut self, filter: CaptureFilterFactory) -> Self {
        self.filter = Some(filter);
        self
    }
}

/// Pure parse+clamp of a raw `CORTI_CAPTURE_RING_SECS` value, factored out so the policy is testable
/// without mutating the process-global environment. Unset / empty / unparseable → default; in-range →
/// as given; out-of-range (including 0) → clamped to `[RING_SECONDS_MIN, RING_SECONDS_MAX]`.
fn clamp_ring_secs(raw: Option<&str>) -> usize {
    match raw.and_then(|s| s.trim().parse::<usize>().ok()) {
        Some(n) => {
            let clamped = n.clamp(RING_SECONDS_MIN, RING_SECONDS_MAX);
            if clamped != n {
                eprintln!(
                    "[corti] {RING_SECONDS_ENV}={n} out of range; clamped to {clamped} \
                     (allowed {RING_SECONDS_MIN}..={RING_SECONDS_MAX})"
                );
            }
            clamped
        }
        None => RING_SECONDS_DEFAULT,
    }
}

/// Resolve the ring duration in seconds, reading the env override once at ring construction.
fn ring_seconds() -> usize {
    clamp_ring_secs(std::env::var(RING_SECONDS_ENV).ok().as_deref())
}

/// Ring capacity in samples for the given rate. Bounded regardless of recording length.
fn ring_capacity(sample_rate: u32) -> usize {
    (sample_rate as usize).max(8_000) * RING_MAX_CHANNELS * ring_seconds()
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

/// The on-disk channel layout the writer thread produces from the interleaved capture stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputLayout {
    /// `ch0` = mic mono ("me"), `ch1` = tap mono ("them"). The 2-track recording.
    TwoTrack,
    /// `ch0` = tap mono ("them") only. The webinar / listen-only recording.
    TapOnlyMono,
    /// Every captured channel, interleaved as-is (diagnostic / spike).
    AllChannels,
}

impl OutputLayout {
    /// Output channel count for a capture carrying `mic` mic + `tap` tap channels.
    fn out_channels(self, mic: u32, tap: u32) -> u32 {
        match self {
            OutputLayout::TwoTrack => 2,
            OutputLayout::TapOnlyMono => 1,
            OutputLayout::AllChannels => (mic + tap).max(1),
        }
    }
}

/// What the optional writer-thread filter did to the retained recording. Higher layers persist this beside
/// the queue row so retries can distinguish legacy/raw input from already-clean audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureFilterDisposition {
    /// No filter applied (not requested or not meaningful for the selected layout).
    NotRequested,
    /// Every retained mic frame came from the filter.
    Applied,
    /// The filter failed before it emitted anything; the retained mic is wholly raw and may be cleaned by a
    /// post-capture fallback without double-processing.
    RawFallback,
    /// The filter violated its contract or failed after emitting; the retained mic may contain a cleaned
    /// prefix and raw remainder, so a second whole-file pass would be unsafe.
    Degraded,
}

#[derive(Debug)]
struct WriterOutcome {
    frames: u64,
    filter: CaptureFilterDisposition,
    quality: corti_core::QualityRecord,
}

/// Metadata about a finished, streamed recording — the samples are already on disk. Replaces the old
/// in-memory `CapturedAudio` as the return of [`CaptureSession::stop`].
#[derive(Debug, Clone)]
pub struct RecordingHandle {
    /// The WAV that was written (may not exist if no audio was ever delivered — see `callbacks`/`frames`).
    pub path: PathBuf,
    /// Output frames written.
    pub frames: u64,
    pub sample_rate: u32,
    /// Leading mic channels in the *captured* stream (0 for tap-only).
    pub mic_channels: u32,
    /// Trailing tap channels in the *captured* stream.
    pub tap_channels: u32,
    /// How many times the IO proc fired (0 ⇒ no audio delivered; see SPIKE.md on TCC).
    pub callbacks: u32,
    /// Samples dropped because the writer couldn't keep up (ring overflow). Should be 0; a non-zero value
    /// means the disk stalled and the recording may have gaps.
    pub dropped_samples: u64,
    /// Source frames rejected by capture, distinct from interleaved sample count.
    pub dropped_frames: u64,
    /// Content-free capture timeline authority, also durable beside the WAV.
    pub quality: corti_core::QualityRecord,
    /// Live-tee chunks dropped because the consumer fell behind (bounded, lossy by design — never affects the
    /// on-disk WAV). 0 when no [`CaptureTee`] was attached.
    pub tee_dropped_chunks: u64,
    /// Whether the writer filter fully applied or fell back. [`CaptureFilterDisposition::RawFallback`] is
    /// safe to recover with a whole-file pass; [`CaptureFilterDisposition::Degraded`] is not.
    pub filter_disposition: CaptureFilterDisposition,
}

impl RecordingHandle {
    /// Total captured channels per frame.
    pub fn channels(&self) -> u32 {
        self.mic_channels + self.tap_channels
    }
}

/// Audio captured by a [`CaptureSession`]: interleaved float frames, mic channel(s) first then tap. Retained
/// as the canonical **me/them channel-split contract** type and exercised by the unit tests; the live
/// pipeline now streams to disk and never materializes this.
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
        self.samples.len().checked_div(ch).unwrap_or(0)
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
            .map(|frame| frame_mean(frame, offset, count))
            .collect()
    }
}

/// Mean of `count` channels of one interleaved frame, starting at `offset`. 0.0 if the range is empty.
fn frame_mean(frame: &[f32], offset: usize, count: usize) -> f32 {
    if count == 0 {
        return 0.0;
    }
    let end = (offset + count).min(frame.len());
    if offset >= end {
        return 0.0;
    }
    let slice = &frame[offset..end];
    slice.iter().sum::<f32>() / slice.len() as f32
}

/// Convert a normalized float sample to 16-bit PCM (lossless for ASR; quantization ~96 dB down).
fn f32_to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

/// An in-progress capture. Audio streams to disk from the moment this is constructed until [`stop`].
///
/// [`stop`]: CaptureSession::stop
pub struct CaptureSession {
    agg_id: ca::AudioObjectID,
    tap_id: ca::AudioObjectID,
    proc_id: ca::AudioDeviceIOProcID,
    state: *mut Cap,
    format_observer: Option<FormatObserver>,
    sample_rate: u32,
    /// The streaming writer thread; joined on `stop`/`Drop` so the file is flushed and finalized.
    writer: Option<JoinHandle<Result<WriterOutcome>>>,
    completion: ordered_writer::CompletionIntent,
    out: PathBuf,
    /// The live-tee dropped-chunk counter, shared with the writer thread; read into [`RecordingHandle`] at
    /// `stop`. `None` when no tee was attached.
    tee_dropped: Option<Arc<AtomicU64>>,
}

// The raw pointers make this !Send by default; the session owns them for its whole lifetime and tears the
// IO proc down before freeing, so moving the handle between threads is sound.
unsafe impl Send for CaptureSession {}

impl CaptureSession {
    /// Begin capturing `target` + the default mic, streaming `layout` to `out`. Returns once IO has started.
    pub fn start_recording(target: TapTarget, out: PathBuf, layout: OutputLayout) -> Result<Self> {
        Self::start_inner(target, true, out, layout, CaptureOptions::default())
    }

    /// Like [`start_recording`], with an optional live tee and/or in-flight mic filter (see
    /// [`CaptureOptions`]).
    ///
    /// [`start_recording`]: CaptureSession::start_recording
    pub fn start_recording_with_options(
        target: TapTarget,
        out: PathBuf,
        layout: OutputLayout,
        options: CaptureOptions,
    ) -> Result<Self> {
        Self::start_inner(target, true, out, layout, options)
    }

    /// Like [`start_recording`], additionally teeing downmixed chunks to a live consumer. The tee is
    /// bounded and lossy — it never blocks the writer and never affects the on-disk WAV (ADR 0009).
    ///
    /// [`start_recording`]: CaptureSession::start_recording
    pub fn start_recording_with_tee(
        target: TapTarget,
        out: PathBuf,
        layout: OutputLayout,
        tee: CaptureTee,
    ) -> Result<Self> {
        Self::start_inner(
            target,
            true,
            out,
            layout,
            CaptureOptions::default().with_tee(tee),
        )
    }

    /// Like [`start_recording`], but **tap-only**: the microphone is never opened (no orange "mic in use"
    /// dot, no microphone TCC prompt) — only the system-audio tap runs. The aggregate is clocked off an
    /// **output** device with no input streams ([`listener::tap_only_clock_device`]). The captured stream is
    /// all tap channels (`mic_channels == 0`).
    ///
    /// [`start_recording`]: CaptureSession::start_recording
    pub fn start_tap_only_recording(
        target: TapTarget,
        out: PathBuf,
        layout: OutputLayout,
    ) -> Result<Self> {
        Self::start_inner(target, false, out, layout, CaptureOptions::default())
    }

    /// Like [`start_tap_only_recording`], with an optional live tee (a tap-only capture has no mic, so an
    /// in-flight mic filter in `options` is ignored).
    ///
    /// [`start_tap_only_recording`]: CaptureSession::start_tap_only_recording
    pub fn start_tap_only_recording_with_options(
        target: TapTarget,
        out: PathBuf,
        layout: OutputLayout,
        options: CaptureOptions,
    ) -> Result<Self> {
        Self::start_inner(target, false, out, layout, options)
    }

    /// Like [`start_tap_only_recording`], additionally teeing downmixed chunks (the `mic` side is empty) to a
    /// live consumer. Bounded, lossy, never affects the WAV (ADR 0009).
    ///
    /// [`start_tap_only_recording`]: CaptureSession::start_tap_only_recording
    pub fn start_tap_only_recording_with_tee(
        target: TapTarget,
        out: PathBuf,
        layout: OutputLayout,
        tee: CaptureTee,
    ) -> Result<Self> {
        Self::start_inner(
            target,
            false,
            out,
            layout,
            CaptureOptions::default().with_tee(tee),
        )
    }

    /// Shared start path. The aggregate's clock-leading main sub-device is the default **input** device when
    /// `capture_mic` is true (mic + tap), or an **input-less output** device when false (tap-only): an output
    /// device supplies the aggregate's clock without opening any input, so the mic stays closed, and choosing
    /// one with no input streams keeps its channels out of the tap (issue #4). A tap with no sub-device at all
    /// has no clock and the IO proc never fires.
    fn start_inner(
        target: TapTarget,
        capture_mic: bool,
        out: PathBuf,
        layout: OutputLayout,
        options: CaptureOptions,
    ) -> Result<Self> {
        let CaptureOptions { tee, filter } = options;
        // 1. Tap.
        let description = unsafe { make_tap_description(&target)? };
        let mut tap_id: ca::AudioObjectID = 0;
        let st = unsafe { AudioHardwareCreateProcessTap(description, &mut tap_id) };
        unsafe { release(description) };
        if st != 0 {
            bail!("AudioHardwareCreateProcessTap failed: OSStatus {st}");
        }
        let tap_guard = TapGuard(tap_id);

        // 2. UIDs for the aggregate. The clock-leading sub-device is the default mic (input) for mic+tap, or
        //    the default output device for tap-only — the latter never opens an input, so no orange dot.
        let tap_uid = read_cfstring(tap_id, ca::kAudioTapPropertyUID)
            .context("reading tap UID")?
            .ok_or_else(|| anyhow!("tap has no UID"))?;
        let clock_device = if capture_mic {
            listener::default_input_device().context("default input device")?
        } else {
            // Tap-only must clock off an output device that has no input streams, or the clock device's own
            // mic leaks into the tap and may light the mic indicator (issue #4). Not simply the default
            // output, which may itself be an input-capable device (AirPods, USB interface).
            listener::tap_only_clock_device().context("tap-only clock device")?
        };
        let clock_uid = read_cfstring(clock_device, ca::kAudioDevicePropertyDeviceUID)
            .context("reading clock-device UID")?
            .ok_or_else(|| anyhow!("clock device has no UID"))?;

        // 3. Aggregate: the clock-leading sub-device + the tap.
        let agg_id = create_aggregate(&clock_uid, &tap_uid)?;
        let agg_guard = AggregateGuard(agg_id);

        let mut format_observer = FormatObserver::new(agg_id)?;
        let sample_rate = format_observer.initial_rate();

        // 4. Shared state + ring + writer thread. The IO proc pushes into `producer`; the writer drains
        //    `consumer` to disk. Layout (channel counts) is discovered on the first callback via `shared`.
        let shared = Arc::new(Shared {
            total_channels: AtomicU32::new(0),
            mic_channels: AtomicU32::new(0),
            callbacks: AtomicU32::new(0),
            dropped: AtomicU64::new(0),
            has_mic: capture_mic,
        });
        let (producer, consumer) = rtrb::RingBuffer::<f32>::new(ring_capacity(sample_rate));
        let (descriptors, descriptor_reader) = rtrb::RingBuffer::new(HAL_DESCRIPTOR_CAPACITY);
        let state = Box::into_raw(Box::new(Cap::new(
            producer,
            descriptors,
            shared,
            format_observer.state(),
            false,
        )?));

        // 5. IO proc.
        let mut proc_id: ca::AudioDeviceIOProcID = None;
        let st = unsafe {
            ca::AudioDeviceCreateIOProcID(agg_id, Some(io_proc), state as *mut c_void, &mut proc_id)
        };
        if st != 0 {
            unsafe { (&*state).disable_capture() };
            if proc_id.is_some() && unsafe { ca::AudioDeviceDestroyIOProcID(agg_id, proc_id) } != 0
            {
                // A failed destroy cannot establish that HAL released the callback pointer.
                std::mem::forget(format_observer);
                std::mem::forget(tap_guard);
                std::mem::forget(agg_guard);
                bail!("capture IO creation teardown failed");
            }
            drop(unsafe { Box::from_raw(state) });
            if format_observer.stop().is_err() {
                std::mem::forget(format_observer);
                std::mem::forget(tap_guard);
                std::mem::forget(agg_guard);
                bail!("capture format listener teardown failed");
            }
            bail!("AudioDeviceCreateIOProcID failed: OSStatus {st}");
        }
        let st = unsafe { ca::AudioDeviceStart(agg_id, proc_id) };
        if st != 0 {
            unsafe { (&*state).disable_capture() };
            unsafe { ca::AudioDeviceStop(agg_id, proc_id) };
            if unsafe { ca::AudioDeviceDestroyIOProcID(agg_id, proc_id) } != 0 {
                std::mem::forget(format_observer);
                std::mem::forget(tap_guard);
                std::mem::forget(agg_guard);
                bail!("capture IO start teardown failed");
            }
            drop(unsafe { Box::from_raw(state) });
            if format_observer.stop().is_err() {
                std::mem::forget(format_observer);
                std::mem::forget(tap_guard);
                std::mem::forget(agg_guard);
                bail!("capture format listener teardown failed");
            }
            bail!("AudioDeviceStart failed: OSStatus {st}");
        }

        // 6. Writer thread. Spawned last — there are no fallible steps after this, so it never needs an
        //    error-path teardown of its own.
        let tee_dropped = tee.as_ref().map(|t| t.dropped.clone());
        let completion = ordered_writer::CompletionIntent::default();
        let writer = {
            let out = out.clone();
            let writer_completion = completion.clone();
            match std::thread::Builder::new()
                .name("corti-capture-writer".into())
                .spawn(move || {
                    ordered_writer::run_writer_with_intent(
                        consumer,
                        descriptor_reader,
                        out,
                        layout,
                        sample_rate,
                        tee,
                        filter,
                        writer_completion,
                    )
                }) {
                Ok(writer) => writer,
                Err(error) => {
                    unsafe { (&*state).disable_capture() };
                    unsafe { ca::AudioDeviceStop(agg_id, proc_id) };
                    if unsafe { ca::AudioDeviceDestroyIOProcID(agg_id, proc_id) } != 0 {
                        std::mem::forget(format_observer);
                        std::mem::forget(tap_guard);
                        std::mem::forget(agg_guard);
                        bail!("capture writer startup teardown failed");
                    }
                    unsafe { drop(Box::from_raw(state)) };
                    if format_observer.stop().is_err() {
                        std::mem::forget(format_observer);
                        std::mem::forget(tap_guard);
                        std::mem::forget(agg_guard);
                        bail!("capture format listener teardown failed");
                    }
                    return Err(error).context("spawning capture writer thread");
                }
            }
        };

        // Ownership transfers to the session; defuse the RAII guards.
        std::mem::forget(tap_guard);
        std::mem::forget(agg_guard);
        Ok(Self {
            agg_id,
            tap_id,
            proc_id,
            state,
            format_observer: Some(format_observer),
            sample_rate,
            writer: Some(writer),
            completion,
            out,
            tee_dropped,
        })
    }

    /// The fixed retained sample rate from the initial valid control-side format observation. Later
    /// source-rate epochs are normalized to this rate off HAL before WAV and tee delivery.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Stop capturing, flush the writer, and return the recording's metadata. Errors if the writer hit an
    /// I/O error (disk full, etc.); the CoreAudio objects are always torn down first.
    pub fn stop(mut self) -> Result<RecordingHandle> {
        unsafe { (&*self.state).disable_capture() };
        let stopped = unsafe { ca::AudioDeviceStop(self.agg_id, self.proc_id) } == 0;
        if unsafe { ca::AudioDeviceDestroyIOProcID(self.agg_id, self.proc_id) } != 0 {
            self.retain_live_io();
            bail!("capture IO destruction failed");
        }
        self.proc_id = None;
        // Successful IO destruction is the ownership barrier. No callback can race terminal
        // accounting, producer closure, listener removal or the final metadata reads below.
        let cap = unsafe { &mut *self.state };
        let terminal = cap.finish();
        let dropped_counts = terminal.as_ref().copied().unwrap_or_default();
        let total = cap.shared.total_channels.load(Ordering::Acquire);
        let mic = cap.shared.mic_channels.load(Ordering::Acquire).min(total);
        let callbacks = cap.shared.callbacks.load(Ordering::Acquire);
        let dropped = cap.shared.dropped.load(Ordering::Acquire);
        let devices = self.release_devices();
        if stopped && terminal.is_ok() && devices.is_ok() {
            self.completion.authorize_complete();
        }
        // Even on a stop/listener/device/terminal failure, IO destruction permits a safe abort:
        // close both producers and join the writer, which retains an in-progress recovery snapshot.
        unsafe { drop(Box::from_raw(self.state)) };
        self.state = std::ptr::null_mut();
        let writer_result = match self.writer.take() {
            Some(h) => h.join(),
            None => Ok(Ok(WriterOutcome {
                frames: 0,
                filter: CaptureFilterDisposition::NotRequested,
                quality: corti_core::QualityRecord::default(),
            })),
        };
        // The writer has joined, so the tee counter is final.
        let tee_dropped_chunks = self
            .tee_dropped
            .as_ref()
            .map(|d| d.load(Ordering::Acquire))
            .unwrap_or(0);

        let out = std::mem::take(&mut self.out);
        let sample_rate = self.sample_rate;
        if !stopped {
            bail!("capture device stop failed");
        }
        if terminal.is_err() {
            bail!("capture terminal accounting failed");
        }
        devices?;
        let writer = writer_result
            .map_err(|_| anyhow!("capture writer thread panicked"))?
            .context("streaming recording to disk")?;
        Ok(RecordingHandle {
            path: out,
            frames: writer.frames,
            sample_rate,
            mic_channels: mic,
            tap_channels: total - mic,
            callbacks,
            dropped_samples: dropped,
            dropped_frames: dropped_counts.frames,
            quality: writer.quality,
            tee_dropped_chunks,
            filter_disposition: writer.filter,
        })
    }

    /// If IO destruction fails, HAL may still own every callback pointer. Retain those allocations
    /// and devices rather than freeing live state. Detach the writer: joining a deliberately retained
    /// producer could wait forever. Its completion intent remains abort and can never be promoted.
    fn retain_live_io(&mut self) {
        self.state = std::ptr::null_mut();
        self.proc_id = None;
        if let Some(observer) = self.format_observer.take() {
            std::mem::forget(observer);
        }
        self.agg_id = 0;
        self.tap_id = 0;
        self.writer.take();
    }

    fn release_devices(&mut self) -> Result<()> {
        if let Some(observer) = self.format_observer.as_mut()
            && observer.stop().is_err()
        {
            // Remaining native listeners can still reference the observer and device. Keep their
            // ownership together; IO has already been destroyed, so the audio writer can abort.
            if let Some(observer) = self.format_observer.take() {
                std::mem::forget(observer);
            }
            self.agg_id = 0;
            self.tap_id = 0;
            bail!("capture format listener teardown failed");
        }
        self.format_observer.take();
        let aggregate = std::mem::take(&mut self.agg_id);
        let tap = std::mem::take(&mut self.tap_id);
        let aggregate_ok =
            aggregate == 0 || unsafe { ca::AudioHardwareDestroyAggregateDevice(aggregate) } == 0;
        let tap_ok = tap == 0 || unsafe { AudioHardwareDestroyProcessTap(tap) } == 0;
        if !aggregate_ok || !tap_ok {
            bail!("capture device teardown failed");
        }
        Ok(())
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        // Drop closes a recoverable prefix but never grants completion intent. A successful stop
        // has already cleared these fields; error paths likewise cannot accidentally retry teardown.
        if !self.state.is_null() {
            unsafe { (&*self.state).disable_capture() };
            unsafe { ca::AudioDeviceStop(self.agg_id, self.proc_id) };
            if unsafe { ca::AudioDeviceDestroyIOProcID(self.agg_id, self.proc_id) } != 0 {
                self.retain_live_io();
                return;
            }
            self.proc_id = None;
            unsafe {
                let _ = (&mut *self.state).finish();
                drop(Box::from_raw(self.state));
                self.state = std::ptr::null_mut();
            }
        }
        if let Some(h) = self.writer.take() {
            let _ = h.join();
        }
        let _ = self.release_devices();
    }
}

/// Ephemeral microphone-only capture for the in-app transcription test. Unlike [`CaptureSession`], this
/// opens the default input device directly: no process tap, aggregate device, system-audio permission, or
/// on-disk WAV. The HAL callback still writes only to the same fixed SPSC ring; a worker downmixes to mono
/// and `try_send`s bounded [`CaptureChunk`]s whose `tap` side is empty.
pub struct MicrophoneCapture {
    device_id: ca::AudioObjectID,
    proc_id: ca::AudioDeviceIOProcID,
    state: *mut Cap,
    format_observer: Option<FormatObserver>,
    sample_rate: u32,
    worker: Option<JoinHandle<Result<ordered_microphone::MicrophoneOutcome>>>,
    tee_dropped: Arc<AtomicU64>,
}

// The session uniquely owns its HAL proc/state and tears them down before freeing, so moving it to the
// test worker thread is sound.
unsafe impl Send for MicrophoneCapture {}

/// Final bounded-capture counters from [`MicrophoneCapture::stop`].
#[derive(Debug, Clone)]
pub struct MicrophoneCaptureHandle {
    pub frames: u64,
    pub sample_rate: u32,
    pub callbacks: u32,
    pub dropped_samples: u64,
    pub dropped_frames: u64,
    pub quality: corti_core::QualityRecord,
    pub tee_dropped_chunks: u64,
}

impl MicrophoneCapture {
    /// Open the default microphone and begin bounded mono tee delivery. Returns after the IO proc and worker
    /// are both running. Failed IO destruction retains callback-owned state instead of freeing live pointers.
    pub fn start(tee: CaptureTee) -> Result<Self> {
        let device_id = listener::default_input_device().context("default input device")?;
        let format_observer = FormatObserver::new(device_id)?;
        let sample_rate = format_observer.initial_rate();
        let shared = Arc::new(Shared {
            total_channels: AtomicU32::new(0),
            mic_channels: AtomicU32::new(0),
            callbacks: AtomicU32::new(0),
            dropped: AtomicU64::new(0),
            has_mic: true,
        });
        let (producer, consumer) = rtrb::RingBuffer::<f32>::new(ring_capacity(sample_rate));
        let (descriptors, descriptor_reader) = rtrb::RingBuffer::new(HAL_DESCRIPTOR_CAPACITY);
        let state = Box::into_raw(Box::new(Cap::new(
            producer,
            descriptors,
            shared,
            format_observer.state(),
            true,
        )?));

        let mut proc_id: ca::AudioDeviceIOProcID = None;
        let status = unsafe {
            ca::AudioDeviceCreateIOProcID(device_id, Some(io_proc), state.cast(), &mut proc_id)
        };
        if status != 0 {
            Self::abort_start(device_id, proc_id, state, format_observer)?;
            bail!("AudioDeviceCreateIOProcID(microphone) failed: OSStatus {status}");
        }
        let status = unsafe { ca::AudioDeviceStart(device_id, proc_id) };
        if status != 0 {
            Self::abort_start(device_id, proc_id, state, format_observer)?;
            bail!("AudioDeviceStart(microphone) failed: OSStatus {status}");
        }

        let tee_dropped = tee.dropped.clone();
        let worker = match std::thread::Builder::new()
            .name("corti-mic-test-capture".into())
            .spawn(move || {
                ordered_microphone::run_microphone_tee(
                    consumer,
                    descriptor_reader,
                    sample_rate,
                    tee,
                )
            }) {
            Ok(worker) => worker,
            Err(error) => {
                Self::abort_start(device_id, proc_id, state, format_observer)?;
                return Err(error).context("spawning microphone capture worker");
            }
        };

        Ok(Self {
            device_id,
            proc_id,
            state,
            format_observer: Some(format_observer),
            sample_rate,
            worker: Some(worker),
            tee_dropped,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Stop the IO proc, drain and close the tee, and return final quality counters.
    pub fn stop(mut self) -> Result<MicrophoneCaptureHandle> {
        unsafe { (&*self.state).disable_capture() };
        let stopped = unsafe { ca::AudioDeviceStop(self.device_id, self.proc_id) } == 0;
        if unsafe { ca::AudioDeviceDestroyIOProcID(self.device_id, self.proc_id) } != 0 {
            self.retain_live_io();
            bail!("microphone IO destruction failed");
        }
        self.proc_id = None;
        let cap = unsafe { &mut *self.state };
        let terminal = cap.finish();
        let dropped_counts = terminal.as_ref().copied().unwrap_or_default();
        let callbacks = cap.shared.callbacks.load(Ordering::Acquire);
        let dropped_samples = cap.shared.dropped.load(Ordering::Acquire);
        let observer = self.stop_observer();
        // The IO destruction barrier permits producer closure even when stop, terminal accounting
        // or listener removal failed. Always finish joining before reporting any such failure.
        unsafe { drop(Box::from_raw(self.state)) };
        self.state = std::ptr::null_mut();
        let outcome = match self.worker.take() {
            Some(worker) => worker.join(),
            None => Ok(Ok(ordered_microphone::MicrophoneOutcome {
                frames: 0,
                quality: corti_core::QualityRecord::default(),
            })),
        };
        let tee_dropped_chunks = self.tee_dropped.load(Ordering::Acquire);
        let sample_rate = self.sample_rate;
        if !stopped {
            bail!("microphone device stop failed");
        }
        if terminal.is_err() {
            bail!("microphone terminal accounting failed");
        }
        observer?;
        let outcome = outcome.map_err(|_| anyhow!("microphone capture worker panicked"))??;
        Ok(MicrophoneCaptureHandle {
            frames: outcome.frames,
            sample_rate,
            callbacks,
            dropped_samples,
            dropped_frames: dropped_counts.frames,
            quality: outcome.quality,
            tee_dropped_chunks,
        })
    }

    /// The default input device is externally owned. Tear down only our proc, callback state and
    /// listeners; never destroy the user's device. All startup error branches share this barrier.
    fn abort_start(
        device_id: ca::AudioObjectID,
        proc_id: ca::AudioDeviceIOProcID,
        state: *mut Cap,
        mut observer: FormatObserver,
    ) -> Result<()> {
        unsafe { (&*state).disable_capture() };
        if proc_id.is_some() {
            unsafe { ca::AudioDeviceStop(device_id, proc_id) };
            if unsafe { ca::AudioDeviceDestroyIOProcID(device_id, proc_id) } != 0 {
                std::mem::forget(observer);
                bail!("microphone IO startup teardown failed");
            }
        }
        unsafe { drop(Box::from_raw(state)) };
        if observer.stop().is_err() {
            std::mem::forget(observer);
            bail!("microphone format listener teardown failed");
        }
        Ok(())
    }

    fn retain_live_io(&mut self) {
        // HAL may still use the raw Cap pointer and its format state. Keep both allocations alive
        // and detach the worker rather than joining a producer that cannot safely be closed yet.
        self.state = std::ptr::null_mut();
        self.proc_id = None;
        if let Some(observer) = self.format_observer.take() {
            std::mem::forget(observer);
        }
        self.worker.take();
    }

    fn stop_observer(&mut self) -> Result<()> {
        if let Some(mut observer) = self.format_observer.take()
            && observer.stop().is_err()
        {
            std::mem::forget(observer);
            bail!("microphone format listener teardown failed");
        }
        Ok(())
    }
}

impl Drop for MicrophoneCapture {
    fn drop(&mut self) {
        if !self.state.is_null() {
            unsafe { (&*self.state).disable_capture() };
            unsafe { ca::AudioDeviceStop(self.device_id, self.proc_id) };
            if unsafe { ca::AudioDeviceDestroyIOProcID(self.device_id, self.proc_id) } != 0 {
                self.retain_live_io();
                return;
            }
            self.proc_id = None;
            unsafe {
                let _ = (&mut *self.state).finish();
                drop(Box::from_raw(self.state));
                self.state = std::ptr::null_mut();
            }
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = self.stop_observer();
    }
}

/// State shared between the IO proc (the single producer) and the writer thread + session (readers).
struct Shared {
    total_channels: AtomicU32,
    mic_channels: AtomicU32,
    callbacks: AtomicU32,
    /// Samples dropped on ring overflow (writer fell behind). Surfaced via [`RecordingHandle`].
    dropped: AtomicU64,
    /// Whether the aggregate has a mic sub-device. Set once at construction, read-only in the proc. When
    /// false (tap-only), buffer 0 is the tap — not the mic — so the mic-channel count is 0.
    has_mic: bool,
}

/// Writer-thread state for the in-flight mic filter (#74): the block being staged for the next
/// [`CaptureFilter::push`], and the far-end ("them") samples waiting for their cleaned mic partner.
///
/// A filter's output lags its input by the warm-up/lookahead window, so each frame has to be queued until
/// its cleaned mic partner arrives. **Those queues are the entire memory cost of running the canceller
/// here**: neither exceeds the filter's lookahead plus one staged block (≈2 MB for the pair at the 5 s
/// default, 48 kHz) and both are independent of call length.
const MAX_FILTER_BACKLOG_SECONDS: usize = 35;

struct FilterStage {
    /// `None` once [`FilterStage::finish`] has consumed it — or once it failed and was dropped.
    filter: Option<Box<dyn CaptureFilter>>,
    mic: Vec<f32>,
    far: Vec<f32>,
    tap_pending: VecDeque<f32>,
    /// Raw mic, queued in lockstep with `tap_pending`. Read only after degradation: the filter's withheld
    /// state dies with it, so this mirror lets those frames be written raw instead of lost.
    mic_pending: VecDeque<f32>,
    /// Availability travels with delayed output; predictor state cannot synthesize a missing frame.
    unavailable_pending: VecDeque<bool>,
    /// Declared filter lag, clamped to a hard 35-second ceiling. Checked after every bounded push.
    max_pending: usize,
    degraded: bool,
    clean_frames: u64,
    /// Clean samples dropped because the filter emitted more than it was fed (a contract violation).
    over_emitted: u64,
}

impl FilterStage {
    fn new(filter: Box<dyn CaptureFilter>, sample_rate: u32) -> Self {
        let declared = filter.max_output_lag_samples();
        let bounded_rate = (sample_rate as usize).clamp(8_000, 192_000);
        let hard = bounded_rate
            .saturating_mul(MAX_FILTER_BACKLOG_SECONDS)
            .max(FILTER_FRAMES_PER_CHUNK);
        let max_pending = declared.min(hard).saturating_add(FILTER_FRAMES_PER_CHUNK);
        if declared > hard {
            eprintln!(
                "corti-capture: capture filter declared {declared} samples of lag; clamped to {hard}"
            );
        }
        Self {
            filter: Some(filter),
            mic: Vec::with_capacity(FILTER_FRAMES_PER_CHUNK),
            far: Vec::with_capacity(FILTER_FRAMES_PER_CHUNK),
            tap_pending: VecDeque::new(),
            mic_pending: VecDeque::new(),
            unavailable_pending: VecDeque::new(),
            max_pending,
            degraded: false,
            clean_frames: 0,
            over_emitted: 0,
        }
    }

    /// Stage one downmixed frame. `them` is both the echo reference and the passthrough ch1 (ADR 0007
    /// Decision 2: only `me` is cleaned).
    fn stage_with_availability(&mut self, me: f32, them: f32, unavailable: bool) {
        self.mic.push(me);
        self.far.push(them);
        self.tap_pending.push_back(them);
        self.mic_pending.push_back(me);
        self.unavailable_pending.push_back(unavailable);
    }

    fn is_full(&self) -> bool {
        self.mic.len() >= FILTER_FRAMES_PER_CHUNK
    }

    /// Cancellation quality is forfeit, the recording is not. Everything still pending and every later
    /// frame is written raw; the final disposition records whether a cleaned prefix already escaped.
    fn degrade(&mut self, reason: &str) {
        self.filter = None;
        if !self.degraded {
            eprintln!(
                "corti-capture: capture filter {reason} — recording pending and later audio uncancelled"
            );
        }
        self.degraded = true;
    }

    /// Write `clean` paired with the queued tap. Stops early if the filter ran ahead of its input; the
    /// surplus is dropped and marks the stream degraded rather than corrupting channel alignment.
    fn emit<W: std::io::Write + std::io::Seek>(
        &mut self,
        writer: &mut hound::WavWriter<W>,
        clean: &[f32],
    ) -> Result<u64, hound::Error> {
        let mut frames = 0u64;
        for &me in clean {
            let Some(them) = self.tap_pending.pop_front() else {
                let dropped = clean.len() as u64 - frames;
                self.over_emitted += dropped;
                break;
            };
            self.mic_pending.pop_front();
            let unavailable = self.unavailable_pending.pop_front().unwrap_or(true);
            writer.write_sample(if unavailable { 0.0 } else { me })?;
            writer.write_sample(if unavailable { 0.0 } else { them })?;
            frames += 1;
        }
        self.clean_frames += frames;
        Ok(frames)
    }

    /// Degraded mode: write the queued frames straight through, mic uncancelled.
    fn drain_raw<W: std::io::Write + std::io::Seek>(
        &mut self,
        writer: &mut hound::WavWriter<W>,
    ) -> Result<u64, hound::Error> {
        let mut frames = 0;
        while let Some(them) = self.tap_pending.pop_front() {
            let me = self.mic_pending.pop_front().unwrap_or(0.0);
            let unavailable = self.unavailable_pending.pop_front().unwrap_or(true);
            writer.write_sample(if unavailable { 0.0 } else { me })?;
            writer.write_sample(if unavailable { 0.0 } else { them })?;
            frames += 1;
        }
        Ok(frames)
    }

    /// Push one bounded staged block. Panics, over-emission, or backlog beyond the declared/hard bound all
    /// fail open to raw without escaping the writer thread or allowing call-length growth.
    fn flush<W: std::io::Write + std::io::Seek>(
        &mut self,
        writer: &mut hound::WavWriter<W>,
    ) -> Result<u64, hound::Error> {
        if self.mic.is_empty() {
            return Ok(0);
        }
        let clean = match self.filter.take() {
            Some(mut filter) => {
                let pushed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    filter.push(&self.mic, &self.far)
                }));
                match pushed {
                    Ok(clean) => {
                        self.filter = Some(filter);
                        clean
                    }
                    Err(_) => {
                        self.degrade("panicked during push");
                        Vec::new()
                    }
                }
            }
            None => Vec::new(),
        };
        self.mic.clear();
        self.far.clear();
        let mut frames = if self.degraded {
            self.drain_raw(writer)?
        } else {
            self.emit(writer, &clean)?
        };
        if !self.degraded && self.over_emitted > 0 {
            self.degrade("emitted more samples than it was fed");
        }
        if !self.degraded && self.tap_pending.len() > self.max_pending {
            self.degrade("exceeded its declared output-lag bound");
        }
        if self.degraded {
            frames += self.drain_raw(writer)?;
        }
        Ok(frames)
    }

    /// Flush the partial block and filter tail. Any samples still withheld after `finish` violate the
    /// contract and are restored from the raw mirror rather than padded with destructive silence.
    fn finish<W: std::io::Write + std::io::Seek>(
        mut self,
        writer: &mut hound::WavWriter<W>,
    ) -> Result<(u64, CaptureFilterDisposition), hound::Error> {
        let mut frames = self.flush(writer)?;
        if let Some(filter) = self.filter.take() {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| filter.finish())) {
                Ok(tail) => frames += self.emit(writer, &tail)?,
                Err(_) => self.degrade("panicked during finish"),
            }
        }
        if !self.degraded && self.over_emitted > 0 {
            self.degrade("emitted more samples than it was fed");
        }
        if !self.degraded && !self.tap_pending.is_empty() {
            self.degrade("withheld samples after finish");
        }
        if self.degraded {
            frames += self.drain_raw(writer)?;
        }
        if self.over_emitted > 0 {
            eprintln!(
                "corti-capture: capture filter over-emitted {} samples in total",
                self.over_emitted
            );
        }
        let disposition = if !self.degraded {
            CaptureFilterDisposition::Applied
        } else if self.clean_frames == 0 {
            CaptureFilterDisposition::RawFallback
        } else {
            CaptureFilterDisposition::Degraded
        };
        Ok((frames, disposition))
    }
}

/// Downmix one interleaved capture frame into the output channels for `layout` and write them. The 2-track
/// AEC-input layout is written **lossless** as 32-bit float (ch0 = mono mic, ch1 = mono tap echo reference);
/// the tap-only / diagnostic layouts are 16-bit PCM (matching the spec chosen in [`run_writer`]).
fn write_frame<W: std::io::Write + std::io::Seek>(
    writer: &mut hound::WavWriter<W>,
    frame: &[f32],
    layout: OutputLayout,
    mic: usize,
    total: usize,
) -> Result<(), hound::Error> {
    match layout {
        OutputLayout::TwoTrack => {
            writer.write_sample(frame_mean(frame, 0, mic))?; // ch0 = me (lossless f32)
            writer.write_sample(frame_mean(frame, mic, total - mic))?; // ch1 = them (mono echo ref)
        }
        OutputLayout::TapOnlyMono => {
            writer.write_sample(f32_to_i16(frame_mean(frame, mic, total - mic)))?; // them
        }
        OutputLayout::AllChannels => {
            for &s in frame {
                writer.write_sample(f32_to_i16(s))?;
            }
        }
    }
    Ok(())
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
/// `clock_uid` is the device the aggregate follows for its clock, added as the clock-leading main
/// sub-device. The mic+tap path passes the default **input** device (its mic channels are captured); the
/// tap-only path passes an input-less **output** device — an output device gives the aggregate a clock
/// without opening any input, so there is no orange "mic in use" dot. Either way the process tap supplies
/// the system-audio channels, with drift against the clock device compensated on the tap. (An aggregate
/// with a tap but *no* sub-device has no clock, so its IO proc never fires.)
fn create_aggregate(clock_uid: &str, tap_uid: &str) -> Result<ca::AudioObjectID> {
    let clock_uid = CFString::new(clock_uid);
    let tap_uid = CFString::new(tap_uid);

    let sub_dev = CFDictionary::from_CFType_pairs(&[(
        key(ca::kAudioSubDeviceUIDKey).as_CFType(),
        clock_uid.as_CFType(),
    )]);
    let tap_entry = CFDictionary::from_CFType_pairs(&[
        (key(ca::kAudioSubTapUIDKey).as_CFType(), tap_uid.as_CFType()),
        (
            key(ca::kAudioSubTapDriftCompensationKey).as_CFType(),
            CFNumber::from(1i32).as_CFType(),
        ),
    ]);
    let sub_list = CFArray::from_CFTypes(&[sub_dev]);
    let tap_list = CFArray::from_CFTypes(&[tap_entry]);

    // A fresh UID per aggregate avoids colliding with a sibling (the detector and webinar capture share one
    // process, so a bare PID would not be unique) or a stale aggregate left by a crashed run.
    let uid = CFString::new(&format!(
        "com.vasovagal.corti.agg.{}.{}",
        std::process::id(),
        AGG_SEQ.fetch_add(1, Ordering::Relaxed)
    ));

    let desc = CFDictionary::from_CFType_pairs(&[
        (
            key(ca::kAudioAggregateDeviceNameKey).as_CFType(),
            CFString::new("corti-capture").as_CFType(),
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
            key(ca::kAudioAggregateDeviceMainSubDeviceKey).as_CFType(),
            clock_uid.as_CFType(),
        ),
        (
            key(ca::kAudioAggregateDeviceSubDeviceListKey).as_CFType(),
            sub_list.as_CFType(),
        ),
        (
            key(ca::kAudioAggregateDeviceTapListKey).as_CFType(),
            tap_list.as_CFType(),
        ),
        (
            key(ca::kAudioAggregateDeviceTapAutoStartKey).as_CFType(),
            CFNumber::from(1i32).as_CFType(),
        ),
    ]);

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
    use super::*;

    // Existing sample/filter tests now exercise the ordered writer through one explicit epoch and
    // audio descriptor. Their fixtures and sample assertions remain unchanged.
    fn fixture_descriptors(
        samples: usize,
        shared: &Shared,
        rate: u32,
    ) -> rtrb::Consumer<CaptureDescriptor> {
        use corti_core::capture_accounting::{
            CallbackInput, CaptureFormat, CaptureTimeline, CaptureTimestamp,
        };
        let (mut producer, consumer) = rtrb::RingBuffer::new(HAL_DESCRIPTOR_CAPACITY);
        let total = shared.total_channels.load(Ordering::Acquire);
        let mic = shared.mic_channels.load(Ordering::Acquire);
        if samples > 0 {
            assert!(total > 0 && samples.is_multiple_of(total as usize));
            let mut timeline = CaptureTimeline::<64>::new().unwrap();
            let admission = timeline
                .plan_callback(
                    CallbackInput {
                        frames: (samples / total as usize) as u64,
                        format: CaptureFormat {
                            generation: 1,
                            source_rate_hz: rate,
                            mic_channels: mic,
                            tap_channels: total - mic,
                        },
                        timestamp: CaptureTimestamp {
                            sample_time: 0.0,
                            sample_time_valid: true,
                            source_offset_ns: 0,
                            host_time_valid: true,
                        },
                    },
                    samples,
                    HAL_DESCRIPTOR_CAPACITY,
                )
                .unwrap();
            assert!(admission.accepted());
            for descriptor in admission {
                producer.push(descriptor).unwrap();
            }
            for descriptor in timeline.finish().unwrap() {
                producer.push(descriptor).unwrap();
            }
        }
        drop(producer);
        consumer
    }

    fn run_writer(
        consumer: rtrb::Consumer<f32>,
        shared: Arc<Shared>,
        out: PathBuf,
        layout: OutputLayout,
        sample_rate: u32,
        tee: Option<CaptureTee>,
        filter: Option<CaptureFilterFactory>,
    ) -> Result<WriterOutcome> {
        let descriptors = fixture_descriptors(consumer.slots(), &shared, sample_rate);
        ordered_writer::run_writer(consumer, descriptors, out, layout, sample_rate, tee, filter)
    }

    fn run_microphone_tee(
        consumer: rtrb::Consumer<f32>,
        shared: Arc<Shared>,
        tee: CaptureTee,
    ) -> u64 {
        let descriptors = fixture_descriptors(consumer.slots(), &shared, 48_000);
        ordered_microphone::run_microphone_tee(consumer, descriptors, 48_000, tee)
            .unwrap()
            .frames
    }

    /// A tap-only capture (no mic sub-device) reports `mic_channels == 0`, so the whole interleaved stream
    /// is the tap: `mic_mono()` is empty and `tap_mono()` averages every channel. This is the split
    /// `start_tap_only_recording` + the `TapOnlyMono` layout rely on.
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

    /// End-to-end writer test (no CoreAudio): push interleaved [mic, tapL, tapR] frames into the ring, drop
    /// the producer to signal end-of-stream, and confirm the writer streams a **lossless 32-bit float**
    /// 2-track WAV (the AEC input — ADR 0007) with the mic on ch0 and the averaged tap on ch1.
    #[test]
    fn writer_streams_two_track_f32_with_downmix() {
        let (mut producer, consumer) = rtrb::RingBuffer::<f32>::new(1024);
        let frames = [
            [1.0f32, 0.25, 0.75], // me=1.0   them=mean(.25,.75)=.5
            [0.0, 0.5, 1.0],      // me=0.0   them=mean(.5,1)=.75
            [-1.0, -0.5, -1.0],   // me=-1.0  them=mean(-.5,-1)=-.75
        ];
        for fr in &frames {
            for &s in fr {
                producer.push(s).unwrap();
            }
        }
        let shared = Arc::new(Shared {
            total_channels: AtomicU32::new(3),
            mic_channels: AtomicU32::new(1),
            callbacks: AtomicU32::new(1),
            dropped: AtomicU64::new(0),
            has_mic: true,
        });
        drop(producer); // end-of-stream

        let dir = std::env::temp_dir().join("corti-writer-test");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("two-track.wav");
        let outcome = run_writer(
            consumer,
            shared,
            out.clone(),
            OutputLayout::TwoTrack,
            48_000,
            None,
            None,
        )
        .unwrap();
        assert_eq!(outcome.frames, 3, "three output frames");
        assert_eq!(outcome.filter, CaptureFilterDisposition::NotRequested);

        let mut r = hound::WavReader::open(&out).unwrap();
        let spec = r.spec();
        assert_eq!(spec.channels, 2);
        assert_eq!(spec.bits_per_sample, 32);
        assert_eq!(spec.sample_format, hound::SampleFormat::Float);
        let got: Vec<f32> = r.samples::<f32>().collect::<Result<_, _>>().unwrap();
        // ch0 = mic (lossless), ch1 = averaged tap — bit-exact, no quantization (the AEC input).
        assert_eq!(got, vec![1.0, 0.5, 0.0, 0.75, -1.0, -0.75]);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The tap-only layout writes a single averaged "them" channel per frame.
    #[test]
    fn writer_streams_tap_only_mono() {
        let (mut producer, consumer) = rtrb::RingBuffer::<f32>::new(1024);
        // Tap-only: mic_channels = 0, 2 tap channels per frame.
        for fr in &[[0.5f32, 1.0], [-1.0, 0.0]] {
            for &s in fr {
                producer.push(s).unwrap();
            }
        }
        let shared = Arc::new(Shared {
            total_channels: AtomicU32::new(2),
            mic_channels: AtomicU32::new(0),
            callbacks: AtomicU32::new(1),
            dropped: AtomicU64::new(0),
            has_mic: false,
        });
        drop(producer);

        let dir = std::env::temp_dir().join("corti-writer-tap-test");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("tap.wav");
        let outcome = run_writer(
            consumer,
            shared,
            out.clone(),
            OutputLayout::TapOnlyMono,
            48_000,
            None,
            None,
        )
        .unwrap();
        assert_eq!(outcome.frames, 2);
        assert_eq!(outcome.filter, CaptureFilterDisposition::NotRequested);

        let mut r = hound::WavReader::open(&out).unwrap();
        assert_eq!(r.spec().channels, 1);
        let got: Vec<i16> = r.samples::<i16>().collect::<Result<_, _>>().unwrap();
        assert_eq!(got, vec![f32_to_i16(0.75), f32_to_i16(-0.5)]);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// No callbacks (e.g. permission denied) ⇒ the writer creates no file.
    #[test]
    fn writer_creates_no_file_without_audio() {
        let (producer, consumer) = rtrb::RingBuffer::<f32>::new(16);
        let shared = Arc::new(Shared {
            total_channels: AtomicU32::new(0),
            mic_channels: AtomicU32::new(0),
            callbacks: AtomicU32::new(0),
            dropped: AtomicU64::new(0),
            has_mic: true,
        });
        drop(producer); // abandoned with zero callbacks

        let dir = std::env::temp_dir().join("corti-writer-empty-test");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("none.wav");
        let outcome = run_writer(
            consumer,
            shared,
            out.clone(),
            OutputLayout::TwoTrack,
            48_000,
            None,
            None,
        )
        .unwrap();
        assert_eq!(outcome.frames, 0);
        assert!(!out.exists(), "no audio ⇒ no file");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A stand-in for `StreamingAec`: "cleans" the mic by subtracting the far end, and reproduces the one
    /// property the writer has to cope with — output lags input by `lag` samples, with the backlog released
    /// by `finish`. Total emitted equals total pushed.
    struct LaggingFilter {
        lag: usize,
        pending: VecDeque<f32>,
    }

    impl CaptureFilter for LaggingFilter {
        fn max_output_lag_samples(&self) -> usize {
            self.lag
        }

        fn push(&mut self, mic: &[f32], far: &[f32]) -> Vec<f32> {
            for (m, f) in mic.iter().zip(far) {
                self.pending.push_back(m - f);
            }
            let ready = self.pending.len().saturating_sub(self.lag);
            self.pending.drain(..ready).collect()
        }

        fn finish(self: Box<Self>) -> Vec<f32> {
            self.pending.into_iter().collect()
        }
    }

    /// #74: with a filter installed the writer emits *cleaned* mic on ch0 and the **untouched** tap on ch1,
    /// frame-for-frame — across chunk boundaries and through the lookahead backlog that `finish` releases.
    #[test]
    fn writer_runs_filter_in_flight_over_two_track() {
        // Two chunks' worth plus a remainder, so both the mid-stream flush and the final drain run.
        let frames = 2 * FILTER_FRAMES_PER_CHUNK + 97;
        let (mut producer, consumer) = rtrb::RingBuffer::<f32>::new(3 * frames + 16);
        let mic = |i: usize| (i % 7) as f32 / 8.0;
        let far = |i: usize| (i % 5) as f32 / 16.0;
        for i in 0..frames {
            producer.push(mic(i)).unwrap();
            // Two tap channels that average to far(i).
            producer.push(far(i) - 0.125).unwrap();
            producer.push(far(i) + 0.125).unwrap();
        }
        let shared = Arc::new(Shared {
            total_channels: AtomicU32::new(3),
            mic_channels: AtomicU32::new(1),
            callbacks: AtomicU32::new(1),
            dropped: AtomicU64::new(0),
            has_mic: true,
        });
        drop(producer);

        let dir = std::env::temp_dir().join("corti-writer-filter-test");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("clean.wav");
        let outcome = run_writer(
            consumer,
            shared,
            out.clone(),
            OutputLayout::TwoTrack,
            48_000,
            None,
            Some(Box::new(|_rate| {
                Box::new(LaggingFilter {
                    lag: 1_000,
                    pending: VecDeque::new(),
                })
            })),
        )
        .unwrap();
        assert_eq!(
            outcome.frames as usize, frames,
            "filtering preserves the frame count"
        );
        assert_eq!(outcome.filter, CaptureFilterDisposition::Applied);

        let mut r = hound::WavReader::open(&out).unwrap();
        assert_eq!(r.spec().channels, 2);
        let got: Vec<f32> = r.samples::<f32>().collect::<Result<_, _>>().unwrap();
        assert_eq!(got.len(), 2 * frames);
        for i in 0..frames {
            assert_eq!(got[2 * i], mic(i) - far(i), "ch0 frame {i} is cleaned");
            assert_eq!(got[2 * i + 1], far(i), "ch1 frame {i} is the raw tap");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A filter that violates its finish contract falls back to the mirrored raw mic: no silence insertion,
    /// no lost tap, and the disposition makes a safe post-capture retry possible.
    #[test]
    fn writer_restores_raw_when_filter_withholds_output() {
        struct Silent;
        impl CaptureFilter for Silent {
            fn max_output_lag_samples(&self) -> usize {
                FILTER_FRAMES_PER_CHUNK
            }

            fn push(&mut self, _mic: &[f32], _far: &[f32]) -> Vec<f32> {
                Vec::new()
            }
            fn finish(self: Box<Self>) -> Vec<f32> {
                Vec::new()
            }
        }

        let (mut producer, consumer) = rtrb::RingBuffer::<f32>::new(64);
        for fr in &[[1.0f32, 0.25, 0.75], [-1.0, 0.5, 1.0]] {
            for &s in fr {
                producer.push(s).unwrap();
            }
        }
        let shared = Arc::new(Shared {
            total_channels: AtomicU32::new(3),
            mic_channels: AtomicU32::new(1),
            callbacks: AtomicU32::new(1),
            dropped: AtomicU64::new(0),
            has_mic: true,
        });
        drop(producer);

        let dir = std::env::temp_dir().join("corti-writer-silent-filter-test");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("silent.wav");
        let outcome = run_writer(
            consumer,
            shared,
            out.clone(),
            OutputLayout::TwoTrack,
            48_000,
            None,
            Some(Box::new(|_rate| Box::new(Silent))),
        )
        .unwrap();
        assert_eq!(outcome.frames, 2);
        assert_eq!(outcome.filter, CaptureFilterDisposition::RawFallback);

        let mut r = hound::WavReader::open(&out).unwrap();
        let got: Vec<f32> = r.samples::<f32>().collect::<Result<_, _>>().unwrap();
        assert_eq!(got, vec![1.0, 0.5, -1.0, 0.75]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn writer_falls_back_to_raw_when_filter_factory_panics() {
        let (mut producer, consumer) = rtrb::RingBuffer::<f32>::new(64);
        for &sample in &[0.75f32, 0.25, -0.5, 0.5] {
            producer.push(sample).unwrap();
        }
        let shared = Arc::new(Shared {
            total_channels: AtomicU32::new(2),
            mic_channels: AtomicU32::new(1),
            callbacks: AtomicU32::new(1),
            dropped: AtomicU64::new(0),
            has_mic: true,
        });
        drop(producer);

        let dir = std::env::temp_dir().join("corti-writer-factory-panic-test");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("raw.wav");
        let outcome = run_writer(
            consumer,
            shared,
            out.clone(),
            OutputLayout::TwoTrack,
            48_000,
            None,
            Some(Box::new(|_rate| panic!("factory boom"))),
        )
        .unwrap();
        assert_eq!(outcome.frames, 2);
        assert_eq!(outcome.filter, CaptureFilterDisposition::RawFallback);
        let mut reader = hound::WavReader::open(&out).unwrap();
        let got: Vec<f32> = reader.samples::<f32>().collect::<Result<_, _>>().unwrap();
        assert_eq!(got, vec![0.75, 0.25, -0.5, 0.5]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn writer_enforces_declared_filter_backlog_bound() {
        struct NeverEmits;
        impl CaptureFilter for NeverEmits {
            fn max_output_lag_samples(&self) -> usize {
                0
            }
            fn push(&mut self, _mic: &[f32], _far: &[f32]) -> Vec<f32> {
                Vec::new()
            }
            fn finish(self: Box<Self>) -> Vec<f32> {
                Vec::new()
            }
        }

        let frames = 3 * FILTER_FRAMES_PER_CHUNK;
        let (mut producer, consumer) = rtrb::RingBuffer::<f32>::new(2 * frames + 16);
        for _ in 0..frames {
            producer.push(0.75).unwrap();
            producer.push(0.25).unwrap();
        }
        let shared = Arc::new(Shared {
            total_channels: AtomicU32::new(2),
            mic_channels: AtomicU32::new(1),
            callbacks: AtomicU32::new(1),
            dropped: AtomicU64::new(0),
            has_mic: true,
        });
        drop(producer);

        let dir = std::env::temp_dir().join("corti-writer-filter-bound-test");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("bounded.wav");
        let outcome = run_writer(
            consumer,
            shared,
            out.clone(),
            OutputLayout::TwoTrack,
            48_000,
            None,
            Some(Box::new(|_rate| Box::new(NeverEmits))),
        )
        .unwrap();
        assert_eq!(outcome.frames as usize, frames);
        assert_eq!(outcome.filter, CaptureFilterDisposition::RawFallback);
        assert_eq!(
            hound::WavReader::open(&out).unwrap().duration() as usize,
            frames
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A panicking filter must cost cancellation quality, never the recording: the frames it was still
    /// withholding are written raw from the mirror queue, every later frame passes straight through, and the
    /// frame count is unchanged.
    #[test]
    fn writer_falls_back_to_raw_when_filter_panics() {
        /// Lags like the real canceller, then panics on its second block.
        struct PanicOnSecondPush {
            pushes: usize,
            pending: VecDeque<f32>,
        }
        impl CaptureFilter for PanicOnSecondPush {
            fn max_output_lag_samples(&self) -> usize {
                1_000
            }

            fn push(&mut self, mic: &[f32], far: &[f32]) -> Vec<f32> {
                self.pushes += 1;
                assert!(self.pushes < 2, "boom");
                for (m, f) in mic.iter().zip(far) {
                    self.pending.push_back(m - f);
                }
                let ready = self.pending.len().saturating_sub(1_000);
                self.pending.drain(..ready).collect()
            }
            fn finish(self: Box<Self>) -> Vec<f32> {
                unreachable!("dropped by the panic before finish")
            }
        }

        let frames = FILTER_FRAMES_PER_CHUNK + 500;
        let (mut producer, consumer) = rtrb::RingBuffer::<f32>::new(2 * frames + 16);
        let mic = |i: usize| (i % 11) as f32 / 16.0;
        let far = |i: usize| (i % 3) as f32 / 4.0;
        for i in 0..frames {
            producer.push(mic(i)).unwrap();
            producer.push(far(i)).unwrap();
        }
        let shared = Arc::new(Shared {
            total_channels: AtomicU32::new(2),
            mic_channels: AtomicU32::new(1),
            callbacks: AtomicU32::new(1),
            dropped: AtomicU64::new(0),
            has_mic: true,
        });
        drop(producer);

        let dir = std::env::temp_dir().join("corti-writer-panic-filter-test");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("degraded.wav");
        let outcome = run_writer(
            consumer,
            shared,
            out.clone(),
            OutputLayout::TwoTrack,
            48_000,
            None,
            Some(Box::new(|_rate| {
                Box::new(PanicOnSecondPush {
                    pushes: 0,
                    pending: VecDeque::new(),
                })
            })),
        )
        .unwrap();
        assert_eq!(
            outcome.frames as usize, frames,
            "a filter panic loses no frames"
        );
        assert_eq!(outcome.filter, CaptureFilterDisposition::Degraded);

        // The first push emitted all but its 1000-sample backlog; everything after that is raw.
        let cleaned = FILTER_FRAMES_PER_CHUNK - 1_000;
        let mut r = hound::WavReader::open(&out).unwrap();
        let got: Vec<f32> = r.samples::<f32>().collect::<Result<_, _>>().unwrap();
        assert_eq!(got.len(), 2 * frames);
        for i in 0..frames {
            let want = if i < cleaned { mic(i) - far(i) } else { mic(i) };
            assert_eq!(got[2 * i], want, "ch0 frame {i}");
            assert_eq!(got[2 * i + 1], far(i), "ch1 frame {i}");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A layout with no mic channel carries no echo reference, so the filter is dropped rather than fed
    /// garbage — the tap-only WAV is written exactly as it would be without one.
    #[test]
    fn writer_ignores_filter_without_mic() {
        struct Boom;
        impl CaptureFilter for Boom {
            fn max_output_lag_samples(&self) -> usize {
                0
            }

            fn push(&mut self, _mic: &[f32], _far: &[f32]) -> Vec<f32> {
                unreachable!("filter must not run without a mic channel")
            }
            fn finish(self: Box<Self>) -> Vec<f32> {
                unreachable!("filter must not run without a mic channel")
            }
        }

        let (mut producer, consumer) = rtrb::RingBuffer::<f32>::new(64);
        for &s in &[0.5f32, 1.0] {
            producer.push(s).unwrap();
        }
        let shared = Arc::new(Shared {
            total_channels: AtomicU32::new(2),
            mic_channels: AtomicU32::new(0),
            callbacks: AtomicU32::new(1),
            dropped: AtomicU64::new(0),
            has_mic: false,
        });
        drop(producer);

        let dir = std::env::temp_dir().join("corti-writer-nomic-filter-test");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("tap.wav");
        let outcome = run_writer(
            consumer,
            shared,
            out.clone(),
            OutputLayout::TapOnlyMono,
            48_000,
            None,
            Some(Box::new(|_rate| Box::new(Boom))),
        )
        .unwrap();
        assert_eq!(outcome.frames, 1);
        assert_eq!(outcome.filter, CaptureFilterDisposition::NotRequested);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ring_secs_unset_or_garbage_uses_default() {
        assert_eq!(clamp_ring_secs(None), RING_SECONDS_DEFAULT);
        assert_eq!(clamp_ring_secs(Some("")), RING_SECONDS_DEFAULT);
        assert_eq!(clamp_ring_secs(Some("   ")), RING_SECONDS_DEFAULT);
        assert_eq!(clamp_ring_secs(Some("abc")), RING_SECONDS_DEFAULT);
        assert_eq!(clamp_ring_secs(Some("-5")), RING_SECONDS_DEFAULT);
        assert_eq!(clamp_ring_secs(Some("4.5")), RING_SECONDS_DEFAULT);
    }

    #[test]
    fn ring_secs_in_range_passes_through_with_trim() {
        assert_eq!(clamp_ring_secs(Some("60")), 60);
        assert_eq!(clamp_ring_secs(Some("  45 ")), 45);
        assert_eq!(clamp_ring_secs(Some("5")), RING_SECONDS_MIN);
        assert_eq!(clamp_ring_secs(Some("300")), RING_SECONDS_MAX);
    }

    #[test]
    fn ring_secs_out_of_range_is_clamped() {
        assert_eq!(clamp_ring_secs(Some("0")), RING_SECONDS_MIN);
        assert_eq!(clamp_ring_secs(Some("1")), RING_SECONDS_MIN);
        assert_eq!(clamp_ring_secs(Some("100000")), RING_SECONDS_MAX);
    }

    #[test]
    fn ring_capacity_reflects_default_seconds() {
        // No env override set in this assertion's expectation: default 30 s.
        assert_eq!(
            ring_capacity(48_000),
            48_000 * RING_MAX_CHANNELS * RING_SECONDS_DEFAULT
        );
        // Sample-rate floor of 8 kHz still applies.
        assert_eq!(
            ring_capacity(4_000),
            8_000 * RING_MAX_CHANNELS * RING_SECONDS_DEFAULT
        );
    }

    /// With a tee attached and a consumer that keeps up, chunks arrive downmixed correctly (ch0 mean → mic,
    /// tap channels mean → tap) and no chunk is dropped. Exercises the writer end-to-end, no CoreAudio.
    #[test]
    fn tee_delivers_downmixed_chunks_without_drops() {
        let frames = TEE_FRAMES_PER_CHUNK + 10; // one full tee chunk + a short trailing flush
        let (mut producer, consumer) = rtrb::RingBuffer::<f32>::new(frames * 3 + 8);
        // Interleaved [mic, tapL, tapR]: mic = 1.0, tap mean = mean(0.25, 0.75) = 0.5, every frame.
        for _ in 0..frames {
            for &s in &[1.0f32, 0.25, 0.75] {
                producer.push(s).unwrap();
            }
        }
        let shared = Arc::new(Shared {
            total_channels: AtomicU32::new(3),
            mic_channels: AtomicU32::new(1),
            callbacks: AtomicU32::new(1),
            dropped: AtomicU64::new(0),
            has_mic: true,
        });
        drop(producer);

        let (tx, rx) = std::sync::mpsc::sync_channel::<CaptureChunk>(8);
        let tee = CaptureTee::new(tx);
        let dropped = tee.dropped_counter();

        let dir = std::env::temp_dir().join("corti-tee-test");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("tee.wav");
        let outcome = run_writer(
            consumer,
            shared,
            out.clone(),
            OutputLayout::TwoTrack,
            48_000,
            Some(tee),
            None,
        )
        .unwrap();
        assert_eq!(outcome.frames as usize, frames);

        let mut got_mic = 0usize;
        let mut got_tap = 0usize;
        while let Ok(chunk) = rx.try_recv() {
            for &m in &chunk.mic {
                assert!((m - 1.0).abs() < 1e-6, "mic downmix");
            }
            for &t in &chunk.tap {
                assert!((t - 0.5).abs() < 1e-6, "tap downmix");
            }
            got_mic += chunk.mic.len();
            got_tap += chunk.tap.len();
        }
        assert_eq!(got_mic, frames, "every frame teed to mic");
        assert_eq!(got_tap, frames, "every frame teed to tap");
        assert_eq!(dropped.load(Ordering::Acquire), 0, "consumer kept up");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn microphone_tee_is_mono_and_has_no_system_audio_side() {
        let frames = TEE_FRAMES_PER_CHUNK + 7;
        let (mut producer, consumer) = rtrb::RingBuffer::<f32>::new(frames * 2 + 8);
        for _ in 0..frames {
            producer.push(0.25).unwrap();
            producer.push(0.75).unwrap();
        }
        let shared = Arc::new(Shared {
            total_channels: AtomicU32::new(2),
            mic_channels: AtomicU32::new(2),
            callbacks: AtomicU32::new(1),
            dropped: AtomicU64::new(0),
            has_mic: true,
        });
        drop(producer);
        let (tx, rx) = std::sync::mpsc::sync_channel::<CaptureChunk>(4);
        let tee = CaptureTee::new(tx);

        assert_eq!(run_microphone_tee(consumer, shared, tee), frames as u64);
        let chunks: Vec<_> = rx.try_iter().collect();
        assert_eq!(
            chunks.iter().map(|chunk| chunk.mic.len()).sum::<usize>(),
            frames
        );
        assert!(chunks.iter().all(|chunk| chunk.tap.is_empty()));
        assert!(
            chunks
                .iter()
                .flat_map(|chunk| &chunk.mic)
                .all(|sample| (*sample - 0.5).abs() < 1e-6)
        );
    }

    /// A bounded tee whose consumer never drains overflows: chunks past the channel capacity are dropped and
    /// counted, and the writer still finishes (never blocks). The WAV is unaffected.
    #[test]
    fn tee_drops_and_counts_when_consumer_stalls() {
        let chunks = 5usize;
        let frames = chunks * TEE_FRAMES_PER_CHUNK;
        let (mut producer, consumer) = rtrb::RingBuffer::<f32>::new(frames * 2 + 8);
        for _ in 0..frames {
            for &s in &[0.5f32, 0.5] {
                producer.push(s).unwrap();
            }
        }
        let shared = Arc::new(Shared {
            total_channels: AtomicU32::new(2),
            mic_channels: AtomicU32::new(0),
            callbacks: AtomicU32::new(1),
            dropped: AtomicU64::new(0),
            has_mic: false,
        });
        drop(producer);

        // Capacity 1, and we never receive → at most 1 chunk fits, the rest are dropped.
        let (tx, _rx) = std::sync::mpsc::sync_channel::<CaptureChunk>(1);
        let tee = CaptureTee::new(tx);
        let dropped = tee.dropped_counter();

        let dir = std::env::temp_dir().join("corti-tee-drop-test");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("tee-drop.wav");
        let outcome = run_writer(
            consumer,
            shared,
            out.clone(),
            OutputLayout::TapOnlyMono,
            48_000,
            Some(tee),
            None,
        )
        .unwrap();
        assert_eq!(
            outcome.frames as usize, frames,
            "writer wrote every frame despite tee drops"
        );
        // 5 chunks produced, channel holds 1 → at least 3 dropped (the undrained receiver still holds 1).
        assert!(
            dropped.load(Ordering::Acquire) >= (chunks as u64 - 2),
            "stalled consumer must drop+count chunks (dropped = {})",
            dropped.load(Ordering::Acquire)
        );
        // Tap-only ⇒ no mic teed; the WAV is a normal 1-channel file.
        assert!(out.exists());

        std::fs::remove_dir_all(&dir).ok();
    }
}
