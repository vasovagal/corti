//! Actual cleanup adapter. Existing decisions are consumed; no second legacy ledger is retained.
//!
//! New capture/candidate ownership is at most 16 MiB per admission, in addition to the retained
//! ledger's 32 MiB default. The pre-existing decision payload is at most 16 MiB and is consumed:
//! replacements/position traces move into their one new owner; duplicate legacy preimages are
//! dropped. With at most 8192 rows/decisions per admitted stage, temporary ID/index/change vectors
//! are below 4 MiB. One active text trace and its origin walk each have a 256 KiB limit. The
//! conservative total allowance for new cleanup evidence allocations is 128 MiB; this includes
//! admission/validation/growth scratch, but excludes the pre-existing cleanup algorithm's caller
//! input/output, caller-owned rule data, allocator bookkeeping, and RSS. Exceeding an evidence
//! bound seals evidence while the original cleanup pipeline still returns its ordinary output.

use super::*;
use crate::segment::{
    self, AudioEvidence, CleanupConfig, CleanupDecisions, CleanupStats, TextRule,
};

/// Bounds on each new capture/candidate and its row-ID/position scratch. These do not cap or discard
/// the caller's transcript: when evidence cannot fit, cleanup still runs and the marker records it.
pub const MAX_CAPTURE_ROWS: usize = 8192;
pub const MAX_CAPTURE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default)]
pub struct InputOrigin {
    pub track: Option<QualityTrack>,
    pub source_region_index: Option<u64>,
}

pub struct RecordingIdentity {
    pub window: u64,
    pub kind: StageKind,
    pub identity: Identity,
    pub signal: SignalIdentity,
}

pub struct CleanupInput<'a> {
    pub origins: &'a [InputOrigin],
    /// Exact retained parent outputs for CleanupAssembly; ASR inputs use None. Never a text join.
    pub source_links: Option<&'a [ParentRow]>,
    pub carry: &'a [TranscriptSegment],
    /// If available, the exact ordered row references from the earlier window. No text/time join.
    pub carry_links: Option<&'a [ParentRow]>,
    pub protected_input_indices: &'a [usize],
}

#[derive(Debug, Clone, Copy)]
pub struct RecordedWindow {
    pub source: InvocationId,
    pub final_stage: Option<InvocationId>,
    pub retained: Append,
}

pub struct RecordedCleanup {
    pub segments: Vec<TranscriptSegment>,
    pub stats: CleanupStats,
    pub evidence: Result<RecordedWindow, Error>,
}

pub(super) fn empty_group(info: &RecordingIdentity) -> Group {
    Group {
        window: info.window,
        identity: info.identity.clone(),
        signal: info.signal,
        rows: Vec::new(),
        stages: Vec::new(),
    }
}
pub(super) fn stage(id: InvocationId, kind: StageKind, input: Option<InvocationId>) -> Stage {
    Stage {
        id,
        kind,
        input,
        assembled_input: vec![],
        context: vec![],
        derivations: vec![],
        protected_output: vec![],
        output: vec![],
        decisions: vec![],
    }
}
fn snapshot_fits(rows: &[TranscriptSegment]) -> bool {
    rows.len() <= MAX_CAPTURE_ROWS
        && rows
            .iter()
            .try_fold(0usize, |bytes, row| {
                bytes
                    .checked_add(std::mem::size_of::<Row>() + std::mem::size_of::<RowId>())?
                    .checked_add(row.text.len())?
                    .checked_add(match &row.speaker {
                        corti_core::Speaker::Me => 0,
                        corti_core::Speaker::Other(label) => label.len(),
                    })
            })
            .is_some_and(|bytes| bytes <= MAX_CAPTURE_BYTES / 2)
}

