# Transcription — trait, backends, queue, filing

This guide describes Corti's transcription contracts: backend dispatch, cleanup, local and AWS
recognition, durable retries, and filing. Local detector recordings can sync transcript windows
while a call runs; batch transcription handles other recordings and live fallback.

The pipeline serializes queue work on one thread. Vagus creates notes; Corti's later writes are
limited to the note returned for that recording. See [ADR 0010](../design/adr/0010-live-inbox-filing.md)
for write authority and [quality accounting](quality.md) for missing or degraded audio/text.

## The `Transcriber` trait

The base method in [`corti-transcribe`](../crates/corti-transcribe/src/lib.rs) is synchronous:

```rust
pub trait Transcriber {
    fn transcribe(&self, audio: &Path, meta: &RecordingMeta) -> Result<DiarizedTranscript>;
}
```

- **Batch, whole-WAV.** Input is a `&Path` to a complete on-disk 2-track WAV; output is one
  `DiarizedTranscript`. Streaming uses the separate `LiveTranscriber` API.
- **Synchronous by design** (no `async-trait`/tokio in the trait). Async backends run a private
  runtime internally (§AWS).
- **Quality-aware batch.** `transcribe_with_quality` returns `TranscriptionOutcome`: transcript,
  settled `QualityRecord`, and `decode_complete`. The default preserves incoming quality; the local
  override records failed/partial regions rather than treating them as silence. A complete decode
  does not imply complete source capture.
- The 2-track layout **is** the diarization prior: ch0 → `Speaker::Me`, ch1 → `Speaker::Other`.

Both backends emit timestamped `Word`s and use [`corti_transcribe::segment`](../crates/corti-transcribe/src/segment.rs)
for word grouping (`SEGMENT_GAP = 1.5 s`), speaker shaping, time merge, and cleanup.

## Backend dispatch

`app/src/transcribe.rs` selects a backend at runtime from config
(`AppConfig::transcribe_backend`, env `CORTI_TRANSCRIBE_BACKEND`). `Backend::init` resolves
`BackendChoice::{Aws,Local}` behind cfg-gated features; a backend absent from the build returns an explicit error. `Backend::transcribe` dispatches to the AWS or local
arm; `transcribe_recording` is the pipeline entry point. Its optional `OfflineAec` request is used
for foreign audio (`corti --input`), marker-less pre-upgrade rows, and a writer filter that failed
before emitting any clean frame. Ordinary recordings carry a versioned `CaptureProcessing` record
in `queue.db`, so retries skip the file pass only when capture positively identifies the WAV as
processed/disabled. The request includes the original lookahead rather than re-reading environment.

<a id="segment-cleanup--echo-fragments-backchannels-149"></a>

## Segment cleanup — protect words, retain suspected duplicates (#149, #161)

Cleanup runs on the merged channel timeline, before filing or hosted post-processing: after batch
recognition in app/src/transcribe.rs and at each live window flush in app/src/live.rs. The live early-drop
path uses the same echo verdict before publishing a held mic region. Rules version 4 requires acoustic evidence for echo deletion and protects critical words.

The deterministic passes run in this order:

1. **Sort, then judge echoes.** Cross-channel text containment identifies a suspected duplicate, never
   permission to delete one. The candidate window is
   O.start ≤ S.start ≤ O.end + echo_window_seconds. Three or more content tokens use the containment
   threshold; one or two require total containment and a subject span inside the source. Current-window
   segments and retained raw live decoder polls supply candidates. A later-starting overlapping source
   is also checked in the reverse direction, so arrival order cannot hide a protected correction.
   Carry itself is read-only and is never emitted.
2. **Merge fragments.** Consecutive same-speaker segments within merge_gap_seconds are joined unless the
   other channel started a turn in between. This preserves turns that words_to_segments separated at
   SEGMENT_GAP (1.5 seconds).
3. **Drop backchannels.** A turn of fewer than four words whose complete normalized text is a
   backchannel, such as “Yeah.”, may be removed when it overlaps the other channel. An answer to the
   nearest earlier other-channel question is kept. Raw numbers, identifiers and protected unit tokens
   prevent an alphabetic normalization from turning an answer into a backchannel.
