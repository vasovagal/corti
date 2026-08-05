//! macOS worker that turns the pure [`crate::machine`] into real recordings.
//!
//! The CoreAudio HAL callbacks (mic-in-use + default-device-change) only forward a [`Msg`] over a channel
//! (guardrail 9 — never block a HAL thread, never start capture from it). A dedicated worker thread owns
//! the [`Machine`], the [`MicMonitor`], the [`DefaultInputDeviceMonitor`], and the in-flight
//! [`Recorder`], and is the only place that touches capture or invokes the user's event callback.

use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::Local;
use corti_capture::Recorder;
use corti_core::RecordingMeta;
use corti_coreaudio::{DefaultInputDeviceMonitor, MicMonitor, mic_owner, other_app_holds_input};

use crate::machine::{Action, Machine};
use crate::{COALESCE, DEBOUNCE, DetectorEvent, MIN_RECORDING, POLL_INTERVAL};

/// Messages from the HAL callbacks (and `Detector::drop`) to the worker thread.
enum Msg {
    /// Mic-in-use transitioned (`true` = now in use).
    Signal(bool),
    /// The system default input device changed; rebind the mic monitor.
    DeviceChanged,
    /// Temporarily suspend detection for Corti's explicit microphone test. The reply is false if a real
    /// detector recording already owns capture and therefore cannot be preempted.
    Pause(Sender<bool>),
    /// Resume detection after the test microphone has closed.
    Resume,
    /// Stop the worker (sent by `Detector::drop`).
    Shutdown,
}

/// App-supplied live-transcription hook (issue #87). The detect worker owns the [`Recorder`], so this is
/// how a live consumer gets a capture tee attached at `Recorder::start` time without corti-detect knowing
/// anything about transcription: the hook hands back a plain [`corti_capture::CaptureTee`] (or `None` for
/// no live path) and is told the full recording meta + sample rate once capture is actually running. All
/// methods are invoked on the detect worker thread and must return promptly — a slow `attach` delays
/// the start of the recording itself.
pub trait LiveHook: Send + 'static {
    /// Decide whether this recording gets a live tee. Called before `Recorder::start`; `app` is the
    /// best-effort owning-app attribution (the full [`RecordingMeta`] doesn't exist yet — the recorder
    /// chooses the output path).
    fn attach(&self, app: &corti_core::OwningApp) -> Option<corti_capture::CaptureTee>;
    /// Capture started with the tee attached: the definitive meta (with `audio_path`) plus the capture
    /// sample rate (to size a resampler/AEC). Only called when [`attach`](Self::attach) returned `Some`.
    fn started(&self, meta: &RecordingMeta, sample_rate: u32);
    /// The recording finished successfully. Called after the recorder has closed the tee and before the
    /// matching [`DetectorEvent::RecordingFinished`] is emitted, so a recording-specific live verdict is
    /// never delayed behind the serial pipeline.
    fn finished(&self, _meta: &RecordingMeta) {}
    /// The recording was discarded below the keep threshold. Called after the recorder closes the tee and
    /// before [`DetectorEvent::RecordingDiscarded`] is emitted.
    fn discarded(&self, _meta: &RecordingMeta) {}
    /// [`attach`](Self::attach) returned `Some` but the recorder failed to start — discard any pending
    /// live state; `started` will not be called.
    fn aborted(&self);
    /// The recording ended in failure (capture could not finish): no
    /// [`DetectorEvent::RecordingFinished`] will follow, so tear down any live state for this
    /// recording. Never called for errors the recording survives (e.g. a mic-monitor rebind failure).
    fn failed(&self, meta: &RecordingMeta);
}

/// Watches the mic and turns confirmed on/off transitions into recordings, emitting [`DetectorEvent`]s
/// from a dedicated worker thread (never the HAL callback thread).
pub struct Detector {
    ctrl: Sender<Msg>,
    worker: Option<JoinHandle<()>>,
}

