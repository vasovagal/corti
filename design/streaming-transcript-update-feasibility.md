## Feasibility: streaming-transcript "tactical last-N-byte edit + notify loop" mechanism

**Verdict: feasible-with-conditions — but it is the _wrong tool_ for corti's topology.** The pure file+notify mechanism is implementable and can be made O(delta), but in a single-process tray app it adds I/O, dependencies, and bug classes to solve a writer→reader handoff that an in-process channel already solves with zero of those costs. Recommend the **hybrid**: in-process channel for partials + a strictly **append-only finals-only log** for durability/replay/search. The "tactical edit last N bytes" trick then becomes unnecessary.

### Why not the pure file mechanism (decisive, verified)
- **Same-process topology.** Reader (webview) and writer (Rust) live in ONE Tauri process (`ActivationPolicy::Accessory`, `windows: []`). A filesystem watcher is inter-process IPC for a boundary that does not exist here.
- **The webview cannot read a file/DB today.** `app/capabilities/{settings,ethics}.json` grant `core:default` **only**; `app/Cargo.toml` has no `tauri-plugin-fs` / `tauri-plugin-sql` (only `tauri-plugin-macos-permissions`). Any "reader reads the file" leg needs a **new plugin + capability + guardrail-#3 review** or a bespoke command — and would _still_ need an in-process notify on top, making the disk round-trip pure tax.
- **No watcher crate exists.** `Cargo.lock` has zero `notify`/`fsevent-sys`/`kqueue`/`inotify` entries (verified). Adding one is a native macOS binding (FSEvents is directory-granular + coalescing = wrong for a single hot file at 0.2s cadence; kqueue pins the inode and is ADR-gated).
- **In-place tail mutation is the single biggest correctness hazard.** A REVISE-then-REPLACE partial can shrink or stay the same length; a size-keyed reader then **silently misses** the revision, and a `seek+write_all+set_len` (three non-atomic syscalls) opens torn-read / split-UTF-8 / phantom-finalized-line windows. Keeping partials **off disk** dissolves this entire class.

### Recommended mechanism
**Two cadences, two transports:**

