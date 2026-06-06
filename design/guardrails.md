# corti guardrails

Binding invariants. Changing one requires updating the matching ADR in `adr/`.

1. **vagus is touched only through the `vagus` CLI** (`corti-vagus`). Never write the iCloud vault, the
   vagus index, or the SQLite DB directly. (ADR 0001)
2. **Apple Silicon + latest macOS only.** No Intel, no universal binaries, no `cfg(target_arch = "x86_64")`,
   no support for macOS more than one release behind current. (ADR 0002)
3. **Own the platform bindings.** No far-reaching third-party macOS-binding dependency without an ADR;
   prefer thin safe wrappers over `coreaudio-sys` / `objc2-*` in `corti-coreaudio`. (ADR 0002)
4. **Capture is CoreAudio (process tap + aggregate device)**; ScreenCaptureKit is a fallback only. (ADR 0002)
5. **Audio and other large/derived artifacts live outside any vault** — recordings under
   `~/Library/Caches/corti/`, job state under `~/.local/share/corti/`. Never in `~/brain`.
6. **Transcription backends are pluggable** behind a single `Transcriber` trait, so the rest of the
   pipeline is backend-agnostic. The `aws` and `local` features compile independently (both can be on at
   once) and the active backend is chosen at **runtime** (`CORTI_TRANSCRIBE_BACKEND`). (ADR 0003)
7. **The pipeline is crash-recoverable.** A recording's progress is persisted (`corti-queue`) so a crash
   mid-upload/transcribe/file resumes rather than loses the note.
8. **Attribution is best-effort and must never block capture.** Prefer a known conferencing app; skip
   `com.apple.*` audio helpers; fall back to the frontmost app, then "Unknown app".
9. **CoreAudio listener callbacks run on a HAL thread** — they must not block and must only hand work to
   the async runtime via a channel.
10. **Audio capture needs a TCC identity.** Any binary that captures (process tap or mic) MUST carry an
    `Info.plist` with `NSAudioCaptureUsageDescription` + `NSMicrophoneUsageDescription` and be code-signed,
    or macOS silently denies it (no prompt, zero IO callbacks). CLI binaries embed the plist via
    `build.rs` (`-sectcreate __TEXT __info_plist`); the app bundle carries it normally. (ADR 0002)
