//! Control-thread format observations. The IO callback only takes a bounded atomic snapshot.

use std::os::raw::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Result, bail};
use coreaudio_sys as ca;

use super::{aggregate_input_format, property};

pub(super) struct FormatState {
    generation: AtomicU64,
    rate: AtomicU32,
    dirty: AtomicBool,
    stopping: AtomicBool,
}

impl FormatState {
    #[cfg(test)]
    pub(super) fn for_test(rate: u32) -> Self {
        let state = Self::new();
        state.refresh_with(|| rate);
        state
    }

    fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            rate: AtomicU32::new(0),
            dirty: AtomicBool::new(true),
            stopping: AtomicBool::new(false),
        }
    }

    /// One bounded atomic operation on the native notification callback. Coalescing notifications is
    /// safe: no PCM is accepted until the control worker completes a query without a later invalidation.
    fn invalidate(&self) {
        self.dirty.store(true, Ordering::SeqCst);
    }

    /// The constructor owns publication until it starts the sole worker. No native callback calls
    /// this method. Publish the odd generation before clearing dirty, so the query interval cannot
    /// expose the previous rate. A notification during the query leaves dirty set after publication.
    fn refresh_with(&self, query: impl FnOnce() -> u32) {
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        let generation = self.generation.load(Ordering::Relaxed);
        let Some(next) = generation.checked_add(2) else {
            self.rate.store(0, Ordering::SeqCst);
            return;
        };
        self.generation.store(generation + 1, Ordering::SeqCst);
        self.dirty.store(false, Ordering::SeqCst);
        let rate = query();
        self.rate.store(rate, Ordering::SeqCst);
        self.generation.store(next, Ordering::SeqCst);
    }

    fn run_worker(&self, mut query: impl FnMut() -> u32) {
        while !self.stopping.load(Ordering::Acquire) {
            if self.dirty.load(Ordering::SeqCst) {
                self.refresh_with(&mut query);
            }
            // Notifications never wake, lock or allocate. Poll only on this control worker; stop()
            // unparks it before joining. A format transition is unavailable during this bounded poll.
            std::thread::park_timeout(Duration::from_millis(5));
        }
    }

    /// No retry loop: a concurrent refresh makes this callback unavailable, never stale-format PCM.
    pub(super) fn snapshot(&self) -> Option<(u64, u32)> {
        if self.stopping.load(Ordering::Acquire) || self.dirty.load(Ordering::SeqCst) {
            return None;
        }
        let before = self.generation.load(Ordering::SeqCst);
        let rate = self.rate.load(Ordering::SeqCst);
        let after = self.generation.load(Ordering::SeqCst);
        (before != 0
            && before == after
            && before.is_multiple_of(2)
            && rate != 0
            && !self.dirty.load(Ordering::SeqCst)
            && !self.stopping.load(Ordering::Acquire))
        .then_some((before, rate))
    }
}

fn observed_rate(device: ca::AudioObjectID) -> u32 {
    unsafe { aggregate_input_format(device) }
        .ok()
        .and_then(|format| {
            (format.mFormatID == ca::kAudioFormatLinearPCM
                && format.mFormatFlags & ca::kAudioFormatFlagIsFloat != 0
                && format.mFormatFlags & ca::kAudioFormatFlagIsBigEndian == 0
                && format.mBitsPerChannel == 32
                && format.mSampleRate.is_finite()
                && format.mSampleRate.fract() == 0.0
                && (8_000.0..=192_000.0).contains(&format.mSampleRate))
            .then_some(format.mSampleRate as u32)
        })
        .unwrap_or(0)
}

pub(super) struct FormatObserver {
    device: ca::AudioObjectID,
    addresses: Vec<ca::AudioObjectPropertyAddress>,
    state: Arc<FormatState>,
    initial_rate: u32,
    worker: Option<JoinHandle<()>>,
}