impl Ledger {
    fn assembly(
        &mut self,
        info: &RecordingIdentity,
        rows: &[TranscriptSegment],
        inputs: &CleanupInput<'_>,
    ) -> Result<(InvocationId, Append), Error> {
        let links = inputs.source_links.ok_or(Error::InvalidLink)?;
        if links.len() != rows.len()
            || (!inputs.origins.is_empty() && inputs.origins.len() != rows.len())
        {
            return Err(Error::InvalidLink);
        }
        if [
            &info.identity.configuration_hash,
            &info.identity.model_hash,
            &info.identity.runtime_hash,
        ]
        .into_iter()
        .flatten()
        .any(|value| value.len() > 64)
        {
            return Err(Error::InvalidIdentity);
        }
        let id = self.next_invocation()?;
        if self.truncation().cause.is_some() || rows.len() > MAX_CAPTURE_ROWS {
            self.record_gap(1, 0, 0, TruncationCause::TransientCap)?;
            return Ok((id, Append::Omitted));
        }
        let mut group = empty_group(info);
        let mut assembly = stage(id, StageKind::CleanupAssembly, None);
        for (index, (link, supplied)) in links.iter().zip(rows).enumerate() {
            let parent = self.stage(link.parent)?;
            let existing = self.row(link.row)?;
            if !parent.output.contains(&link.row)
                || existing.segment != *supplied
                || existing.segment.start.to_bits() != supplied.start.to_bits()
                || existing.segment.end.to_bits() != supplied.end.to_bits()
            {
                return Err(Error::InvalidLink);
            }
            if let Some(origin) = inputs.origins.get(index)
                && (origin.track != existing.track
                    || origin.source_region_index != existing.source_region_index)
            {
                return Err(Error::InvalidLink);
            }
            assembly.assembled_input.push(*link);
            assembly.output.push(link.row);
            let protected = inputs.protected_input_indices.contains(&index);
            // Evidence cannot change the ordinary caller's protection flags or claim they ran.
            if parent.protected_output.contains(&link.row) && !protected {
                return Err(Error::InvalidLink);
            }
            if protected {
                assembly.protected_output.push(link.row);
            }
        }
        group.stages.push(assembly);
        Ok((id, self.append(group)?))
    }
    /// Borrow one retained stage and its explicit output/protection IDs; omitted stages are unavailable.
    pub fn stage(&self, id: InvocationId) -> Result<&Stage, Error> {
        self.groups()
            .iter()
            .flat_map(|g| &g.stages)
            .find(|s| s.id == id)
            .ok_or(Error::Unavailable)
    }
    /// Borrow the unique retained owner. This never reconstructs identity from text or time.
    pub fn row(&self, id: RowId) -> Result<&Row, Error> {
        self.groups()
            .iter()
            .find_map(|g| g.row(id))
            .ok_or(Error::Unavailable)
    }
    pub(super) fn record_gap(
        &mut self,
        count: usize,
        rows: usize,
        decisions: usize,
        cause: TruncationCause,
    ) -> Result<(), Error> {
        if count == 0 {
            return Ok(());
        }
        let first = self.next_invocation()?;
        let last = InvocationId(
            first
                .0
                .checked_add(count as u64 - 1)
                .ok_or(Error::InvalidLink)?,
        );
        let t = &mut self.document.truncation;
        t.cause.get_or_insert(cause);
        t.first_omitted_invocation.get_or_insert(first);
        t.last_omitted_invocation = Some(last);
        for (counter, added) in [
            (&mut t.omitted_groups, count as u64),
            (&mut t.omitted_stages, count as u64),
            (&mut t.omitted_rows, rows as u64),
            (&mut t.omitted_decisions, decisions as u64),
        ] {
            if let Some(next) = counter.checked_add(added) {
                *counter = next;
            } else {
                *counter = u64::MAX;
                t.counter_saturated = true;
            }
        }
        self.document.last_invocation = last;
        Ok(())
    }

