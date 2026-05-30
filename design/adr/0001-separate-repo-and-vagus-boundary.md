# ADR 0001 — corti is a separate project; vagus is touched only via the CLI

- **Status:** Accepted (2026-05-29)

## Context

corti needs a long-running tray daemon, microphone + system-audio capture, and (optionally) cloud
transcription. Those directly contradict vagus's hard invariants: local-first/offline, no daemon, lean
dependency set. Folding corti into the `vagus` crate would force Tauri, audio, and AWS deps onto a tool
whose whole value is being a small, offline, single-binary CLI.

## Decision

- corti is its **own repo** (`vasovagal/corti`), a Cargo workspace with many small crates.
- corti integrates with vagus **only** by shelling out to the installed `vagus` binary —
  `vagus add-note "<title>" --source "<…>" --print-path` with the note body piped on stdin — exactly the
  contract vagus's own Claude Code skills use. corti never writes the iCloud vault or the vagus index
  directly.
- The boundary lives in one crate, `corti-vagus`. Everything else is unaware of vagus.

## Consequences

- vagus stays lean and offline; corti owns all the heavy/networked/long-running machinery.
- The integration is resilient to vagus internals changing, since it depends only on the stable CLI
  surface (`add-note`, `--source`, `--print-path`, stdin body).
- corti gets its own release pipeline and can target a much narrower platform (see ADR 0002).
