# corti guardrails

Binding invariants. Changing one requires updating the matching ADR in `adr/`.

1. **The Vagus boundary lives only in `corti-vagus`.** Create notes through `vagus add-note` (including
   ADR 0014's safe child-only provenance input); never write the Vagus index or SQLite DB. Against only a
   note path returned for the current recording, ADR 0010 permits bounded append/state/body/delete writes,
   ADR 0014 lets the fallback body rewrite replace/insert only Corti's own `corti:` frontmatter field, and
   ADR 0015 permits the one current-note read/final rewrite needed to publish validated hosted text before
   the state flip. Provider adapters receive typed text/metadata and never a Vagus path. Vagus alone derives
   searchable metadata chunks. Never touch any other vault path. (ADRs 0001, 0010, 0014, 0015)
2. **Apple Silicon + latest macOS only.** No Intel, no universal binaries, no `cfg(target_arch = "x86_64")`,
   no support for macOS more than one release behind current. (ADR 0002)
3. **Own the platform bindings.** No far-reaching third-party macOS-binding dependency without an ADR;
   prefer thin safe wrappers over `coreaudio-sys` / `objc2-*` in `corti-coreaudio`. ADR 0015 narrowly permits
   an app-only AppKit secure-entry sheet and Security.framework Keychain wrapper for hosted API/cache keys;
   secrets must never cross into React/config/SQLite/logs/events/subprocess arguments. (ADRs 0002, 0015)
4. **Capture is CoreAudio (process tap + aggregate device)**; ScreenCaptureKit is a fallback only. (ADR 0002)
5. **Audio and other large/derived artifacts live outside any vault** — recordings under
   `~/Library/Caches/corti/`, job state under `~/.local/share/corti/`. Never in `~/brain`.
6. **Transcription backends are pluggable** behind a single `Transcriber` trait, so the rest of the
   pipeline is backend-agnostic. The `aws` and `local` features compile independently (both can be on at
   once) and the active backend is chosen at **runtime** (`CORTI_TRANSCRIBE_BACKEND`). (ADR 0003)
7. **The pipeline is crash-recoverable.** A recording's progress is persisted (`corti-queue`) so a crash
   mid-upload/transcribe/file resumes rather than loses the note. *(#85 restored durability via `corti-jobs`:
   a transient transcribe/file failure schedules a durable retry job (backoff, ≤5 attempts) that survives
   restarts, orphaned jobs are re-queued at startup (`recover_running`), and an hourly sweep enforces audio
   retention. Job-level, not full first-attempt resume — a crash mid-first-transcribe still strands that row.)*
8. **Attribution is best-effort and must never block capture.** Prefer a known conferencing app; skip
   `com.apple.*` audio helpers; fall back to the frontmost app, then "Unknown app".
9. **CoreAudio listener callbacks run on a HAL thread** — they must not block and must only hand work to
   the async runtime via a channel.
10. **Audio capture needs a TCC identity.** Any binary that captures (process tap or mic) MUST carry an
    `Info.plist` with `NSAudioCaptureUsageDescription` + `NSMicrophoneUsageDescription` and be code-signed,
    or macOS silently denies it (no prompt, zero IO callbacks). CLI binaries embed the plist via
    `build.rs` (`-sectcreate __TEXT __info_plist`); the app bundle carries it normally. (ADR 0002)
11. **Hosted transcript egress is explicit, fenced, and reversible.** It is default-off; connecting a
    provider never enables it; selected transcript text/word-bank/steering/questions may leave the Mac only
    after a persisted disclosure acknowledgement, while audio never does. Raw text is immutable fallback,
    provider/tool support tiers stay visible, ambiguous paid calls never auto-repeat, and unknown or
    subscription cost stays nullable rather than `$0.00`. Claude Free/Pro/Max routing is blocked absent
    written Anthropic permission. (ADR 0015)
12. **Offline tracing is local, opt-in, schema-bound, and content-free.** It has no network collector/path
    override/Settings surface. Only static catalogue operations and reviewed low-cardinality values may use
    the exact `vasovagal::trace` target: never transcript/audio/title/app identity, recording/job IDs, paths,
    cloud configuration/credentials, arguments/results, raw errors, or unbounded labels. Missing/invalid
    activation, storage failure, subscriber conflict, and compiled-out support are no-ops. (ADR 0016)
