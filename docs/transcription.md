# Transcription — trait, backends, queue, filing

> Verified against main + transcribe.cpp integration PR #92 and crash-safe rolling live commits (#85, #87, #93, #103).

Transcription is one synchronous, blocking trait over a finished 2-track WAV, with two
interchangeable backends behind it (local Parakeet, AWS Transcribe). The pipeline worker runs
`enqueue → transcribe → file → Done` serially; the `Queue` is a durable SQLite store, and #85 added a
`corti-jobs` layer on top of it for durable retry-with-backoff and an hourly retention sweep (see
_The queue_ and _The pipeline worker_ below). #87 adds a **live** first-class path for detector
recordings: when the local backend + models are available, bounded transcribed/diarized windows are
OS-synced to the note *during* the call and this whole batch machinery becomes the fallback (§Live inbox
filing). Filing shells out to the
external `vagus` CLI, the only subprocess in the note path; after creation corti may also append to /
state-flip / delete that one note (ADR 0010).

## The `Transcriber` trait

`crates/corti-transcribe/src/lib.rs:23`:

```rust
pub trait Transcriber {
    fn transcribe(&self, audio: &Path, meta: &RecordingMeta) -> Result<DiarizedTranscript>;
}
```

- **Batch, whole-WAV.** Input is a `&Path` to a complete on-disk 2-track WAV; output is one
  fully-formed `DiarizedTranscript`. No chunk/partial surface — the batch shape is the contract.
- **Synchronous by design** (no `async-trait`/tokio in the trait). Async backends run a private
  runtime internally (§AWS).
- The 2-track layout **is** the diarization prior: ch0 → `Speaker::Me`, ch1 → `Speaker::Other`.

Shared post-processing lives in `corti_transcribe::segment` (`segment.rs`): `Word{start,end,text}`
(`:16`), `SpeakerTurn` (`:24`), `words_to_segments` (`:38`), `merge_by_time` (`:73`),
`diarize_words` (`:83`), and `SEGMENT_GAP = 1.5s` (`:33`). Both backends emit timestamped `Word`s
and reuse these to shape the final transcript.

## Backend dispatch

