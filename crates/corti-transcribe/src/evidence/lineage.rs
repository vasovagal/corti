//! Actual caller-produced raw Echo and word-derived display stages. No classifier is rerun.
use super::record::{empty_group, stage};
use super::*;
use crate::segment::CleanupDecisions;

/// An actual output and its caller-supplied UTF-8 source range. The caller emits the range while
/// iterating words; this adapter never recovers ownership by matching text or timestamps.
/// ```compile_fail
/// fn requires_debug<T: std::fmt::Debug>() {}
/// requires_debug::<corti_transcribe::evidence::DerivedSegment>();
/// ```
/// ```compile_fail
/// fn requires_clone<T: Clone>() {}
/// requires_clone::<corti_transcribe::evidence::DerivedSegment>();
/// ```
pub struct DerivedSegment {
    pub original: RowId,
    pub source: ByteSpan,
    pub segment: TranscriptSegment,
}

fn valid_identity(info: &RecordingIdentity) -> bool {
    [
        &info.identity.configuration_hash,
        &info.identity.model_hash,
        &info.identity.runtime_hash,
    ]
    .into_iter()
    .flatten()
    .all(|value| value.len() <= 64)
}

impl Ledger {
    /// Seal one caller-side recording operation that could not produce a bounded linked group.
    /// Use after a failed low-level append/preparation; rows/decisions count omitted evidence only.
    pub fn mark_unavailable(&mut self, rows: usize, decisions: usize) -> Result<(), Error> {
        self.record_gap(1, rows, decisions, TruncationCause::TraceUnavailable)
    }

    /// Capture one actual ASR invocation before raw Echo/word-derived display transformations.
    pub fn capture(
        &mut self,
        info: RecordingIdentity,
        rows: &[TranscriptSegment],
        origins: &[InputOrigin],
        protected: &[usize],
    ) -> Result<RecordedWindow, Error> {
        let result = (|| {
            if !matches!(info.kind, StageKind::TrueAsr | StageKind::FallbackAsr)
                || protected.iter().any(|index| *index >= rows.len())
            {
                return Err(Error::InvalidLink);
            }
            let (source, retained) = self.snapshot(&info, rows, origins, info.kind, protected)?;
            Ok(RecordedWindow {
                source,
                final_stage: (retained == Append::Retained).then_some(source),
                retained,
            })
        })();
        self.seal_recording_error(result, 1, rows.len(), 0)
    }

    /// Select/order exact retained outputs, adding the protection the actual caller will use.
    /// This stores only row references. It cannot resurrect withheld rows or clear protection.
    pub fn assemble(
        &mut self,
        info: RecordingIdentity,
        parents: &[ParentRow],
        protected: &[usize],
    ) -> Result<RecordedWindow, Error> {
        let result = (|| {
            if info.kind != StageKind::CleanupAssembly
                || protected.iter().any(|index| *index >= parents.len())
            {
                return Err(Error::InvalidLink);
            }
            if !valid_identity(&info) {
                return Err(Error::InvalidIdentity);
            }
            let id = self.next_invocation()?;
            if self.truncation().cause.is_some() || parents.len() > MAX_CAPTURE_ROWS {
                self.record_gap(1, 0, 0, TruncationCause::TransientCap)?;
                return Ok(RecordedWindow {
                    source: id,
                    final_stage: None,
                    retained: Append::Omitted,
                });
            }
            let mut group = empty_group(&info);
            let mut invocation = stage(id, StageKind::CleanupAssembly, None);
            for (position, source) in parents.iter().enumerate() {
                let parent = self.stage(source.parent)?;
                if !parent.output.contains(&source.row) {
                    return Err(Error::InvalidLink);
                }
                invocation.assembled_input.push(*source);
                invocation.output.push(source.row);
                if parent.protected_output.contains(&source.row) || protected.contains(&position) {
                    invocation.protected_output.push(source.row);
                }
            }
            group.stages.push(invocation);
            let retained = self.append(group)?;
            Ok(RecordedWindow {
                source: id,
                final_stage: (retained == Append::Retained).then_some(id),
                retained,
            })
        })();
        self.seal_recording_error(result, 1, 0, 0)
    }

