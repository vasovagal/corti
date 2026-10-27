# Live track alignment and durable recovery regressions (#138)

This change keeps the available audio from a mismatched microphone/reference chunk and records the
missing tail as processing loss. It carries those facts through live errors, batch recovery, retry
storage, and retention. All fixtures use synthetic audio/text; no model, hosted service, or private
recording is needed for the portable proofs below.

## Behavior and accepted limits

When an epoch expects both tracks, the shorter track is padded to the longer track's frame count.
Padding enters the existing AEC in its normal order; unavailable microphone samples are masked back to
zero after filtering, including delayed output and the final tail. A legitimately absent track does
not count as a mismatch. Pure tap-only input does not start the microphone filter or decoder. The AEC
algorithm and its defaults are unchanged.

Each tee chunk now carries its producer's retained-frame start and exact epoch binding. Mismatch
positions use those fields, not the number of frames the live consumer happened to receive. With a
dropped middle tee chunk, a 37-frame short microphone tail at retained start 8,192 maps to
`[12,251, 12,288)`; after an unfilled source jump whose new epoch starts at source frame 96,000, the
same tail maps to `[100,059, 100,096)`. Existing tee loss remains in the settled record. Rate conversion
rounds interval endpoints outward and refuses uncertain/conflicting or cross-epoch mappings.

Accounting retains at most **64 observed epoch bindings and 256 mismatch intervals**, coalescing
compatible adjacent tails before the interval cap. Omitted epochs, omitted processing spans, and
saturation remain explicit. Epochs from good chunks are retained too, so a later layout mismatch can
keep an exact interval before Finish arrives. Audio and availability scratch slices are at most
**4,096 frames**. Warnings report mismatch count **1 and each multiple of 100**, once per received
mismatched chunk rather than once per window/filter block.

Each channel fixes its word-time origin at first presence. A long never-present prefix is not replayed
as silence; absence after a channel starts advances it with bounded silence. AEC evidence receives the
microphone origin once. This does **not** repair later tee-drop compression of live word times: origins
still use the received-frame timeline. The accepted recovery for that case is the whole-file batch
replacement. Exact mismatch quality positions use the independent producer timeline.

Live mismatch evidence uses the existing `QualityRecord`. Before the recorder verdict, an Observed
record is **supplemental processing information**, not proof of capture or storage completeness.
Normal Finish freezes it against the immutable capture snapshot before fallible filtering/filing.
Consume errors and panics retain accumulated facts; a consumption panic waits for Finish/Discard,
never resumes the decoder, and never invents a missing Start epoch if authority disconnects. Discard
still removes the owned note. Batch recovery independently reloads mandatory capture/storage authority
before merging the supplemental record. Missing, corrupt, unfinished, or unverified storage cannot be
certified by live facts. Successful authorized whole-file recovery resolves processing loss only;
source loss remains incomplete. Capture fields and immutable `capture_processing` remain unchanged;
the existing sidecar can also retain a separate optional settlement of that same quality record.

The existing retry payload carries `settled_quality`; recording-id deduplication and failure/reschedule
settlement merge it monotonically without resetting origin, claim, budget, backoff, or note ownership.
No-note fallback has the same durable carrier. Manual retry atomically resets the Failed recording and
rearms its job, consuming `first_batch=false` without inventing historical origin. Failed-row recovery
parks evidence rather than deleting it as successful stale work. Spent budgets block additional ASR;
a valid checkpoint or canonical note still permits filing/completion at claim `max_attempts + 1` after
a crash. An exhausted but still-active job keeps its spent budget, so an explicit manual click may
require another click after that job is parked.

Unknown/changed automatic backend origins refuse before staging/ASR/hosted effects. Malformed quality
and manual-retry checkpoint authorities fail with content-free errors; malformed/future checkpoints
cannot be silently converted into a manual retry's known AWS permission. The inherited automatic
same-backend handling of an unreadable checkpoint is unchanged and is not certified as fail-closed by
this work. Failed-job GC preserves recording retry authority for the recording's lifetime, including
365-day retention and failed artifact deletion. Confirmed artifact removal or explicit discard removes
that exact recording's retry metadata. Malformed/unbound retry identities remain conservatively
retained rather than guessing ownership.