    // A returned error is transient caller state. Persist the missing suffix as well so a
    // write/read cycle cannot make later windows appear to follow complete evidence.
    pub(super) fn seal_recording_error<T>(
        &mut self,
        result: Result<T, Error>,
        stages: usize,
        rows: usize,
        decisions: usize,
    ) -> Result<T, Error> {
        if result.is_err() {
            self.record_gap(
                stages.max(1),
                rows,
                decisions,
                TruncationCause::TraceUnavailable,
            )?;
        }
        result
    }
    pub(super) fn snapshot(
        &mut self,
        info: &RecordingIdentity,
        rows: &[TranscriptSegment],
        origins: &[InputOrigin],
        kind: StageKind,
        protected: &[usize],
    ) -> Result<(InvocationId, Append), Error> {
        if !origins.is_empty() && origins.len() != rows.len() {
            return Err(Error::InvalidLink);
        }
        // Hash metadata is bounded before any clone, including invalid external identities.
        if [
            &info.identity.configuration_hash,
            &info.identity.model_hash,
            &info.identity.runtime_hash,
        ]
        .into_iter()
        .flatten()
        .any(|s| s.len() > 64)
        {
            return Err(Error::InvalidIdentity);
        }
        let id = self.next_invocation()?;
        if self.truncation().cause.is_some() || !snapshot_fits(rows) {
            self.record_gap(1, rows.len(), 0, TruncationCause::TransientCap)?;
            return Ok((id, Append::Omitted));
        }
        let mut group = empty_group(info);
        let mut source = stage(id, kind, None);
        group.rows.reserve_exact(rows.len());
        source.output.reserve_exact(rows.len());
        for (slot, segment) in rows.iter().enumerate() {
            let origin = origins.get(slot).copied().unwrap_or_default();
            let row_id = RowId {
                invocation: id,
                slot: slot as u32,
            };
            group.rows.push(Row {
                id: row_id,
                track: origin.track,
                source_region_index: origin.source_region_index,
                segment: segment.clone(),
            });
            source.output.push(row_id);
            if protected.contains(&slot) {
                source.protected_output.push(row_id);
            }
        }
        group.stages.push(source);
        if group.heap_bytes() > MAX_CAPTURE_BYTES {
            self.record_gap(1, rows.len(), 0, TruncationCause::TransientCap)?;
            return Ok((id, Append::Omitted));
        }
        Ok((id, self.append(group)?))
    }