    /// Consume decisions already made by the actual raw Echo path against this exact parent order.
    /// The parent can be an explicit assembly; context names the actual retained comparison rows.
    pub fn record_echo(
        &mut self,
        info: RecordingIdentity,
        parent: InvocationId,
        decisions: CleanupDecisions,
        context: &[ParentRow],
    ) -> Result<RecordedWindow, Error> {
        let (entries, omitted) = decisions.into_parts();
        let count = entries.len().saturating_add(omitted);
        let result = (|| {
            if info.kind
                != (StageKind::Cleanup {
                    pass: CleanupStage::Echo,
                })
                || entries
                    .iter()
                    .any(|entry| entry.stage != CleanupStage::Echo)
            {
                return Err(Error::InvalidLink);
            }
            if !valid_identity(&info) {
                return Err(Error::InvalidIdentity);
            }
            let id = self.next_invocation()?;
            if omitted != 0 || context.len() > MAX_CAPTURE_ROWS {
                self.record_gap(
                    1,
                    0,
                    count,
                    if omitted != 0 {
                        TruncationCause::UpstreamDecisions
                    } else {
                        TruncationCause::TransientCap
                    },
                )?;
                return Ok(RecordedWindow {
                    source: parent,
                    final_stage: None,
                    retained: Append::Omitted,
                });
            }
            let retained =
                self.record_stage(&info, parent, CleanupStage::Echo, entries, context)?;
            Ok(RecordedWindow {
                source: parent,
                final_stage: (retained == Append::Retained).then_some(id),
                retained,
            })
        })();
        self.seal_recording_error(result, 1, 0, count)
    }

    /// Snapshot actual pause/diarization descendants into one bounded stage. Every non-whitespace source
    /// byte must appear exactly once and in order; timings/speaker changes follow the typed stage.
    /// Unchanged whole rows keep their original owner. Failure seals evidence, not caller output.
    pub fn derive_segments(
        &mut self,
        info: RecordingIdentity,
        parent: InvocationId,
        descendants: &[DerivedSegment],
    ) -> Result<RecordedWindow, Error> {
        let count = descendants.len();
        let result = (|| {
            if !matches!(info.kind, StageKind::Segmentation | StageKind::Diarization) {
                return Err(Error::InvalidLink);
            }
            if !valid_identity(&info) {
                return Err(Error::InvalidIdentity);
            }
            let id = self.next_invocation()?;
            let owned = descendants.iter().fold(0usize, |bytes, descendant| {
                bytes
                    .saturating_add(
                        std::mem::size_of::<Row>()
                            + std::mem::size_of::<Derivation>()
                            + 2 * std::mem::size_of::<RowId>(),
                    )
                    .saturating_add(descendant.segment.text.capacity())
                    .saturating_add(match &descendant.segment.speaker {
                        corti_core::Speaker::Me => 0,
                        corti_core::Speaker::Other(label) => label.capacity(),
                    })
            });
            if self.truncation().cause.is_some()
                || count > MAX_CAPTURE_ROWS
                || owned > MAX_CAPTURE_BYTES / 2
            {
                self.record_gap(1, count, count, TruncationCause::TransientCap)?;
                return Ok(RecordedWindow {
                    source: parent,
                    final_stage: None,
                    retained: Append::Omitted,
                });
            }
            let input = self.stage(parent)?;
            let mut group = empty_group(&info);
            let mut invocation = stage(id, info.kind, Some(parent));
            for descendant in descendants {
                if !input.output.contains(&descendant.original) {
                    return Err(Error::InvalidLink);
                }
                let original = self.row(descendant.original)?;
                let unchanged = descendant.source.start == 0
                    && descendant.source.end as usize == original.segment.text.len()
                    && descendant.segment == original.segment
                    && descendant.segment.start.to_bits() == original.segment.start.to_bits()
                    && descendant.segment.end.to_bits() == original.segment.end.to_bits();
                let replacement = if unchanged {
                    descendant.original
                } else {
                    let replacement = RowId {
                        invocation: id,
                        slot: group.rows.len() as u32,
                    };
                    group.rows.push(Row {
                        id: replacement,
                        track: original.track,
                        source_region_index: original.source_region_index,
                        segment: descendant.segment.clone(),
                    });
                    replacement
                };
                invocation.derivations.push(Derivation {
                    original: descendant.original,
                    source: descendant.source,
                    replacement,
                });
                invocation.output.push(replacement);
                if input.protected_output.contains(&descendant.original) {
                    invocation.protected_output.push(replacement);
                }
            }
            group.stages.push(invocation);
            let retained = self.append(group)?;
            Ok(RecordedWindow {
                source: parent,
                final_stage: (retained == Append::Retained).then_some(id),
                retained,
            })
        })();
        self.seal_recording_error(result, 1, 0, count)
    }
}