- **Partials (~54k/3h, NOT persisted):** one in-memory slot overwrite (≤2 slots: `me` + `them`) + one `tauri::ipc::Channel<PartialEvt>` send. Per-stream monotonic `seq`; stale/out-of-order partials dropped by integer compare. Coalesce to ~5–10/s on the write side (issue #25's prescription).
- **Finals (seconds cadence):** one **strictly append-only** write — `write_all(serde_json(LogRecord)) + write_all(b"\n") + flush` (page cache, **no fsync** — ADR 0007 defers durability). No `seek`/`set_len` ⇒ monotonic size. Also `app.emit("transcript-final", ...)` for late subscribers, reusing the proven pattern (`app/src/settings.rs:519` ↔ `app/ui/src/lib/api.ts:104-105`).

`LogRecord` wraps `corti_core::TranscriptSegment` (serde already derived, `crates/corti-core/src/transcript.rs:31-39`) so the log deserializes straight into a `DiarizedTranscript` for the existing `to_markdown` → vagus path.

**Reader cursor protocol (O(1)/event):** `finals: Map<seq,Seg>`, `live[me|them]`, `lastFinalSeq`. On partial → replace one slot, mark dirty. On final → `finals.set`, clear slot, bump cursor; if `seq > lastFinalSeq+1` (Channel dropped under backpressure) `invoke finals_since(job_id, lastFinalSeq)` to backfill from the log. On reload → `finals_since` / `get_live_snapshot` rehydrates without replaying partials. **Render:** virtualized list keyed by `seq`, **sorted by `start`** (not arrival), finalized rows memoized. **Never call `to_markdown()` on the live path** (`transcript.rs:54-68` rebuilds the whole string O(n) — reuse = O(n²)).

### 3-hour scaling
- 10,800s; ~54k partials worst case (coalesced). Finalized text ≈ **160 KB–2.9 MB** (the brief's 16 KB/min is ~18× high; 150 wpm × ~6 ch ≈ 0.9 KB/min). Use ~3 MB as the cap.
- Disk: **only finals** ⇒ ~3–4 MB append-only JSONL, monotonic, no rewrites. **Partials write zero bytes to disk.**
- Per-event: partial = O(1) in-memory + send; final = one ~120 B append + flush; reader = O(1); log read only on subscribe/reload/gap.
- Naive baseline contrast: whole-file rewrite/re-render per event = **O(n²)** ≈ tens-to-hundreds of GB I/O + 54k full re-renders.
- **Verdict: every leg bounded-O(delta)** under the protocol above.

### AEC plug-in (#74)
`io_proc → rtrb ring → [corti-capture-writer thread: reassemble → frame_mean downmix → me=StreamingAec.push(mic_mono, tap_mono) inline; them=tap passthrough] → bounded channel → [transcribe stage]`. Transcription **must** be on the second stage (inline would back-pressure the 30s ring → `io_proc` drops callbacks, `capture.rs:529-533`). Relocation point is the `write_frame` call site (`crates/corti-coreaudio/src/capture.rs:602`). The ~5s warm-up (`DEFAULT_LOOKAHEAD_SECS=5.0`, `crates/corti-aec/src/streaming.rs:37`) is empty-then-burst ⇒ several finals appended at once (clean multi-append, not a rewrite); UI shows "warming". Timestamps from the AEC cumulative emitted-sample counter (length invariant makes them exact), never wall-clock. Call `StreamingAec.finish()` at stop. Because `me` is lookahead-delayed but `them` is not, sort the view by `start` to absorb the reorder with no byte-level prefix rewrite.

### Risk register (top items)
| Risk | Severity | Mitigation |
|---|---|---|
| In-place tail mutation misses same-length/shrinking partial revisions | **High** | Keep partials off disk; finals log append-only/monotonic |
| Torn reads / split-UTF-8 / phantom finalized line | Medium | Hybrid eliminates it; else fatal-decode + clamp to `finalized_through` + checksum |
| Two concurrent partials (Me+Them) vs single mutable tail | Medium | Two in-memory slots; never one tail |
| AEC start-vs-arrival reorder | Medium | Sort display by `start`; file stays append-by-seq |
| Path A interim re-decode (#21) O(utterance²)/utterance | Medium | Cap buffer to current VAD segment; own stage |
| Two-transport seq coherence / dropped final | Medium | Single writer-owned `seq`; gap-detect + `finals_since` backfill (test it) |
| Live single-`Them` vs finalize `Them N` relabel | Low | UI marks live labels provisional |
| FS-watch crate trips guardrail #3 | Low | No watcher in-process; size-poll the append-only log if #26 is out-of-process |

### First steps
1. Land the two-stage capture→transcribe pipeline (#74); assert `dropped_samples==0` across the t≈5s burst.
2. Define `PartialEvt`/`FinalMsg`/`LogRecord` reusing `TranscriptSegment`; sample-counter timestamps.
3. Wire `start_live_transcript(channel)` + `app.emit("transcript-final")` (extend the model-download-progress pattern).
4. Append-only finals JSONL under `~/Library/Caches/corti/transcripts/<job_id>.jsonl` + `finals_since(job_id, after_seq)` and `get_live_snapshot()` commands.
5. Live webview: virtualized, key by `seq`, sort by `start`, memoize finals; stale-seq drop + gap backfill. (Needs an ADR 0004 amendment — first push-driven window; mind the Accessory↔Regular focus-steal.)
6. Cap Path A interim decode to the current VAD segment (#21); wire `finish()` into stop.
7. 3h soak test: O(delta) per event, `dropped_samples==0`, log ~3–4 MB monotonic, mid-call reload rehydrates.

### Open decisions for you
- **Is #26 (Gemma copilot) in-process or a separate OS process?** Single biggest factor for whether any on-disk artifact/watcher is justified beyond crash-replay.
- **Does #25 FTS5 index across calls or per-call?** FTS5 is compiled into bundled `libsqlite3-sys 0.38.1` / `rusqlite 0.40.1` (verified) — use raw `CREATE VIRTUAL TABLE ... USING fts5`, index **finals only**.
- **Should the live log become the SOURCE for the filed note** (requires streaming quality == batch + a re-diarization decision)?
- **Path A vs Path B first?** Path A reuses today's offline models (lowest risk); Path B lacks word durations (provisional `end`).
- **ADR 0004 amendment + capability for the first non-static window?**

### Verified vs uncertain
- **Verified:** no watcher crate in `Cargo.lock`; `core:default`-only webview with no fs/sql plugin (`app/capabilities/*.json`, `app/Cargo.toml`); `emit`/`listen` precedent (`settings.rs:519` ↔ `api.ts:104-105`); AEC lookahead default 5.0s (`streaming.rs:37`); writer thread + `write_frame` site (`capture.rs:340,543,602`); bundled SQLite/FTS5.
- **Uncertain / unbuilt:** the streaming transcriber (#25) and capture-time AEC (#74) do not exist yet — today AEC runs post-capture over the whole WAV (`crates/corti-capture/src/lib.rs:102-105`) and the writer writes raw frames inline-AEC-free. Cadence/shape of partials may shift during implementation; treat this as a design contract, not running code.

🤖 Generated with [Claude Code](https://claude.com/claude-code)