    /// Run exactly the existing protected cleanup once, record original input before any removal,
    /// then consume its bounded decisions. Evidence failure never substitutes or omits transcript.
    #[allow(clippy::too_many_arguments)]
    pub fn cleanup(
        &mut self,
        info: RecordingIdentity,
        segments: Vec<TranscriptSegment>,
        cfg: &CleanupConfig,
        inputs: CleanupInput<'_>,
        audio: Option<AudioEvidence<'_>>,
        rule: Option<&dyn TextRule>,
    ) -> RecordedCleanup {
        let source_rows = if info.kind == StageKind::CleanupAssembly {
            0
        } else {
            segments.len()
        };
        let source = if info.kind == StageKind::CleanupAssembly {
            self.assembly(&info, &segments, &inputs)
        } else if matches!(info.kind, StageKind::TrueAsr | StageKind::FallbackAsr)
            && inputs.source_links.is_none()
        {
            self.snapshot(
                &info,
                &segments,
                inputs.origins,
                info.kind,
                inputs.protected_input_indices,
            )
        } else {
            Err(Error::InvalidLink)
        };
        let result = segment::cleanup_with_traced_decisions(
            segments,
            cfg,
            inputs.carry,
            audio,
            rule,
            inputs.protected_input_indices,
        );
        let passes = [
            Some(CleanupStage::Sort),
            cfg.echo_drop.then_some(CleanupStage::Echo),
            (cfg.merge_gap_seconds > 0.0).then_some(CleanupStage::Merge),
            cfg.drop_backchannels.then_some(CleanupStage::Backchannel),
            cfg.strip_fillers.then_some(CleanupStage::Fillers),
            rule.map(|_| CleanupStage::Lexicon),
        ];
        let (entries, omitted) = result.decisions.into_parts();
        let mut pending_stages = passes.iter().flatten().count() + usize::from(source.is_err());
        let mut pending_rows = if source.is_err() { source_rows } else { 0 };
        let mut pending_decisions = entries.len().saturating_add(omitted);
        let evidence = source.and_then(|(id, retained)| {
            if retained == Append::Omitted || omitted != 0 {
                self.record_gap(
                    passes.iter().flatten().count(),
                    0,
                    entries.len().saturating_add(omitted),
                    TruncationCause::UpstreamDecisions,
                )?;
                return Ok(RecordedWindow {
                    source: id,
                    final_stage: None,
                    retained: Append::Omitted,
                });
            }
            let mut context = Vec::new();
            if !inputs.carry.is_empty() {
                if inputs.carry.len() > MAX_CAPTURE_ROWS {
                    self.record_gap(
                        passes.iter().flatten().count(),
                        inputs.carry.len(),
                        entries.len(),
                        TruncationCause::TransientCap,
                    )?;
                    return Ok(RecordedWindow {
                        source: id,
                        final_stage: None,
                        retained: Append::Omitted,
                    });
                }
                if let Some(links) = inputs.carry_links {
                    if links.len() != inputs.carry.len() {
                        return Err(Error::InvalidLink);
                    }
                    for (link, row) in links.iter().zip(inputs.carry) {
                        if !self.stage(link.parent)?.output.contains(&link.row)
                            || self.row(link.row)?.segment != *row
                        {
                            return Err(Error::InvalidLink);
                        }
                    }
                    context.extend_from_slice(links);
                } else {
                    let context_info = RecordingIdentity {
                        window: info.window,
                        kind: StageKind::ContextSnapshot,
                        identity: Identity {
                            configuration_hash: None,
                            model_hash: None,
                            runtime_hash: None,
                        },
                        signal: SignalIdentity {
                            source: SignalSource::UnattributedContext,
                            aec: AecOrigin::None,
                            source_rate_hz: None,
                            lookahead_frames: None,
                        },
                    };
                    pending_stages += 1;
                    pending_rows = inputs.carry.len();
                    let (parent, retained) = self.snapshot(
                        &context_info,
                        inputs.carry,
                        &[],
                        StageKind::ContextSnapshot,
                        &[],
                    )?;
                    pending_stages -= 1;
                    pending_rows = 0;
                    if retained == Append::Retained {
                        context.extend(
                            self.stage(parent)?
                                .output
                                .iter()
                                .map(|row| ParentRow { parent, row: *row }),
                        );
                    }
                }
            }
            let mut current = id;
            let mut entries = entries.into_iter().peekable();
            for pass in passes.into_iter().flatten() {
                let mut selected = Vec::new();
                let mut excess = 0usize;
                while entries.peek().is_some_and(|entry| entry.stage == pass) {
                    let entry = entries.next().ok_or(Error::InvalidLink)?;
                    if selected.len() < MAX_CAPTURE_ROWS {
                        selected.push(entry);
                    } else {
                        excess = excess.saturating_add(1);
                    }
                }
                let next = self.next_invocation()?;
                let selected_count = selected.len().saturating_add(excess);
                if excess != 0 {
                    self.record_gap(
                        1,
                        0,
                        selected.len().saturating_add(excess),
                        TruncationCause::TransientCap,
                    )?;
                } else {
                    self.record_stage(
                        &info,
                        current,
                        pass,
                        selected,
                        if pass == CleanupStage::Echo {
                            &context
                        } else {
                            &[]
                        },
                    )?;
                }
                pending_stages -= 1;
                pending_decisions = pending_decisions.saturating_sub(selected_count);
                // Continue consuming the same real pass list to keep omissions and IDs explicit.
                current = next;
            }
            if entries.next().is_some() {
                return Err(Error::InvalidLink);
            }
            let retained = if self.truncation().cause.is_some() {
                Append::Omitted
            } else {
                Append::Retained
            };
            Ok(RecordedWindow {
                source: id,
                final_stage: (retained == Append::Retained).then_some(current),
                retained,
            })
        });
        let evidence =
            self.seal_recording_error(evidence, pending_stages, pending_rows, pending_decisions);
        RecordedCleanup {
            segments: result.segments,
            stats: result.stats,
            evidence,
        }
    }