4. **Strip fillers and stutters.** The optional pass removes filler tokens, collapses repeated
   words/bigrams and repairs punctuation and capitalization. It preserves quantity-adjacent tokens,
   meaningful case and markers that normalization would erase, so “15 Ah” is not a filler and “15 mm
   15 mM” is not a stutter. Unmarked medial filler spellings are retained as ambiguous content, including
   units after worded quantities. Leading and comma-marked ordinary hesitations remain eligible.
   Capitalization repair keeps unknown names and units intact; ordinary subject pronouns retain the
   existing sentence-repair behavior. Case-only stutter equivalence is limited to closed grammar,
   subject pronouns and “very”: “Very very important.” still collapses, while “May may approve” stays.
   The pass remains optional.
5. **Apply the lexicon.** The TextRule seam runs last on surviving segments, including when
   structural cleanup is disabled.

### Echo decisions require acoustic evidence

A matching text can be either an echo or an independently spoken reply. Under rules 4, a match without
usable acoustic evidence stays in the transcript and its in-memory decision records a suspected
duplicate. In particular, **AWS and corti-tap --inbox perform no echo deletion**: neither path supplies
the AEC evidence that could authorize one. They still run the other enabled cleanup passes.

Before acoustic deletion, a cross-channel text match is checked against the original text for changed
numbers, units, versions, identifiers, names and English/Spanish polarity. These values are not compared
through the older alphabetic content-token normalizer, which would erase digits and discard negators.
A protected difference keeps the reply even when the acoustic predicate would otherwise pass.
Unfamiliar lexical tokens are treated conservatively rather than assumed not to be names. The ordered,
case-sensitive raw signature trims surrounding sentence punctuation and retains every filler spelling:
a word that resembles hesitation can also be a unit. Only the closed grammar list
a/an/the/am/is/are/was/were/be/been/being is exempt, in ordinary case, outside numeric context, and without
meaningful markers. Numeric tokens and both immediate neighbours are protected;
“A”/“Ah”, “mm”/“mM” and a missing unit therefore remain distinct. Internal punctuation, leading decimal
points, signs, identifiers, currency and percent markers survive. No broader language-model rewrite or
confidence threshold is introduced.

A protected input row keeps a flag independent of the bounded decision log. Sorting carries its original
input index; merging propagates protection from either input. Protected rows bypass automatic
backchannel, filler, stutter and capitalization changes, even if the explanatory decision was omitted
at the cap. The explicit lexicon still runs last. Thus a protected “Got IT.” or “square um” cannot be lost
after the echo pass. Short case variants with no content tokens use equality of a nonempty normalized
token sequence inside the same channel/time gates; unrelated short answers do not become matches.

Deletion is possible only for a **Me** segment when all of the existing acoustic conditions hold:

1. Its interval overlaps far-end speech: S.start < O.end and O.start < S.end.
2. The evidence covers at least one AEC block.
3. The double-talk fraction is below 0.5.
4. The finite difference mic_db − echo_estimate_db is at most echo_audio_margin_db (default 3.0).

A Them segment is never deleted as an echo of Me: the canceller measures the mic, not contamination of
the tap. No accessor, no covered blocks, a non-finite energy difference, or a firing double-talk guard
cannot authorize deletion. The existing audio-only predicate can still identify a garbled mic ghost
whose text has no matching source; raw critical-value protection applies when a cross-channel text
match exists. Acoustic deletions are counted in CleanupStats.echo_dropped_audio. The legacy
echo_dropped_me and echo_dropped_them text-deletion counters remain zero.

SpanEvidence contains mic_db, echo_estimate_db, double_talk_fraction and blocks. An AudioEvidence
accessor maps a segment's start/end seconds to an optional summary. The transcribe crate has no
production DSP dependency: the caller converts corti_aec span statistics into that summary.

The double-talk flag comes from the AEC's raw-energy adaptation gate. It is a conservative guard for
this policy, not proof of a person's identity or a calibrated near-speech detector. Gain-sensitive false gating and near-end attenuation remain known limits; the predicate does not
claim to solve them. See [the AEC limits](../design/04-corti-aec.md).

### Evidence, decisions and live publication

