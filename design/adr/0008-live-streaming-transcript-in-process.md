# ADR 0008 — Live streaming transcript: first push-driven window, in-process transport, canonical in-memory buffer

- **Status:** Proposed (2026-06-18)
- **Amends:** ADR 0004 (informational webview) — its "static-content windows need no capability / make no `invoke()` calls" stance no longer covers every window; see Decision 6.
- **References:** ADR 0007 (#74 — relocate `StreamingAec.push()` into the capture-writer thread, the prerequisite for a live `me` source); the prior 22-agent feasibility study; plan #25 / umbrella #10; the in-process Gemma copilot #26.

## Context

corti records → AEC → transcribes → files a vagus note. Today transcription runs **only** as a single
`OfflineRecognizer` batch pass at stop (`crates/corti-transcribe-local/src/engine.rs`), and the only UI is the
tray. We want a **live transcript window** — opened on demand from the tray — that shows partial and final
segments as a call proceeds, and that will later be the surface the in-process Gemma copilot (#26) reads.

A prior feasibility study examined the user's "tactically edit the last N bytes of an on-disk transcript file
and have a reader follow it via a filesystem-notify loop" mechanism. Its verdict (build **on** it, do not
redo): the literal idea is implementable and O(delta), but it is the **wrong tool for corti** — the would-be
reader (webview) and the writer (Rust) live in **one Tauri process**, so a filesystem watcher is IPC across a
boundary that does not exist. The study also flagged one HIGH-severity hazard in the literal scheme: in-place
tail mutation breaks the monotonic-size invariant, so a recognizer revising a partial to the **same or fewer**
bytes is silently missed by a size-keyed reader.

Four user decisions frame this ADR and are treated as authoritative:

1. The Gemma copilot (#26) is **in-process** — another thread/window in the same Tauri app, not a separate OS
   process. No second process ever reads the transcript.
2. Recognizer Path A (re-decode the growing buffer every ~0.2s with the offline Parakeet recognizer) vs Path B
   (sherpa-onnx `OnlineRecognizer`, true incremental streaming) is **open** and is resolved here.
3. Live is **additive** via a single **canonical in-memory transcript buffer** that is the source of truth: it
   yields partial/final events to the UI **and** at finalize is rendered (`to_markdown`) into the filed vagus
   note. The existing batch `OfflineRecognizer` finalize stays canonical for the filed note.
4. This is the **first push-driven (non-static) webview window**, so it needs a new ADR amending ADR 0004.

Verified architecture facts this design rests on (cite by reading; line numbers re-verified this session):
`tauri 2.11.2` provides `tauri::ipc::Channel` (`Cargo.lock`); commands register via `generate_handler!` at
`app/src/main.rs:199` (list spans `199`–`213`); managed state via `app.manage` (`AppState` at `main.rs:239`,
`ConfigState` at `main.rs:251`, mirroring `settings::ConfigState`, `settings.rs:24-28`, with
`SharedConfig = Arc<Mutex<AppConfig>>` at `settings.rs:21`); the pipeline runs on a dedicated `corti-pipeline`
thread spawned at `main.rs:265-266`, the sole owner of serial per-recording state (`pipeline::run`,
`pipeline.rs:61`); finals can ride the existing `app.emit` event pattern (`settings.rs:519`,
`model-download-progress`); `DiarizedTranscript`/`TranscriptSegment`/`Speaker` derive `Serialize`/`Deserialize`
(`crates/corti-core/src/transcript.rs:11,31,42`) so they cross IPC unchanged; `to_markdown`
(`transcript.rs:54`) rebuilds the whole string and must be called once at finalize, never per partial (it is
O(n²) on the live path). AEC warm-up: `StreamingAec.push()` returns empty while warming
(`CORTI_AEC_LOOKAHEAD_SECS` default 5.0s, `crates/corti-aec/src/streaming.rs:37`), then emits a burst of the
buffered opening (`lock_and_emit_opening`, `streaming.rs:243`), then steady, with the
`total_emitted == total_pushed` length invariant for deriving timestamps from a cumulative emitted-sample
counter rather than wall-clock. The capture writer is `run_writer` on the `corti-capture-writer` thread
(`crates/corti-coreaudio/src/capture.rs:340,543`); ring overflow is dropped and counted
(`RecordingHandle.dropped_samples`, `capture.rs:145,398`). `Cargo.lock` carries **no**
`notify`/`fsevent`/`kqueue`/`inotify` crate, and guardrail #3 would require an ADR to add one.

**ADR numbering note:** the `0006-*` slot already has two files (`0006-distribution-unsigned-cask-tap.md`,
`0006-far-end-recording-notice-spike.md`). That collision is pre-existing; this ADR is the next free number,
**0008**, and the `0006` files are left as-is.

## Decision

1. **Add the first push-driven live-transcript webview window, opened on demand from the tray.** It follows
   ADR 0004's lifecycle: created at runtime from a tray action (never declared in `tauri.conf.json`, so nothing
   opens at launch); the app stays `Accessory` and flips to `ActivationPolicy::Regular` only while the window
   is open, reverting on close — the same open/flip pattern the Ethics/Settings/Console windows already use.
   Unlike those ADR 0004 windows, this page is **not static** — it calls `invoke()` and receives pushed events.

2. **In-process transport — no filesystem watcher, no live on-disk transcript file in v1.** Partials stream
   over a per-window `tauri::ipc::Channel<LiveEvent>`; finals over `app.emit`/`listen` (the existing
   `model-download-progress` pattern, `settings.rs:519`). Because the copilot is in-process (Decision 1
   above), reader and writer share one address space and there is no boundary for a watcher to bridge. The only
   remaining purpose for an on-disk live artifact would be crash-replay durability, which **ADR 0007 Decision 3
   already defers** ("OK to lose an in-flight recording"). Partials are coalesced to **~5–10/s** with **two
   in-memory slots** (`Me`, `Them`) keyed by a **per-stream monotonic `seq`**; the webview replaces by `seq`
   and drops any `seq` below the last seen (absorbing out-of-order coalesced sends). The study's
   monotonic-size-invariant hazard is moot because nothing tails a file.

3. **A single canonical in-memory transcript buffer is the source of truth.** A growing `DiarizedTranscript`
   (`transcript.rs:42`) is the live model. To expose it to both the pipeline/live-recognizer threads (writers)
   and the Tauri command threads (readers), it lives behind `Arc<Mutex<Option<DiarizedTranscript>>>` (the
   `Option` is `None` when no recording is in flight) inside a new managed `LiveState` (Decision 6), managed
   via `app.manage` mirroring `ConfigState` (`main.rs:251`). **Only committed (final) segments enter the
   `Mutex`; partials travel over the Channel only.** The lock is held only for the push/clone, never across a
   decode. At finalize the buffer is the object rendered via `to_markdown` (when/if the live buffer is ever
   filed directly — not v1; see Decision 5).

4. **Recognizer: Path A first, behind a `LiveEvent` interface that makes Path B a clean swap.** Path A
   (re-decode with the already-loaded offline Parakeet-TDT-0.6b-v3 recognizer + Silero VAD) is structurally
   **O(T²/dt) per utterance** — decode tick *k* re-encodes the buffer from sample 0 because offline decode is a
   stateless full forward pass (sherpa-onnx 1.13.2 `offline_asr.rs`; the only decode primitive in the codebase,
   `asr_segment`, does `create_stream` + `accept_waveform(whole buffer)` + `decode`, `engine.rs:187`). For
   ticks every dt=0.2s over an utterance of T live-seconds the work is `Σ a·k·dt = a·T²/(2·dt) ≈ 2.5·a·T²`
   encoder-seconds vs the ideal `a·T` — overhead `T/(2·dt)`, ≈50× for a 20s segment. This is exactly the
   "punishing as the input grows" cost the user intuited (independently corroborated by sherpa-onnx FR
   #3573, whose own motivation cites growing-buffer pseudo-streaming's "latency, duplicated boundary words, and
   rising compute"). The quadratic is **bounded per VAD segment**: `MAX_SPEECH_SECONDS = 20.0` force-closes any
   segment (`engine.rs:30,75`) and `min_silence_duration = 0.25s` (`engine.rs:72`) resets it on natural pauses,
   so the buffer never exceeds one in-progress segment and resets to ~0 at each endpoint. Path B
   (`OnlineRecognizer`) advances persistent decoder state per `accept_waveform(chunk)` and is genuinely
   **O(T)** with no end-of-segment latency spike.

   **We choose Path A for v1** because Decision 3 makes the live transcript's accuracy, language coverage, and
   word-level end timestamps **not load-bearing** — the filed note comes from the batch offline pass. That
   removes Path B's structural-efficiency win as a deciding factor while its concrete costs remain:
   - Parakeet-TDT is **offline-only** in sherpa-onnx (true-streaming Parakeet is open upstream, FR
     k2-fsa/sherpa-onnx #3573). Path B therefore requires a **separate streaming model** — a streaming zipformer
     or the newer multilingual Nemotron-3.5 streaming — a new ~180MB+ download (ties #13) that **cannot reuse
     the resident Parakeet-TDT-v3 weights or match its language coverage**. (Streaming options are **not**
     strictly English-only; multilingual streaming models exist, but with narrower coverage than
     Parakeet-TDT-v3 and as a distinct download.)
   - The online `RecognizerResult` has **no `durations` field** (the offline result does), so `tokens_to_words`
     (`engine.rs:206`, which sets `word.end = word.start + dur[i]` at `engine.rs:220`) **cannot be reused
     verbatim**: under Path B every partial word's `end` collapses to its `start`, forcing a separate
     durations-free word assembler.
   - It is a second parallel recognizer code path alongside the existing offline one.

   Path A reuses the resident model and the existing decode plumbing with **zero new downloads** and ships a
   multilingual live view matching the final note's engine.

   **Path A mitigations (required) to tame the bounded sawtooth.** Note that capping the buffer to the current
   VAD segment alone does **not** remove the in-segment quadratic — it only bounds the segment length B≤20s, so
   the worst-case single re-decode is still `a·B`. To linearize per segment and kill the end-of-segment stall:
   (a) **windowed live decode, not whole-segment decode.** The live path does **not** reuse `asr_segment`
   verbatim (`asr_segment` decodes the whole growing segment buffer); it re-decodes only a bounded **trailing
   interim window** of the current segment — start the window at **6s** (benchmark-tunable; see Consequences),
   which makes each interim decode `a·W` and the per-segment total `a·W·B/dt`, i.e. **linear in B**. This
   degrades partial word-boundary accuracy at the window seam, which is acceptable because live accuracy is
   throwaway (Decision 3). (b) **adaptive cadence:** skip a ~0.2s tick if the previous decode has not returned
   (coalesce to the ~5–10/s budget in Decision 2), bounding queue depth. (c) **dedicated lower-priority
   thread:** run the live re-decode off the capture-writer / AEC `push()` thread (guardrail 9 — `io_proc`/writer
   must never block; a stall risks `RecordingHandle.dropped_samples`). Path B is recorded as the pre-designed
   future swap behind the same `LiveEvent` interface, triggered if a future requirement makes live accuracy
   load-bearing (e.g. the #26 copilot reasons over live partials rather than the filed note) or a long-monologue
   stress case proves Path A's peak still too costly — at which point the separate streaming-model download
   (#13) and the durations-free word assembler are justified.

5. **Finalize policy is unambiguous: the batch `OfflineRecognizer` pass produces the filed note.** On stop the
   live buffer freezes; the existing batch pass (`transcribe_and_file` → `transcribe::transcribe_recording`
   → `to_markdown` → `vagus.file_recording`, `pipeline.rs:182,209,266`) supersedes it as the canonical note.
   The live buffer is the UI source and the `get_live_snapshot` rehydrate source; it is **never** treated as
   durable (lost on crash per ADR 0007 Decision 3) and is **never filed in v1**. Because the same
   `buffer.to_markdown()` path is wired, filing the live buffer directly later is a one-call flip. Exactly
   **one** note is ever filed, and `to_markdown` runs **once**, at finalize, never on the live path.

   **Partial→final mapping (explicit).** A `Partial { stream, seq, start, text }` carries only a start. When
   the VAD closes a segment (endpoint or the 20s cap), the live recognizer produces the **`Final` by a fresh
   decode of the closed segment's full audio** (not by promoting the last partial's windowed text), so the
   committed `TranscriptSegment.text` is the best Path-A transcription of the whole segment and its `.end` is
   the segment's endpoint sample index / sample_rate (both timestamps from the cumulative emitted-sample
   counter, never wall-clock). The `Final` segment is what enters the canonical buffer (Decision 3) and is
   emitted over `app.emit`. **If the recording stops mid-utterance** with an open partial, that partial is
   committed as a `Final` by decoding whatever audio has accumulated (its `.end` = last fed sample index);
   nothing is silently dropped, but it is still not durable across a crash.

6. **Two new commands, one managed `LiveState`, and a new push-capable window capability — amending ADR 0004.**
   - Managed state: `struct LiveState { buffer: Arc<Mutex<Option<DiarizedTranscript>>>, sinks: Arc<Mutex<HashMap<String, tauri::ipc::Channel<LiveEvent>>>> }`, managed via `app.manage` next to `ConfigState`
     (`main.rs:251`). `buffer` is the canonical in-memory transcript (Decision 3); `sinks` is the per-window
     Channel registry keyed by **window label**. The live-recognizer thread obtains a clone of this `Arc`s via
     a handle stored alongside the pipeline (or passed when the live stage is spawned), so it can push committed
     segments into `buffer` and fan partials out by iterating `sinks`. A send error on any sink (window gone)
     removes that entry — **send error ⇒ unsubscribe** (mirroring `settings.rs` swallowing reload-tx send
     errors). The window's `on_close`/`on_destroyed` also removes its label from `sinks`.
   - Commands added to `generate_handler!` (`main.rs:199-213`):
     `start_live_transcript(state, window, channel: tauri::ipc::Channel<LiveEvent>) -> Result<(), String>`
     registers the channel in `sinks` under the window label and, on registration, **first flushes the current
     `buffer` snapshot as `Final` events** so a mid-call-opened window is consistent before deltas; and
     `get_live_snapshot(state) -> DiarizedTranscript` clones `buffer` under a brief lock (pure in-memory read,
     no file) for mount/reload rehydrate.
   - Capability: new per-window `app/capabilities/transcript.json` mirroring `settings.json`
     (`{ identifier: "transcript-window", platforms: ["macOS"], windows: ["transcript"],
     permissions: ["core:default"] }`). This **amends ADR 0004 Decision 4** ("static-content windows need no
     capability … the page makes no `invoke()` calls"): that held only because the Ethics window never called
     `invoke()`. This window does, so the capability is load-bearing, not merely forward-compat.
   - `LiveEvent` is defined in **`corti-core` alongside `transcript.rs`** (it embeds `Speaker` and
     `TranscriptSegment`, which already live there) and derives `Serialize`/`Deserialize`:
     `enum LiveEvent { Partial { stream: Speaker, seq: u64, start: f64, text: String }, Final { segment: TranscriptSegment }, Reset }`.
   - Render the UI **virtualized and sorted by segment start-time** (never arrival order), every timestamp
     derived from the cumulative emitted-sample counter / sample_rate. This absorbs the AEC warm-up burst:
     `me`'s 0–5s opening arrives after later `them` segments (`them` is tap passthrough, no AEC, ADR 0007
     Decision 2, streaming from t=0) and back-fills its correct earlier slot. The webview rehydrates once on
     mount via `get_live_snapshot`, then relies on the Channel for deltas (it must never poll the snapshot).

7. **Defer (with reasons).** **(a) The on-disk tactical-edit transcript file + filesystem watcher.** Unjustified
   under Decisions 1 and 3 — no out-of-process reader exists and crash durability is already deferred by ADR
   0007 — and it carries the study's HIGH-severity monotonic-size hazard. It re-enters **only** if a future
   requirement adds a genuinely out-of-process transcript consumer (copilot moved to a separate OS process,
   reversing Decision 1) or live crash-replay durability (reversing ADR 0007 Decision 3); adding any
   `notify`/`fsevent`/`kqueue` crate then needs its own ADR per guardrail #3. **(b) An FTS5 cross-call
   transcript index (#25).** rusqlite + bundled libsqlite3-sys give FTS5 today, but search across past calls is
   orthogonal to a single live call's source-of-truth buffer; keep it out of v1's critical path. **(c) Path B /
   `OnlineRecognizer`** itself — see Decision 4's swap trigger.

## Consequences

- **#74 is a hard prerequisite and v1 BLOCKS on it.** Until `StreamingAec.push()` moves into the
  `corti-capture-writer` thread (ADR 0007 #74), AEC runs offline *after* stop via
  `corti_capture::write_clean_wav` (`transcribe.rs:142`) — there is no in-flight cleaned `me` stream to
  transcribe live (verified: no `StreamingAec.push()` call exists anywhere in `app/` today). The live-transcript
  Feature therefore either (preferred) **lands after / co-delivers the #74 writer-thread relocation**, or — only
  if explicitly chosen — ships a degraded v0 against the post-stop on-disk AEC with **no live `me` stream**
  (live `them` only). The implementing issue states this resolution; a fresh session must not start the
  writer-thread staging step before #74's relocation is in place.
- **First non-static window in the tree.** ADR 0004's blanket "no capability / no invoke" simplification no
  longer holds; future push-driven windows follow this ADR (own capability + commands), not ADR 0004.
- **No new files on disk in v1.** The only on-disk artifact remains the final vagus note (`pipeline.rs:266`).
  The live buffer is in-memory; the UI is fed over the Channel; reload rehydrates from `get_live_snapshot`.
- **Writer prioritizes the lossless WAV, never the live feed.** The writer→transcribe staging channel is
  bounded and `try_send`-only: a full channel **drops live frames** (acceptable per Decision 3, best-effort UI)
  but never blocks the disk write or starves the canonical AEC input, mirroring the `io_proc`→ring overflow-drop
  discipline (`capture.rs`).
- **No new model download in v1** (Path A reuses the resident Parakeet-TDT-v3 model). Choosing Path B later
  pulls in a separate ~180MB+ streaming model (#13) with narrower language coverage and a durations-free word
  assembler.
- **Live CPU is bursty per VAD segment.** Two channels (`Me` + `Them`) roughly double it; the pipeline is serial
  so peak is capture+live then batch, not all three at once. A `CORTI_LIVE_TRANSCRIPT` toggle lets
  battery/contended machines disable the live path entirely.
- **The 6s interim-window size and the live-path VAD tuning are benchmark-tunable and coupled to Path A's cost.**
  Confirm with a one-shot Parakeet-0.6b CPU decode microbenchmark on the target Apple-silicon before pinning;
  the asymptotic O(T²/dt)-vs-O(T) argument is exact, the absolute stall numbers are estimates. Pin the live-path
  VAD (interim window / forced partial endpoints) explicitly — do not inherit it blindly from the batch path.
- **Live accuracy is explicitly a draft.** The window labels the live view as a draft; the correct transcript is
  always the filed offline note. Under a future Path B a non-English call could show poor live partials while
  the filed note (Parakeet multilingual) stays correct.
- **Re-opens if reversed:** the tactical-edit file + watcher and an FTS5 cross-call index return only on the
  triggers in Decision 7; Path B returns on the trigger in Decision 4.
