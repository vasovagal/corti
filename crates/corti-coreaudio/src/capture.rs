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
//! channel(s)** — the "me" / "them" split that downstream code (and offline echo cancellation) relies on.

use std::cell::UnsafeCell;
use std::ffi::CStr;
use std::os::raw::c_void;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::thread::JoinHandle;
use std::time::Duration;

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
    pub mic: Vec<f32>,
    pub tap: Vec<f32>,
}

/// A **bounded, lossy** tee of the downmixed capture stream, for live consumers (AEC / transcription).
///
/// The writer thread `try_send`s ~[`TEE_FRAMES_PER_CHUNK`]-frame chunks and **never blocks**: if the consumer
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
    /// Live-tee chunks dropped because the consumer fell behind (bounded, lossy by design — never affects the
    /// on-disk WAV). 0 when no [`CaptureTee`] was attached.
    pub tee_dropped_chunks: u64,
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
    sample_rate: u32,
    /// The streaming writer thread; joined on `stop`/`Drop` so the file is flushed and finalized.
    writer: Option<JoinHandle<Result<u64>>>,
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
        Self::start_inner(target, true, out, layout, None)
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
        Self::start_inner(target, true, out, layout, Some(tee))
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
        Self::start_inner(target, false, out, layout, None)
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
        Self::start_inner(target, false, out, layout, Some(tee))
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
        tee: Option<CaptureTee>,
    ) -> Result<Self> {
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

        let sample_rate = unsafe { aggregate_input_format(agg_id) }
            .map(|f| f.mSampleRate as u32)
            .unwrap_or(48_000);

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
        let state = Box::into_raw(Box::new(Cap {
            producer: UnsafeCell::new(producer),
            shared: shared.clone(),
        }));

        // 5. IO proc.
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

        // 6. Writer thread. Spawned last — there are no fallible steps after this, so it never needs an
        //    error-path teardown of its own.
        let tee_dropped = tee.as_ref().map(|t| t.dropped.clone());
        let writer = {
            let shared = shared.clone();
            let out = out.clone();
            std::thread::Builder::new()
                .name("corti-capture-writer".into())
                .spawn(move || run_writer(consumer, shared, out, layout, sample_rate, tee))
                .context("spawning capture writer thread")?
        };

        // Ownership transfers to the session; defuse the RAII guards.
        std::mem::forget(tap_guard);
        std::mem::forget(agg_guard);
        Ok(Self {
            agg_id,
            tap_id,
            proc_id,
            state,
            sample_rate,
            writer: Some(writer),
            out,
            tee_dropped,
        })
    }

    /// The capture sample rate (read from the aggregate's input format, default 48 kHz). Available before
    /// `stop` so a live consumer can build its resampler / AEC at the right rate.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Stop capturing, flush the writer, and return the recording's metadata. Errors if the writer hit an
    /// I/O error (disk full, etc.); the CoreAudio objects are always torn down first.
    pub fn stop(mut self) -> Result<RecordingHandle> {
        unsafe { ca::AudioDeviceStop(self.agg_id, self.proc_id) };
        unsafe { ca::AudioDeviceDestroyIOProcID(self.agg_id, self.proc_id) };
        // The IO proc can no longer fire, so the shared atomics are final — read them straight off `Cap`
        // before freeing it (no `Arc` is kept on the session, so `forget(self)` below leaks nothing).
        let cap = unsafe { &*self.state };
        let total = cap.shared.total_channels.load(Ordering::Acquire);
        let mic = cap.shared.mic_channels.load(Ordering::Acquire).min(total);
        let callbacks = cap.shared.callbacks.load(Ordering::Acquire);
        let dropped = cap.shared.dropped.load(Ordering::Acquire);
        // Drop `Cap` (and with it the ring producer) so the writer sees end-of-stream, drains, and finalizes.
        unsafe { drop(Box::from_raw(self.state)) };
        self.state = std::ptr::null_mut();
        let writer_result = match self.writer.take() {
            Some(h) => h.join(),
            None => Ok(Ok(0)),
        };
        // Tear down the CoreAudio objects regardless of the writer's fate.
        unsafe {
            ca::AudioHardwareDestroyAggregateDevice(self.agg_id);
            AudioHardwareDestroyProcessTap(self.tap_id);
        }

        // The writer has joined, so the tee counter is final. Read it before forgetting `self`.
        let tee_dropped_chunks = self
            .tee_dropped
            .as_ref()
            .map(|d| d.load(Ordering::Acquire))
            .unwrap_or(0);

        let out = std::mem::take(&mut self.out);
        let sample_rate = self.sample_rate;
        std::mem::forget(self);

        let frames = writer_result
            .map_err(|_| anyhow!("capture writer thread panicked"))?
            .context("streaming recording to disk")?;
        Ok(RecordingHandle {
            path: out,
            frames,
            sample_rate,
            mic_channels: mic,
            tap_channels: total - mic,
            callbacks,
            dropped_samples: dropped,
            tee_dropped_chunks,
        })
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        // Only runs if the session is dropped without `stop()` (e.g. an early `?` or a discard). `stop()`
        // forgets self. Tear down like `stop` but keep no handle; the writer finalizes whatever partial file
        // exists (a finalized partial is recoverable — the higher-level discard path deletes it explicitly).
        unsafe {
            ca::AudioDeviceStop(self.agg_id, self.proc_id);
            ca::AudioDeviceDestroyIOProcID(self.agg_id, self.proc_id);
            if !self.state.is_null() {
                drop(Box::from_raw(self.state));
                self.state = std::ptr::null_mut();
            }
        }
        if let Some(h) = self.writer.take() {
            let _ = h.join();
        }
        unsafe {
            ca::AudioHardwareDestroyAggregateDevice(self.agg_id);
            AudioHardwareDestroyProcessTap(self.tap_id);
        }
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

/// Capture buffer state handed to the IO proc by raw pointer. The ring `producer` is touched **only** by the
/// single IO-proc thread (single-producer); `shared` is atomic, read-only after start.
struct Cap {
    producer: UnsafeCell<rtrb::Producer<f32>>,
    shared: Arc<Shared>,
}

/// IO proc: interleave every input buffer's channels into one frame-major float stream and push it into the
/// ring **without locking or allocating** (guardrail 9). In mic+tap mode the aggregate presents one buffer
/// per sub-stream (buffer 0 = mic, the rest = tap), so the first buffer's channel count is the mic-channel
/// count. In tap-only mode the clock sub-device is an input-less output device, contributing no input
/// channels, so the mic-channel count is 0 and every channel is tap.
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
    cap.shared.callbacks.fetch_add(1, Ordering::Relaxed);
    let list = unsafe { &*input };
    let nbuf = list.mNumberBuffers as usize;
    if nbuf == 0 {
        return 0;
    }
    let buffers = unsafe { std::slice::from_raw_parts(list.mBuffers.as_ptr(), nbuf) };

    let total_channels: u32 = buffers.iter().map(|b| b.mNumberChannels).sum();
    let mic = if cap.shared.has_mic {
        buffers[0].mNumberChannels
    } else {
        0
    };
    cap.shared.mic_channels.store(mic, Ordering::Relaxed);
    // Release, stored last: pairs with the writer's Acquire load of `total_channels`, publishing
    // `mic_channels` to the writer once it observes a non-zero channel count (it gates on that).
    cap.shared
        .total_channels
        .store(total_channels, Ordering::Release);

    // Derive the frame count from a buffer that actually carries channels: a tap-only aggregate's clock
    // sub-device (the default output) can present a 0-channel input buffer that must not zero the count.
    let frame_buf = buffers
        .iter()
        .find(|b| b.mNumberChannels > 0)
        .unwrap_or(&buffers[0]);
    let ch0 = frame_buf.mNumberChannels.max(1) as usize;
    let frames = frame_buf.mDataByteSize as usize / (4 * ch0);

    let total = total_channels as usize;
    if total == 0 || frames == 0 {
        return 0;
    }
    let needed = frames * total;

    // Single producer (this thread only): take a write chunk and fill it in interleaved order. If the ring
    // is full (the writer/disk fell behind) drop this callback whole — never block the real-time thread and
    // never desync mid-frame — and count it so the gap is reported, not silent.
    let producer = unsafe { &mut *cap.producer.get() };
    match producer.write_chunk_uninit(needed) {
        Ok(mut chunk) => {
            let (head, tail) = chunk.as_mut_slices();
            let mut slots = head.iter_mut().chain(tail.iter_mut());
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
                        if let Some(slot) = slots.next() {
                            slot.write(v);
                        }
                    }
                }
            }
            // SAFETY: the loops above wrote exactly `needed` values (frames × total_channels), initializing
            // every slot of the chunk.
            unsafe { chunk.commit_all() };
        }
        Err(_) => {
            cap.shared
                .dropped
                .fetch_add(needed as u64, Ordering::Relaxed);
        }
    }
    0
}