## Runtime failure-first evidence

The receipt paths are local execution artifacts, not repository fixtures. Each listed failure reached
a behavioral assertion; compilation errors are not used as bug evidence.

| Receipt | Failure observed before the corresponding correction |
| --- | --- |
| `/tmp/corti-mismatch-check/before.log` | Actual original consume/filter source delivered 8,192 rather than 12,288 frames in all four AEC on/off × mic-short/tap-short cases. |
| `/tmp/corti-mismatch-check/epoch-before.log` | A good Start followed by a layout mismatch produced no provisional epoch inventory instead of the two required bindings. |
| `/tmp/corti-mismatch-check/panic-before.log` | A scripted consume panic disconnected before waiting for the recorder verdict; the skipped-Start ownership regression failed. |
| `/tmp/corti-138-quality-carrier-before.log` | A duplicate recording lost the second quality span; malformed quality was accepted. |
| `/tmp/corti-138-manual-rearm-before.log` | Split rearm/reset routed a recovered Failed row to stale deletion; resetting attempts restored first-batch hosted permission. |
| `/tmp/corti-138-exhaustion-crash-before.log` | Failed-row restart returned successful stale completion, which would delete the sole evidence payload. |
| `/tmp/corti-138-unreadable-origin-before.log` | Manual rearm proceeded despite an unreadable checkpoint while preserving a known AWS origin. |
| `/tmp/corti-138-retention-before.log` | Day-31 cleanup deleted both generic debris and the recording's still-needed quality authority: two rows instead of one. |
| `/tmp/corti-138-active-budget-before.log` | Explicit reset around an exhausted active job allowed another backend operation. |
| `/tmp/corti-138-final-checkpoint-before.log` | The initial broad budget guard rejected filing after the last permitted ASR had durably published its checkpoint. |

## New permanent test inventory

All **34** new Rust test functions from `/tmp/corti-138-new-tests.json` appear exactly once below.
The required result describes the fault each test would expose, rather than just the code it executes.

### `crates/corti-core/src/capture_mismatch.rs` — 7

| Test | Fault detected / required result |
| --- | --- |
| `both_short_sides_and_empty_expected_tracks_keep_exact_missing_tail` | Mic-short, tap-short, and fully empty expected tracks produce one exact tail on the missing track; intentionally absent layouts produce no mismatch and capture input remains unchanged. |
| `retained_positions_survive_tee_drops_unfilled_jumps_and_rate_rounding` | Consumer-frame counting would misplace loss after a dropped tee chunk/jump; producer mapping must yield epoch 1 `[100059,100096)` and outward-rounded fractional-rate `[104,107)`. |
| `absent_conflicting_cross_epoch_or_arithmetic_mapping_remains_counted_loss` | Missing/conflicting bindings, a boundary crossing, invalid rates, underflow, or overflow must become counted omitted processing loss, never a fabricated exact span. |
| `cap_coalescing_warning_cadence_and_saturation_keep_all_loss_accounted` | 300 disjoint mismatches retain 256 spans plus 44 outstanding omissions; adjacent tails coalesce, warnings are 1/100/200/300, and counter overflow stays saturated. |
| `good_start_epoch_keeps_a_later_mismatch_exact_before_finish` | Dropping good-chunk epoch bindings would erase a later exact interval; provisional quality must retain Start plus layout epoch and span `(epoch 1, 6..8)`, which verified recovery can resolve. |
| `observed_epoch_inventory_is_bounded_and_conflicts_remain_counted_loss` | The 65th epoch must not grow the 64-entry inventory or be recounted for repeated chunks; mismatches without retained or consistent bindings remain outstanding omissions. |
| `provisional_processing_facts_merge_only_with_independent_capture_authority` | Supplemental Observed loss may resolve over a verified observed base but cannot turn a LegacyUnknown base complete; missing Start produces an omission rather than invented mapping. |

### `crates/corti-coreaudio/src/capture_writer.rs` — 2