impl FormatObserver {
    pub(super) fn new(device: ca::AudioObjectID) -> Result<Self> {
        let mut observer = Self {
            device,
            addresses: Vec::with_capacity(3),
            state: Arc::new(FormatState::new()),
            initial_rate: 0,
            worker: None,
        };
        for address in [
            property::global(ca::kAudioDevicePropertyNominalSampleRate),
            property::address(
                ca::kAudioDevicePropertyStreamFormat,
                ca::kAudioObjectPropertyScopeInput,
                ca::kAudioObjectPropertyElementMain,
            ),
            property::address(
                ca::kAudioDevicePropertyStreamConfiguration,
                ca::kAudioObjectPropertyScopeInput,
                ca::kAudioObjectPropertyElementMain,
            ),
        ] {
            let status = unsafe {
                ca::AudioObjectAddPropertyListener(
                    device,
                    &address,
                    Some(changed),
                    Arc::as_ptr(&observer.state) as *mut c_void,
                )
            };
            if status != 0 {
                bail!("capture format listener registration failed: OSStatus {status}");
            }
            observer.addresses.push(address);
        }
        // This first query runs on the starting/control thread, before the publication worker exists.
        observer.state.refresh_with(|| observed_rate(device));
        let Some((_, initial_rate)) = observer.state.snapshot() else {
            bail!("capture input format is unavailable or unsupported");
        };
        observer.initial_rate = initial_rate;
        let worker_state = observer.state.clone();
        observer.worker = Some(
            std::thread::Builder::new()
                .name("corti-capture-format".into())
                .spawn(move || worker_state.run_worker(|| observed_rate(device)))?,
        );
        Ok(observer)
    }

    pub(super) fn state(&self) -> Arc<FormatState> {
        self.state.clone()
    }

    /// Fixed retained rate, validated before startup. A later notification may invalidate state()
    /// immediately, so callers must not take a second snapshot and unwrap it to obtain this value.
    pub(super) fn initial_rate(&self) -> u32 {
        self.initial_rate
    }

    /// Stop property queries and remove every listener before the caller destroys the device. An
    /// error cannot certify listener removal; the owner must retain the device/observer on failure.
    pub(super) fn stop(&mut self) -> Result<()> {
        self.state.stopping.store(true, Ordering::Release);
        self.state.invalidate();
        let mut failure = None;
        self.addresses.retain(|address| {
            let status = unsafe {
                ca::AudioObjectRemovePropertyListener(
                    self.device,
                    address,
                    Some(changed),
                    Arc::as_ptr(&self.state) as *mut c_void,
                )
            };
            if status != 0 {
                failure = Some(status);
            }
            status != 0
        });
        let worker_panicked = self.worker.take().is_some_and(|worker| {
            worker.thread().unpark();
            worker.join().is_err()
        });
        if let Some(status) = failure {
            bail!("capture format listener removal failed: OSStatus {status}");
        }
        if worker_panicked {
            bail!("capture format worker panicked");
        }
        Ok(())
    }
}

impl Drop for FormatObserver {
    fn drop(&mut self) {
        if self.stop().is_err() && !self.addresses.is_empty() {
            // An unsuccessful native removal may still invoke changed(). Retain its small atomic
            // state rather than free the callback context. The query worker has already joined.
            std::mem::forget(self.state.clone());
        }
    }
}

unsafe extern "C" fn changed(
    _device: ca::AudioObjectID,
    _count: u32,
    _addresses: *const ca::AudioObjectPropertyAddress,
    context: *mut c_void,
) -> ca::OSStatus {
    if !context.is_null() {
        unsafe { &*context.cast::<FormatState>() }.invalidate();
    }
    0
}

#[repr(C)]
struct MachTimebase {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_timebase_info(info: *mut MachTimebase) -> i32;
    fn mach_absolute_time() -> u64;
}

/// The platform conversion factor is read once off HAL. Only checked integer arithmetic runs there.
pub(super) struct HostClock {
    numer: u32,
    denom: u32,
    first: Option<u64>,
}

impl HostClock {
    pub(super) fn new() -> Result<Self> {
        let mut timebase = MachTimebase { numer: 0, denom: 0 };
        if unsafe { mach_timebase_info(&mut timebase) } != 0
            || timebase.numer == 0
            || timebase.denom == 0
        {
            bail!("capture host clock conversion unavailable");
        }
        Ok(Self {
            numer: timebase.numer,
            denom: timebase.denom,
            // Bind recording-relative host time before IO starts. A later first valid timestamp must
            // not silently reset the source clock after earlier callbacks with invalid timestamp flags.
            first: Some(unsafe { mach_absolute_time() }),
        })
    }

