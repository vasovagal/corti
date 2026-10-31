# Evidence persistence regression gate

Run from the repository root with the workspace's cached dependencies and a private target:

```sh
python3 app/tests/evidence/run.py --output-dir /tmp/corti-evidence-harness --clippy
python3 app/tests/evidence/test_receipt.py
python3 app/tests/evidence/run.py --allocations --output-dir /tmp/corti-evidence-allocation -- actual_maximum_envelope_allocation_probe -- --nocapture --test-threads=1
```

The default gate requires exactly the 44 named Rust tests in `verify_receipt.py`, no ignored or filtered tests, and successful strict Clippy. Missing, extra, duplicate, failed, ignored and zero-test receipts fail. A filtered invocation is explicitly a scoped probe, not the complete suite. The two Python receipt tests exercise this distinction. Cargo's temporary harness lock may prune workspace members, but cannot introduce a registry name/version/source/checksum absent from the workspace lock. Every actual source and reused fixture input is printed with SHA-256.

## Actual paths and boundaries

`run.py` reuses the decode-quality runner's production-item copier and lock validation. It copies actual batch transcription, pipeline/checkpoint/replay, retention dispatch, CLI note rendering, private-file operations, provenance and the evidence modules. `live_harness.py` copies actual consume/flush/finish, writer, raw echo, window, boundary, discard/reaper and quality methods. CaptureChunk and its chunk constant are copied verbatim; their platform transport is absent. The native manager/constructors, Tauri/tray/stats and lexicon-loading boundaries remain those declared by the reused harness. ASR and diarization responses are invented. The AEC signal test uses the real streaming filter on synthetic PCM.

`hosted_boundary.rs` scripts only the existing hosted service/transport outcome and acceptance fence. Production finalization orchestration, note rewrite and evidence selection are actual functions. No provider is called. The read-only/export test executes the actual transcription wrapper and CLI renderer; it does not certify CLI argument parsing, native feature compilation, GUI startup or the full CLI executable.

`scheduling.rs` supplies two declared, test-only scheduling seams in the copied source: a one-shot callback at actual `Session::persist` entry, and a one-shot crash immediately after actual `CurrentNote::rewrite_transcript` succeeds. Neither replaces an operation or changes its outcome. They reproduce the independent cold review's opt-out interleaving and accepted-prose crash window deterministically.

`live_optout.rs` runs its capture-directory override in a child test process, avoiding mutation of a multithreaded test process's environment. It uses actual `recordings_dir` and `recording_stem` helpers and then the actual live publication and opt-out sweep. Both filler-only output and a first note-creation failure must leave an empty queue, yet the next sweep must remove evidence before the worker polls the disabled setting. The capture crate source is included in the source-hash receipt.

`tracing.rs` installs a capturing Layer over actual events, including Display/Debug fields. The worker explicitly inherits its Dispatch. The sentinel runs actual batch cleanup, live flush and persistence failures and validates returned error formatting; invented transcript, audio-content and participant markers must be absent. It does not substitute a derived-type formatting test for these paths.

## Falsifier map

Names below omit their module prefix; the complete inventory is pinned in `verify_receipt.py`.

