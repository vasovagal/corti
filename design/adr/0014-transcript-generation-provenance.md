# ADR 0014 — Versioned transcript-generation provenance in note frontmatter

- **Status:** Accepted (2026-08-07; #110)
- **Amends:** ADR 0001 (Vagus CLI contract), ADR 0010 decision 3 (fallback rewrite owns one Corti
  frontmatter field as well as the body)
- **References:** ADR 0003 (local models), ADR 0009 (shared live/batch core), ADR 0011 (GGML/Metal),
  ADR 0012 (durable live windows), Vagus ADR 0027 (safe producer frontmatter), Vagus ADR 0028
  (searchable producer metadata)

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
   preferable to dropping/parking a transcript because release order differed. Under Vagus ADR 0028, a
   current Vagus also projects the `corti` JSON into dedicated lexical + semantic metadata chunks, making
   release/model/config provenance searchable without indexing Vagus lifecycle fields.

5. **A live→batch fallback updates provenance in the existing rewrite.** Live note creation uses the live
   session's snapshot. If batch becomes canonical, `rewrite_body_with_provenance` reads only the bounded
   prefix, replaces/inserts the one top-level `corti:` field, then truncates and writes the canonical body on
   the same inode. Provenance is placed in the prefix before the final body bytes. The existing state line
   remains `transcribing` until that operation publishes the batch body, so a crash between checkpoint and
   rewrite is visibly incomplete and retryable. No second note and no call-sized old-tail read are introduced.

6. **Every note-producing surface uses the same schema.** The menu app's live and batch paths, `--redo`,
   standalone `--input --output`, `corti-tap --inbox`, and the integration example all supply provenance.
   The standalone renderer emits the exact same `corti: <JSON>` line as `corti-vagus`.

## Capture-quality amendment (2026-10-27; #162)

Capture completeness must be durable even when a live transcript exceeds its bounded canonical-text
ledger, or recovery refuses a changed/unknown backend. These paths cannot safely reconstruct the body
merely to publish quality metadata. `CurrentNote::refresh_provenance` therefore replaces/inserts only
the current recording's producer-owned `corti:` field, retaining every existing body byte and the inode.
It validates the complete replacement against a 64 KiB frontmatter-prefix bound before mutation and
uses one 64 KiB buffer to shift the body in place. It returns no note text and grants no access to other
paths. This narrowly precedes the later general provenance repair because #162 cannot certify or
annotate capture quality without it. Failure prevents successful completion/state publication.

The in-place operation is **not atomic**. A crash or write error during a shift can leave a damaged
prefix/body, as with the existing same-inode rewrite. The mandatory content-free capture sidecar remains
authoritative independently of the note; a successful same-backend fallback writes the canonical body
from its durable checkpoint. A refused backend leaves the result incomplete rather than treating a
possibly interrupted metadata update as success. No note copy, new scheduler, or startup discovery is
introduced.

The full schema-1 `QualityRecord` stays in the bounded 256 KiB mandatory sidecar and the existing
checkpoint JSON. A valid at-capacity record can exceed the note-prefix limit. Frontmatter consequently
uses a labelled projection of that **same record**, capped at 48 KiB: all retained epoch offsets remain;
omitted spans contribute class/resolution-aware omission totals; full-record completeness is recorded
separately from projection truncation. Body markers are rendered from the full authority, including every
retained exact loss span. Oversized or malformed unrelated user metadata still fails closed. No queue schema
version or competing quality model is added.

Before the first batch ASR attempt, the existing `RETRY_TRANSCRIPTION` payload durably records the
original backend. Recording-id-aware enqueue preserves existing origin, claim, backoff, and budget;
the existing same-iteration job drain performs the attempt. Fresh batch work and a first live-to-batch
fallback have six total attempts (the previous initial attempt plus five retries); existing active or
claimed jobs keep their stored attempt budget, attempts, and backoff unchanged.
Automatic recovery with unknown or changed origin refuses before staging, ASR, or hosted work. A live
crash before a durable origin is available likewise refuses. Explicit `--redo --local` remains a separate
user-authorized recovery choice; a completed redo with unavailable historical origin otherwise refuses.
Final provenance retains known origin after successful job cleanup; existing checkpoints carry it until
their normal successful removal.

Successful explicit `--redo --local` (including `--backend local`) may resolve processing loss when
normal Done cleanup has removed the historical backend authority. This is represented by the distinct,
nonserialized `BatchRecoveryOutcome::ExplicitLocalWholeFileSuccess`, not by a claim that the backend
matches an unknown historical origin. Only the explicit-local CLI path, after successful whole-file
decode by the actual local backend and verified `audio_durable` storage, may select that outcome.
Historical origin stays unknown; immutable capture metadata and the mandatory sidecar are not rewritten.
Source loss, unknown source mapping, epoch omissions, and saturated accounting remain incomplete or
unknown. Failed/partial decode and unverified/interrupted storage do not authorize processing recovery.
Automatic origin matching and unknown-origin default/`--aws` redo refusal are unchanged; no note-read
API or new durable origin retention is introduced.

## Consequences

- A bad note is self-describing enough to group quality reports by Corti release, final path, model set, and
  tuning without relying on mutable local logs.
- Frontmatter stays one producer-owned namespace. Current Vagus indexes a normalized projection as
  dedicated producer-metadata chunks, so searches such as `parakeet` find the generating model without
  repeating provenance across transcript body chunks.
- Batch filing retries cannot drift to a newer Settings snapshot; historical checkpoints are honest about
  missing identity.
- Current Vagus is needed for provenance on a newly batch-filed note. Version skew still preserves the note,
  and a later live→batch same-note rewrite can insert provenance even if the initial older Vagus omitted it.
- The compact flow mapping favors safe, deterministic cross-process serialization over hand-authored YAML
  aesthetics. It remains valid, inspectable Obsidian YAML.
