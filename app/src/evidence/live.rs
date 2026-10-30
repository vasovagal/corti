//! Live adapters consume decisions and emitter ranges at their point of production. They never run
//! an echo classifier, reconstruct a parent by text/time, or lend evidence to the hosted finalizer.

use super::integration::Run;
use super::{Error, Status};
use corti_core::{QualityTrack, RecordingMeta, TranscriptSegment};
use corti_transcribe::evidence::{
    AecOrigin, CleanupInput, DerivedSegment, InputOrigin, MAX_CAPTURE_ROWS, ParentRow,
    RecordedWindow, RecordingIdentity, SignalIdentity, SignalSource, StageKind,
};
use corti_transcribe::segment::{
    CleanupConfig, CleanupDecisions, CleanupStage, CleanupStats, SegmentedRows, TextRule,
};
use corti_vagus::provenance::TranscriptProvenance;
use std::path::Path;

/// The retained ledger and the current window's small parent vectors have separate ownership.
/// Once capped, parent vectors remain empty while the ordinary transcript continues unchanged.
pub(crate) struct LiveRun {
    run: Run,
    window: u64,
    rate: u32,
    lookahead: Option<u64>,
    aec_ran: bool,
    completed: Vec<ParentRow>,
}

impl LiveRun {
    pub(crate) fn begin(
        id: &str,
        meta: &RecordingMeta,
        provenance: &TranscriptProvenance,
        rate: u32,
        lookahead: Option<u64>,
    ) -> Result<Self, Error> {
        Ok(Self {
            run: Run::begin(id, meta, provenance)?,
            window: 0,
            rate,
            lookahead,
            aec_ran: false,
            completed: Vec::new(),
        })
    }

    pub(crate) fn observe_aec(&mut self) {
        self.aec_ran = true;
    }

    fn info(&self, kind: StageKind, track: Option<QualityTrack>) -> RecordingIdentity {
        let applied = self.aec_ran && track != Some(QualityTrack::Tap);
        RecordingIdentity {
            window: self.window,
            kind,
            identity: self.run.session.identity(),
            signal: SignalIdentity {
                source: SignalSource::LiveStream,
                aec: if applied {
                    AecOrigin::LiveShortLookahead
                } else {
                    AecOrigin::None
                },
                source_rate_hz: Some(self.rate),
                lookahead_frames: if applied { self.lookahead } else { None },
            },
        }
    }

    fn output(
        &mut self,
        result: Result<RecordedWindow, corti_transcribe::evidence::Error>,
    ) -> Vec<ParentRow> {
        match result {
            Ok(recorded) => match recorded.final_stage {
                Some(parent) => match self.run.session.ledger().stage(parent) {
                    Ok(stage) if stage.output.len() <= MAX_CAPTURE_ROWS => stage
                        .output
                        .iter()
                        .map(|&row| ParentRow { parent, row })
                        .collect(),
                    _ => {
                        self.missing(0);
                        Vec::new()
                    }
                },
                None => Vec::new(),
            },
            Err(_) => {
                self.run.unavailable = true;
                super::unavailable();
                Vec::new()
            }
        }
    }

    pub(crate) fn missing(&mut self, rows: usize) {
        if self.run.session.ledger().truncation().cause.is_some() {
            let _ = self.run.session.ledger_mut().mark_unavailable(rows, 0);
            self.run.final_stage = None;
        } else {
            self.run.failed(rows);
        }
    }

    #[cfg(test)]
    pub(crate) fn set_test_wire_cap(&mut self, cap: usize) {
        self.run.session.ledger = corti_transcribe::evidence::Ledger::with_limits(
            cap,
            corti_transcribe::evidence::DEFAULT_OWNED_CAP,
        )
        .unwrap();
    }

    pub(crate) fn capture(
        &mut self,
        rows: &[TranscriptSegment],
        track: QualityTrack,
    ) -> Vec<ParentRow> {
        if rows.len() > MAX_CAPTURE_ROWS {
            self.missing(rows.len());
            return Vec::new();
        }
        let origins: Vec<_> = rows
            .iter()
            .map(|_| InputOrigin {
                track: Some(track),
                source_region_index: None,
            })
            .collect();
        let info = self.info(StageKind::TrueAsr, Some(track));
        let result = self
            .run
            .session
            .ledger_mut()
            .capture(info, rows, &origins, &[]);
        self.output(result)
    }