- **Live:** the canceller's bounded block-statistics buffer supplies both the early verdict and window
  cleanup. A short mic region may wait for the far-end region or its existing echo-window deadline.
  Release preserves its original words and timestamps; even a region suppressed from publication is
  returned with its decision so a later evidence consumer can retain its input. Missing evidence means
  the suspected duplicate is published. FIFO release and hold limits remain in effect. Both early
  publication and filing retain each original returned poll on both tracks before pause splitting,
  diarization or lexicon changes. A poll can contain several VAD regions; this inventory does not infer
  their exact identities. Every microphone child shares its raw poll context and is compared again at
  release and filing, including against far-end results that arrived later. An early protected decision
  stays attached to its microphone group. Far-end
  raw polls survive every filing window still inside the lookback; they are never rendered from the
  inventory.
  Both whole polls and their raw pause-separated children remain comparison sources: whole-poll
  protection supplements each child's own match. Child text and intervals are included in the same
  logical cap. Only those real speech intervals authorize acoustic overlap. A microphone region inside
  a known far-end pause cannot be deleted using
  the whole decoder result's envelope. Each inventory holds at most 256 sources and 1 MiB of logical
  source data, including child text and metadata. If either cap omits a source, acoustic and automatic
  text deletion are disabled for affected mic polls and children over its remaining lookback. That
  missing-context protection remains attached to a released child; only omission counts enter tracing.
  Mic poll contexts have a separate 1 MiB per-window logical budget, charged once and shared by child
  regions. Explicitly omitted context disables acoustic and automatic text deletion for affected mic
  children; an incomplete mic inventory also protects far-end descendants. Original far poll end
  indices are charged to the existing window text budget. Successful window reset releases these
  contexts and indices; none is reconstructed by text matching or by the bounded decision log.
- **Batch:** the local backend reads the existing -aec-stats.json sidecar beside the recording. Capture
  and the offline clean-WAV pass both use this path. Missing, corrupt or unsupported statistics are
  treated as absent, so they cannot cause echo deletion.
- **AWS and corti-tap:** evidence is absent. Text-only duplicates are kept, and the provenance records
  the acoustic requirement explicitly.

Cleanup decisions are available in memory with the input stage/span, change, reason and acoustic
evidence where applicable. These content-bearing decisions are not tracing fields. The optional
[local evidence store](local-transcript-evidence.md) persists bounded original rows and decisions.

### Settings and provenance

The defaults remain echo_drop = true, echo_window_seconds = 6.0, echo_containment = 0.7,
merge_gap_seconds = 2.5, drop_backchannels = true, echo_audio_margin_db = 3.0 and strip_fillers = true.
The app reads cleanup settings and environment overrides with the bounds in
[`config.rs`](../app/src/config.rs).
A sufficiently negative acoustic margin disables echo deletion while leaving suspected-duplicate
decisions and other enabled passes available. Turning off echo_drop also disables live early-drop.

Every note records corti.configuration.segment_cleanup, mirrored by corti-tap --inbox. It is “off” when
every pass is disabled; otherwise it includes the existing effective settings and these policy fields:

~~~json
{
  "rules": 4,
  "echo_policy": "acoustic_required_mic_only",
  "text_only_duplicates": "keep_suspected",
  "critical_text_protection": true,
  "live_early_drop": false,
  "audio_evidence": false
}
~~~

The example represents a completed recording without acoustic evidence; its other cleanup settings and
lexicon identity are omitted here for brevity. live_early_drop identifies whether the live region pass
was enabled. audio_evidence reports availability, not a user setting or a guarantee that every span has
covered blocks. False means that no echo deletion is permitted by this policy.

### Offline evaluation

corti-bench process --cleanup/--no-cleanup invokes the same cleanup after ASR; with --aec it can use
the sidecar it produced. corti-bench clean can replay an existing transcript with optional --aec-stats
and --echo-audio-margin-db arguments.

bench/scoring/cleanup.py reports text similarity, fragmentation and retention diagnostics over JSON or
display-only Markdown replay. Its historical echo_pairs_remaining field counts **suspected text
matches**, not acoustically established echoes; lower is not inherently better. A genuine repeated
command must remain when evidence cannot distinguish it from echo. Likewise, orphan_drops is a text
diagnostic, not proof that a removed row was genuine speech or that a similar surviving row preserves a
correction. A dropped row is textually accounted for only when its raw critical signature survives in
order and contiguously in the neighbouring text, as it can after a fragment merge; content containment
alone no longer hides a changed number or negator. The backchannel exemption uses the same raw-safe
classification as production: “Okay, 15000.” and “I see 15000.” are answers with critical values. The
historical alphabetic content-retention metric remains unchanged; inspect orphan_drops and the raw
decisions when that metric cannot distinguish values. Use labelled cases to assess false-positive
deletion.

