# Transcription — trait, backends, queue, filing

> Verified against v0.8.0 + feat/pipeline-docs-and-streaming + #85 (durable-jobs stack).

Transcription is one synchronous, blocking trait over a finished 2-track WAV, with two
interchangeable backends behind it (local Parakeet, AWS Transcribe). The pipeline worker runs
`enqueue → transcribe → file → Done` serially; the `Queue` is a durable SQLite store, and #85 added a
`corti-jobs` layer on top of it for durable retry-with-backoff and an hourly retention sweep (see
_The queue_ and _The pipeline worker_ below). Filing shells out to the external `vagus` CLI, the only
subprocess in the note path.

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
arm; `transcribe_recording` (`:132`) is the pipeline entry point — it runs offline AEC via
`corti_capture::write_clean_wav` (`:145`), then hands the cleaned (or raw, if AEC skipped) path to
the backend.

## Local backend — Parakeet via sherpa-onnx

`crates/corti-transcribe-local`. Engine = the official **`sherpa-onnx` Rust crate** (which wraps
ONNX Runtime), **not** raw `ort`. Model is **NVIDIA Parakeet-TDT-0.6B-v3 int8**, CPU provider by
default (CoreML measured 4.6–11× slower on the int8 transducer, ADR 0003).

Per job, `LocalTranscriber::transcribe` (`src/lib.rs:145`) runs:

1. **Discover models** — `models::resolve_dir` (`models.rs:66`) → `~/Library/Caches/corti/models/`
   (guardrail 5, outside any vault); `models::discover` (`:79`) validates the Parakeet
   `{encoder,decoder,joiner}.int8.onnx` + `tokens.txt` under `PARAKEET_DIR` (`:18`), `silero_vad.onnx`
   (`:22`), and — only when far-end diarization is on — pyannote + a selectable embedding model.
   Missing required files bail with a fetch-script hint.
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
5. **Per-region ASR** — `asr_segment` (`:213`): offline transducer `create_stream` →
   `accept_waveform(16000, …)` → `decode` → tokens + timestamps + durations
   (`model_type = "nemo_transducer"`, `:64`).
6. **Token → word** — `tokens_to_words` (`:236`): reassembles SentencePiece subwords into whole
   `Word`s at the `▁` (U+2581) boundary marker (`:242`), shifting timestamps by the region offset.
7. **Shape** (`lib.rs:179-212`): ch0 → `words_to_segments(.., Speaker::Me, ..)`; ch1 →
   `words_to_segments(.., Other("Them"), ..)` by default, or opt-in `diarize_words` when
   `diarize_far_end` is set; then `merge_by_time` → `DiarizedTranscript::new`.

Structurally **batch-only**: it uses the *offline* recognizer over complete regions, decodes the
whole file up front, and merges only after both channels finish. Nothing is emitted mid-run.