    /// Raw comparison context that has no upstream output ID is explicitly unattributed. It is never
    /// counted as a fresh decoder invocation, nor matched to a released child by text or timestamp.
    pub(crate) fn context(&mut self, row: &TranscriptSegment) -> Option<ParentRow> {
        use corti_transcribe::evidence::{Append, Group, Row, RowId, Stage};
        if row.text.len().saturating_add(row.speaker.display().len())
            > corti_transcribe::evidence::MAX_CAPTURE_BYTES / 2
        {
            self.missing(1);
            return None;
        }
        let result = (|| {
            let id = self.run.session.ledger().next_invocation()?;
            let row_id = RowId {
                invocation: id,
                slot: 0,
            };
            let group = Group {
                window: self.window,
                identity: self.run.session.identity(),
                signal: self
                    .info(StageKind::ContextSnapshot, Some(QualityTrack::Mic))
                    .signal,
                rows: vec![Row {
                    id: row_id,
                    track: Some(QualityTrack::Mic),
                    source_region_index: None,
                    segment: row.clone(),
                }],
                stages: vec![Stage {
                    id,
                    kind: StageKind::ContextSnapshot,
                    input: None,
                    assembled_input: Vec::new(),
                    context: Vec::new(),
                    protected_output: Vec::new(),
                    output: vec![row_id],
                    decisions: Vec::new(),
                    derivations: Vec::new(),
                }],
            };
            self.run.session.ledger_mut().append(group).map(|result| {
                (
                    result,
                    ParentRow {
                        parent: id,
                        row: row_id,
                    },
                )
            })
        })();
        match result {
            Ok((Append::Retained, link)) => Some(link),
            Ok((Append::Omitted, _)) => None,
            Err(_) => {
                self.missing(1);
                None
            }
        }
    }

