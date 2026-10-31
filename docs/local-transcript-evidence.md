# Local transcript evidence (#164)

Corti can retain `<recording>.evidence.json` beside each retained recording to explain how its
transcript changed. This optional file contains pre-cleanup transcript text, typed edit decisions,
stage/parent identities and the final bounded quality record. It contains no audio. This page
covers disabling it, replay, identity and resource limits.

The file can contain text removed from the note. It is separate from the vault/exported note,
hosted request, queue schema and mandatory quality sidecar; none of those embeds this record.
The setting defaults to enabled, including when an older settings file omits it. To disable it,
quit Corti, add or edit this section in `~/.local/share/corti/config.toml` (or
`$CORTI_DATA_DIR/config.toml` when that override is set), then restart Corti:

```toml
[evidence]
enabled = false
```

Disabled sessions write no evidence. The next retention sweep removes existing evidence and
registered temporary files, including active recordings before their first note/queue row and
AWS-owned rows:

- Discovery scans the configured capture directory without recursion, accepting only production
  evidence names and exact private-file temporary names, then checks queue-owned paths. This also
  finds interrupted publication after restart without creating an early queue row.
- Unknown names, subdirectories, audio and mandatory quality are untouched. Symlinks are unlinked
  without following their targets. Audio and quality retain their existing deletion authority.
- Opt-out cleanup runs before AWS cleanup enqueue; an enqueue failure cannot bypass it. A live
  session that observes opt-out releases its evidence owners and cannot recreate evidence later
  in that session.
- Explicit discard and its post-worker reaper remove evidence. Done keeps it; ordinary recording
  deletion removes it.

## Durability and replay

The private-file helper writes a 0600 sibling temporary, syncs it, atomically renames it and syncs
the directory. Temporary names participate in the checkpoint scan. Write, file-sync and rename
failures preserve prior canonical bytes. A directory-sync failure follows rename: complete new
bytes may already be visible, but the result remains unavailable. Cleanup tries the canonical
unlink even if directory enumeration fails and reports incomplete temporary cleanup.

Batch publishes evidence before the filing checkpoint. The settings guard covers both the enabled
check and publication, preventing a concurrent disable/sweep from completing before a later
recreation. The checkpoint carries an optional content-free receipt bound to the recording ID,
start time and random attempt generation.

`PendingNote` validates that receipt without changing evidence or invoking ASR, even if audio is
gone. Missing, corrupt, disabled or mismatched evidence does not invalidate the filing checkpoint.
A pre-checkpoint crash uses the existing retry and replaces the generation instead of appending
another ASR generation. Evidence adds no retry scheduler or queue column.

Live atomically replaces the same bounded file after each logical window, before note append.
Persistence errors and truncation do not discard transcript words or fail filing. Truncation is
sticky, with bounded omission counts; an incomplete ledger is labelled incomplete.

## Stage and processing identity

Batch records backend output once, followed by the actual cleanup passes. Live retains parent IDs
through early echo, raw-echo comparison, segmentation and diarization:

- Early echo records the classifier's source inventory at assessment time, before later pruning.
  Immediate publication without classification has no comparison context.
- Emitters supply UTF-8 source ranges. Cleanup assembles retained parents without creating another
  ASR snapshot. Raw mic polls used for external comparison are `ContextSnapshot` inputs.
- Missing lineage is unavailable. Text/time similarity never invents a parent.

`ProviderInput` identifies the immutable transcript handed to an enabled finalizer. It proves an
application handoff, not a network request or provider acceptance; the evidence file is not sent.
The finalizer can decline or fall back.

`FinalProse` records selected application output, not a note-commit certificate. After hosted
acceptance succeeds, live persists that stage before rewriting the note; typed fallback follows
the same order. A crash or write failure can leave selected output in evidence before note commit.
Rejected generations are excluded. Live final input sorts known parent IDs in writer order,
propagates protections and checks the complete ordered rows against the offered finalizer input.

The identity document retains allowlisted configuration, selected model identifiers, package
version, backend and mode. Its SHA-256 values fingerprint that document; they are not model
artifact digests or verified executable hashes. AWS's acoustic model is opaque. Credentials,
buckets, profiles, directory paths, arbitrary settings and duplicate quality data are excluded.
Readback validates the document and every group's fingerprints.

Each stage records actual signal processing. Live mic AEC records its 0.1-second requested
lookahead and actual frame count; far audio records no AEC. Capture and offline processing are
distinct. A processed WAV cannot claim raw input or exact live replay. Live's immutable
configuration starts with pending execution; stage identity and final quality carry the actual
execution and activity observations.

## Bounds and validation

| Resource | Bound |
| --- | --- |
| Whole file | 16 MiB, including a 4 KiB app identity/receipt envelope, 4 KiB portable truncation metadata and 256 KiB quality reserve |
| Portable retained ownership | 32 MiB, including a 64 KiB owned quality reserve |
| One invocation | 8 MiB and 8,192 source rows |
| Early-echo comparison IDs | 257 per assessed region; 1 MiB retained vector capacity across one returned release batch, including split polls releasing more than 64 regions |

App parent vectors and terminal inputs have separate admission bounds. Allocation, lineage or
capacity failure marks evidence incomplete while preserving words and verdicts. An exact-size
allocation request must fit the remaining budget; a returned capacity exceeding it is dropped.
Encoding uses one exact-length buffer and borrowed JSON envelopes, without a second
`serde_json::Value` document.

Backend transcripts are caller-owned: evidence rejects an oversized optional snapshot before
cloning, while transcription still files its words. These limits are not a whole-process or
arbitrary-backend-input cap. Conservative portable parse/validation accounting plus the app file
buffer is 592 MiB. An original ledger, encoded buffer and caller input retained alongside readback
add their own ownership. The bounds exclude allocator overhead, RSS and native model memory.

The [evidence test guide](../app/tests/evidence/README.md) defines the actual-function harness;
its [allocation fixture observations](../app/tests/evidence/README.md#allocation-fixture-observations)
retain the measured encode/readback payloads. The probe holds caller input, the original ledger, encoded file and parsed
ledger together; its measured fixtures are not universal memory or latency guarantees. The
model-free harness exercises real batch cleanup, live consume/flush/finish, persistence,
checkpoint replay and retention with invented inputs. Native macOS builds, decoder gates and
full application integration remain separate checks.