| Test | Fault detected / required result |
| --- | --- |
| `mic_only_epoch_emits_absent_tap_and_bounded_positioned_chunks` | A two-track→mic-only epoch must flush at the boundary, emit starts `[0,1,4097]`, leave tee tap vectors empty, keep chunks ≤4096 frames, and still write zero tap samples in the stereo WAV without false mismatch. |
| `tee_position_after_a_drop_keeps_the_writer_clock` | A full tee may drop its second 4096-frame chunk, but the next three-frame chunk must start at retained frame 8192 with the writer's epoch, not at the consumer's compressed position. |

### `app/src/live.rs` — 8

| Test | Fault detected / required result |
| --- | --- |
| `mismatched_chunks_keep_both_tracks_in_filter_order` | Across AEC on/off, either short side, and 37-frame/whole-chunk loss, both decoders receive the padded ordered timeline; post-AEC mic gaps stay zero, later words end at frame 12288, and the exact missing tail survives fallback. |
| `single_track_layouts_do_not_create_mismatches` | Legitimate mic-only/tap-only input must keep its present channel exact, leave the absent decoder unstarted, and finish without invented mismatch loss, with either AEC setting. |
| `mismatch_uses_writer_position_after_a_tee_drop_and_unfilled_jump` | Exact mismatch intervals must follow retained writer position and epoch source offset despite a missing tee chunk; the pre-existing TeeDrop span must also survive. |
| `mismatch_warnings_report_the_leading_edge_and_each_hundred` | Actual tracing events for 300 mismatches must contain counts `[1,100,200,300]`, preventing per-chunk/window log floods or missing leading-edge notice. |
| `late_track_origins_and_absent_epochs_preserve_word_and_filter_clocks` | Late first presence and later legitimate absence must preserve channel origins, original first/final word times, ordered AEC input, and zeroed unavailable output through rolling windows. |
| `late_microphone_origin_shifts_acoustic_evidence_once` | A one-second microphone origin must move AEC block times 0.25→1.25 and 0.5→1.5 across window clearing, without missing or double-applying the shift. |
| `mismatch_history_survives_final_filing_failure_later_consume_error_and_panic` | A final filer error, subsequent consume error, or decoder panic must retain the earlier exact epoch-1 `6..8` mismatch in fallback and leave immutable capture facts untouched. |
| `consumed_panic_waits_for_finish_when_the_start_epoch_tee_was_dropped` | A panicked decoder must wait for ownership: Finish supplies exact Start-backed quality, Discard deletes the owned note, and disconnect retains a counted omission without inventing Start or resuming decoding. |

### `crates/corti-jobs/src/lib.rs` — 4

| Test | Fault detected / required result |
| --- | --- |
| `recording_quality_merge_survives_duplicate_stale_claim_and_reopen_without_reset` | Duplicate quality and canonical note ownership must survive stale-claim failure settlement and DB reopening, while original backend, first-batch flag, attempt count, budget, and due time remain authoritative. |
| `malformed_recording_quality_fails_before_mutation_without_payload_diagnostics` | Invalid quality and conflicting epochs must reject insertion/update without mutation; synthetic private enum text must be absent from the full diagnostic chain, including corrupt existing payloads. |
| `failed_recording_quality_merge_and_manual_rearm_are_atomic_without_implicit_restart` | Incidental duplicates may merge into parked failure but cannot restart it; injected rearm failure rolls back, explicit rearm preserves evidence/unknown origin, and active work keeps its schedule/budget. |
| `recording_reschedule_merges_new_quality_and_rejects_stale_claims` | Terminal-write rescheduling must retain quality merged since claim; settlement by a no-longer-running claim must fail without altering the newer row. |

### `crates/corti-queue/src/lib.rs` — 3

| Test | Fault detected / required result |
| --- | --- |
| `manual_recording_rearm_rolls_back_both_authorities_and_reopens_together` | An injected recording-reset error after job rearm must roll back both writes; success reopens with matching pending states, preserved quality/note/origin/capture metadata, and consumed first-batch permission. |
| `manual_recording_rearm_preserves_absent_origin_active_claims_and_backoff` | Absent, pending, and running job branches must retain unknown origin and active scheduling/budget; a stale pre-reset claim cannot restore `first_batch=true`. |
| `recording_retry_gc_preserves_owned_and_unknown_payloads_until_explicit_cleanup` | GC may delete an old orphan but must retain bound and malformed/unbound retry authorities; explicit exact-id cleanup removes only its owner while history can remain. |