    pub(super) fn record_stage(
        &mut self,
        info: &RecordingIdentity,
        parent: InvocationId,
        pass: CleanupStage,
        entries: Vec<segment::CleanupDecision>,
        context: &[ParentRow],
    ) -> Result<Append, Error> {
        let id = self.next_invocation()?;
        if self.truncation().cause.is_some() || entries.iter().any(|d| d.edits.gap().is_some()) {
            self.record_gap(1, 0, entries.len(), TruncationCause::TraceUnavailable)?;
            return Ok(Append::Omitted);
        }
        let input = &self.stage(parent)?.output;
        let protected = &self.stage(parent)?.protected_output;
        if input.len() > MAX_CAPTURE_ROWS || entries.len() > MAX_CAPTURE_ROWS {
            self.record_gap(1, 0, entries.len(), TruncationCause::TransientCap)?;
            return Ok(Append::Omitted);
        }
        let lookup = RowLookup::new(self.groups());
        let mut group = empty_group(info);
        let mut invocation = stage(id, StageKind::Cleanup { pass }, Some(parent));
        invocation.context.extend_from_slice(context);
        let mut changes: Vec<(usize, usize, Vec<RowId>)> = Vec::new();
        let mut candidate_bytes = 0usize;
        for entry in entries {
            let indices: Vec<usize> = if pass == CleanupStage::EarlyEcho {
                entry
                    .input_indices
                    .iter()
                    .map(|index| {
                        let mut matches = input.iter().enumerate().filter(|(_, row)| {
                            lookup
                                .get(**row)
                                .is_some_and(|r| r.source_region_index == Some(*index as u64))
                        });
                        let position = matches
                            .next()
                            .map(|(position, _)| position)
                            .ok_or(Error::InvalidLink)?;
                        if matches.next().is_some() {
                            return Err(Error::InvalidLink);
                        }
                        Ok(position)
                    })
                    .collect::<Result<_, _>>()?
            } else {
                entry.input_indices.clone()
            };
            let originals = indices
                .iter()
                .map(|index| input.get(*index).copied().ok_or(Error::InvalidLink))
                .collect::<Result<Vec<_>, _>>()?;
            if originals.len() != entry.originals.len()
                || originals
                    .iter()
                    .zip(&entry.originals)
                    .any(|(id, row)| lookup.get(*id).is_none_or(|stored| stored.segment != *row))
            {
                return Err(Error::InvalidLink);
            }
            let mut replacements = Vec::new();
            if matches!(
                pass,
                CleanupStage::Sort | CleanupStage::Echo | CleanupStage::EarlyEcho
            ) && !entry.replacements.is_empty()
            {
                replacements.extend_from_slice(&originals);
            } else {
                for segment in entry.replacements {
                    candidate_bytes = candidate_bytes
                        .saturating_add(segment.text.capacity())
                        .saturating_add(match &segment.speaker {
                            corti_core::Speaker::Me => 0,
                            corti_core::Speaker::Other(label) => label.capacity(),
                        })
                        .saturating_add(std::mem::size_of::<Row>());
                    if candidate_bytes > MAX_CAPTURE_BYTES || group.rows.len() >= MAX_CAPTURE_ROWS {
                        self.record_gap(1, 0, 1, TruncationCause::TransientCap)?;
                        return Ok(Append::Omitted);
                    }
                    let first = lookup
                        .get(*originals.first().ok_or(Error::InvalidLink)?)
                        .ok_or(Error::Unavailable)?;
                    let row_id = RowId {
                        invocation: id,
                        slot: group.rows.len() as u32,
                    };
                    group.rows.push(Row {
                        id: row_id,
                        track: first.track,
                        source_region_index: if pass == CleanupStage::Merge {
                            None
                        } else {
                            first.source_region_index
                        },
                        segment,
                    });
                    replacements.push(row_id);
                }
            }
            changes.push((
                *indices.first().ok_or(Error::InvalidLink)?,
                indices.len(),
                replacements.clone(),
            ));
            invocation.decisions.push(Decision {
                reason: entry.reason,
                input_indices: entry.input_indices.into_iter().map(|i| i as u64).collect(),
                originals,
                replacements,
                edits: entry.edits.into_edits(),
                evidence: entry.evidence.map(|e| EchoEvidence {
                    mic_db_bits: e.mic_db.to_bits(),
                    echo_estimate_db_bits: e.echo_estimate_db.to_bits(),
                    double_talk_fraction_bits: e.double_talk_fraction.to_bits(),
                    blocks: e.blocks as u64,
                }),
                source: entry.source.map(|s| EchoSourceDescription {
                    me: s.me,
                    start: s.start,
                    end: s.end,
                }),
            });
            // entry.originals are dropped here; their immutable owners already live in the ledger.
        }
        if pass == CleanupStage::Sort {
            invocation.output.extend_from_slice(input);
            invocation.output.sort_by(|a, b| {
                lookup
                    .get(*a)
                    .map(|r| r.segment.start)
                    .unwrap_or(0.0)
                    .total_cmp(&lookup.get(*b).map(|r| r.segment.start).unwrap_or(0.0))
            });
        } else {
            let mut position = 0;
            while position < input.len() {
                if let Some((_, count, replacements)) =
                    changes.iter().find(|(index, _, _)| *index == position)
                {
                    invocation.output.extend_from_slice(replacements);
                    position += count;
                } else {
                    invocation.output.push(input[position]);
                    position += 1;
                }
            }
        }
        for row in &invocation.output {
            let inherited = protected.contains(row)
                || invocation.decisions.iter().any(|decision| {
                    decision.replacements.contains(row)
                        && (decision.originals.iter().any(|id| protected.contains(id))
                            || decision.reason
                                == CleanupReason::Echo(EchoVerdict::KeepCriticalDifference))
                });
            if inherited {
                invocation.protected_output.push(*row);
            }
        }
        group.stages.push(invocation);
        if group.heap_bytes() > MAX_CAPTURE_BYTES {
            self.record_gap(
                1,
                0,
                group.stages[0].decisions.len(),
                TruncationCause::TransientCap,
            )?;
            return Ok(Append::Omitted);
        }
        drop(lookup);
        self.append(group)
    }

