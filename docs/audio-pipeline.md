# Audio pipeline — the data plane

> Verified against v0.8.0 + feat/pipeline-docs-and-streaming, plus per-block AEC statistics
> (`feat/aec-block-stats`, #149 phase 3) re-verified on v0.16.1.

The audio data plane runs mic-in-use → capture → clean, entirely on OS threads with one lock-free
ring. CoreAudio HAL threads push (an event when the mic starts/stops, PCM frames while recording);
those hop onto the `corti-detect` worker and the `corti-capture-writer` thread. Since #74 the writer
thread also runs the echo canceller, so the only WAV a recording produces is already cleaned — no
raw 2-track lands on disk, and peak RAM is `O(block + filter state + lookahead)` end to end.

## Mic-in-use detection

Two mechanisms, because corti's own capture pollutes the obvious signal:

- **Idle → event-driven.** `MicMonitor` (`crates/corti-coreaudio/src/listener.rs:120`) installs a
  HAL property listener on `kAudioDevicePropertyDeviceIsRunningSomewhere` scoped to the default
  **input** device (`:135`, `AudioObjectAddPropertyListener` `:140`). The C trampoline re-reads the
  property and forwards a `bool` to a `Fn(bool)+Send` closure, which sends `Msg::Signal(on)` to the
  detect worker. A companion `DefaultInputDeviceMonitor` (`:212`) watches
  `kAudioHardwarePropertyDefaultInputDevice` and fires `Msg::DeviceChanged` so the worker rebinds
  `MicMonitor` when the user switches input (AirPods, etc.).
- **Recording → poll-driven.** Once corti's aggregate device is running, it pins
  `IsRunningSomewhere` true, so the falling edge never arrives via the listener (LESSONS §2). While
  a recording is in flight the worker instead wakes every `POLL_INTERVAL` (1 s,
  `crates/corti-detect/src/lib.rs:39`) and re-derives the signal from process attribution:
  `other_app_holds_input(self_pid)` (`crates/corti-coreaudio/src/process.rs:53`) enumerates
  `kAudioHardwarePropertyProcessObjectList` (`:115`) and checks each object's
  `kAudioProcessPropertyIsRunningInput` (`:121`), excluding corti's own pid. Attribution of *who*
  is on the call is `mic_owner()` (`:28`), preferring a known conferencing app.

The worker's wait is `min(machine deadline, POLL_INTERVAL)` (`platform.rs:148`): it blocks on
`recv_timeout` when idle, polls at 1 Hz when recording.

## The debounce state machine

`Machine` (`crates/corti-detect/src/machine.rs`) is pure and time-injected (no HAL deps,
unit-tested). It coalesces noisy edges into confirmed `Action::Start` / `Action::Stop{keep,
duration}` (`:24`). States (`:34`): `Idle → Arming → Recording → Coasting`. Timing constants
(`lib.rs`):

| Const | Value | Role |
|-------|-------|------|
| `DEBOUNCE` | 1.5 s | rising-edge confirm before `Action::Start` |
| `COALESCE` | 2 s | falling-edge wait + gap-bridging before `Action::Stop` |
| `MIN_RECORDING` | 3 s | discard floor — shorter captures are dropped |
| `POLL_INTERVAL` | 1 s | during-recording attribution poll |

`Action::Start` → `Recorder::start(&owner.app, owner.pid)` (`platform.rs:189-193`); `Action::Stop`
→ `finish()` (or `discard()` under the floor).

## Capture: aggregate device + process tap

`CaptureSession::start_recording` (`crates/corti-coreaudio/src/capture.rs:287`) builds two
CoreAudio objects:

- a **process tap** via the directly-declared `AudioHardwareCreateProcessTap`
  (`capture.rs:44`) from a `CATapDescription` — per-PID (`initStereoMixdownOfProcesses:`) or global
  (`initStereoGlobalTapButExcludeProcesses:`);
- an **aggregate device** (`create_aggregate`, `:855`) whose clock-leading sub-device is the
  default **input** (mic + tap), or an input-less **output** device for tap-only so no orange mic
  dot appears (`tap_only_clock_device`, `listener.rs:50`). The tap is added with drift
  compensation; the aggregate is private with a fresh per-process UID.

Native format is f32 (4 bytes/sample assumed throughout); sample rate is read from the aggregate's
input stream format (`aggregate_input_format`, `:927`), defaulting to **48000** (`:376`).
CoreAudio, not corti, picks the callback buffer size.

## The `io_proc` contract (ADR 0005)

`io_proc` (`capture.rs:541`) is a real-time C callback and obeys one rule: **never accumulate,
never allocate, never block.** Each fire it interleaves the input buffers frame-major (mic
channel(s) first, then tap — the me/them contract), reserves a slot run on the SPSC ring via
`write_chunk_uninit` (`:594`), writes each f32, and `commit_all`s (`:618`). On ring-full it drops
the **whole** callback and bumps a `dropped` atomic (`:620-623`) rather than stalling the RT thread
(guardrail 9). Channel layout is discovered on the first callback and published via atomics on
`Shared` (`:573`).

## The writer thread & `OutputLayout`

A dedicated `corti-capture-writer` thread (`:416`) runs `run_writer` (`:634`): it blocks until the
first callback publishes channel counts, then per-frame downmixes to the requested `OutputLayout`
(`:150`) and writes incrementally with `hound`. The file is created lazily, so a permission-denied
(zero-callback) run leaves no file behind.

| `OutputLayout` | WAV | Contents |
|----------------|-----|----------|
| `TwoTrack` | 2-ch **32-bit float** (`:663`) | ch0 = mono mic mean, echo-cancelled in flight (#74); ch1 = mono tap mean, untouched |
| `TapOnlyMono` | 1-ch 16-bit (`:664`) | webinar / tap-only |
| `AllChannels` | 16-bit passthrough | debug spike |

`CaptureSession::stop` (`:444`) tears down the io_proc, drops the ring producer (the writer sees
end-of-stream), joins the writer, and returns a `RecordingHandle` (`:173`) with frame/callback/
dropped counts.

**In the app pipeline, PCM is not surfaced as in-memory chunks.** It exists as f32 only transiently
inside `io_proc` and inside `run_writer`'s per-frame `Vec`, and every downstream *app* stage reads
the WAV file — which is why the app transcribes batch (see [architecture.md](architecture.md)
§Corrections). The one in-memory PCM consumer is the optional `CaptureTee`: it attaches to the
`run_writer` ring drain and feeds a live stream — used today by `corti-tap --live`, see
[streaming.md](streaming.md).

## In-flight AEC — on the writer thread (#74)

The AEC kernel is streaming (a frequency-domain block adaptive filter, ADR 0007) and is now driven
**as audio arrives**, on the writer thread, before anything is encoded.

`CaptureOptions::with_filter` hands `start_recording_with_options` a `CaptureFilterFactory` — a
`FnOnce(sample_rate) -> Box<dyn CaptureFilter>`, deferred so the FFT plan is built on the writer
thread once the aggregate's rate is known. The trait is the whole seam between the HAL crate and the
DSP: declared maximum output lag plus `push(&mic,&far) -> Vec<f32>` / `finish() -> Vec<f32>`.
`corti-capture` supplies `StreamingAecFilter`, wired by `RecordingOptions::with_aec`; the config and
effective lookahead are captured once from `LiveHook::aec_config()` when a recording starts.

`FilterStage` is the writer-side buffer: it stages downmixed mic/far into 4096-frame blocks, pushes
them through the filter, and pairs each cleaned mic sample with its FIFO tap partner. `tap_pending`
holds the filter's declared lag and `mic_pending` mirrors it for fail-open recovery (≈2 MB for the
pair at the default 5 s / 48 kHz). The writer checks that declaration after every block and imposes
a 35-second hard ceiling, so an under-emitting implementation cannot grow with call length.

The mirror exists for one reason: **a DSP bug must not cost the recording.** Factory construction,
`push`, and `finish` all run under `catch_unwind`; panic, over-emission, under-emission at finish, or
excess backlog drops the filter and writes pending/later audio raw. `RecordingHandle` distinguishes a
wholly raw fallback from a cleaned-prefix/raw-remainder degradation. No silence is substituted and
frame/tap ordering remains exact. The filter is only installed for `TwoTrack` with a mic channel.

`StreamingAec` (`crates/corti-aec/src/streaming.rs`) is overlap-save FDAF: block hop = `filter_len`
(default 8192 ≈ 170 ms @ 48 kHz), FFT size `2·hop`. A tunable lookahead window
(`CORTI_AEC_LOOKAHEAD_SECS`, default 5 s) warms the filter and locks the room delay before emitting.
Total-emitted == total-pushed across all calls (not per-call), which is what lets the writer keep
exact frame accounting. `cancel(mic,far,sr,cfg)` remains the intentional full-input-lookahead
scoring shim.

**The file-to-file pass survives, but off the ordinary recording path.**
`corti_capture::write_clean_wav` reads a 2-track WAV and writes a `-clean.wav` sibling. It serves
foreign audio (`corti --input`), `corti-bench`, marker-less pre-upgrade queue rows, and a wholly-raw
writer-construction fallback. A 1-channel webinar has no mic → `Ok(None)`. Its RAM is bounded by the
input file, which is why normal app capture never uses it. Both drives produce sample-identical
output (`in_flight_filter_matches_post_capture_pass`).

On successful finish, `CaptureProcessing` records `disabled | not_applicable | applied |
raw_fallback | degraded` plus the exact AEC config/lookahead. The pipeline serializes that versioned
record into `queue.db` before retryable work. Existing rows migrate with NULL identity and retain the
old offline pass; retries skip AEC only for positively identified processed/disabled files. Filed
provenance comes from this immutable record and reports degraded capture instead of reconstructing
AEC from later Settings.

**The live tee is unrelated to this.** `run_writer` still tees the **raw** downmix to `corti-live`,
which runs its own `StreamingAec` for the in-call transcript (#78/#87). Leaving it alone was a
deliberate scope choice: the two cancellers are independent and each bounded.

### Per-block echo statistics — instrumentation, not a gate change (#149 phase 3)

`StreamingAec` records one `BlockStats` per **emitted** block into a bounded ring:
`{ t_start_secs, mic_energy, far_energy, echo_estimate_energy, error_energy, double_talk,
suppressed }`. Nothing here is new DSP — these are the quantities the adaptation gate
(`streaming.rs`, `Σd² > double_talk_ratio · Σx²`) and the residual suppressor already compute, captured
instead of discarded. Energies are sums of squares over the block on the raw `f32` sample scale, so a
ratio of two of them is directly an ERLE-style figure. Emitted audio is unchanged, sample for sample,
whether or not anyone reads the ring (`recording_stats_does_not_change_the_audio`, and the existing
`in_flight_filter_matches_post_capture_pass` parity test still holds).

**The timeline contract is the load-bearing part.** `t_start_secs` is call-relative on the **emitted
(cleaned)** timeline: the offset of the block's first cleaned sample from the first cleaned sample of the
call. Because the filter emits exactly one sample per pushed mic sample, that is also the mic-input
offset — the timestamp the transcriber will put on this audio. **The lookahead is already subtracted.**
The warm-up convergence sub-pass discards its output and records nothing; the opening re-emit starts the
clock at `0.0`. Block *k* covers cleaned samples `[k·filter_len, (k+1)·filter_len)` — of `-clean.wav`,
or of the `push` return stream — with no offset to correct for. A consumer can compare a `BlockStats`
time to a `TranscriptSegment` time directly.

`block_stats(&mut self) -> Vec<BlockStats>` **drains** the ring; `stats_dropped() -> u64` is the lifetime
count of blocks evicted because a consumer drained too slowly. The ring holds `MAX_BLOCK_STATS` = 4096
blocks (≈11.6 min at the default 8192-tap hop / 48 kHz, ≈128 KiB), so a consumer that never drains costs
a fixed amount rather than growing with call length — and the hole is always at the *old* end, counted,
never silent. `finish` consumes the filter, so `finish_with_stats() -> FinishOutput` is how a caller
reaches the blocks the flush itself records (for a call shorter than the lookahead, that is every block)
plus the locked delay.

`span_stats(&[BlockStats], start, end) -> Option<SpanStats>` folds the blocks overlapping a span into
`{ blocks, mean_mic_db, mean_echo_estimate_db, mean_error_db, double_talk_fraction, suppressed_fraction }`.
Means are taken over *energies* and converted to dB once, so one silent block cannot dominate. A span
shorter than one hop collapses to the block containing its start.

**The consumer is the deterministic segment cleanup** (#149 phase 3b; see
[`transcription.md`](./transcription.md) § Audio evidence). `corti_transcribe::segment::cleanup_with_evidence`
takes an `Fn(f64, f64) -> Option<SpanEvidence>` over the transcript timeline; `corti-transcribe` does not
depend on this crate, so `app/src/transcribe.rs::span_evidence` folds `SpanStats` into that four-field
shape. A `Me` row overlapping far-end speech whose span shows `mean_mic_db − mean_echo_estimate_db ≤ 3 dB`,
with `double_talk_fraction < 0.5`, is dropped as echo whatever its wording. The timeline contract above is
what makes that comparison legal: a block time and a `TranscriptSegment` time are the same clock.

**`double_talk` and `suppressed` are recorded separately on purpose.** Today the suppressor's bypass test
is the *same* `Σd² > ratio · Σx²` comparison as the adaptation gate, so `suppressed == !double_talk`
whenever `suppress_residual > 0` — which is exactly #107 root cause #1 (a hot mic freezes adaptation *and*
disengages suppression during far-end-only speech). Keeping both fields means the record stays meaningful
the day those gates diverge, and `gate_flags_agree_with_the_double_talk_regions` fails loudly if they do.
This PR changed no gate math and no default.

**The locked delay is logged once per call**, at `info!(target: "corti::aec", …)` from the delay lock:
`delay_samples`, `delay_ms`, the `max_lag_samples` / `max_lag_ms` search window, the warm-up span, and the
filter length. The delay is estimated once at the end of warm-up and never re-estimated (ADR 0007
Decision 2), so a lock pinned at the edge of a 10 ms window — or at 0 — is visible in one line. This is
#107's cheapest diagnostic.

**Batch/bench sidecar.** `write_clean_wav_with_options(raw, cfg, lookahead, AecStatsSidecar::Write)`
writes `<stem>-aec-stats.json` beside the `-clean.wav`: schema version, source, sample rate, frames,
lookahead, the exact `AecConfig`, the locked delay, the block hop, the drop count, and every block. It
drains per push, so a call longer than the ring loses nothing. Opt-in — `write_clean_wav` and
`write_clean_wav_with_lookahead` pass `Off`, and the cleaned WAV is byte-identical either way.
`corti-bench process --aec` turns it on by default (`--no-aec-stats` to suppress), surfaces the path in
its JSON envelope as `aec_stats`, keeps the sidecar even when the `-clean.wav` is deleted, and feeds it
straight back into its own `--cleanup` pass. The app's offline fallback
(`transcribe_recording`'s `OfflineAec` request) asks for it too.

**In-flight sidecar.** The ordinary app recording never runs that pass — the mic is cleaned in the capture
writer — so the filter produces the same file through its own seam. `StreamingAecFilter` drains
`block_stats()` after every push into a shared, bounded `AecStatsCollector`
(`MAX_CAPTURE_BLOCK_STATS` = 32 768 blocks ≈ 93 min ≈ 1 MiB, oldest evicted and counted), and hands over
its trailing blocks plus the locked delay at `finish`. **`Recorder::stop_capture` writes the file, not the
writer thread**: the trait returns only audio, the writer thread should not do file I/O it can avoid, and —
the load-bearing reason — only the stopping thread knows the `CaptureFilterDisposition`. The sidecar is
written **only for `Applied`**. A `RawFallback` recording is re-cleaned by the offline pass, which writes
its own; a `Degraded` recording is a cleaned prefix plus a raw remainder, so the record would describe
audio the WAV does not contain, and no record is better than a wrong one. Writing it is best-effort and
logged: a diagnostic sidecar never costs a finished recording. `write_clean_wav`'s cleaned WAV and the
capture writer's retained WAV are both byte-identical whether or not anyone collects.

## corti-tap shares the engine, not the app's filter policy

`crates/corti-tap/src/main.rs` constructs the same `Recorder`, but ordinary Ctrl-C/`--inbox` mode
intentionally uses `RecordingOptions::default()` and writes raw mic+tap (its note provenance says
AEC disabled). `--no-mic` remains tap-only. `--live` attaches the raw bounded tee and runs its own
streaming AEC for terminal output. Only the detector-driven app passes `with_aec`, so “no raw mic on
disk” refers to automatic app recordings with AEC enabled, not this diagnostic/manual CLI.
