# ADR 0014 — Versioned transcript-generation provenance in note frontmatter

- **Status:** Accepted (2026-08-07; #110)
- **Amends:** ADR 0001 (Vagus CLI contract), ADR 0010 decision 3 (fallback rewrite owns one Corti
  frontmatter field as well as the body)
- **References:** ADR 0003 (local models), ADR 0009 (shared live/batch core), ADR 0011 (GGML/Metal),
  ADR 0012 (durable live windows), Vagus ADR 0027 (safe producer frontmatter)

## Context

A transcript can be unusually poor because of the Corti release, backend, selected model representation,
far-end diarization model, VAD/AEC thresholds, or live-versus-file-backed path. The note currently records
none of those facts. Logs and Settings show only the present machine state and may be gone or changed by the
time quality is investigated.

The metadata must describe the configuration that actually generated the durable text, not whatever Settings
contains when a delayed filing retry finally runs. Live notes add another complication: a note begins while
live decoding is in progress, but dropped tee chunks or any live error can cause a later batch transcript to
replace that body in the same note. Keeping `mode: live` over batch-generated text would be worse than no
provenance.

Raw YAML crossing the Corti→Vagus boundary would permit key/newline injection. Requiring a newly added CLI
flag would make staggered upgrades destructive because an older Vagus rejects unknown arguments.

## Decision

1. **One versioned, namespaced object.** Every Corti transcript note carries a top-level `corti` frontmatter
   value. Its value is compact JSON, which is valid YAML flow syntax:

   ```yaml
   corti: {"schema":1,"version":"0.12.0","mode":"live","backend":"local","models":{...},"configuration":{...}}
   ```

   `schema` versions the metadata shape independently of the app release. `version` is the Corti package
   version compiled into the generating binary. `mode` describes the final text source: `live` for the
   bounded capture tee or `batch` for a completed recording (including live fallback).

2. **Record model identity and only quality-relevant effective configuration.** The schema names ASR, VAD,
   optional pyannote segmentation, and optional speaker-embedding identities plus the selected artifact
   representation. Configuration records backend-specific language/engine/provider/thread and
   speaker-attribution settings, VAD/diarization thresholds, complete effective AEC settings, input shape,
   and the live checkpoint interval where applicable. Absolute model paths, AWS bucket/profile/credentials,
   and other secrets or non-quality storage settings are excluded. A custom GGUF records only its filename,
   never its directory.

3. **The exact generation snapshot is durable.** The app builds provenance from the same immutable
   `AppConfig` snapshot owned by the backend/live session. Successful batch ASR stores it alongside the
   `DiarizedTranscript` in the existing atomic `FilingCheckpoint`; filing and all later retries use that
   copy. The checkpoint addition is serde-defaulted without changing checkpoint version 1. A checkpoint
   written before this ADR becomes explicit `version/backend/model: unknown` rather than borrowing current
   Settings and lying.

4. **Vagus creates the initial field safely.** `corti-vagus` serializes `{"corti": provenance}` before
   spawning the child, then sets `VAGUS_ADD_NOTE_FRONTMATTER_JSON` only on that `vagus add-note` process.
   Vagus ADR 0027 validates the object, protects Vagus-owned fields, and JSON-encodes values into YAML. An
   older Vagus ignores the environment variable and still creates the note; this availability fallback is
   preferable to dropping/parking a transcript because release order differed.

5. **A live→batch fallback updates provenance in the existing rewrite.** Live note creation uses the live
   session's snapshot. If batch becomes canonical, `rewrite_body_with_provenance` reads only the bounded
   prefix, replaces/inserts the one top-level `corti:` field, then truncates and writes the canonical body on
   the same inode. Provenance is placed in the prefix before the final body bytes. The existing state line
   remains `transcribing` until that operation publishes the batch body, so a crash between checkpoint and
   rewrite is visibly incomplete and retryable. No second note and no call-sized old-tail read are introduced.

6. **Every note-producing surface uses the same schema.** The menu app's live and batch paths, `--redo`,
   standalone `--input --output`, `corti-tap --inbox`, and the integration example all supply provenance.
   The standalone renderer emits the exact same `corti: <JSON>` line as `corti-vagus`.

## Consequences

- A bad note is self-describing enough to group quality reports by Corti release, final path, model set, and
  tuning without relying on mutable local logs.
- Frontmatter stays one producer-owned namespace and is excluded from Vagus's indexed Markdown body.
- Batch filing retries cannot drift to a newer Settings snapshot; historical checkpoints are honest about
  missing identity.
- Current Vagus is needed for provenance on a newly batch-filed note. Version skew still preserves the note,
  and a later live→batch same-note rewrite can insert provenance even if the initial older Vagus omitted it.
- The compact flow mapping favors safe, deterministic cross-process serialization over hand-authored YAML
  aesthetics. It remains valid, inspectable Obsidian YAML.