    pub(crate) fn early(
        &mut self,
        rows: &[TranscriptSegment],
        first_ordinal: usize,
        decisions: CleanupDecisions,
        context: &[ParentRow],
    ) -> Vec<ParentRow> {
        if rows.len() > MAX_CAPTURE_ROWS {
            self.missing(rows.len());
            return Vec::new();
        }
        let origins: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(index, _)| InputOrigin {
                track: Some(QualityTrack::Mic),
                source_region_index: Some(first_ordinal.saturating_add(index) as u64),
            })
            .collect();
        let info = self.info(StageKind::TrueAsr, Some(QualityTrack::Mic));
        let result = self
            .run
            .session
            .ledger_mut()
            .early_echo_with_context(info, rows, &origins, decisions, context);
        self.output(result)
    }

    pub(crate) fn echo(
        &mut self,
        input: &[ParentRow],
        protected: &[usize],
        decisions: CleanupDecisions,
        context: &[ParentRow],
        track: QualityTrack,
    ) -> Vec<ParentRow> {
        let info = self.info(StageKind::CleanupAssembly, Some(track));
        let result = self
            .run
            .session
            .ledger_mut()
            .assemble(info, input, protected);
        let parent = match result {
            Ok(recorded) => recorded.final_stage,
            Err(_) => {
                self.run.unavailable = true;
                super::unavailable();
                None
            }
        };
        let Some(parent) = parent else {
            return Vec::new();
        };
        let info = self.info(
            StageKind::Cleanup {
                pass: CleanupStage::Echo,
            },
            Some(track),
        );
        let result = self
            .run
            .session
            .ledger_mut()
            .record_echo(info, parent, decisions, context);
        self.output(result)
    }

    /// The actual emitter hands over its outputs and ranges together; on any evidence failure those
    /// same output values are returned to the caller. No fallible evidence operation owns transcript.
    pub(crate) fn derive(
        &mut self,
        parent: Option<ParentRow>,
        emitted: SegmentedRows,
        kind: StageKind,
    ) -> (Vec<TranscriptSegment>, Vec<ParentRow>) {
        let Some(parent) = parent else {
            self.missing(emitted.segments.len());
            return (emitted.segments, Vec::new());
        };
        let Some(ranges) = emitted.source_ranges else {
            self.missing(emitted.segments.len());
            return (emitted.segments, Vec::new());
        };
        if emitted.segments.len() > MAX_CAPTURE_ROWS || ranges.len() != emitted.segments.len() {
            self.missing(emitted.segments.len());
            return (emitted.segments, Vec::new());
        }
        let info = self.info(StageKind::CleanupAssembly, Some(QualityTrack::Tap));
        let assembled = self.run.session.ledger_mut().assemble(info, &[parent], &[]);
        let Some(invocation) = self.output(assembled).first().map(|link| link.parent) else {
            return (emitted.segments, Vec::new());
        };
        let descendants: Vec<_> = emitted
            .segments
            .into_iter()
            .zip(ranges)
            .map(|(segment, source)| DerivedSegment {
                original: parent.row,
                source,
                segment,
            })
            .collect();
        let info = self.info(kind, Some(QualityTrack::Tap));
        let result = self
            .run
            .session
            .ledger_mut()
            .derive_segments(info, invocation, &descendants);
        let links = self.output(result);
        (
            descendants.into_iter().map(|row| row.segment).collect(),
            links,
        )
    }

    pub(crate) fn cleanup(
        &mut self,
        segments: Vec<TranscriptSegment>,
        links: &[ParentRow],
        protected: &[usize],
        config: &CleanupConfig,
        rule: Option<&dyn TextRule>,
    ) -> (Vec<TranscriptSegment>, CleanupStats) {
        let info = self.info(StageKind::CleanupAssembly, None);
        let result = self.run.session.ledger_mut().cleanup(
            info,
            segments,
            config,
            CleanupInput {
                origins: &[],
                source_links: Some(links),
                carry: &[],
                carry_links: Some(&[]),
                protected_input_indices: protected,
            },
            None,
            rule,
        );
        match result.evidence {
            Ok(recorded) => {
                self.run.final_stage = recorded.final_stage;
                let links = self.output(Ok(recorded));
                if self.completed.len().saturating_add(links.len()) <= MAX_CAPTURE_ROWS {
                    self.completed.extend(links);
                } else {
                    self.missing(links.len());
                    self.completed.clear();
                }
            }
            Err(_) => {
                self.run.unavailable = true;
                self.run.final_stage = None;
                super::unavailable();
            }
        }
        (result.segments, result.stats)
    }

    pub(crate) fn persist_window(&mut self, audio: &Path) -> Status {
        self.run.append_terminal(None, false);
        self.run.persist(audio)
    }

    pub(crate) fn advance_window(&mut self) {
        self.window = self.window.saturating_add(1);
        self.run.window = self.window;
    }

    pub(crate) fn settle(&mut self, quality: &corti_core::QualityRecord, audio: &Path) -> Status {
        self.run.settle_quality(quality);
        self.run.persist(audio)
    }

    pub(crate) fn provider_input(&mut self, transcript: &corti_core::DiarizedTranscript) {
        // These are already exact parent IDs, never candidates found by matching text/time. Apply
        // NoteWriter::final_transcript's stable ordering to them, then verify the complete ordered
        // rows against the actual finalizer input before recording ProviderInput.
        let ledger = self.run.session.ledger();
        self.completed.sort_by(
            |left, right| match (ledger.row(left.row), ledger.row(right.row)) {
                (Ok(left), Ok(right)) => left.segment.start.total_cmp(&right.segment.start),
                _ => std::cmp::Ordering::Equal,
            },
        );
        let protected: Vec<_> = self
            .completed
            .iter()
            .enumerate()
            .filter_map(|(index, link)| {
                ledger
                    .stage(link.parent)
                    .ok()
                    .filter(|stage| stage.protected_output.contains(&link.row))
                    .map(|_| index)
            })
            .collect();
        let info = self.info(StageKind::CleanupAssembly, None);
        let result = self
            .run
            .session
            .ledger_mut()
            .assemble(info, &self.completed, &protected);
        match result {
            Ok(recorded) => self.run.final_stage = recorded.final_stage,
            Err(_) => self.missing(self.completed.len()),
        }
        self.run.provider_input(transcript);
    }

    pub(crate) fn final_prose(
        &mut self,
        transcript: &corti_core::DiarizedTranscript,
        applied: bool,
    ) {
        self.run.final_prose(transcript, applied);
    }
}
