# ADR 0010 — Live inbox filing: corti may write to notes it created

- **Status:** Accepted (2026-07-08; #87)
- **Amends:** ADR 0001 (vagus touched only via the CLI), ADR 0008 Decision 2/7a (no live on-disk
  transcript in v1)
- **References:** ADR 0009 (chunked transcription core), ADR 0007 (#74 — streaming AEC in-flight),
  guardrail 9

## Context

Live inbox filing (#87) appends transcript segments to the vagus note *while the call records*, so a
`tail -f` follower and inbox agents see the conversation arriving. ADR 0001 restricts corti to
`vagus add-note`; ADR 0008 rejected an on-disk live transcript — but its reader was in-process (the
webview), where a file is IPC across a boundary that doesn't exist. Here the reader is **external**
(`tail -f`, editors, inbox agents), where a file IS the right transport.

## Decision

corti may perform exactly three writes beyond `add-note`, all confined to `corti-vagus` and only
against a path `vagus add-note --print-path` returned for a recording corti is processing:

1. **Append** body content (transcript segments) while the recording is live.
2. **Flip the state line in place.** The first corti-authored body line is exactly
   `State: transcribing` while streaming and exactly `State: transcribed ` (one trailing space —
   same byte width) when final. The flip seeks and overwrites only that line's bytes: no rename, no
   truncation, so a `tail -f` follower keeps its inode. Batch-filed notes carry the same
   `State: transcribed ` first line, so inbox agents have one contract.
3. **Delete** the note when its recording is discarded (too short / capture error).

Nothing else in the vault, ever — no index writes, no other files, no notes corti didn't create.

## Consequences

- ADR 0001's boundary widens by these three operations but stays in one crate (`corti-vagus/src/note.rs`);
  everything else remains unaware of vagus.
- ADR 0008's "no live on-disk transcript" stance holds for the in-process window; it no longer covers
  external readers.
- vagus indexes the note at creation; the body growing afterwards is invisible to the index until a
  reindex — acceptable, vagus reindexes on its own cadence.
- The batch path may rewrite a partial live note's body in place (truncate + write, same inode) when it
  supersedes a failed live session — the one case a follower can see the file shrink.
