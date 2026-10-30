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
Historical origin stays unknown; immutable capture metadata and the sidecar's capture fields are not
rewritten. The later #138 retention amendment below adds only an optional settled-history field.
Source loss, unknown source mapping, epoch omissions, and saturated accounting remain incomplete or
unknown. Failed/partial decode and unverified/interrupted storage do not authorize processing recovery.
Automatic origin matching and unknown-origin default/`--aws` redo refusal are unchanged; no note-read
API or new durable origin retention is introduced.

## Live alignment and settled-quality amendment (2026-10-27; #138)

A received live chunk with two expected tracks must preserve the available audio when one track is
shorter. The consumer pads the missing tail before the existing AEC and masks unavailable microphone
output after filtering, including delayed/tail output. An epoch's legitimately absent track is not a
mismatch. The producer supplies each tee chunk's exact epoch and retained-frame start, so mismatch
intervals remain positioned after tee loss and unfilled source jumps. Each chunk belongs to one epoch;
producer epoch transitions flush any partial tee tail.

Mismatch accounting retains at most 64 observed epoch bindings (including good chunks) and 256
intervals, coalesces compatible adjacent tails, and preserves omitted processing/epoch counts and
saturation. Temporary audio/availability slices are bounded to 4096 frames. Warning cadence is the
first mismatch and each hundredth mismatch. Source conversions round outward with checked arithmetic;
missing/conflicting or boundary-crossing bindings produce counted omissions rather than invented spans.

Per-channel first-presence origins avoid replaying a never-present prefix and shift words/AEC evidence
once; later legitimate absence advances the started channel with bounded silence. These origins retain
the existing received-frame clock: later tee loss can still compress live word times. Whole-file batch
replacement is the accepted recovery for that limitation. Positioned mismatch quality instead uses
the independent producer clock. The AEC algorithm and defaults are unchanged.

The same `QualityRecord` carries supplemental live processing facts. Before the recorder verdict,
Observed coverage describes those observations only; it is not capture/storage authority. Finish freezes
quality against immutable capture before fallible finalization. Consume errors and panics retain known
facts; a panicked consumer waits for Finish/Discard without resuming its decoder. Finish can supply a
Start binding lost by the tee, Discard keeps deletion ownership, and a disconnected verdict cannot
authorize a fabricated Start. Recovery reloads mandatory capture/storage independently before merging
live evidence. Missing/corrupt/unfinished storage remains uncertified; successful authorized whole-file
recovery may resolve processing loss but never source loss. Capture fields in sidecar/queue provenance
are immutable; the optional lifetime settlement described below does not replace that authority.

The existing retry payload gains optional `settled_quality`, including no-note fallback. Recording-id
enqueue and failure/reschedule transactions merge evidence monotonically while preserving original
backend (including unknown), note ownership, claims, budgets, and backoff. Manual retry resets the Failed
recording and rearms its job in one queue transaction, consuming `first_batch=false`; no retry flags or
queue schema version are added. An already-active job keeps its budget/backoff. If that budget is spent,
the click may require a second explicit retry after parking. Failed-row restart preserves the payload
instead of deleting it as successful stale work. Attempt limits gate backend work only: a valid checkpoint
or canonical note remains filing/completion authority even at `max_attempts + 1` after a crash.

Malformed quality and unreadable/newer manual-retry checkpoint authority fail with content-free errors
before rearming. Unknown/changed backend origins retain the prior refusal rule. The inherited automatic
same-backend handling of an unreadable checkpoint is unchanged; this amendment does not certify that
path as fail-closed. Failed-job GC retains recording retry authority while its recording row exists,
including long audio retention and failed unlink. Confirmed artifact removal or explicit discard deletes
the exact owned retry metadata. Malformed/unbound identities are retained conservatively.

The [regression inventory](../../docs/live-alignment-regressions.md) maps all 34 new tests to their faults,
records behavioral failure-first receipts, and distinguishes portable source proofs from pending native
validation. This amendment does not add a note-read API, new startup discovery, a new scheduler, a second
quality model, or new egress permission.

### Retain settled history through redo and successful cleanup

Successful job/checkpoint cleanup must not delete the only machine-readable history of a live mismatch.
The existing mandatory sidecar therefore has an optional `settled_quality` containing the same bounded
`QualityRecord`. Capture quality, recording/audio binding, expected frame count and storage phase remain
immutable during settlement. The existing private atomic/rollback path and 256 KiB envelope bound cover
both fields; recording-owned retention and temporary cleanup remain unchanged. Capture finalization
preserves an already-published adjunct without treating it as permission to advance storage state.

Intrinsic source coverage remains separate from storage eligibility. Loading an unfinished envelope
must not overwrite that coverage with a temporary unknown value that settlement would retain forever.
Note/provenance classifications project otherwise-complete coverage to unknown while explicit storage
is unverified; known loss and genuine legacy uncertainty remain unchanged. Recovery still requires
verified storage. Later capture finalization can establish storage eligibility without erasing any
source or processing facts. Legacy checkpoints with absent storage metadata keep their existing meaning.