impl Detector {
    /// Begin watching the default input device. `on_event` is invoked from the worker thread; it must not
    /// be invoked from a HAL callback (guardrail 9), which this guarantees.
    pub fn start(on_event: impl Fn(DetectorEvent) + Send + 'static) -> Result<Self> {
        Self::start_with_live_hook(on_event, None)
    }

    /// Like [`start`](Self::start), with an optional [`LiveHook`] consulted at every recording start
    /// (issue #87). Additive: `start` delegates here with `None` and behaves exactly as before.
    pub fn start_with_live_hook(
        on_event: impl Fn(DetectorEvent) + Send + 'static,
        live: Option<Box<dyn LiveHook>>,
    ) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<Msg>();

        // Default-device-change listener: created once (it watches the system object, which never
        // changes) and never recreated. Only the mic monitor is rebound on a change.
        let dtx = tx.clone();
        let device_monitor = DefaultInputDeviceMonitor::new(move || {
            let _ = dtx.send(Msg::DeviceChanged);
        })?;

        // Mic-in-use listener bound to the current default device.
        let mic_monitor = new_mic_monitor(&tx)?;
        // Seed: if a call is already in progress at launch, pick it up — still via the normal debounce,
        // not an instant start.
        let initial = mic_monitor.current().unwrap_or(false);

        let ctrl = tx.clone();
        let worker = std::thread::Builder::new()
            .name("corti-detect".into())
            .spawn(move || {
                let worker = Worker {
                    rx,
                    tx,
                    machine: Machine::new(DEBOUNCE, COALESCE, MIN_RECORDING),
                    mic_monitor,
                    _device_monitor: device_monitor,
                    self_pid: std::process::id() as i32,
                    current: None,
                    paused: false,
                    on_event,
                    live,
                };
                worker.run(initial);
            })?;

        Ok(Self {
            ctrl,
            worker: Some(worker),
        })
    }

    /// Pause mic-edge detection before Corti opens its own explicit microphone test. The worker acks only
    /// after clearing any pending debounce state, so the test's orange-dot edge cannot race into a duplicate
    /// detector recording. Returns false when a real recording is already in flight.
    pub fn pause(&self) -> Result<bool> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.ctrl
            .send(Msg::Pause(reply_tx))
            .map_err(|_| anyhow::anyhow!("detector worker is unavailable"))?;
        match reply_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(accepted) => Ok(accepted),
            Err(_) => {
                // `Pause` may still be queued behind a slow capture operation. Queue a compensating resume
                // so a caller timeout can never leave detection permanently suspended.
                let _ = self.ctrl.send(Msg::Resume);
                Err(anyhow::anyhow!("detector worker did not acknowledge pause"))
            }
        }
    }

    /// Resume normal detection. The worker re-seeds from the current microphone state, so a call that began
    /// during the test is picked up through the ordinary debounce after the test device is closed.
    pub fn resume(&self) -> Result<()> {
        self.ctrl
            .send(Msg::Resume)
            .map_err(|_| anyhow::anyhow!("detector worker is unavailable"))
    }
}