Tunable `LocalConfig` defaults (`lib.rs:85-99`): `provider = "cpu"`, `num_threads = 4`,
`diarize_far_end = false`, `vad_threshold = 0.5`, `vad_min_silence = 1.0` (benchmark-tuned up from
Silero's 0.25 — see `design/06-benchmark-harness.md`). Far-end diarization over-clusters on English
audio (issue #18); it stays off by default.

## AWS backend

`crates/corti-transcribe-aws`. Implements the same sync trait (`src/lib.rs:338`) by building a
private `new_current_thread` tokio runtime (`:339`) and `block_on`-ing an upload → start → poll →
fetch → parse against the async SDK. `AwsOptions.job_name` (`:49`) is set to the durable recording
id, so a resumed `Transcribing` job re-attaches to the same AWS job rather than resubmitting;
`start_job` tolerates the resulting `ConflictException` (`:46`). From the pipeline thread's view
this is an ordinary blocking call.

## The queue — durable store + background jobs

`crates/corti-queue`: one SQLite DB in **WAL** at `~/.local/share/corti/queue.db` (override
`$CORTI_DATA_DIR`; outside any vault, guardrail 5). One `recordings` row per recording mirrors `Job`;
`job_id` is the recording filename stem, making everything idempotent on it. #85 added a
`transcribe_secs` column (`src/lib.rs:56`) for the Recording Queue window's "transcribed 55 min in 30 s"
line, and a v0→v1 migration (`:400`) that rewrites every stored timestamp to the fixed-width UTC `…Z`
form so string ordering is chronological.

`JobStatus` (`corti-core recording.rs:119`) is the state machine:

```
Recording → PendingTranscription → Transcribing → PendingNote → Done
                                                              ↘ Failed
```

API the app uses: `enqueue` (`:152`, `INSERT OR IGNORE` — preserves progress on re-enqueue),
`set_status`, `update` (partial via SQL `COALESCE` — only `Some` fields change), plus #85's `retry_reset`
(`:254`), `expired` (`:314`), `delete_terminal_older_than` (`:336`), and `all` (`:301`). `queue.jobs()`
(`:129`) hands out a `corti_jobs::Jobs` borrowing the same live `Connection` for the background-job table.

**Durability is delivered by `corti-jobs` (#85), on top of the queue.** `crates/corti-jobs/src/lib.rs` is
a small background-job layer sharing `queue.db`: kinds are strings with JSON payloads; `claim_due`
(`:142`) marks a row `running` and bumps `attempts` *before* the handler runs, so `recover_running`
(`:250`) can flip any still-`running` row back to due-now at startup — crash recovery of jobs. The
pipeline seeds tray history (issue #3) and then calls `recover_running` on boot
(`app/src/pipeline.rs:126`); `corti --list` and the tray `History ▸` submenu survive restarts. (A crash
during a recording's *first* in-process attempt, before any failure schedules a retry job, still strands
that row — durability is job-level, not a full sweep of non-terminal recordings.)

## The pipeline worker

`app/src/pipeline.rs` — a single `corti-pipeline` thread, the sole `Queue` owner (rusqlite
`Connection` is `Send`, not `Sync`). Its loop (`run`, `:87`) is a **tick**:
`rx.recv_timeout(next_wake(..))` (`:195`) blocks for a new message or until the next background job is
due (clamped to `MAX_IDLE_WAIT = 60 s`, `:44`), then `drain_due_jobs` (`:210`) claims and runs every due
job. Messages are `PipelineMsg::{Process, Retry, ReloadConfig}` (`:47`).

Per `Process` job, `transcribe_and_file` (`:413`): `queue.update(Transcribing)` →
`transcribe::transcribe_recording` (offline AEC then `Backend::transcribe`, a **blocking** call on this
thread) → `queue.update(PendingNote, transcribe_secs)` → `file_and_done` (`:485`); on success the
transient WAVs are pruned (`prune_transient`). It returns `Result` instead of terminal-failing — a
live-path error goes to `schedule_retry` (`:320`), which keeps the row at `PendingTranscription` and
enqueues a durable `retry_transcription` job.

**Retry with backoff.** `retry_transcription` (`app/src/jobs.rs:71`) re-runs `transcribe_and_file` from
the capture audio (no transcript sidecar exists, so a `PendingNote` re-file re-transcribes too). Failed
attempts back off `1 m → 2 m → … → 1 h` cap (`JOB_BACKOFF`, `pipeline.rs:37`) over `RETRY_MAX_ATTEMPTS =
5` (`jobs.rs:23`); exhaustion terminal-fails the recording (`on_exhausted` → `fail`). The Recording Queue
window's Retry button sends `PipelineMsg::Retry`, handled by `manual_retry` (`:266`).

**Retention sweep.** An hourly periodic singleton `sweep_expired` (`jobs.rs:107`), armed by
`enqueue_periodic(SWEEP_EXPIRED, SWEEP_PERIOD = 3600 s)` at `pipeline.rs:135` (also fires at startup),
deletes audio older than `retention_days` (config, default 7) plus the AEC sibling, then GCs terminal
recording rows after 90 days and parked job rows after 30. Rows outlive the audio so history stays
visible in the Recording Queue window.

`ReloadConfig` (sent by the Settings screen on save) rebuilds the backend + AEC toggle between jobs.

## Filing to vagus

`file_and_done` calls `corti_vagus::Vagus::file_recording` (`crates/corti-vagus/src/lib.rs:132`) →
`add_note` (`:100`), which **shells out** to the external `vagus` binary:

```
vagus add-note "<title>" --source "<source>" --print-path   < body-on-stdin
```

`--print-path` skips the editor and prints the created note path, which corti captures. The body
(`recording_body`, `:182`) is an auto-capture context line plus
`DiarizedTranscript::to_markdown()`. The binary is resolved via `$VAGUS_BIN` → `vagus` on `PATH` →
Homebrew/cargo locations (`discover`, `:37`), re-probed on each filing attempt so a mid-session
install works without relaunch.

Ordering is crash-safe: `queue.update(note_path=…)` is persisted **before** `set_status(Done)`
(`file_and_done`, `pipeline.rs:485`), so a retry re-file can't duplicate a note. A transient error →
`schedule_retry` / the retry job's backoff; only unrecoverable states go through `fail` → tray `Failed`.

## Shared types (`corti-core`)

Platform-free, depended on by every crate:

- `transcript.rs`: `Speaker{Me, Other(String)}`, `TranscriptSegment{speaker,start,end,text}`,
  `DiarizedTranscript` + `to_markdown()` (renders `**[mm:ss] Speaker:** text`) — the common backend
  output and filing input.
- `recording.rs`: `RecordingMeta` (+ `mode()`, `note_title()`, `source()`), `RecordingMode{Call,
  Webinar}`, `JobStatus` (`:119`) + `is_terminal()`.

`Word`/`SpeakerTurn` are **not** in core — they are per-backend intermediates in
`corti_transcribe::segment`.