### `app/src/jobs.rs` — 9

| Test | Fault detected / required result |
| --- | --- |
| `settled_retry_quality_survives_long_retention_and_failed_artifact_deletion` | Day-31 GC under 365-day audio retention deletes only generic debris; day-366 unlink failure preserves quality and row, while successful artifact removal deletes the exact authority. |
| `manual_rearm_crash_before_recording_reset_repairs_routing_without_losing_quality` | Reopening after the atomic retry transaction must route to transcription or checkpoint filing with full quality; no pending-job/still-Failed intermediate state remains. |
| `manual_rearm_never_restores_first_batch_hosted_authorization` | A formerly fresh local job reclaimed at attempt one after explicit retry must be `Retry(Local)` with first-batch false and no restored hosted-final permission. |
| `exhaustion_crash_retains_settled_quality_without_repeating_backend_work` | Crash after Failed-row publication but before job parking must make zero additional backend calls and retain full quality/origin in a parked payload, rather than complete/delete it as stale. |
| `manual_rearm_of_exhausted_active_job_cannot_extend_its_attempt_budget` | Manual retry racing recovered active work must keep its spent `2/1` claim budget, refuse another backend operation/hosted permission, and retain evidence when parked. |
| `final_attempt_checkpoint_crash_recovers_filing_without_an_extra_backend_call` | A final permitted ASR checkpoint followed by a crash must route claim `2/1` to filing/canonical completion with zero ASR; transcription actions remain over-budget refusals. |
| `manual_retry_unreadable_checkpoint_cannot_rearm_known_aws_origin` | Malformed/future checkpoints and top-level/provenance enum sentinels must cause zero manual rearms, retain parked AWS origin, and redact diagnostics; missing checkpoint remains a valid lookup case. |
| `settled_mismatch_quality_survives_retry_rewrite_reopen_refusal_and_checkpoint` | Local/AWS/unknown origin × selected backend matrix must preserve source+mismatch history through note-payload rewrite/reopen/checkpoint; refused closures run zero times, matching recovery resolves processing only, and capture authorities stay byte-identical. |
| `supplemental_live_quality_never_certifies_missing_corrupt_or_unfinished_capture` | Supplemental Observed facts cannot certify missing, corrupt, or in-progress mandatory storage; verified storage permits processing recovery while source loss remains incomplete and durable input snapshots stay unchanged. |

### `app/src/pipeline.rs` — 1

| Test | Fault detected / required result |
| --- | --- |
| `note_less_live_fallback_retains_settled_quality_for_durable_batch` | A mismatch fallback without a note must still transfer its full typed quality record into the existing durable retry payload. |

## Existing test edits

Existing live tests add epoch/retained-position fixture fields, optional fallback quality fields, and
pattern wildcards; their previous transcript, availability, publication, and discard assertions remain.
The existing pipeline checkpoint-rewrite test likewise adds the new optional field without changing its
same-inode recovery expectations. Scripted decoder instrumentation supports the new audio/time/panic
checks without replacing those old assertions.

Three existing producer tests gain behavioral assertions rather than merely fixture repairs:
`complete_eleven_second_loss_run_across_rates_is_not_filled` and
`large_microphone_loss_requires_a_new_epoch_without_expanding_the_tee` now require two epoch-homogeneous
tee chunks with retained starts 0 and 2; `changed_microphone_rate_keeps_fixed_output_clock_and_unlabelled_pcm_is_rejected`
also checks exact epoch bindings and the second retained start. Epoch boundaries now flush the preceding
partial tee tail. Their previous retained PCM counts, no-large-gap-fill behavior, and availability/rate
checks remain. No prior assertion was weakened to accommodate dropped audio.

## Validation status