impl Drop for Detector {
    fn drop(&mut self) {
        // Explicit Shutdown (not channel-disconnect): the worker holds its own sender clones for
        // rebinding, so dropping this one would never disconnect the channel.
        let _ = self.ctrl.send(Msg::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Deliver the live finish verdict and only then expose the terminal event to downstream work. Kept as a
/// small model/HAL-free seam so the ordering contract has a deterministic regression test.
fn deliver_finished(
    live: Option<&dyn LiveHook>,
    meta: RecordingMeta,
    audio_path: std::path::PathBuf,
    emit: impl FnOnce(DetectorEvent),
) {
    if let Some(hook) = live {
        hook.finished(&meta);
    }
    emit(DetectorEvent::RecordingFinished { meta, audio_path });
}

/// Deliver the live discard verdict before exposing the corresponding terminal event.
fn deliver_discarded(
    live: Option<&dyn LiveHook>,
    meta: RecordingMeta,
    emit: impl FnOnce(DetectorEvent),
) {
    if let Some(hook) = live {
        hook.discarded(&meta);
    }
    emit(DetectorEvent::RecordingDiscarded { meta });
}

/// Create a [`MicMonitor`] bound to the current default input device that forwards transitions as
/// [`Msg::Signal`].
fn new_mic_monitor(tx: &Sender<Msg>) -> Result<MicMonitor> {
    let stx = tx.clone();
    MicMonitor::new(move |on| {
        let _ = stx.send(Msg::Signal(on));
    })
}

/// Worker-thread state. Owns everything that must not be touched from a HAL callback.
struct Worker<F: Fn(DetectorEvent)> {
    rx: mpsc::Receiver<Msg>,
    /// Held to clone for a freshly-rebound [`MicMonitor`] on [`Msg::DeviceChanged`].
    tx: Sender<Msg>,
    machine: Machine,
    /// Rebound on a default-device change; otherwise held to keep its HAL listener alive.
    mic_monitor: MicMonitor,
    /// Held only to keep its HAL listener alive for the worker's lifetime (dropped, never read).
    _device_monitor: DefaultInputDeviceMonitor,
    /// Our own PID, excluded from the during-recording "is another app still holding the mic?" poll —
    /// our capture aggregate would otherwise count as a mic user and the recording would never end.
    self_pid: i32,
    current: Option<(Recorder, RecordingMeta)>,
    /// True only while Corti's explicit microphone test owns the default input. HAL edges are ignored and
    /// no debounce deadline runs in this state.
    paused: bool,
    on_event: F,
    /// Optional live-transcription hook (issue #87), consulted at every recording start.
    live: Option<Box<dyn LiveHook>>,
}

impl<F: Fn(DetectorEvent)> Worker<F> {
    fn run(mut self, initial: bool) {
        if initial {
            self.machine.on_signal(true, Instant::now());
        }
        loop {
            let msg = match self.next_wait() {
                Some(t) => self.rx.recv_timeout(t),
                None => self.rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match msg {
                Ok(Msg::Signal(on)) if !self.paused => self.machine.on_signal(on, Instant::now()),
                Ok(Msg::Signal(_)) => {}
                Ok(Msg::DeviceChanged) => self.rebind(),
                Ok(Msg::Pause(reply)) => {
                    let accepted = self.current.is_none();
                    if accepted {
                        self.paused = true;
                        self.machine.reset();
                    }
                    let _ = reply.send(accepted);
                }
                Ok(Msg::Resume) => self.resume_detection(),
                Ok(Msg::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => self.on_wakeup(),
            }
        }
        // Falling out of `run` drops `mic_monitor` and `device_monitor` (removing their HAL listeners)
        // and any in-flight `current` recorder (capture torn down, no partial WAV written).
    }

    /// How long to block before the next wake-up. The machine's debounce/coalesce deadline normally
    /// drives this; while a recording is in progress we additionally wake every [`POLL_INTERVAL`] to
    /// re-check process attribution (see [`on_wakeup`](Worker::on_wakeup)).
    fn next_wait(&self) -> Option<Duration> {
        if self.paused {
            return None;
        }
        let deadline = self
            .machine
            .next_deadline()
            .map(|d| d.saturating_duration_since(Instant::now()));
        if self.current.is_some() {
            Some(deadline.map_or(POLL_INTERVAL, |d| d.min(POLL_INTERVAL)))
        } else {
            deadline
        }
    }

    /// Handle a timer wake-up. While recording, corti's own capture aggregate pins the device-level
    /// "running somewhere" signal true, so the [`MicMonitor`] can never report the call ending. We
    /// instead re-derive the mic-in-use signal from process attribution — "does any app other than us
    /// still hold input?" — and feed it to the machine, which debounces the stop through `COALESCE`.
    fn on_wakeup(&mut self) {
        if self.paused {
            return;
        }
        if self.current.is_some() {
            let still_on = other_app_holds_input(self.self_pid);
            self.machine.on_signal(still_on, Instant::now());
        }
        if let Some(action) = self.machine.on_tick(Instant::now()) {
            self.apply(action);
        }
    }

    /// Rebind the mic monitor to the (new) default input device after a device change, then re-seed its
    /// current state. Best-effort: never panics, and the in-flight recorder is intentionally *not*
    /// rebound (detection follows the device switch; capture stays on the device it started on).
    fn rebind(&mut self) {
        match new_mic_monitor(&self.tx) {
            Ok(monitor) => {
                let current = monitor.current();
                self.mic_monitor = monitor; // drops the old monitor → removes its HAL listener
                if let Ok(on) = current
                    && !self.paused
                {
                    self.machine.on_signal(on, Instant::now());
                }
            }
            Err(e) => self.emit(DetectorEvent::Error(format!(
                "re-binding mic monitor after device change failed: {e:#}"
            ))),
        }
    }

    /// Leave explicit-test suspension and re-seed the normal debounce from the now-current device state.
    fn resume_detection(&mut self) {
        self.paused = false;
        // A compensating Resume can arrive after a timed-out Pause that the worker ultimately rejected
        // because a real recorder had started. Never reset that recorder's state machine underneath it.
        if self.current.is_some() {
            return;
        }
        self.machine.reset();
        if self.mic_monitor.current().unwrap_or(false) {
            self.machine.on_signal(true, Instant::now());
        }
    }

    /// Apply a confirmed state-machine action: start or stop the recorder and emit the matching event.
    fn apply(&mut self, action: Action) {
        match action {
            Action::Start => {
                // Attribution is best-effort and never blocks capture (guardrail 8): a missing PID just
                // means a global tap.
                let owner = mic_owner();
                // Live hook (issue #87): the tee must exist before the recorder starts (the writer thread
                // captures it at session creation), so `attach` runs on the incomplete attribution.
                let tee = self.live.as_ref().and_then(|h| h.attach(&owner.app));
                let live_attached = tee.is_some();
                let started = match tee {
                    Some(tee) => Recorder::start_with_tee(&owner.app, owner.pid, tee),
                    None => Recorder::start(&owner.app, owner.pid),
                };
                match started {
                    Ok(recorder) => {
                        let meta = RecordingMeta {
                            started_at: Local::now(),
                            ended_at: None,
                            owning_app: owner.app,
                            audio_path: recorder.output_path().to_path_buf(),
                        };
                        tracing::info!(
                            target: "corti::detect",
                            app = %meta.owning_app.name,
                            pid = owner.pid,
                            started_at = %meta.started_at,
                            live = live_attached,
                            path = %meta.audio_path.display(),
                            "call started — recording"
                        );
                        if live_attached && let Some(hook) = &self.live {
                            hook.started(&meta, recorder.sample_rate());
                        }
                        self.current = Some((recorder, meta.clone()));
                        self.emit(DetectorEvent::RecordingStarted { meta });
                    }
                    Err(e) => {
                        if live_attached && let Some(hook) = &self.live {
                            hook.aborted();
                        }
                        tracing::error!(
                            target: "corti::detect",
                            error = %format!("{e:#}"),
                            "failed to start recording"
                        );
                        self.emit(DetectorEvent::Error(format!(
                            "failed to start recording: {e:#}"
                        )));
                        // No live recorder — don't leave the machine stuck in `Recording`.
                        self.machine.reset();
                    }
                }
            }
            Action::Stop { keep, duration } => {
                let Some((recorder, mut meta)) = self.current.take() else {
                    return;
                };
                if !keep {
                    // Discard: the writer has already streamed a partial WAV to disk, so `discard()` stops
                    // the session and deletes that file (a plain `drop` would leave the partial behind).
                    tracing::info!(
                        target: "corti::detect",
                        app = %meta.owning_app.name,
                        duration_secs = duration.as_secs_f64(),
                        kept = false,
                        "call ended — recording discarded (below keep threshold)"
                    );
                    recorder.discard();
                    deliver_discarded(self.live.as_deref(), meta, |event| self.emit(event));
                    return;
                }
                match recorder.finish() {
                    Ok(audio_path) => {
                        // ended_at = start + the mic-open span (a monotonic delta mapped onto the wall
                        // clock). This is the span up to the last mic-off, excluding the coalesce tail, so
                        // it agrees with the `keep` decision; the written WAV may be a hair longer.
                        let delta = chrono::TimeDelta::from_std(duration).unwrap_or_default();
                        meta.ended_at = Some(meta.started_at + delta);
                        tracing::info!(
                            target: "corti::detect",
                            app = %meta.owning_app.name,
                            duration_secs = duration.as_secs_f64(),
                            kept = true,
                            path = %audio_path.display(),
                            "call ended — recording kept"
                        );
                        // Deliver the recording-specific live verdict before the event callback can queue
                        // `Process` behind unrelated serial pipeline work.
                        deliver_finished(self.live.as_deref(), meta, audio_path, |event| {
                            self.emit(event);
                        });
                    }
                    Err(e) => {
                        // The recording is over but produced nothing to process: let the live hook
                        // tear down its session (no RecordingFinished will follow).
                        if let Some(hook) = &self.live {
                            hook.failed(&meta);
                        }
                        tracing::error!(
                            target: "corti::detect",
                            error = %format!("{e:#}"),
                            "failed to finish recording"
                        );
                        self.emit(DetectorEvent::Error(format!(
                            "failed to finish recording: {e:#}"
                        )));
                    }
                }
            }
        }
    }

    fn emit(&self, event: DetectorEvent) {
        (self.on_event)(event);
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use corti_core::OwningApp;

    use super::*;

    struct OrderedHook(Arc<Mutex<Vec<&'static str>>>);

    impl LiveHook for OrderedHook {
        fn attach(&self, _app: &OwningApp) -> Option<corti_capture::CaptureTee> {
            None
        }

        fn started(&self, _meta: &RecordingMeta, _sample_rate: u32) {}

        fn finished(&self, _meta: &RecordingMeta) {
            self.0.lock().unwrap().push("finish verdict");
        }

        fn discarded(&self, _meta: &RecordingMeta) {
            self.0.lock().unwrap().push("discard verdict");
        }

        fn aborted(&self) {}

        fn failed(&self, _meta: &RecordingMeta) {}
    }

    fn meta() -> RecordingMeta {
        RecordingMeta {
            started_at: chrono::Local::now(),
            ended_at: None,
            owning_app: OwningApp::from_bundle_id("us.zoom.xos"),
            audio_path: PathBuf::from("/tmp/corti-ordering.wav"),
        }
    }

    #[test]
    fn terminal_live_verdict_precedes_downstream_event() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let hook = OrderedHook(order.clone());

        let finish_order = order.clone();
        deliver_finished(
            Some(&hook),
            meta(),
            PathBuf::from("/tmp/corti-ordering.wav"),
            move |event| {
                assert!(matches!(event, DetectorEvent::RecordingFinished { .. }));
                finish_order.lock().unwrap().push("finished event");
            },
        );
        let discard_order = order.clone();
        deliver_discarded(Some(&hook), meta(), move |event| {
            assert!(matches!(event, DetectorEvent::RecordingDiscarded { .. }));
            discard_order.lock().unwrap().push("discarded event");
        });

        assert_eq!(
            *order.lock().unwrap(),
            [
                "finish verdict",
                "finished event",
                "discard verdict",
                "discarded event"
            ]
        );
    }
}