## Local backend — Parakeet via sherpa or transcribe.cpp

`crates/corti-transcribe-local` always uses **NVIDIA Parakeet-TDT-0.6B-v3**, with a selectable
per-region inference runtime behind `Asr::{Sherpa,Ggml}`:

- `sherpa` (compatibility/default): int8 ONNX through the official `sherpa-onnx` Rust crate / ONNX
  Runtime on CPU, the fixed ONNX execution provider (ADR 0003).
- `ggml`: the official Q8_0 GGUF through transcribe.cpp/GGML on Metal (ADR 0011), included in standard
  app builds and selected in Settings or with `CORTI_LOCAL_ASR_ENGINE=ggml`. The exact native
  revision is pinned in [`corti-transcribe-local/Cargo.toml`](../crates/corti-transcribe-local/Cargo.toml).

Only ASR decode changes. Resampling, Silero VAD, optional pyannote/embedding diarization, speaker shaping,
and timeline merge remain on the shared sherpa-backed path.

Per job, [`LocalTranscriber::transcribe_with_quality`](../crates/corti-transcribe-local/src/lib.rs) runs:

1. **Discover models** — `models::resolve_dir` → `~/Library/Caches/corti/models/` (guardrail 5,
   outside any vault). Engine-aware discovery requires either the Parakeet
   `{encoder,decoder,joiner}.int8.onnx` + `tokens.txt` set (`sherpa`) or the verified Q8_0 GGUF (`ggml`),
   plus shared `silero_vad.onnx` and — only when far-end diarization is on — pyannote + the selected
   embedding. Settings → Models shows only the selected ASR representation and downloads it with a pinned
   size/SHA-256; missing files fail clearly.
2. **Decode the WAV** — `audio::read_two_track` (`audio.rs`) `hound`-decodes the whole file (int16
   or float32) and deinterleaves to `mic`/`them` f32 at source rate. Mono → all `them`.