    pub(super) fn offset_ns(&mut self, ticks: u64, valid: bool) -> Option<u64> {
        if !valid {
            return None;
        }
        let first = *self.first.get_or_insert(ticks);
        let delta = ticks.checked_sub(first)?;
        u64::try_from(u128::from(delta) * u128::from(self.numer) / u128::from(self.denom)).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_snapshot_never_accepts_an_in_progress_or_invalid_observation() {
        let state = FormatState::new();
        assert_eq!(state.snapshot(), None);
        state.refresh_with(|| 48_000);
        assert_eq!(state.snapshot(), Some((2, 48_000)));
        state.generation.store(1, Ordering::Release);
        assert_eq!(state.snapshot(), None);
        state.generation.store(2, Ordering::Release);
        assert_eq!(state.snapshot(), Some((2, 48_000)));
        state.generation.store(3, Ordering::Release);
        state.rate.store(44_100, Ordering::Release);
        assert_eq!(state.snapshot(), None);
        state.generation.store(4, Ordering::Release);
        assert_eq!(state.snapshot(), Some((4, 44_100)));
        state.rate.store(0, Ordering::Release);
        assert_eq!(state.snapshot(), None);
    }

    #[test]
    fn native_notification_only_invalidates_until_control_side_refresh() {
        let state = FormatState::new();
        state.refresh_with(|| 48_000);
        for _ in 0..3 {
            assert_eq!(
                unsafe {
                    changed(
                        ca::AudioObjectID::MAX,
                        0,
                        std::ptr::null(),
                        (&state as *const FormatState).cast_mut().cast(),
                    )
                },
                0
            );
            assert_eq!(state.snapshot(), None);
            assert_eq!(state.generation.load(Ordering::SeqCst), 2);
            assert_eq!(state.rate.load(Ordering::SeqCst), 48_000);
        }
        state.refresh_with(|| 44_100);
        assert_eq!(state.snapshot(), Some((4, 44_100)));
    }

    #[test]
    fn notification_during_query_cannot_publish_a_valid_snapshot() {
        let state = FormatState::new();
        state.refresh_with(|| 48_000);
        state.invalidate();
        state.refresh_with(|| {
            assert_eq!(state.snapshot(), None);
            // A native invalidation after dirty was cleared must survive query publication.
            state.invalidate();
            44_100
        });
        assert_eq!(state.generation.load(Ordering::SeqCst), 4);
        assert_eq!(state.snapshot(), None);
        state.refresh_with(|| {
            assert_eq!(state.snapshot(), None);
            44_100
        });
        assert_eq!(state.snapshot(), Some((6, 44_100)));
        state.invalidate();
        assert_eq!(state.snapshot(), None);
    }

    #[test]
    fn publication_generation_exhaustion_stays_unavailable_without_reusing_an_id() {
        let state = FormatState::new();
        state.generation.store(u64::MAX - 1, Ordering::SeqCst);
        state.rate.store(48_000, Ordering::SeqCst);
        state.dirty.store(false, Ordering::SeqCst);
        assert_eq!(state.snapshot(), Some((u64::MAX - 1, 48_000)));
        state.invalidate();
        state.refresh_with(|| panic!("exhausted publication must not query"));
        assert_eq!(state.snapshot(), None);
        assert_eq!(state.generation.load(Ordering::SeqCst), u64::MAX - 1);
    }

    #[test]
    fn worker_requeries_interrupted_observations_and_joins_before_state_teardown() {
        let state = Arc::new(FormatState::new());
        let queries = Arc::new(AtomicU32::new(0));
        let worker_state = state.clone();
        let worker_queries = queries.clone();
        let worker = std::thread::spawn(move || {
            worker_state.run_worker(|| {
                let query = worker_queries.fetch_add(1, Ordering::SeqCst);
                if query == 0 {
                    worker_state.invalidate();
                    48_000
                } else {
                    44_100
                }
            });
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while state.snapshot() != Some((4, 44_100)) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        let observed = state.snapshot();
        state.stopping.store(true, Ordering::Release);
        worker.thread().unpark();
        worker.join().unwrap();
        assert_eq!(observed, Some((4, 44_100)));
        assert_eq!(queries.load(Ordering::SeqCst), 2);
        state.invalidate();
        assert_eq!(state.snapshot(), None);
        state.refresh_with(|| panic!("stopped observer must not query the device"));
    }

    #[test]
    fn host_clock_honors_validity_and_checked_relative_offsets() {
        let mut clock = HostClock {
            numer: 125,
            denom: 3,
            first: None,
        };
        assert_eq!(clock.offset_ns(999, false), None);
        assert_eq!(clock.offset_ns(12, true), Some(0));
        assert_eq!(clock.offset_ns(15, true), Some(125));
        assert_eq!(clock.offset_ns(11, true), None);
        assert_eq!(clock.offset_ns(18, true), Some(250));
        assert_eq!(clock.offset_ns(u64::MAX, true), None);
    }
}