pub(super) fn validate<'a>(
    stage: &Stage,
    input: &[RowId],
    protected: &[RowId],
    lookup: &impl Fn(&RowId) -> Option<&'a Row>,
) -> Result<(), Error> {
    if stage.output.len() != stage.derivations.len() {
        return Err(Error::InvalidLink);
    }
    let mut written = 0;
    let mut inherited = Vec::new();
    for original in input {
        let before = lookup(original).ok_or(Error::InvalidLink)?;
        let mut cursor = 0;
        let mut last_start = before.segment.start;
        while let Some(derived) = stage
            .derivations
            .get(written)
            .filter(|derived| derived.original == *original)
        {
            if stage.output.get(written) != Some(&derived.replacement) {
                return Err(Error::InvalidLink);
            }
            let start = derived.source.start as usize;
            let end = derived.source.end as usize;
            let gap = before
                .segment
                .text
                .get(cursor..start)
                .ok_or(Error::InvalidEdit)?;
            let text = before
                .segment
                .text
                .get(start..end)
                .ok_or(Error::InvalidEdit)?;
            if !gap.chars().all(char::is_whitespace)
                || (start == end && !before.segment.text.is_empty())
            {
                return Err(Error::InvalidEdit);
            }
            let after = lookup(&derived.replacement).ok_or(Error::InvalidLink)?;
            if text != after.segment.text
                || before.track != after.track
                || before.source_region_index != after.source_region_index
            {
                return Err(Error::InvalidEdit);
            }
            if derived.replacement == *original {
                if start != 0 || end != before.segment.text.len() {
                    return Err(Error::InvalidLink);
                }
            } else if derived.replacement.invocation != stage.id {
                return Err(Error::InvalidLink);
            }
            let speaker_valid = match stage.kind {
                StageKind::Segmentation => before.segment.speaker == after.segment.speaker,
                StageKind::Diarization => matches!(
                    (&before.segment.speaker, &after.segment.speaker),
                    (corti_core::Speaker::Other(_), corti_core::Speaker::Other(_))
                ),
                _ => false,
            };
            if !speaker_valid
                || after.segment.start < last_start
                || after.segment.end > before.segment.end
            {
                return Err(Error::InvalidLink);
            }
            if protected.contains(original) {
                inherited.push(derived.replacement);
            }
            cursor = end;
            last_start = after.segment.start;
            written += 1;
        }
        if !before
            .segment
            .text
            .get(cursor..)
            .ok_or(Error::InvalidEdit)?
            .chars()
            .all(char::is_whitespace)
        {
            return Err(Error::InvalidEdit);
        }
    }
    if written != stage.derivations.len() || stage.protected_output != inherited {
        return Err(Error::InvalidLink);
    }
    Ok(())
}