3. **Per channel** (`lib.rs`): `engine::resample_to_16k` (`engine.rs`) via sherpa
   `LinearResampler` (VAD/diarizer don't resample internally, so 16k is fed everywhere); a fresh
   Silero VAD (`build_vad`); then `transcribe_resampled_channel` with source-frame/rate metadata.
4. **VAD chunking** — the production wrapper drives a `LiveTranscriber` with one whole-channel
   `push_resampled` + `finish` and retains the decode report and completeness result. The test-only
   `transcribe_channel` wrapper uses `push` at 16 kHz and returns words. Both share the live decode
   path ([streaming.md](streaming.md)), feeding one Silero VAD in **512-sample windows**
   (`VAD_WINDOW = 512`). `MAX_SPEECH_SECONDS = 20` is the configured maximum speech duration;
   native settlement is not a hard cut at 20 seconds. Regions are non-overlapping and VAD-delimited.
   Failed or partial regions retain explicit quality outcomes.
5. **Per-region ASR** — `Asr::asr_segment` dispatches the same 16 kHz VAD region. Sherpa creates an
   offline recognizer stream and reassembles timestamped SentencePiece tokens. transcribe.cpp runs one
   resident mutex-serialized session with word timestamps, falling back to segment/text rows rather than
   silently losing speech. Both lift region-relative times by the same call offset.
6. **Token/result → word** — sherpa reassembles subwords at the `▁` (U+2581) boundary; GGML maps owned
   transcribe.cpp word rows. Both produce the shared `Word { start, end, text }` type.
7. **Shape** (`lib.rs`): ch0 → `words_to_segments(.., Speaker::Me, ..)`; ch1 →
   `words_to_segments(.., Other("Them"), ..)` by default, or opt-in `diarize_words` when
   `diarize_far_end` is set; then `merge_by_time` → `DiarizedTranscript::new`.

The `Transcriber` trait entry remains whole-WAV/batch, but it drives the same `LiveTranscriber` core used
by live filing. `checkpoint()` resets bounded resampler/VAD state while retaining either resident ASR model
and a cumulative timestamp epoch (ADRs 0009/0012).

Tunable `LocalConfig` defaults include `asr_engine = "sherpa"` (upgrade-safe compatibility),
`num_threads = 4`, `diarize_far_end = false`, `vad_threshold = 0.5`, and
`vad_min_silence = 1.0`. On the M1 Pro excerpt benchmark, transcribe.cpp/Metal was 4.09× faster with 19%
lower peak RSS at identical normalized excerpt WER; these measurements do not establish general
accuracy. See [ADR 0011](../design/adr/0011-spike-transcribe-cpp-ggml-asr.md#m1-pro-benchmark-result).
Far-end diarization over-clusters on English audio (issue #18); it stays off by default.

## AWS backend

`crates/corti-transcribe-aws`. Implements the same sync trait (`src/lib.rs`) by building a
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
`job_id` is the recording filename stem and queue deduplication key. `transcribe_secs` supplies
the Recording Queue's elapsed-transcription display. `capture_processing` stores bounded, versioned
AEC disposition/config/lookahead and capture quality;
NULL rows remain raw/unknown and retain compatibility cleaning. Earlier migration also
rewrites stored timestamps to fixed-width UTC `…Z` so string ordering is chronological.

`JobStatus` (`corti-core recording.rs`) is the state machine:

```
Recording → PendingTranscription → Transcribing → PendingNote → Done
                                                              ↘ Failed
```

`PendingNote` has a durable meaning: `<recording-stem>.transcript.json` was atomically written beside the
raw recording and contains a versioned `DiarizedTranscript`, generation provenance captured from the
backend's exact config snapshot, an optional existing/returned note path plus partial/canonical ownership,
and any exact AWS staging location still awaiting deletion. A canonical path
means vagus/live filing already completed the note body, so only SQLite completion remains even if the path
has moved. Only cloud cleanup (when marked) and filing/completion remain.

API the app uses: `enqueue` (`INSERT OR IGNORE` — preserves progress on re-enqueue),
`set_status`, `update` (partial via SQL `COALESCE` — only `Some` fields change), `retry_reset`, `all`,
and per-row terminal deletion after artifact cleanup. `queue.jobs()` hands out a `corti_jobs::Jobs`
borrowing the same live `Connection` for the background-job table.

**`corti-jobs` owns durable retries on the queue connection.** `crates/corti-jobs/src/lib.rs` is
a small background-job layer sharing `queue.db`: kinds are strings with JSON payloads; `claim_due`
marks a row `running` and bumps `attempts` *before* the handler runs, so `recover_running`
can flip any still-`running` row back to due-now at startup — crash recovery of jobs. The
pipeline seeds tray history and then calls `recover_running` on boot; `corti --list` and the tray
`History ▸` submenu survive restarts. It also scans for valid filing checkpoints and schedules them
immediately, including the checkpoint-written/`PendingNote`-write-failed window.

Before the first batch ASR call, the pipeline persists and claims a retry job carrying the original
backend and attempt budget. Unknown or changed automatic origins refuse before staging or decoding.
Rows left at `Recording` are handled by the [startup reaper](#live-inbox-filing-87).

## The pipeline worker

`app/src/pipeline.rs` — a single `corti-pipeline` thread, the sole owner of the writable `Queue` connection (rusqlite
`Connection` is `Send`, not `Sync`). Its loop (`run`) is a **tick**:
`rx.recv_timeout(next_wake(..))` blocks for a new message or until the next background job is
due (clamped to `MAX_IDLE_WAIT = 60 s`), then `drain_due_jobs` claims and runs every due
job. Messages include `PipelineMsg::{Process, Retry, ReloadConfig}` and live-filing updates.

A `Process` message either completes a canonical live note or arms a durable batch job.
The batch handler calls `transcribe_and_file`: `queue.update(Transcribing)` → publish any exact pre-ASR AWS
owner → choose capture identity / legacy-safe offline fallback → `transcribe::transcribe_recording`
(`Backend::transcribe`, a **blocking** call on this thread) → atomically write the filing checkpoint →
`queue.update(PendingNote, transcribe_secs)` → clean AWS staging (when applicable) → file. Cloud cleanup
errors propagate and retry from the checkpoint. Completion is one `Queue::complete_with_note` SQL update
for `note_path + Done`; errors propagate before any success UI. On durable success the checkpoint is
removed, while audio remains for the configured retention sweep.

**Retry with backoff.** A valid checkpoint is authoritative in any nonterminal transcription state, so a
failed/crashed adjacent `PendingNote` write cannot repeat ASR. Without one, `PendingTranscription` and
`Transcribing` run ASR from retained raw audio; `PendingNote` is a legacy row and falls back once using a
new persisted stable AWS name. Failed attempts back off
`1 m → 2 m → … → 1 h`. A fresh batch or initial live fallback has six total attempts
(the first plus `RETRY_MAX_ATTEMPTS = 5` retries); a fresh manual retry budget has five attempts.
Filing backoff remains visibly `PendingNote`,
while transcription backoff returns to `PendingTranscription`. Exhaustion persists the recording's terminal
state before parking its job; unresolved AWS cleanup continues under its own durable job. The Recording
Queue window's Retry button starts a fresh manual attempt budget and remains available when raw audio is
gone but a validated filing checkpoint survives.

**Retention sweep.** An hourly periodic singleton `sweep_expired` (`jobs.rs`), armed by
`enqueue_periodic(SWEEP_EXPIRED, SWEEP_PERIOD = 3600 s)` at `pipeline.rs` (also fires at startup),
deletes raw audio older than `retention_days` (config, default 7), plus clean/checkpoint leftovers and
crash-left atomic-write temps, then GCs terminal recording rows after `max(90, retention_days)` days and
parked job rows after 30. One timestamp defines both horizons, and a failed artifact deletion retains the
path-bearing row for a later sweep. A row/checkpoint with unresolved AWS staging is never swept or GCed:
the exact cloud address remains until deletion is acknowledged.

`ReloadConfig` (sent by the Settings screen on save) rebuilds the backend + AEC toggle between jobs.

## Live inbox filing (#87)

Eligible local detector recordings transcribe and file **during the call**; `tail -f` on the note
shows each committed window. The wiring (tee → AEC → `LiveTranscriber`s → note) is in
[streaming.md](streaming.md#in-app-consumer--live-inbox-filing-87-adr-0010); the write authority is
[ADR 0010](../design/adr/0010-live-inbox-filing.md). ADR 0013 adds a separate transient reader: each closed
VAD region reaches the timestamped Live Transcript window immediately, before the configured durable-note
boundary, through a 2,000-row/~1 MiB bounded store. That observer never owns filing, fallback, or note state.
The filing semantics:

- **State-line + storage contract (for inbox agents).** The first corti-authored body line is exactly
  `State: transcribing` while windows stream in, and exactly `State: transcribed ` — one trailing space,
  same byte width — once final. The flip is a same-inode seek+write. The note's `corti` frontmatter object
  separately records `mode: live` plus the session's release/models/config (ADR 0014). ADR 0012 makes
  persistence explicit:
  the initial note + parent directory, every complete transcript chunk, body rewrites, and the final flip
  call `sync_all`. The final short chunk is synced *before* the state is synced. Batch notes carry the same
  final line, so inbox agents have one contract.
- **Lazy note creation.** The note is created (`vagus add-note --print-path`, with the safe child-only
  provenance payload and initial body = `live_initial_body`) on the **first non-empty committed window**,
  not at recording start. If discarded,
  the session deletes it. Corti retains the path before the fallible first sync, then publishes it to the
  queue only after syncing the file and directory (`PipelineMsg::LiveNoteCreated`, status `Recording`).
- **Configurable bounded windows.** `live_buffer_minutes` / `CORTI_LIVE_BUFFER_MINUTES` defaults to 1 and
  is clamped to 1–10. After that nominal interval, the boundary policy waits for a shared drained
  speech gap, with at most 22 seconds of deferral at current VAD settings. A forced cut records
  degradation; timestamp continuity is preserved without reloading ASR models. The one optional far-end PCM window
  is capped at 128 MiB, including a 32 MiB deferral reserve. Pending text has a 1 MiB cap with
  settlement headroom; text pressure forces a boundary. Unsupported configured windows fail setup.
  The capture tee holds at most about 72 MiB of PCM/availability plus less than 1 MiB of metadata;
  [streaming](streaming.md#in-app-consumer--live-inbox-filing-87-adr-0010) owns the full memory bounds.
  No audio/text collection grows with call duration. After the first commit, a crash preserves every
  prior window; uncommitted note loss includes the configured interval, bounded boundary deferral,
  and decoder/tee lag. See [boundary limits](quality.md#live-boundaries-155).
- **Diarize, merge, append once.** If far-end diarization is enabled, its models are loaded once and only the
  active tap window is processed; words become `Them N` before any bytes for that window are written. Mic +
  far segments are merged by start, rendered with the batch formatter, and appended in one synced write.
  `Them N` numbers are window-local and may be renumbered at a boundary (ADR 0012).
- **Finish ownership and quality.** After the recorder closes its tee, the detector calls
  `LiveHook::finished(meta)` before emitting `RecordingFinished`. `LiveManager::finish(id)` freezes the
  dropped-chunk count and keeps the handle/outcome by ID while it flushes AEC and both transcriber tails.
  The later `Process` calls `collect(id)`. Only a result with no dropped tee chunks and complete settled quality skips batch;
  its `note_path + Done` completion is one fallible SQL update, and raw audio remains revealable until
  retention expiry. A collecting/finishing sentinel prevents a second model-backed session from
  overlapping the tail join. Live work adds no telemetry stage; it remains under `Recording`.
- **Fallback — no double notes, no repeated ASR after checkpoint.** Factory ineligible (config off,
  non-local backend, any configured model missing), no note created, a live-path error, dropped tee chunks, or incomplete settled quality
  sends the recording to batch from the WAV. A returned partial path is passed directly as the preferred
  rewrite target and persisted in the retry payload before fallible row repair. Successful batch ASR puts
  that path, transcript, and generation identity in the post-ASR checkpoint; subsequent filing/completion
  retries load the checkpoint, replace/insert that note's one Corti-owned frontmatter field with
  `mode: batch`, rewrite the same path/inode, and never invoke ASR again. Missing-audio failure and exhaustion
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
  `app/src/pipeline.rs`): audio still on disk → reset to `PendingTranscription` + a due-now
  durable retry (which rewrites the note in place); audio gone → terminal `Failed`, with the note's
  state line flipped and an "incomplete" annotation appended (`close_out_note`, `pipeline.rs` —
  every terminal failure path does this) so no inbox agent waits on `State: transcribing` forever.

## Filing to vagus (batch path)

`file_and_done` calls `corti_vagus::Vagus::file_recording` (`crates/corti-vagus/src/lib.rs`) →
`add_note`, which **shells out** to the external `vagus` binary:

```
VAGUS_ADD_NOTE_FRONTMATTER_JSON='{"corti":{...}}' \
  vagus add-note "<title>" --source "<source>" --print-path   < body-on-stdin
```

The child-only JSON object is validated and rendered as YAML flow frontmatter by current Vagus releases;
Vagus also projects it into dedicated BM25 + semantic metadata chunks, so `vagus search parakeet` (or a
version/mode/config term) can retrieve the transcript by generation provenance. Vagus lifecycle fields stay
out of chunk text. An older Vagus ignores the environment variable and still files the transcript.
`--print-path` skips the editor and prints the
created note path, which corti captures. The body
(`recording_body`) is the `State: transcribed ` line (#87), an auto-capture context line, and
`DiarizedTranscript::to_markdown()`. The binary is resolved via `$VAGUS_BIN` → `vagus` on `PATH` →
Homebrew/cargo locations (`discover`), re-probed on each filing attempt so a mid-session
install works without relaunch.

ADR 0014's versioned `corti` schema records package version, final `live`/`batch` mode, backend,
ASR/VAD/optional segmentation+embedding identities, and an allowlist of quality settings. It excludes
credentials, AWS bucket/profile, model directories, and absolute override paths. Standalone
`--input --output` emits the same field without Vagus.

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
  Webinar}`, `JobStatus` + `is_terminal()`.

`Word`/`SpeakerTurn` are **not** in core — they are per-backend intermediates in
`corti_transcribe::segment`.