Backend refusal is processing incompleteness, not unknown capture coverage. Refusal retains an existing
outstanding processing loss or, if there is none, records one bounded unpositioned processing obligation.
It represents the recording-wide refusal without a decoder-region result; it is not an exact audio
interval or a claim that a decoder ran. Existing recovery provenance/body explains the refusal. A later
authorized whole-file success may resolve this obligation; genuine unknown capture and source loss
remain unchanged. The backend gate and its zero-staging/zero-decoder/zero-hosted behavior are unchanged.

Redo loads lifetime history plus any remaining retry/checkpoint facts before decoding. A historical
outstanding Live interval already covered by retained recovered Live history stays recovered only on
matching epoch/track/cause/class/stage and covered frame portions. Unmatched extensions, Batch-stage
failures and ambiguous omitted counts stay conservative. An old recovered checkpoint is not a fresh
whole-file success and cannot erase a later Batch failure at the same interval. Generic snapshot merge
semantics are unchanged; only an actual authorized successful whole-file operation resolves loss.

Settlement claims the existing exclusive `.previous` transaction ownership before reading the latest
envelope. Stale writers fail rather than overwrite a concurrent winner; reload/merge/retry preserves
distinct facts. Successful batch settlement is retained before checkpoint publication, and both Done
paths require retention before completing the row and deleting transient recovery authorities. Failure
keeps the recording/job/checkpoint recoverable. Non-print redo retains its result before filing;
`--print` remains read-only. Missing/corrupt capture authority is never fabricated to retain an adjunct.

The new reader accepts existing sidecars without the optional field. Intermediate commit `77ca04b`
rejects the new field because its private envelope uses `deny_unknown_fields`; no compatibility with
that unreleased intermediate format is claimed. The released base had no quality sidecar. Existing
released queue/notes/settings formats and SQLite schema version are unchanged.

### Pre-AEC channel activity and finish-time AEC identity (#156)

The same schema-1 `QualityRecord` carries bounded Capture, Offline and Live activity observations,
each bound to its raw input and retained rate. Writers observe normalized/downmixed channels before
filtering, live consumers before mismatch padding/AEC, and an authorized offline raw pass before its
filter. An unavailable numerical zero is not observed silence. Legacy, foreign and already-clean
artifacts cannot supply missing historical pre-AEC availability. Positive observations and uncertainty
survive retries; only an independently complete traversal with an exact bound terminal extent can
prove absence. Partial traversals cannot combine into that proof. A valid complete same-input retry
can establish current coverage while retaining uncertainty history; contradictory input/proof facts
remain unknown.

Offline observation additionally requires the selected recording's existing, validated capture-processing
marker to affirm wholly raw input and agree with the carried record's unambiguous input binding. An
ordinary capture WAV may already contain filtered mic PCM; its name, Capture identity and sample rate
cannot replace that marker. `--input` and disk-only redo keep existing audio/AEC routing but retain
unknown Offline activity when the marker is missing. Input/proof conflict cannot seed a fresh definite
scope. This gate does not reject historical uncertainty by itself and changes no quality wire field.

Five complete one-second bins of active microphone RMS above the accepted, explicitly uncalibrated
−60 dBFS threshold and exactly zero tap produce a suspected silent-reference diagnostic. This is
`DegradedOnly`, including bounded omissions and diagnostic counter saturation. It does not request
fallback, make source/text incomplete, change storage authority or alter AEC/audio routing.

AEC provenance records actual execution disposition separately from the matching observation scope
and `far_reference` (`unknown`, `present`, `intermittent`, `absent`). Only an applied disposition is
refined by reference evidence; disabled, not-applicable, raw fallback, mixed/degraded and failed
dispositions retain precedence. An enabled live session starts pending and records the actual short
0.1-second lookahead. Settings alone cannot establish that a canceller ran, nor can a clean-WAV redo
invent observations of its earlier raw input. Offline observations remain in the existing quality
handoff after an AEC/backend failure and are retained by the existing recording owner; print-only
callers do not publish quality history. A retention failure preserves the original backend error.

The existing owned-note metadata-only finish refresh also publishes these AEC/activity fields, even
without hosted work or an assembled final transcript ledger. It continues to operate only on the
current returned note path and preserve the body and inode before the state flip. Its existing
non-atomic failure behavior, ownership rules and checkpoint/retry authority remain unchanged. There
is no additional note-read API, quality carrier, queue schema, capture graph or AEC default change.

The live consumer observes the actual pre-filter tee slices, retained positions, epoch bindings and
availability before mismatch padding, AEC or decoder admission. Its one recording-scoped tracker
survives text-window rotation and publishes open runs before a later decoder failure can discard
them. Only a matching, unambiguous Capture completion proof supplies its terminal extent; channel
closure or the last received chunk cannot do so. Supplementary activity merges against Finish's
epoch authority without adding capture coverage or omitted epochs. Unmappable runs use the same
bounded diagnostic omission lower bounds, so repeated finish/fallback snapshots cannot recount them
or turn activity-only capacity loss into source/processing incompleteness.

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