`app/src/transcribe.rs` selects a backend at runtime from config
(`AppConfig::transcribe_backend`, env `CORTI_TRANSCRIBE_BACKEND`). `Backend::init` (`:37`) resolves
`BackendChoice::{Aws,Local}` behind cfg-gated features; unavailable backends degrade to a stringly
error rather than failing the build. `Backend::transcribe` (`:60`) dispatches to the AWS or local
arm; `transcribe_recording` (`:132`) is the pipeline entry point. Its `AecConfig` argument is `Some`
only for **foreign** audio (`corti --input <wav>`), cleaned via `corti_capture::write_clean_wav`;
recordings arrive already echo-cancelled by the capture writer (#74), so the pipeline passes `None`
and hands the path straight to the backend.

## Local backend — Parakeet via sherpa or transcribe.cpp

`crates/corti-transcribe-local` always uses **NVIDIA Parakeet-TDT-0.6B-v3**, with a selectable
per-region inference runtime behind `Asr::{Sherpa,Ggml}`:

- `sherpa` (compatibility/default): int8 ONNX through the official `sherpa-onnx` Rust crate / ONNX
  Runtime on CPU. CoreML measured 2.7–11× slower and is not shipped (ADR 0003).
- `ggml`: the official Q8_0 GGUF through transcribe.cpp/GGML on Metal (ADR 0011), included in standard
  app builds and selected in Settings or with `CORTI_LOCAL_ASR_ENGINE=ggml`. Corti pins upstream revision
  `553f1099…` exactly while 0.2.0 is unreleased.

Only ASR decode changes. Resampling, Silero VAD, optional pyannote/embedding diarization, speaker shaping,
and timeline merge remain on the shared sherpa-backed path.

Per job, `LocalTranscriber::transcribe` (`src/lib.rs:145`) runs:

1. **Discover models** — `models::resolve_dir` → `~/Library/Caches/corti/models/` (guardrail 5,
   outside any vault). Engine-aware discovery requires either the Parakeet
   `{encoder,decoder,joiner}.int8.onnx` + `tokens.txt` set (`sherpa`) or the verified Q8_0 GGUF (`ggml`),
   plus shared `silero_vad.onnx` and — only when far-end diarization is on — pyannote + the selected
   embedding. Settings → Models shows only the selected ASR representation and downloads it with a pinned
   size/SHA-256; missing files fail clearly.
2. **Decode the WAV** — `audio::read_two_track` (`audio.rs`) `hound`-decodes the whole file (int16
   or float32) and deinterleaves to `mic`/`them` f32 at source rate. Mono → all `them`.
3. **Per channel** (`lib.rs:171-208`): `engine::resample_to_16k` (`engine.rs:35`) via sherpa
   `LinearResampler` (VAD/diarizer don't resample internally, so 16k is fed everywhere); a fresh
   Silero VAD (`build_vad`, `:92`); then `transcribe_channel` (`:185`).
4. **VAD chunking** — `transcribe_channel` now drives a `LiveTranscriber` (one whole-channel `push`
   + `finish`; batch and live share a single decode path — see [streaming.md](streaming.md)), which
   feeds one Silero VAD in **512-sample windows** (`VAD_WINDOW = 512`, `:30`) and drains completed
   speech regions, each capped at `MAX_SPEECH_SECONDS = 20` (`:32`). **No overlap** — regions are
   non-overlapping and VAD-delimited, sidestepping Parakeet's ~30 s clip limit and its
   empty-on-silence bug.
5. **Per-region ASR** — `Asr::asr_segment` dispatches the same 16 kHz VAD region. Sherpa creates an
   offline recognizer stream and reassembles timestamped SentencePiece tokens. transcribe.cpp runs one
   resident mutex-serialized session with word timestamps, falling back to segment/text rows rather than
   silently losing speech. Both lift region-relative times by the same call offset.
6. **Token/result → word** — sherpa reassembles subwords at the `▁` (U+2581) boundary; GGML maps owned
   transcribe.cpp word rows. Both produce the shared `Word { start, end, text }` type.
7. **Shape** (`lib.rs:179-212`): ch0 → `words_to_segments(.., Speaker::Me, ..)`; ch1 →
   `words_to_segments(.., Other("Them"), ..)` by default, or opt-in `diarize_words` when
   `diarize_far_end` is set; then `merge_by_time` → `DiarizedTranscript::new`.

The `Transcriber` trait entry remains whole-WAV/batch, but it drives the same `LiveTranscriber` core used
by live filing. `checkpoint()` resets bounded resampler/VAD state while retaining either resident ASR model
and a cumulative timestamp epoch (ADRs 0009/0012).

Tunable `LocalConfig` defaults include `asr_engine = "sherpa"` (upgrade-safe compatibility),
`provider = "cpu"`, `num_threads = 4`, `diarize_far_end = false`, `vad_threshold = 0.5`, and
`vad_min_silence = 1.0`. On the M1 Pro excerpt benchmark, transcribe.cpp/Metal was 4.09× faster with 19%
lower peak RSS at identical normalized WER; see ADR 0011 and `bench/results/transcribe_cpp_round1.jsonl`.
Far-end diarization over-clusters on English audio (issue #18); it stays off by default.

## AWS backend

`crates/corti-transcribe-aws`. Implements the same sync trait (`src/lib.rs:338`) by building a
private `new_current_thread` tokio runtime and `block_on`-ing an attach-or-upload → start → poll →
fetch → parse flow against the async SDK. `AwsOptions.job_name` is the stable name persisted on the
recording row, so a retry probes AWS first and re-attaches without re-encoding or uploading the full call.
A terminally failed job is deleted so the stable name can start a fresh attempt. Before upload/start, the
app atomically publishes `<recording-stem>.aws-staging.json` with the exact bucket/prefix/job/region; transient
poll/fetch/parse failures therefore retain both staged objects **and their durable owner**. A successful
result carries the same identity into the transcript checkpoint. Backend/bucket/region changes first clean
the old marker, cleanup completion is persisted before filing, and terminal pipeline exhaustion hands any
remaining marker to a separate effectively non-exhausting cleanup job. Fresh `--redo --aws` attempts use
unique names and attempt S3 cleanup on failure as well as success because they have no reattachment owner.
From the pipeline thread's view this remains an ordinary blocking call.

## The queue — durable store + background jobs

`crates/corti-queue`: one SQLite DB in **WAL** at `~/.local/share/corti/queue.db` (override
`$CORTI_DATA_DIR`; outside any vault, guardrail 5). One `recordings` row per recording mirrors `Job`;
`job_id` is the recording filename stem, making everything idempotent on it. #85 added a
`transcribe_secs` column (`src/lib.rs:56`) for the Recording Queue window's "transcribed 55 min in 30 s"
line, and a v0→v1 migration (`:409`) that rewrites every stored timestamp to the fixed-width UTC `…Z`
form so string ordering is chronological.

`JobStatus` (`corti-core recording.rs:119`) is the state machine:

```
Recording → PendingTranscription → Transcribing → PendingNote → Done
                                                              ↘ Failed
```

`PendingNote` has a durable meaning: `<recording-stem>.transcript.json` was atomically written beside the
raw recording and contains a versioned `DiarizedTranscript`, an optional existing/returned note path plus
partial/canonical provenance, and any exact AWS staging location still awaiting deletion. A canonical path
means vagus/live filing already completed the note body, so only SQLite completion remains even if the path
has moved. Only cloud cleanup (when marked) and filing/completion remain.

API the app uses: `enqueue` (`:155`, `INSERT OR IGNORE` — preserves progress on re-enqueue),
`set_status`, `update` (partial via SQL `COALESCE` — only `Some` fields change), `retry_reset`, `all`,
and per-row terminal deletion after artifact cleanup. `queue.jobs()` hands out a `corti_jobs::Jobs`
borrowing the same live `Connection` for the background-job table.

**Durability is delivered by `corti-jobs` (#85), on top of the queue.** `crates/corti-jobs/src/lib.rs` is
a small background-job layer sharing `queue.db`: kinds are strings with JSON payloads; `claim_due`
(`:142`) marks a row `running` and bumps `attempts` *before* the handler runs, so `recover_running`
(`:250`) can flip any still-`running` row back to due-now at startup — crash recovery of jobs. The
pipeline seeds tray history and then calls `recover_running` on boot; `corti --list` and the tray
`History ▸` submenu survive restarts. It also scans for valid filing checkpoints and schedules them
immediately, including the checkpoint-written/`PendingNote`-write-failed window. A crash during a
recording's *first* in-process attempt **before** the post-ASR checkpoint can still strand that row — this
is not a full sweep of non-terminal recordings. Rows stranded at `Recording` are the other narrow
exception: #87's startup reaper revives or fails them — see §Live inbox filing.

## The pipeline worker

`app/src/pipeline.rs` — a single `corti-pipeline` thread, the sole `Queue` owner (rusqlite
`Connection` is `Send`, not `Sync`). Its loop (`run`, `:101`) is a **tick**:
`rx.recv_timeout(next_wake(..))` (`:185`) blocks for a new message or until the next background job is
due (clamped to `MAX_IDLE_WAIT = 60 s`, `:45`), then `drain_due_jobs` (`:324`) claims and runs every due
job. Messages are `PipelineMsg::{Process, Retry, ReloadConfig}` plus #87's live-filing messages (`:48`).

Per `Process` job, `transcribe_and_file`: `queue.update(Transcribing)` → publish any exact pre-ASR AWS
owner → `transcribe::transcribe_recording` (`Backend::transcribe`, a **blocking** call on this thread —
the recording is already echo-cancelled, #74) → atomically write the filing checkpoint →
`queue.update(PendingNote, transcribe_secs)` → clean AWS staging (when applicable) → file. Cloud cleanup
errors propagate and retry from the checkpoint. Completion is one `Queue::complete_with_note` SQL update
for `note_path + Done`; errors propagate before any success UI. On durable success the checkpoint is
removed, while audio remains for the configured retention sweep.

**Retry with backoff.** A valid checkpoint is authoritative in any nonterminal transcription state, so a
failed/crashed adjacent `PendingNote` write cannot repeat ASR. Without one, `PendingTranscription` and
`Transcribing` run ASR from retained raw audio; `PendingNote` is a legacy row and falls back once using a
new persisted stable AWS name. Failed attempts back off
`1 m → 2 m → … → 1 h` cap over `RETRY_MAX_ATTEMPTS = 5`; filing backoff remains visibly `PendingNote`,
while transcription backoff returns to `PendingTranscription`. Exhaustion persists the recording's terminal
state before parking its job; unresolved AWS cleanup continues under its own durable job. The Recording
Queue window's Retry button starts a fresh manual attempt budget and remains available when raw audio is
gone but a validated filing checkpoint survives.

**Retention sweep.** An hourly periodic singleton `sweep_expired` (`jobs.rs:107`), armed by
`enqueue_periodic(SWEEP_EXPIRED, SWEEP_PERIOD = 3600 s)` at `pipeline.rs:175` (also fires at startup),
deletes raw audio older than `retention_days` (config, default 7), plus clean/checkpoint leftovers and
crash-left atomic-write temps, then GCs terminal recording rows after `max(90, retention_days)` days and
parked job rows after 30. One timestamp defines both horizons, and a failed artifact deletion retains the
path-bearing row for a later sweep. A row/checkpoint with unresolved AWS staging is never swept or GCed:
the exact cloud address remains until deletion is acknowledged.

`ReloadConfig` (sent by the Settings screen on save) rebuilds the backend + AEC toggle between jobs.

## Live inbox filing (#87)

For detector recordings, transcription + filing now happen **while the call records**, so `tail -f`
on the note shows the conversation arriving. The wiring (tee → AEC → `LiveTranscriber`s → note) is in
[streaming.md](streaming.md#in-app-consumer--live-inbox-filing-87-adr-0010); the write authority is
[ADR 0010](../design/adr/0010-live-inbox-filing.md). ADR 0013 adds a separate transient reader: each closed
VAD region reaches the timestamped Live Transcript window immediately, before the configured durable-note
boundary, through a 2,000-row/~1 MiB bounded store. That observer never owns filing, fallback, or note state.
The filing semantics:

- **State-line + storage contract (for inbox agents).** The first corti-authored body line is exactly
  `State: transcribing` while windows stream in, and exactly `State: transcribed ` — one trailing space,
  same byte width — once final. The flip is a same-inode seek+write. ADR 0012 makes persistence explicit:
  the initial note + parent directory, every complete transcript chunk, body rewrites, and the final flip
  call `sync_all`. The final short chunk is synced *before* the state is synced. Batch notes carry the same
  final line, so inbox agents have one contract.
- **Lazy note creation.** The note is created (`vagus add-note --print-path`, initial body =
  `live_initial_body`) on the **first non-empty committed window**, not at recording start. If discarded,
  the session deletes it. Corti retains the path before the fallible first sync, then publishes it to the
  queue only after syncing the file and directory (`PipelineMsg::LiveNoteCreated`, status `Recording`).
- **Configurable bounded windows.** `live_buffer_minutes` / `CORTI_LIVE_BUFFER_MINUTES` defaults to 1 and
  is clamped to 1–10. Input is split at the boundary; `LiveTranscriber::checkpoint` force-closes both VAD
  tails without reloading models and preserves call-relative timestamps. The one optional far-end PCM window
  is capped at 128 MiB and pending text at 1 MiB; either commits early. The capture tee is fixed at at most
  ~64 MiB. No audio/text collection grows with call duration. After the first commit, a crash preserves every
  prior window; ordinary uncommitted note loss is the configured interval, plus any fixed/bounded decoder-tee lag.
- **Diarize, merge, append once.** If far-end diarization is enabled, its models are loaded once and only the
  active tap window is processed; words become `Them N` before any bytes for that window are written. Mic +
  far segments are merged by start, rendered with the batch formatter, and appended in one synced write.
  `Them N` numbers are window-local and may be renumbered at a boundary (ADR 0012).
- **Finish ownership and quality.** After the recorder closes its tee, the detector calls
  `LiveHook::finished(meta)` before emitting `RecordingFinished`. `LiveManager::finish(id)` freezes the
  dropped-chunk count and keeps the handle/outcome by ID while it flushes AEC and both transcriber tails.
  The later `Process` calls `collect(id)`. Only a zero-drop result flips the state line and skips batch;
  its `note_path + Done` completion is one fallible SQL update, and raw audio remains revealable until
  retention expiry. A collecting/finishing sentinel prevents a second model-backed session from
  overlapping the tail join. Live work adds no telemetry stage; it remains under `Recording`.
- **Fallback — no double notes, no repeated ASR after checkpoint.** Factory ineligible (config off,
  non-local backend, any configured model missing), no note created (silent call), a live-path error, or a dropped tee
  chunk ⇒ batch runs from the lossless WAV. A returned partial path is passed directly as the preferred
  rewrite target and persisted in the retry payload before fallible row repair. Successful batch ASR puts
  that path and transcript in the post-ASR checkpoint; subsequent filing/completion retries load the
  checkpoint, rewrite the same path/inode, and never invoke ASR again. Missing-audio failure and exhaustion
  also retain/close the directly-owned path. Webinar/manual captures have no live hook and always take batch.
- **Discard.** The detector similarly delivers `discarded(meta)` before `RecordingDiscarded`. The live
  thread deletes its partial note; its reaper remains manager-owned and inside the one-model gate until
  decode/drain and cleanup finish. If reaper spawn fails, the original handle/reporter are retained and the
  pipeline performs the join. If unlink fails, the path is reported for another attempt and then retained in
  a Failed row/closed note rather than being forgotten at `State: transcribing`.
- **Ephemeral microphone test.** When no call/webinar/transcription is active, the tray can load the same
  selected local ASR/VAD and feed it a direct default-microphone capture. Detector edges are paused before the
  microphone opens and resumed after it closes. Test text is retained only in the bounded UI store; no WAV,
  queue row, checkpoint, note, or cleanup job exists.
- **Startup reaper.** A quit/crash mid-call can strand a row at `Recording` (created by the live
  note's mid-call persist). At startup the worker reaps them (`reap_recording_rows`,
  `app/src/pipeline.rs:623`): audio still on disk → reset to `PendingTranscription` + a due-now
  durable retry (which rewrites the note in place); audio gone → terminal `Failed`, with the note's
  state line flipped and an "incomplete" annotation appended (`close_out_note`, `pipeline.rs:676` —
  every terminal failure path does this) so no inbox agent waits on `State: transcribing` forever.

## Filing to vagus (batch path)

`file_and_done` calls `corti_vagus::Vagus::file_recording` (`crates/corti-vagus/src/lib.rs:134`) →
`add_note` (`:102`), which **shells out** to the external `vagus` binary:

```
vagus add-note "<title>" --source "<source>" --print-path   < body-on-stdin
```

`--print-path` skips the editor and prints the created note path, which corti captures. The body
(`recording_body`, `:186`) is the `State: transcribed ` line (#87), an auto-capture context line, and
`DiarizedTranscript::to_markdown()`. The binary is resolved via `$VAGUS_BIN` → `vagus` on `PATH` →
Homebrew/cargo locations (`discover`, `:39`), re-probed on each filing attempt so a mid-session
install works without relaunch.

After `vagus add-note` returns, its path is marked canonical in the local checkpoint, then
`Queue::complete_with_note` atomically persists `note_path + Done`. A completion failure leaves the row at
`PendingNote`; retry performs completion only and trusts the canonical path even when vagus has moved it.
If both local writes fail, a structured error transfers the path/provenance into the durable retry payload;
job rescheduling persists that replacement payload in the same SQL settlement. The irreducible process-death
window while the external `vagus` process creates a note but before Corti receives its returned path is not
claimed as exactly-once.

## Shared types (`corti-core`)

Platform-free, depended on by every crate:

- `transcript.rs`: `Speaker{Me, Other(String)}`, `TranscriptSegment{speaker,start,end,text}`,
  `DiarizedTranscript` + `to_markdown()` (renders `**[mm:ss] Speaker:** text`) — the common backend
  output and filing input.
- `recording.rs`: `RecordingMeta` (+ `mode()`, `note_title()`, `source()`), `RecordingMode{Call,
  Webinar}`, `JobStatus` (`:119`) + `is_terminal()`.

`Word`/`SpeakerTurn` are **not** in core — they are per-backend intermediates in
`corti_transcribe::segment`.