    /// Record all early inputs, including withheld rows, before consuming the actual EarlyEcho
    /// decisions. Region ordinals are supplied by the caller and are never text/time lookup keys.
    pub fn early_echo(
        &mut self,
        info: RecordingIdentity,
        all_inputs: &[TranscriptSegment],
        origins: &[InputOrigin],
        decisions: CleanupDecisions,
    ) -> Result<RecordedWindow, Error> {
        self.early_echo_with_context(info, all_inputs, origins, decisions, &[])
    }

    /// The same consuming EarlyEcho adapter with exact retained comparison/carry row references.
    /// These links come from the actual caller, never the decision's descriptive source times.
    pub fn early_echo_with_context(
        &mut self,
        info: RecordingIdentity,
        all_inputs: &[TranscriptSegment],
        origins: &[InputOrigin],
        decisions: CleanupDecisions,
        context: &[ParentRow],
    ) -> Result<RecordedWindow, Error> {
        let (entries, omitted) = decisions.into_parts();
        let pending_decisions = entries.len().saturating_add(omitted);
        let mut pending_stages = 2;
        let mut pending_rows = all_inputs.len();
        let evidence = (|| {
            if !matches!(info.kind, StageKind::TrueAsr | StageKind::FallbackAsr) {
                return Err(Error::InvalidLink);
            }
            let (source, retained) = self.snapshot(&info, all_inputs, origins, info.kind, &[])?;
            pending_stages = 1;
            pending_rows = 0;
            if omitted != 0 || retained == Append::Omitted || context.len() > MAX_CAPTURE_ROWS {
                self.record_gap(
                    1,
                    0,
                    pending_decisions,
                    if context.len() > MAX_CAPTURE_ROWS {
                        TruncationCause::TransientCap
                    } else {
                        TruncationCause::UpstreamDecisions
                    },
                )?;
                return Ok(RecordedWindow {
                    source,
                    final_stage: None,
                    retained: Append::Omitted,
                });
            }
            if entries.iter().any(|d| d.stage != CleanupStage::EarlyEcho) {
                return Err(Error::InvalidLink);
            }
            let final_stage = self.next_invocation()?;
            let retained =
                self.record_stage(&info, source, CleanupStage::EarlyEcho, entries, context)?;
            Ok(RecordedWindow {
                source,
                final_stage: (retained == Append::Retained).then_some(final_stage),
                retained,
            })
        })();
        self.seal_recording_error(evidence, pending_stages, pending_rows, pending_decisions)
    }
}
