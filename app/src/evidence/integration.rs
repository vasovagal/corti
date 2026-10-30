//! Actual transcription-stage adapters; the private-file and generation boundary lives in the parent.

use super::{Error, Session, Status};
use corti_core::{QualityTrack, RecordingMeta, Speaker, TranscriptSegment};
use corti_transcribe::evidence::{
    AecOrigin, Append, CleanupInput, Group, InputOrigin, InvocationId, RecordingIdentity, Row,
    RowId, SignalIdentity, SignalSource, Stage, StageKind,
};
use corti_transcribe::segment::{AudioEvidence, CleanupConfig, CleanupStats, TextRule};
use corti_vagus::provenance::TranscriptProvenance;
use std::path::Path;

/// Pipeline-owned optional text state. The backend never receives the session or its filename.
pub(crate) struct Run {
    pub(super) session: Session,
    pub(super) final_stage: Option<InvocationId>,
    pub(super) unavailable: bool,
    pub(super) window: u64,
}

pub(crate) struct BatchRequest<'a> {
    pub(crate) output: &'a mut Option<Run>,
    pub(crate) capture: Option<&'a corti_capture::CaptureProcessing>,
    pub(crate) already_clean: bool,
    pub(crate) kind: StageKind,
}

impl Run {
    pub(crate) fn begin(
        id: &str,
        meta: &RecordingMeta,
        provenance: &TranscriptProvenance,
    ) -> Result<Self, Error> {
        Ok(Self {
            session: Session::with_provenance(id, meta, provenance)?,
            final_stage: None,
            unavailable: false,
            window: 0,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn cleanup(
        &mut self,
        segments: Vec<TranscriptSegment>,
        config: &CleanupConfig,
        audio: Option<AudioEvidence<'_>>,
        rule: Option<&dyn TextRule>,
        kind: StageKind,
        signal: SignalIdentity,
        local_tracks: bool,
    ) -> (Vec<TranscriptSegment>, CleanupStats) {
        // Source rows retain the actual decoder's channel attribution. A provider without that
        // contract gets explicit unknown attribution rather than guessed speaker-to-track identity.
        let origins: Vec<_> = segments
            .iter()
            .take(
                if segments.len() <= corti_transcribe::evidence::MAX_CAPTURE_ROWS {
                    segments.len()
                } else {
                    0
                },
            )
            .map(|segment| InputOrigin {
                track: local_tracks.then_some(match segment.speaker {
                    Speaker::Me => QualityTrack::Mic,
                    Speaker::Other(_) => QualityTrack::Tap,
                }),
                source_region_index: None,
            })
            .collect();
        let identity = self.session.identity();
        let info = RecordingIdentity {
            window: 0,
            kind,
            identity,
            signal,
        };
        if config.is_noop() && rule.is_none() {
            match self
                .session
                .ledger_mut()
                .capture(info, &segments, &origins, &[])
            {
                Ok(recorded) => self.final_stage = recorded.final_stage,
                Err(_) => {
                    self.unavailable = true;
                    super::unavailable();
                }
            }
            return (segments, CleanupStats::default());
        }
        let result = self.session.ledger_mut().cleanup(
            info,
            segments,
            config,
            CleanupInput {
                origins: &origins,
                source_links: None,
                carry: &[],
                carry_links: None,
                protected_input_indices: &[],
            },
            audio,
            rule,
        );
        match result.evidence {
            Ok(recorded) => self.final_stage = recorded.final_stage,
            Err(_) => {
                self.unavailable = true;
                super::unavailable();
            }
        }
        (result.segments, result.stats)
    }

    pub(crate) fn decision_counts(&self) -> (usize, u64) {
        (
            self.session
                .ledger()
                .groups()
                .iter()
                .flat_map(|group| &group.stages)
                .map(|stage| stage.decisions.len())
                .sum(),
            self.session.ledger().truncation().omitted_decisions,
        )
    }

    pub(super) fn failed(&mut self, rows: usize) {
        let _ = self.session.ledger_mut().mark_unavailable(rows, 0);
        self.final_stage = None;
        self.unavailable = true;
        super::unavailable();
    }

    /// The selected immutable text actually passed to the existing finalizer. This adds no hosted
    /// call and passes no evidence record to that finalizer. Parent rows remain the same owners.
    pub(crate) fn provider_input(&mut self, transcript: &corti_core::DiarizedTranscript) {
        self.append_terminal(Some((transcript, false)), true);
    }

    pub(crate) fn final_prose(
        &mut self,
        transcript: &corti_core::DiarizedTranscript,
        hosted_applied: bool,
    ) {
        self.append_terminal(Some((transcript, hosted_applied)), false);
    }

    pub(super) fn append_terminal(
        &mut self,
        final_text: Option<(&corti_core::DiarizedTranscript, bool)>,
        provider: bool,
    ) {
        let Some(parent) = self.final_stage else {
            return;
        };
        let maximum = corti_transcribe::evidence::MAX_CAPTURE_ROWS;
        if final_text.is_some_and(|(text, _)| {
            text.segments.len() > maximum
                || text
                    .segments
                    .iter()
                    .try_fold(0usize, |total, row| {
                        total
                            .checked_add(std::mem::size_of::<Row>())?
                            .checked_add(row.text.len())?
                            .checked_add(row.speaker.display().len())
                    })
                    .is_none_or(|bytes| bytes > corti_transcribe::evidence::MAX_CAPTURE_BYTES / 2)
        }) {
            self.failed(final_text.map_or(0, |(text, _)| text.segments.len()));
            return;
        }
        let result = (|| {
            let ledger = self.session.ledger();
            let previous = ledger.stage(parent)?;
            if previous.output.len() > maximum {
                return Err(corti_transcribe::evidence::Error::Capacity);
            }
            let next = ledger.next_invocation()?;
            let new_rows = final_text.is_some_and(|(_, applied)| applied);
            if let Some((text, false)) = final_text
                && (previous.output.len() != text.segments.len()
                    || previous.output.iter().zip(&text.segments).any(|(id, row)| {
                        ledger
                            .row(*id)
                            .map_or(true, |stored| stored.segment != *row)
                    }))
            {
                return Err(corti_transcribe::evidence::Error::InvalidLink);
            }
            let rows: Vec<Row> = if new_rows {
                final_text
                    .unwrap()
                    .0
                    .segments
                    .iter()
                    .enumerate()
                    .map(|(index, segment)| Row {
                        id: RowId {
                            invocation: next,
                            slot: index as u32,
                        },
                        track: None,
                        source_region_index: None,
                        segment: segment.clone(),
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let output = if new_rows {
                rows.iter().map(|row| row.id).collect()
            } else {
                previous.output.clone()
            };
            let protected_output = if new_rows {
                Vec::new()
            } else {
                previous.protected_output.clone()
            };
            // Only rows the provider returned are provider text. The handoff input and a declined
            // final prose are the same local rows, so they keep the previous lineage.
            let signal = if new_rows {
                SignalIdentity {
                    source: SignalSource::ProviderText,
                    aec: AecOrigin::None,
                    source_rate_hz: None,
                    lookahead_frames: None,
                }
            } else {
                let previous_group = ledger
                    .groups()
                    .iter()
                    .find(|group| group.stages.iter().any(|stage| stage.id == parent))
                    .ok_or(corti_transcribe::evidence::Error::InvalidLink)?;
                SignalIdentity {
                    source: previous_group.signal.source,
                    aec: previous_group.signal.aec,
                    source_rate_hz: previous_group.signal.source_rate_hz,
                    lookahead_frames: previous_group.signal.lookahead_frames,
                }
            };
            let group = Group {
                window: self.window,
                identity: self.session.identity(),
                signal,
                rows,
                stages: vec![Stage {
                    id: next,
                    kind: if provider {
                        StageKind::ProviderInput
                    } else {
                        StageKind::FinalProse
                    },
                    input: Some(parent),
                    assembled_input: Vec::new(),
                    context: Vec::new(),
                    protected_output,
                    output,
                    decisions: Vec::new(),
                    derivations: Vec::new(),
                }],
            };
            let appended = self.session.ledger_mut().append(group)?;
            Ok((next, appended))
        })();
        match result {
            Ok((next, Append::Retained)) => self.final_stage = Some(next),
            Ok((_, Append::Omitted)) => self.final_stage = None,
            Err(_) => self.failed(final_text.map_or(0, |(text, _)| text.segments.len())),
        }
    }

    pub(crate) fn settle_quality(&mut self, quality: &corti_core::QualityRecord) {
        if self
            .session
            .ledger_mut()
            .set_final_quality(quality.clone())
            .is_err()
        {
            self.unavailable = true;
            super::unavailable();
        }
    }

    pub(crate) fn persist(&self, audio: &Path) -> Status {
        let status = self.session.persist(audio);
        if self.unavailable {
            Status::Unavailable
        } else {
            status
        }
    }
}

/// The same immutable execution carrier used by note provenance determines which signal produced
/// these words. A retained capture WAV never claims to be raw or an exact replay of live AEC.
pub(crate) fn batch_signal(
    capture: Option<&corti_capture::CaptureProcessing>,
    offline: Option<&crate::transcribe::OfflineAec>,
    outcome: crate::transcribe::OfflineAecOutcome,
    quality: &corti_core::QualityRecord,
    already_clean: bool,
) -> SignalIdentity {
    let rate = quality
        .epochs
        .first()
        .map(|epoch| epoch.retained_rate_hz)
        .filter(|rate| {
            quality
                .epochs
                .iter()
                .all(|epoch| epoch.retained_rate_hz == *rate)
        });
    let executed = if already_clean && offline.is_none() {
        capture.and_then(|capture| match &capture.aec {
            corti_capture::CaptureAecState::RawFallback {
                config,
                lookahead_seconds,
            } => Some((AecOrigin::OfflineFile, config, *lookahead_seconds)),
            _ => None,
        })
    } else if outcome == crate::transcribe::OfflineAecOutcome::Applied {
        offline.map(|aec| (AecOrigin::OfflineFile, &aec.config, aec.lookahead_seconds))
    } else if offline.is_none() {
        capture.and_then(|capture| match &capture.aec {
            corti_capture::CaptureAecState::Applied {
                config,
                lookahead_seconds,
            }
            | corti_capture::CaptureAecState::Degraded {
                config,
                lookahead_seconds,
            } => Some((AecOrigin::Capture, config, *lookahead_seconds)),
            _ => None,
        })
    } else {
        None
    };
    match executed {
        Some((origin, config, seconds)) => SignalIdentity {
            source: SignalSource::RetainedProcessedWav,
            aec: origin,
            source_rate_hz: rate,
            lookahead_frames: rate.map(|rate| {
                corti_aec::lookahead_samples_for(rate, config.filter_len, seconds) as u64
            }),
        },
        None => SignalIdentity {
            source: if already_clean {
                SignalSource::RetainedProcessedWav
            } else {
                SignalSource::OfflineInput
            },
            aec: AecOrigin::None,
            source_rate_hz: rate,
            lookahead_frames: None,
        },
    }
}

/// A filing checkpoint remains authoritative when optional evidence disappears or is corrupt. This
/// check returns only a content-free status and does not write either the checkpoint or the ledger.
pub(crate) fn replay_status(
    id: &str,
    meta: &RecordingMeta,
    audio: &Path,
    status: Option<Status>,
    enabled: bool,
) -> Status {
    if !enabled {
        return Status::Disabled;
    }
    match status {
        Some(status @ (Status::Available { .. } | Status::Truncated { .. })) => {
            if Session::load(id, meta, audio, status).is_ok() {
                status
            } else {
                super::unavailable();
                Status::Unavailable
            }
        }
        Some(Status::Disabled) => Status::Disabled,
        _ => Status::Unavailable,
    }
}