The integrated portable aggregate passed **509 unit tests + 1 doc test = 510**, with four inherited
model-dependent tests ignored. `/tmp/corti-138-integrated-gates.log` records all five gates exiting zero:
diff check, formatting, default-feature strict Clippy, all-feature strict Clippy, and tests. Both Clippy
gates cover 13 packages. Included package totals are **118 core tests, 19 jobs tests, and 26 queue tests**.
Separate source-extraction gates passed **34 live/writer tests and 10 carrier/recovery tests**, with strict
Clippy receipts. These suite totals include existing controls and are not a count of the 34 new functions.

The exact source candidate `46f61b4249b84caffc45d522698f1ac23a9a42dd` passed the
[macOS preflight](https://github.com/vasovagal/corti/actions/runs/40495859566/job/122161002595).
Default/AWS-only/local-only app suites passed **277/273/276** tests with their strict lint gates.
The native suites passed **277 app, 118 core, 50 coreaudio, 16 capture, 14 detect, 19 jobs, and
26 queue tests**. All **48 required sentinels** (34 new plus 14 retained), the **18 existing native
transcription checks**, and **90 rate measurements** passed. Python checks report **39 + 18** passes.
The [preflight artifact](https://github.com/vasovagal/corti/actions/runs/40495859566/artifacts/12730771004)
is 43,039 bytes with SHA-256
`ad14dab3230eba09e44d0ecfdc5ed945a7c50d453e8333730a8c13992a312122`.
This proves the pinned candidate; the final implementation commit still requires its own PR-head
and per-commit gates. Real HAL/device behavior remains an owner-Mac check.

Portable extraction runs use actual source logic and actual permanent test bodies. The live harness
uses explicit fakes at note-I/O, hosted, native-decoder, and HAL boundaries; the carrier harness uses
the real queue/jobs/store/checkpoint code with backend/live enum-shape shims. They prove the stated
logic and persistence behavior, not a native app, device, permission, decoder, or end-to-end capture run.
No independent-review acceptance is claimed here.

| Evidence bundle | Final portable receipts |
| --- | --- |
| `/tmp/corti-mismatch-check/README.md` | `epoch-core-tests.log`, `epoch-core-clippy.log`, `epoch-extraction-tests.log`, `epoch-extraction-clippy.log` in the same directory; original `before.rs` and `epoch-before.rs` retain the failing source extractions. |
| `/tmp/corti-138-carrier-receipts.md` | `/tmp/corti-138-jobs-queue-after.log`, `/tmp/corti-138-jobs-queue-clippy.log`, `/tmp/corti-138-carrier-after.log`, `/tmp/corti-138-carrier-clippy.log`; exact commands and frozen source hashes are in the bundle. |

The design choice is recorded in [ADR 0014](../design/adr/0014-transcript-generation-provenance.md#live-alignment-and-settled-quality-amendment-2026-10-27-138).

## Fresh checkpoint review: quality history survives redo

The original retry/checkpoint carriers disappeared at successful cleanup, and redo did not consume
them even while present. The initial actual-source CLI regression failed with zero mismatch spans
instead of one (`/tmp/corti-redo-settled-before.log`). The existing mandatory sidecar now retains an
optional settled `QualityRecord`, independently of its immutable capture/storage fields. Redo consumes
that lifetime history plus remaining transient facts. The 256 KiB bound, private transaction files,
recording lifetime and exact-owner cleanup apply unchanged. Publication failure cannot complete the
recording or delete its checkpoint/job. `--print` remains read-only, and missing/corrupt capture
authority cannot be invented or certified by retained processing facts.

| New permanent test | Fault detected |
| --- | --- |
| `redo_retains_live_mismatch_while_retry_payload_exists` | Dropping available retry history, or fabricating missing/corrupt capture authority. |
| `redo_history_overlay_splits_live_overlap_but_never_recovers_new_batch_failure` | Recovering unmatched Live extensions, fresh Batch failures, or ambiguous omitted counts from an old recovered snapshot. |
| `redo_history_overlay_stays_bounded_when_overlap_splits_at_the_span_cap` | Unbounded interval splitting or silently dropping outstanding overflow. |
| `concurrent_settlement_publishers_preserve_the_winner_and_retry_union` | Two simultaneous publishers losing a winner's facts, or a reloaded retry failing to preserve their union. |
| `settled_history_uses_existing_publication_and_rejects_stale_attempts` | Stale settlement, publication/rollback failures, or mutation of immutable capture fields. |
| `settled_partial_history_survives_capture_finalization_without_promoting_storage` | Capture finalization losing an existing adjunct, or settlement falsely promoting capture storage. |
| `redo_in_progress_retains_source_coverage_through_finalization` | Persisting an in-progress storage projection as permanent source uncertainty, or clearing genuine unknown/source loss when capture later becomes durable. |
| `redo_finalizing_retains_source_coverage_through_finalization` | The same coverage poisoning during finalization, including both settlement/finalization orderings. |
| `storage_completeness_projection_preserves_intrinsic_quality` | Reporting unfinished storage as complete, mutating the checkpoint's intrinsic quality, or downgrading known loss to unknown; checks all storage states and legacy absent-status behavior. |
| `local_redo_after_backend_refusal_recovers_processing_without_rewriting_source` | Persisting a backend refusal as permanent source uncertainty, duplicating an existing processing obligation, or allowing refused backend side effects; retains source-loss/unknown controls through actual completion and later local redo. |

The existing `explicit_local_redo_after_done_cleanup_resolves_tee_loss_without_inventing_origin`
now includes live-only mismatch history and exercises the actual production completion helper under
publication failure and success. Its old whole-sidecar byte assertion becomes equality of every frozen
capture/storage/identity/frame field while allowing the optional settlement. Queue capture JSON stays
byte-identical. Existing explicit-local authorization and source-loss controls remain; normal successful
checkpoint/job removal, restart and disk-only redo must retain exact mismatch history. The test also
imports an old recovered checkpoint while a newer same-interval Batch loss remains outstanding.

The two phase regressions initially failed through actual app helpers with Unknown instead of Complete
after clean capture finalization and successful whole-file recovery (`/tmp/corti-redo-phase-before.log`).
Intrinsic record coverage is now retained separately from the note/provenance storage projection;
unfinished storage still prevents recovery and a complete classification. Genuine unknown capture and
source loss remain unknown/incomplete. The existing supplemental-quality test changes its unfinished
record's intrinsic source expectation from Unknown to Complete while requiring reported Unknown.
The provenance matrix also failed before the correction with Complete instead of Unknown
(`/tmp/corti-redo-storage-projection-before.log`); it preserves known loss precedence and existing
partial/unverified body markers without changing the full quality record stored in a checkpoint.

The same lifetime boundary applies to backend refusal. Refusal must not overwrite intrinsic source
coverage: it retains outstanding processing loss or adds one unpositioned processing obligation when
none exists. This is a recording-wide refusal with no decoder-region result, not exact interval
coverage. Existing refusal provenance/markers remain explicit, and the backend gate performs no staging,
decode or hosted work. Verified authorized whole-file recovery can settle processing while preserving
source loss and genuine unknown coverage.

`/tmp/corti-redo-refusal-permanent-before.log` records the permanent test failing Unknown versus
Complete with the original production assignment. Its eight cases retain the same production helper,
checkpoint/completion path, cleanup/restart and explicit local recovery, with and without earlier
processing loss, source loss and genuine unknown coverage. Repeated refusal is idempotent, and a
refused backend dispatch must execute zero staging/decoder/hosted effects.

At this source freeze, the final actual-source harness passes eleven CLI/storage/provenance tests
with strict Clippy (`/tmp/corti-redo-refusal-{after,clippy}.log`). The unchanged portable core/jobs/queue/capture
proof passes 125/19/26/15 tests; receipts are
`/tmp/corti-redo-settled-{portable-tests,clippy}.log`. The phase tests cover 24 combinations and the
reporting test 32 combinations. Native validation for this fix is pending at this freeze; the earlier
receipts above apply to the prior implementation.
The new reader accepts old sidecars with the adjunct absent. Intermediate `77ca04b` rejects the added
field because of its private envelope's `deny_unknown_fields`; the released base had no such sidecar.