| Test | Failure it detects |
| --- | --- |
| `private_atomic_windows_keep_precise_asr_and_bind_generation` | Changed float bits/preimages, non-0600 output, stale recording or attempt attachment, unregistered temps. |
| `failed_atomic_publication_preserves_old_bytes_and_cleans_registered_temps` | Real write/sync/rename faults destroying old canonical bytes or leaving temps. |
| `directory_sync_failure_reports_error_after_complete_atomic_replacement` | Incorrect claim that a post-rename directory-sync error preserves old bytes or means success. |
| `truncated_evidence_is_durable_and_cannot_fail_filing` | Cap overflow losing explicit durable truncation or exceeding the file cap. |
| `optout_removes_evidence_and_temps_but_never_audio_or_quality` | Optional opt-out deleting mandatory quality/audio or leaving text temps. |
| `capture_optout_discovers_only_owned_names_and_preserves_unrelated_targets` | Capture-root discovery accepting unknown names or malformed temporary names, following symlink targets, recursing, deleting audio/quality, or concealing partial unlink failure. |
| `optout_unlinks_known_file_when_directory_listing_is_denied_and_reports_partial_failure` | The reviewed 0300-directory bug, false total-cleanup success, and mishandled NotFound/blocked-temp cases. Run as a nonroot user to exercise DAC denial; root bypasses that branch. |
| `identity_documents_are_recoverable_and_exclude_secrets_paths_and_quality` | Config/model/runtime changes failing to affect fingerprints, secrets/paths/quality affecting them, missing recoverable IDs or incorrect artifact-hash claims. |
| `readback_rejects_rebound_identity_documents_and_keeps_fixed_error_categories` | Malformed/unknown identity fields, document/group fingerprint rebinding, or content-bearing error output. |
| `merged_maximum_quality_and_activity_fit_reserved_capacity_after_text_truncation` | Combined 64 epochs/256 spans, maximum activity scopes and saturation failing the actual merged quality reserves or disturbing mandatory quality authority. |
| `legacy_settings_enable_evidence_and_explicit_optout_round_trips` | Breaking old settings or losing explicit opt-out. |
| `actual_sweep_optout_and_deletion_cover_active_and_aws_owned_rows` | Wrong expiry/discard ownership or exempting nonterminal/AWS rows from opt-out. |
| `actual_sweep_removes_opted_out_evidence_before_aws_job_enqueue_failure` | The reviewed real AWS jobs-INSERT failure bypassing opt-out cleanup. |
| `actual_batch_pending_note_reuses_byte_identical_asr_without_backend_or_audio` | PendingNote rerunning ASR, rewriting evidence, requiring deleted audio, or removing evidence at Done. |
| `actual_batch_precheckpoint_crash_retries_and_atomically_replaces_one_asr_generation` | Wrong ledger/checkpoint order or retry retaining two ASR generations. A checkpoint-directory fault fires after ledger publication. |
| `actual_batch_optout_and_evidence_write_failure_never_block_filing` | Optional disabled/write-error paths blocking Done, deleting audio/quality, or writing an opted-out ledger. |
| `actual_batch_optout_cannot_complete_between_enabled_check_and_publication` | The reviewed race in which disable/removal completes after the enabled check but before an unguarded publication recreates text. The shared guard must fence that callback; the subsequent actual opt-out sweep preserves audio/quality. |
| `actual_pending_note_survives_optional_corruption_and_optout_without_another_asr` | Optional invalid bytes or opt-out defeating the existing authoritative filing checkpoint. |
| `actual_batch_live_and_persistence_diagnostics_omit_content_under_failures` | Actual tracing/error paths exposing invented transcript/audio/participant markers. |
| `actual_readonly_transcription_and_export_exclude_evidence_and_clean_redo_records_processed_input` | Read-only wrapper creating a ledger, exports embedding preimages/metadata, or a processed redo claiming raw signal input. |
| `actual_live_flush_keeps_one_asr_per_input_and_persists_windows_before_note_failure` | Flush reminting ASR, omitting exact lineage, or a note append failure preceding evidence durability. |
| `actual_live_optout_releases_text_and_never_recreates_removed_evidence` | An active session recreating removed text after observing opt-out. |
| `actual_live_pre_note_optout_sweep_removes_capture_owned_evidence` | The reviewed missing pre-note ownership: filler-only and first note-creation failure leave raw evidence undiscoverable by the next sweep; restart temporary files require in-memory ownership; audio/quality change. |
| `actual_live_evidence_cap_never_drops_words_or_stops_later_windows` | A full optional ledger dropping any of twenty subsequently filed windows or missing truncation. |
| `actual_live_withheld_early_echo_retains_original_ordinal_and_explicit_context` | Losing withheld source text, original ordinal, actual context, or accidentally displaying the withheld row. |
| `actual_live_early_echo_context_matches_pruned_classifier_inventory` | The reviewed mismatch between EarlyDrop's pruned source inventory and the separate carry ring recorded as complete context. |
| `actual_live_early_echo_context_survives_post_assessment_pruning` | Reading the source ring after its post-assessment prune instead of transporting the exact IDs with the assessment. |
| `actual_live_early_echo_immediate_publication_has_no_comparison_context` | Attributing unconsulted source rows to a bypassed classifier. |
| `actual_live_early_echo_missing_lineage_and_cap_preserve_output` | Presenting missing comparison parents as complete, changing region storage size, or optional evidence failure changing publisher/note output. |
| `actual_live_early_echo_split_poll_bounds_context_capacity_and_preserves_every_word` | A single split poll returning more than 64 released regions exceeds the aggregate 1 MiB comparison-ID capacity budget, resets that budget during a long-region drain, or loses original words when optional context is omitted. |
| `actual_live_discard_and_post_panic_reaper_remove_optional_text_and_temps` | Discard or a worker panic leaving text/temps, or the reaper taking over audio ownership. |
| `actual_live_evidence_preserves_critical_raw_protection_through_diarization` | Evidence-enabled pause/diarization/cleanup/provider assembly losing #161 protection in either direction. |
| `actual_live_aec_consume_and_finish_record_track_specific_short_lookahead` | Invented AEC execution, wrong live lookahead/rate, or assigning mic AEC to tap rows. |
| `actual_live_finalization_records_only_selected_hosted_prose` | Rejected hosted prose entering evidence, disabled finalization getting a ProviderInput, or cross-window parent order differing from the actual offered input. |
| `actual_live_selected_and_fallback_prose_precede_note_rewrite_crash` | The reviewed crash window in which accepted note prose becomes durable before evidence. Both accepted hosted output and typed fallback must have a durable FinalProse stage descending from the actual ProviderInput before the injected post-rewrite crash. |
| `activity_live_unknown_authority_keeps_empty_asr_note_free` | Source6 V2's diagnostic-only empty-ASR note/fallback regression; eight actual cases keep source/processing-loss controls. |
| Three reused `live_*protection*` tests | Existing #161 raw poll, units, backchannel and filler behavior remains unchanged with the original assertions. |
| `production_recording_path_invokes_scripted_backend_once_and_keeps_partial_output`; `full_production_chain_counts_backend_and_replays_checkpoint_without_audio` | Existing one-backend invocation, partial quality, same note/checkpoint retry and no-audio replay contracts regress. |
| `successful_live_quality_survives_completion_replay_and_publication_failure`; `note_less_quality_uses_existing_batch_payload_without_fallback` | Existing quality settlement, canonical completion and note-less recovery authority regress. |
| `actual_maximum_envelope_allocation_probe` (separate) | Wire/retained/encode/read ceilings are exceeded with the encoded buffer, parsed record and original caller payload alive together. Counts allocator payload, not RSS. |

No existing behavioral expectation is weakened. Shared source5/6 imports retain their reviewed tests and API changes; the source6 mutable-quality backend-failure fixture adopts affirmative raw CaptureProcessing authority. Added fields in live test fixtures start empty. The early-echo integration originally used missing ordinals and failed its actual lineage test; the repaired caller passes the actual global source ordinal. Reviewed opt-out failures and their original immutable archives remain separate review receipts.

The documentation in [local transcript evidence](../../../docs/local-transcript-evidence.md) records persistence, ownership, identity, bounds and native-gate limitations. Native macOS CI and the full application are separate required gates, not results of this model-free suite.

## Allocation fixture observations

The synthetic allocation probe holds caller input, the original ledger, an encoded file buffer,
and the newly parsed ledger simultaneously. A 16,476,055-byte file measured 16,476,567 additional
bytes during encode and 32,953,617 during readback, with 82,380,864 total accounted payload bytes
at that read peak. The 8,192-row fixture measured 5,506,471 additional read bytes. These recorded
debug-build observations are fixtures, not universal bounds or latency guarantees. They count
allocator payload, not RSS, allocator overhead or native model memory.