/// The streaming writer thread: drain the ring, downmix each interleaved frame per `layout`, and write it to
/// `out` incrementally — lossless 32-bit float for the 2-track AEC input, 16-bit PCM for the tap-only /
/// diagnostic layouts. Returns the number of output frames written. Creates the file lazily — only
/// once the first callback has revealed the channel layout — so a permission-denied run (no callbacks) leaves
/// no file behind.
fn run_writer(
    mut consumer: rtrb::Consumer<f32>,
    shared: Arc<Shared>,
    out: PathBuf,
    layout: OutputLayout,
    sample_rate: u32,
    tee: Option<CaptureTee>,
) -> Result<u64> {
    // Wait for the first callback to publish the channel layout (Acquire pairs with the io proc's Release
    // store of `total_channels`), or for end-of-stream (producer dropped ⇒ no audio ⇒ no file).
    let total = loop {
        let t = shared.total_channels.load(Ordering::Acquire) as usize;
        if t > 0 {
            break t;
        }
        if consumer.is_abandoned() {
            return Ok(0); // no audio ever delivered — no file
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let mic = shared.mic_channels.load(Ordering::Acquire) as usize;
    let tap = total.saturating_sub(mic);
    let out_channels = layout.out_channels(mic as u32, tap as u32);

    // The 2-track recording is the **AEC input** (ADR 0007 §1/§2: the streaming canceller runs over this
    // lossless 2-track post-capture). Writing it as 32-bit float preserves the bit-exact mic + mono echo
    // reference the adaptive filter needs — quantizing here would impose an ERLE ceiling (ADR 0007 Context).
    // The tap-only / diagnostic layouts carry no mic, so they are never an AEC input and stay compact 16-bit.
    let (bits_per_sample, sample_format) = match layout {
        OutputLayout::TwoTrack => (32, hound::SampleFormat::Float),
        OutputLayout::TapOnlyMono | OutputLayout::AllChannels => (16, hound::SampleFormat::Int),
    };
    let spec = hound::WavSpec {
        channels: out_channels as u16,
        sample_rate,
        bits_per_sample,
        sample_format,
    };
    let mut writer = hound::WavWriter::create(&out, spec)
        .map_err(|e| anyhow!("creating {}: {e}", out.display()))?;

    let mut frame: Vec<f32> = Vec::with_capacity(total);
    let mut frames_written: u64 = 0;
    // Live-tee accumulators: downmixed mono mic + tap, flushed to the consumer every TEE_FRAMES_PER_CHUNK
    // frames. Only touched when a tee is attached — the default path is byte-identical.
    let mut tee_mic: Vec<f32> = Vec::new();
    let mut tee_tap: Vec<f32> = Vec::new();
    loop {
        let avail = consumer.slots();
        if avail == 0 {
            if consumer.is_abandoned() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        let chunk = match consumer.read_chunk(avail) {
            Ok(c) => c,
            Err(_) => break,
        };
        let (a, b) = chunk.as_slices();
        for &s in a.iter().chain(b.iter()) {
            frame.push(s);
            if frame.len() == total {
                write_frame(&mut writer, &frame, layout, mic, total)
                    .map_err(|e| anyhow!("writing to {}: {e}", out.display()))?;
                frames_written += 1;
                if let Some(t) = &tee {
                    if mic > 0 {
                        tee_mic.push(frame_mean(&frame, 0, mic));
                    }
                    tee_tap.push(frame_mean(&frame, mic, total - mic));
                    if tee_tap.len() >= TEE_FRAMES_PER_CHUNK {
                        send_tee_chunk(t, &mut tee_mic, &mut tee_tap);
                    }
                }
                frame.clear();
            }
        }
        chunk.commit_all();
    }
    // Flush a trailing partial tee chunk (the WAV drops its partial frame, but a short live chunk is fine).
    if let Some(t) = &tee {
        send_tee_chunk(t, &mut tee_mic, &mut tee_tap);
    }
    // A trailing partial frame (incomplete) is discarded.
    writer
        .finalize()
        .map_err(|e| anyhow!("finalizing {}: {e}", out.display()))?;
    Ok(frames_written)
}

/// Hand the accumulated downmixed chunk to the tee consumer, non-blocking. On a full or hung-up channel the
/// chunk is dropped and counted — capture never stalls on a slow live consumer (ADR 0009).
fn send_tee_chunk(tee: &CaptureTee, mic: &mut Vec<f32>, tap: &mut Vec<f32>) {
    if tap.is_empty() {
        return;
    }
    let chunk = CaptureChunk {
        mic: std::mem::take(mic),
        tap: std::mem::take(tap),
    };
    // `mem::take` left both Vecs at capacity 0; re-reserve so the next chunk doesn't re-grow by repeated
    // reallocation on the writer thread.
    *mic = Vec::with_capacity(TEE_FRAMES_PER_CHUNK);
    *tap = Vec::with_capacity(TEE_FRAMES_PER_CHUNK);
    if tee.tx.try_send(chunk).is_err() {
        tee.dropped.fetch_add(1, Ordering::Relaxed);
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
        let n = run_writer(
            consumer,
            shared,
            out.clone(),
            OutputLayout::TwoTrack,
            48_000,
            None,
        )
        .unwrap();
        assert_eq!(n, 3, "three output frames");

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
        let n = run_writer(
            consumer,
            shared,
            out.clone(),
            OutputLayout::TapOnlyMono,
            48_000,
            None,
        )
        .unwrap();
        assert_eq!(n, 2);

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
        let n = run_writer(
            consumer,
            shared,
            out.clone(),
            OutputLayout::TwoTrack,
            48_000,
            None,
        )
        .unwrap();
        assert_eq!(n, 0);
        assert!(!out.exists(), "no audio ⇒ no file");

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
        let n = run_writer(
            consumer,
            shared,
            out.clone(),
            OutputLayout::TwoTrack,
            48_000,
            Some(tee),
        )
        .unwrap();
        assert_eq!(n as usize, frames);

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
        let n = run_writer(
            consumer,
            shared,
            out.clone(),
            OutputLayout::TapOnlyMono,
            48_000,
            Some(tee),
        )
        .unwrap();
        assert_eq!(
            n as usize, frames,
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
