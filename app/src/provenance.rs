//! Build note-frontmatter provenance from the exact runtime config snapshot used for transcription.
//!
//! This is an allowlist of quality-relevant settings. Credentials, AWS profile/bucket, model directories,
//! and absolute override paths are deliberately excluded.

use std::collections::BTreeMap;

use corti_transcribe::segment::{CLEANUP_RULES_VERSION, CleanupConfig};
use corti_vagus::provenance::{
    GenerationMode, ModelIdentity, TranscriptModels, TranscriptProvenance,
};
use serde_json::{Map, Number, Value};

use crate::config::{AppConfig, BackendChoice};

/// Leave 16 KiB of the existing 64 KiB note-prefix bound for non-quality provenance/frontmatter.
/// Mandatory sidecar and checkpoint records remain full; only the display projection is bounded here.
pub(crate) const QUALITY_FRONTMATTER_BYTES: usize = 48 * 1024;

pub(crate) fn quality_frontmatter_projection(
    quality: &corti_core::QualityRecord,
) -> anyhow::Result<corti_core::QualityRecord> {
    quality.validate()?;
    let mut projection = quality.clone();
    while serde_json::to_vec(&projection)?.len() > QUALITY_FRONTMATTER_BYTES {
        let span = projection
            .spans
            .pop()
            .ok_or_else(|| anyhow::anyhow!("quality_epoch_projection_exceeds_prefix_budget"))?;
        // Keep every epoch/source offset. Omitted facts remain class/resolution-aware lower bounds in
        // the same record type; successful recovery cannot erase an omitted source-loss fact.
        projection.record_omitted_spans(span.class, span.resolution, 1)?;
    }
    Ok(projection)
}

/// Carry the same bounded record into note frontmatter; no private text or recording path is added.
pub(crate) fn attach_quality(
    provenance: &mut TranscriptProvenance,
    quality: &corti_core::QualityRecord,
    storage: Option<crate::transcribe::RecordingStorageStatus>,
) -> anyhow::Result<()> {
    quality.validate()?;
    if let Some(Value::Object(aec)) = provenance.configuration.get_mut("aec") {
        project_aec_activity(aec, Some(quality));
    }
    let projection = quality_frontmatter_projection(quality)?;
    provenance
        .configuration
        .insert("quality".into(), serde_json::to_value(&projection)?);
    provenance.configuration.insert(
        "quality_projection_truncated".into(),
        Value::Bool(projection != *quality),
    );
    provenance.configuration.insert(
        "quality_full_span_count".into(),
        Value::from(quality.spans.len()),
    );
    provenance.configuration.insert(
        "source_completeness".into(),
        serde_json::to_value(completeness_with_storage(
            quality.source_completeness(),
            storage,
        ))?,
    );
    provenance.configuration.insert(
        "text_completeness".into(),
        serde_json::to_value(completeness_with_storage(quality.completeness(), storage))?,
    );
    if let Some(storage) = storage {
        provenance
            .configuration
            .insert("quality_storage".into(), serde_json::to_value(storage)?);
    }
    Ok(())
}

/// Storage uncertainty affects completion claims, never the retained source record. Known losses
/// remain incomplete; intrinsic unknown coverage remains unknown. An absent legacy storage field
/// retains its previous reporting semantics rather than inventing a new storage observation.
fn completeness_with_storage(
    completeness: corti_core::SourceCompleteness,
    storage: Option<crate::transcribe::RecordingStorageStatus>,
) -> corti_core::SourceCompleteness {
    if completeness == corti_core::SourceCompleteness::Complete
        && storage
            .is_some_and(|status| status != crate::transcribe::RecordingStorageStatus::AudioDurable)
    {
        corti_core::SourceCompleteness::Unknown
    } else {
        completeness
    }
}

/// Preserve the Markdown turn syntax while displaying known source offsets. A turn crossing an
/// unfilled epoch boundary keeps its text exactly once; its endpoints are mapped independently and
/// the timing uncertainty is explicit. Word positions inside such a turn are not inferred.
pub(crate) fn quality_body(
    meta: &corti_core::RecordingMeta,
    transcript: &corti_core::DiarizedTranscript,
    quality: Option<&corti_core::QualityRecord>,
    storage: Option<crate::transcribe::RecordingStorageStatus>,
) -> String {
    use corti_core::{QualityClass, SourceCompleteness};
    let Some(quality) = quality else {
        return corti_vagus::recording_body(meta, transcript);
    };
    if quality.validate().is_err() {
        return format!(
            "{}\n\n> Capture quality is invalid; source and transcript completeness are unknown.\n",
            corti_vagus::recording_body(meta, transcript)
        );
    }
    let mut display = transcript.clone();
    let mut uncertain_turns = 0usize;
    for segment in &mut display.segments {
        let start = source_endpoint(quality, segment.start, false);
        let end = source_endpoint(quality, segment.end, true);
        match (start, end) {
            (Some((start, first)), Some((end, last))) if end >= start => {
                segment.start = start;
                segment.end = end;
                uncertain_turns += usize::from(first != last);
            }
            _ if !quality.epochs.is_empty() => uncertain_turns += 1,
            _ => {}
        }
    }
    let mut body = corti_vagus::recording_body(meta, &display);
    if completeness_with_storage(quality.completeness(), storage) != SourceCompleteness::Complete {
        body.push_str("\n\n> Transcript coverage is partial or unknown. Retained audio can be retried with `corti --redo <id> --local`.\n");
    }
    if let Some(storage) = storage
        && storage != crate::transcribe::RecordingStorageStatus::AudioDurable
    {
        body.push_str(&format!(
            "\n> Capture storage: {}. Completion is unverified.\n",
            enum_label(storage)
        ));
    }
    for span in &quality.spans {
        if span.class == QualityClass::DegradedOnly {
            continue;
        }
        let epoch = quality.epochs.iter().find(|epoch| epoch.id == span.epoch);
        let times = epoch.and_then(|epoch| {
            Some((
                epoch.source_time_ns(span.start_frame).ok()??,
                epoch.source_time_ns(span.end_frame).ok()??,
            ))
        });
        let location = match times {
            Some((start, end)) => format!("{:.3}–{:.3} s", start as f64 / 1e9, end as f64 / 1e9),
            None => format!(
                "epoch {} frames {}–{} (source offset unknown)",
                span.epoch, span.start_frame, span.end_frame
            ),
        };
        body.push_str(&format!(
            "\n> Audio/text availability: {} at {location}; {}, {}, {}.\n",
            enum_label(span.cause),
            enum_label(span.track),
            enum_label(span.class),
            enum_label(span.resolution)
        ));
    }
    for pair in quality.epochs.windows(2) {
        let previous = &pair[0];
        let next = &pair[1];
        let retained_delta = next
            .retained_start_frame
            .saturating_sub(previous.retained_start_frame);
        let expected = previous.source_start_ns.and_then(|base| {
            u64::try_from(
                u128::from(retained_delta) * 1_000_000_000 / u128::from(previous.retained_rate_hz),
            )
            .ok()
            .and_then(|delta| base.checked_add(delta))
        });
        if next.source_start_ns != expected {
            body.push_str(&format!(
                "\n> Unfilled capture epoch {}: source offset {}, retained frame {} at {} Hz.\n",
                next.id,
                next.source_start_ns
                    .map(|ns| format!("{:.3} s", ns as f64 / 1e9))
                    .unwrap_or_else(|| "unknown".into()),
                next.retained_start_frame,
                next.retained_rate_hz
            ));
        }
    }
    if quality.overflow.omitted_spans != [0; 3]
        || quality.overflow.omitted_epochs != 0
        || quality.overflow.counter_saturated
    {
        body.push_str(&format!("\n> Quality accounting overflow: source/processing/degraded spans {:?}; outstanding {:?}; omitted epochs {}; counter saturated {}.\n", quality.overflow.omitted_spans, quality.overflow.outstanding_omitted_spans, quality.overflow.omitted_epochs, quality.overflow.counter_saturated));
    }
    if uncertain_turns != 0 {
        body.push_str(&format!("\n> {uncertain_turns} transcript turn(s) cross an epoch boundary or have an unknown source offset; within-turn source timing is uncertain.\n"));
    }
    body
}

fn enum_label(value: impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

fn source_endpoint(
    quality: &corti_core::QualityRecord,
    seconds: f64,
    end: bool,
) -> Option<(f64, u32)> {
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    let (index, epoch) = quality.epochs.iter().enumerate().rev().find(|(_, epoch)| {
        let boundary = epoch.retained_start_frame as f64 / f64::from(epoch.retained_rate_hz);
        seconds > boundary || (!end && seconds == boundary) || (seconds == 0.0 && boundary == 0.0)
    })?;
    if index + 1 == quality.epochs.len() && quality.overflow.omitted_epochs != 0 {
        return None;
    }
    let source = epoch.source_start_ns? as f64 / 1e9;
    let retained = epoch.retained_start_frame as f64 / f64::from(epoch.retained_rate_hz);
    Some((source + seconds - retained, epoch.id))
}

/// How the audio supplied to ASR was echo-processed. Capture/retry callers select this from the durable
/// queue record, never mutable Settings.
#[derive(Clone, Copy)]
pub(crate) enum AecExecution<'a> {
    #[cfg_attr(not(feature = "local"), allow(dead_code))]
    Live,
    Capture(&'a corti_capture::CaptureProcessing),
    Offline {
        config: &'a corti_aec::AecConfig,
        lookahead_seconds: f32,
        outcome: crate::transcribe::OfflineAecOutcome,
        capture_fallback: bool,
    },
    /// The caller supplied an already-clean artifact but no capture record survives to identify settings.
    PreprocessedUnknown,
    Disabled,
}

/// Describe the final transcript generated under `cfg`. Callers must pass the same immutable snapshot the
/// backend/live engine owns, never re-read shared Settings at filing time.
#[cfg_attr(not(feature = "local"), allow(dead_code))]
/// Describe the final transcript generated under `cfg`, deriving the audio-evidence flag from the mode:
/// only the live engine runs its own canceller alongside the transcriber, so only live can hand the
/// cleanup a `SpanEvidence` accessor without being told. Batch callers know per-recording whether a
/// `-aec-stats.json` sidecar was found and use [`from_config_with_aec`] directly.
pub(crate) fn from_config(cfg: &AppConfig, mode: GenerationMode) -> TranscriptProvenance {
    let effective = cfg.aec_config();
    let aec = if mode == GenerationMode::Live {
        AecExecution::Live
    } else if cfg.aec_enabled {
        AecExecution::Offline {
            config: &effective,
            lookahead_seconds: corti_aec::configured_lookahead_seconds(),
            outcome: crate::transcribe::OfflineAecOutcome::Applied,
            capture_fallback: false,
        }
    } else {
        AecExecution::Disabled
    };
    let audio_evidence = mode == GenerationMode::Live && cfg.aec_enabled;
    from_config_with_aec(cfg, mode, aec, audio_evidence)
}

/// `audio_evidence` records whether the segment cleanup actually had the canceller's per-block statistics
/// for this transcript (#149 phase 3b). It is a runtime fact, not a setting: the same config yields `false`
/// for an AWS job, for a recording with no `-aec-stats.json` sidecar, and for a capture whose AEC was
/// disabled.
pub(crate) fn from_config_with_aec(
    cfg: &AppConfig,
    mode: GenerationMode,
    aec: AecExecution<'_>,
    audio_evidence: bool,
) -> TranscriptProvenance {
    match cfg.transcribe_backend {
        BackendChoice::Aws => aws(cfg, mode, aec, audio_evidence),
        BackendChoice::Local => local(cfg, mode, aec, audio_evidence),
    }
}

fn aws(
    cfg: &AppConfig,
    mode: GenerationMode,
    aec: AecExecution<'_>,
    audio_evidence: bool,
) -> TranscriptProvenance {
    let mut configuration = base_configuration(cfg, mode, aec, audio_evidence);
    configuration.insert("language".into(), Value::String(cfg.language.clone()));
    configuration.insert(
        "speaker_attribution".into(),
        Value::String("channel_identification_for_multichannel".into()),
    );
    TranscriptProvenance::new(
        mode,
        "aws",
        TranscriptModels {
            // AWS does not expose a selectable/versioned acoustic model for ordinary Transcribe jobs.
            asr: ModelIdentity::new("aws/transcribe-default", None::<String>),
            vad: None,
            diarization: None,
            speaker_embedding: None,
        },
        configuration,
    )
}

#[cfg(feature = "local")]
fn local(
    cfg: &AppConfig,
    mode: GenerationMode,
    aec: AecExecution<'_>,
    audio_evidence: bool,
) -> TranscriptProvenance {
    use corti_transcribe_local::{LocalConfig, models};

    let defaults = LocalConfig::default();
    let wants_ggml = cfg.local_asr_engine == models::GGML_ASR_ENGINE;
    let asr_artifact = if wants_ggml {
        cfg.local_ggml_model
            .as_deref()
            .and_then(std::path::Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| models::GGML_FILE.to_string())
    } else {
        models::PARAKEET_DIR.to_string()
    };
    let selected_embedding = models::embedding_spec(&cfg.local_embedding_model);

    let mut configuration = base_configuration(cfg, mode, aec, audio_evidence);
    configuration.insert(
        "asr_engine".into(),
        Value::String(cfg.local_asr_engine.clone()),
    );
    configuration.insert("threads".into(), Value::from(i64::from(cfg.local_threads)));
    configuration.insert(
        "diarize_far_end".into(),
        Value::Bool(cfg.local_diarize_far_end),
    );
    configuration.insert(
        "speaker_attribution".into(),
        Value::String(if cfg.local_diarize_far_end {
            "channels+pyannote".into()
        } else {
            "channels".into()
        }),
    );
    configuration.insert(
        "diarization_threshold".into(),
        float_value(cfg.local_diarize_threshold),
    );
    configuration.insert("vad_threshold".into(), float_value(defaults.vad_threshold));
    configuration.insert(
        "vad_min_silence_seconds".into(),
        float_value(defaults.vad_min_silence),
    );

    TranscriptProvenance::new(
        mode,
        "local",
        TranscriptModels {
            asr: ModelIdentity::new(models::PARAKEET_MODEL_ID, Some(asr_artifact)),
            vad: Some(ModelIdentity::new(
                models::VAD_MODEL_ID,
                Some(models::VAD_FILE),
            )),
            diarization: cfg.local_diarize_far_end.then(|| {
                ModelIdentity::new(
                    models::SEGMENTATION_MODEL_ID,
                    Some(format!("{}/model.onnx", models::SEGMENTATION_DIR)),
                )
            }),
            speaker_embedding: cfg.local_diarize_far_end.then(|| {
                ModelIdentity::new(selected_embedding.id, Some(selected_embedding.install_rel))
            }),
        },
        configuration,
    )
}

#[cfg(not(feature = "local"))]
fn local(
    cfg: &AppConfig,
    mode: GenerationMode,
    aec: AecExecution<'_>,
    audio_evidence: bool,
) -> TranscriptProvenance {
    // This can only be observed if a build without the local backend somehow files a successful local
    // transcript. Keep it truthful rather than borrowing AWS/current settings.
    let mut provenance = TranscriptProvenance::legacy_unknown(mode);
    provenance.backend = "local-unavailable".into();
    provenance.configuration = base_configuration(cfg, mode, aec, audio_evidence);
    provenance
}

fn base_configuration(
    cfg: &AppConfig,
    mode: GenerationMode,
    execution: AecExecution<'_>,
    audio_evidence: bool,
) -> BTreeMap<String, Value> {
    let current = cfg.aec_config();
    let (enabled, aec_mode, outcome, effective, lookahead_seconds) = match execution {
        AecExecution::Live => (
            cfg.aec_enabled,
            if cfg.aec_enabled {
                "streaming"
            } else {
                "disabled"
            },
            if cfg.aec_enabled {
                "pending"
            } else {
                "disabled"
            },
            &current,
            cfg.aec_enabled.then_some(0.1), // The actual live short-lookahead canceller.
        ),
        AecExecution::Capture(processing) => match &processing.aec {
            corti_capture::CaptureAecState::Disabled => {
                (false, "disabled", "disabled", &current, None)
            }
            corti_capture::CaptureAecState::NotApplicable => {
                (false, "not_applicable", "not_applicable", &current, None)
            }
            corti_capture::CaptureAecState::Applied {
                config,
                lookahead_seconds,
            } => (
                true,
                "capture_streaming",
                "applied",
                config,
                Some(*lookahead_seconds),
            ),
            corti_capture::CaptureAecState::RawFallback {
                config,
                lookahead_seconds,
            } => (
                true,
                "capture_streaming",
                "raw_fallback",
                config,
                Some(*lookahead_seconds),
            ),
            corti_capture::CaptureAecState::Degraded {
                config,
                lookahead_seconds,
            } => (
                true,
                "capture_streaming",
                "degraded_mixed",
                config,
                Some(*lookahead_seconds),
            ),
        },
        AecExecution::Offline {
            config,
            lookahead_seconds,
            outcome,
            capture_fallback,
        } => match outcome {
            crate::transcribe::OfflineAecOutcome::Applied => (
                true,
                if capture_fallback {
                    "offline_capture_fallback"
                } else {
                    "offline"
                },
                "applied",
                config,
                Some(lookahead_seconds),
            ),
            crate::transcribe::OfflineAecOutcome::NotApplicable => {
                (false, "not_applicable", "not_applicable", config, None)
            }
            crate::transcribe::OfflineAecOutcome::Failed => (
                true,
                if capture_fallback {
                    "offline_capture_fallback"
                } else {
                    "offline"
                },
                "failed_raw_fallback",
                config,
                Some(lookahead_seconds),
            ),
            crate::transcribe::OfflineAecOutcome::NotRequested => {
                (false, "disabled", "disabled", config, None)
            }
        },
        AecExecution::PreprocessedUnknown => {
            (true, "preprocessed_unknown", "unknown", &current, None)
        }
        AecExecution::Disabled => (false, "disabled", "disabled", &current, None),
    };
    let mut aec = Map::new();
    aec.insert("enabled".into(), Value::Bool(enabled));
    aec.insert("mode".into(), Value::String(aec_mode.into()));
    aec.insert("outcome".into(), Value::String(outcome.into()));
    aec.insert("execution_outcome".into(), Value::String(outcome.into()));
    aec.insert(
        "observation_scope".into(),
        Value::String(
            match execution {
                AecExecution::Live => "live",
                AecExecution::Capture(_) => "capture",
                AecExecution::Offline { .. } => "offline",
                AecExecution::PreprocessedUnknown | AecExecution::Disabled => "unknown",
            }
            .into(),
        ),
    );
    project_aec_activity(
        &mut aec,
        match execution {
            AecExecution::Capture(processing) => processing.quality.as_ref(),
            _ => None,
        },
    );
    aec.insert(
        "filter_len".into(),
        Value::from(effective.filter_len as u64),
    );
    aec.insert("mu".into(), float_value(effective.mu));
    aec.insert("eps".into(), float_value(effective.eps));
    aec.insert(
        "power_smoothing".into(),
        float_value(effective.power_smoothing),
    );
    aec.insert(
        "double_talk_ratio".into(),
        float_value(effective.double_talk_ratio),
    );
    aec.insert(
        "suppress_residual".into(),
        float_value(effective.suppress_residual),
    );
    aec.insert("max_lag_ms".into(), float_value(effective.max_lag_ms));
    if let Some(seconds) = lookahead_seconds {
        aec.insert("lookahead_seconds".into(), float_value(seconds));
    }

    let mut configuration = BTreeMap::new();
    configuration.insert("aec".into(), Value::Object(aec));
    configuration.insert(
        "segment_cleanup".into(),
        segment_cleanup(
            &cfg.cleanup_config(),
            mode == GenerationMode::Live,
            audio_evidence,
        ),
    );
    configuration.insert(
        "input".into(),
        Value::String(
            if mode == GenerationMode::Live {
                "live_pcm_stream"
            } else {
                "completed_recording"
            }
            .into(),
        ),
    );
    if mode == GenerationMode::Live {
        configuration.insert(
            "live_buffer_minutes".into(),
            Value::from(u64::from(cfg.live_buffer_minutes)),
        );
    }
    configuration
}

/// Observation projection never changes execution disposition or selects another audio path.
fn project_aec_activity(aec: &mut Map<String, Value>, quality: Option<&corti_core::QualityRecord>) {
    use corti_core::{ActivityScope, FarReference};
    let scope = match aec.get("observation_scope").and_then(Value::as_str) {
        Some("capture") => Some(ActivityScope::Capture),
        Some("offline") => Some(ActivityScope::Offline),
        Some("live") => Some(ActivityScope::Live),
        _ => None,
    };
    let evidence = scope
        .and_then(|scope| quality.map(|quality| *quality.activity.get(scope)))
        .unwrap_or_default();
    let reference = evidence.far_reference();
    let label = match reference {
        FarReference::Unknown => "unknown",
        FarReference::Present => "present",
        FarReference::Intermittent => "intermittent",
        FarReference::Absent => "absent",
    };
    let execution = aec
        .get("execution_outcome")
        .or_else(|| aec.get("outcome"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let outcome = if execution == "applied" {
        match reference {
            FarReference::Unknown => "applied_far_reference_unknown",
            FarReference::Present => "applied",
            FarReference::Intermittent => "applied_with_intermittent_far_reference",
            FarReference::Absent => "no_far_reference",
        }
    } else {
        &execution
    }
    .to_owned();
    aec.insert("outcome".into(), Value::String(outcome));
    aec.entry("execution_outcome")
        .or_insert(Value::String(execution));
    aec.insert("far_reference".into(), Value::String(label.into()));
    aec.insert(
        "suspected_silent_far_reference".into(),
        Value::Bool(evidence.suspected()),
    );
    aec.insert(
        "activity_uncertain".into(),
        Value::Bool(evidence.uncertain()),
    );
}

/// Called from the live consumer when it actually feeds the canceller, and at its finish refresh.
/// Pending construction is not evidence that any mic/reference samples reached AEC.
#[cfg_attr(not(feature = "local"), allow(dead_code))]
pub(crate) fn observe_live_aec_execution(
    provenance: &mut TranscriptProvenance,
    ran: bool,
    saw_mic: bool,
) {
    if let Some(Value::Object(aec)) = provenance.configuration.get_mut("aec")
        && aec.get("observation_scope").and_then(Value::as_str) == Some("live")
        && aec.get("enabled").and_then(Value::as_bool) == Some(true)
    {
        let outcome = if ran {
            "applied"
        } else if !saw_mic {
            "not_applicable"
        } else {
            "pending"
        };
        aec.insert("execution_outcome".into(), Value::String(outcome.into()));
        // The following attach_quality supplies the actual scope's reference observations.
        aec.insert("outcome".into(), Value::String(outcome.into()));
    }
}

/// The deterministic segment-cleanup rules a transcript was produced under (#149), or `"off"`.
/// Live additionally records whether its early drop was running (#149 phase 2).
fn segment_cleanup(cleanup: &CleanupConfig, live: bool, audio_evidence: bool) -> Value {
    if cleanup.is_noop() {
        return Value::String("off".into());
    }
    let mut map = Map::new();
    map.insert(
        "rules".into(),
        Value::from(u64::from(CLEANUP_RULES_VERSION)),
    );
    map.insert("echo_drop".into(), Value::Bool(cleanup.echo_drop));
    map.insert(
        "echo_policy".into(),
        Value::String("acoustic_required_mic_only".into()),
    );
    map.insert(
        "text_only_duplicates".into(),
        Value::String("keep_suspected".into()),
    );
    map.insert("critical_text_protection".into(), Value::Bool(true));
    map.insert(
        "echo_window_seconds".into(),
        double_value(cleanup.echo_window_seconds),
    );
    map.insert(
        "echo_containment".into(),
        double_value(cleanup.echo_containment),
    );
    map.insert(
        "merge_gap_seconds".into(),
        double_value(cleanup.merge_gap_seconds),
    );
    map.insert(
        "drop_backchannels".into(),
        Value::Bool(cleanup.drop_backchannels),
    );
    map.insert(
        "echo_audio_margin_db".into(),
        double_value(f64::from(cleanup.echo_audio_margin_db)),
    );
    // Rules 3 (#154): filler/stutter stripping, and the learned lexicon applied as the last pass. The
    // lexicon is described by revision/digest/count only; its rules never enter a note.
    map.insert("strip_fillers".into(), Value::Bool(cleanup.strip_fillers));
    map.insert("lexicon".into(), crate::lexicon::provenance_summary());
    // Whether short mic regions were withheld and judged before publication rather than only at the
    // durability boundary (#149 phase 2). Live-only, and gated by the same `echo_drop` switch — a batch
    // note has no publication to be early for.
    map.insert(
        "live_early_drop".into(),
        Value::Bool(live && cleanup.echo_drop),
    );
    // Whether the AEC's per-block record was actually available to the echo pass, not whether it was
    // wanted: `false` means no segment can be deleted as an echo under rules 4.
    map.insert("audio_evidence".into(), Value::Bool(audio_evidence));
    Value::Object(map)
}

/// [`float_value`] for the `f64` cleanup thresholds.
fn double_value(value: f64) -> Value {
    Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or_else(|| Value::String(value.to_string()))
}

/// JSON numbers cannot represent NaN/±inf. Preserve a malformed-but-effective config value as an explicit
/// string rather than making provenance serialization fail after transcription has already succeeded.
fn float_value(value: f32) -> Value {
    Number::from_f64(f64::from(value))
        .map(Value::Number)
        .unwrap_or_else(|| Value::String(value.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_completeness_projection_preserves_intrinsic_quality() {
        use crate::transcribe::RecordingStorageStatus;
        use corti_core::{QualityClass, QualityRecord, QualityResolution};
        let mut source_loss = QualityRecord::observed();
        source_loss
            .record_omitted_spans(QualityClass::SourceLoss, QualityResolution::Outstanding, 1)
            .unwrap();
        let mut processing_loss = QualityRecord::observed();
        processing_loss
            .record_omitted_spans(
                QualityClass::ProcessingLoss,
                QualityResolution::Outstanding,
                1,
            )
            .unwrap();
        let meta = corti_core::RecordingMeta {
            started_at: chrono::Local::now(),
            ended_at: None,
            owning_app: corti_core::OwningApp::unknown(),
            audio_path: "synthetic.wav".into(),
        };
        for (quality, verified, unverified_expected) in [
            (
                QualityRecord::observed(),
                ["complete", "complete"],
                ["unknown", "unknown"],
            ),
            (
                QualityRecord::default(),
                ["unknown", "unknown"],
                ["unknown", "unknown"],
            ),
            (
                source_loss,
                ["incomplete", "incomplete"],
                ["incomplete", "incomplete"],
            ),
            (
                processing_loss,
                ["complete", "incomplete"],
                ["unknown", "incomplete"],
            ),
        ] {
            for storage in [
                None,
                Some(RecordingStorageStatus::AudioDurable),
                Some(RecordingStorageStatus::LegacyUnknown),
                Some(RecordingStorageStatus::InProgress),
                Some(RecordingStorageStatus::Finalizing),
                Some(RecordingStorageStatus::AudioUnverified),
                Some(RecordingStorageStatus::FrameCountMismatch),
                Some(RecordingStorageStatus::InterruptedPublication),
            ] {
                let unverified =
                    storage.is_some_and(|status| status != RecordingStorageStatus::AudioDurable);
                let mut provenance = TranscriptProvenance::legacy_unknown(GenerationMode::Batch);
                attach_quality(&mut provenance, &quality, storage).unwrap();
                assert_eq!(
                    provenance.configuration["quality"],
                    serde_json::to_value(&quality).unwrap(),
                    "storage must not mutate intrinsic quality or checkpoint identity"
                );
                let expected = if unverified {
                    unverified_expected
                } else {
                    verified
                };
                for (field, expected) in ["source_completeness", "text_completeness"]
                    .into_iter()
                    .zip(expected)
                {
                    assert_eq!(provenance.configuration[field], expected);
                }
                let body = quality_body(
                    &meta,
                    &corti_core::DiarizedTranscript::default(),
                    Some(&quality),
                    storage,
                );
                assert_eq!(body.contains("Completion is unverified."), unverified);
                assert_eq!(
                    body.contains("Transcript coverage is partial or unknown."),
                    expected[1] != "complete"
                );
            }
        }
    }

    #[test]
    fn full_capacity_quality_uses_labelled_projection_without_losing_authoritative_facts() {
        use corti_core::{
            CaptureEpoch, CaptureEpochReason, QualityCause, QualityClass, QualityRecord,
            QualityResolution, QualitySpan, QualityStage, QualityTrack,
        };
        let mut quality = QualityRecord::observed();
        for index in 0..64_u32 {
            quality
                .insert_epoch(CaptureEpoch {
                    id: index,
                    source_start_ns: Some(
                        u64::MAX - 64_000_000_000 + u64::from(index) * 1_000_000_000,
                    ),
                    source_rate_hz: u32::MAX,
                    source_frame_base: u64::MAX - 1_000,
                    retained_start_frame: u64::MAX - 1_000 + u64::from(index) * 10,
                    retained_rate_hz: u32::MAX,
                    mic_channels: 1,
                    tap_channels: 1,
                    reason: if index == 0 {
                        CaptureEpochReason::Start
                    } else {
                        CaptureEpochReason::HostDiscontinuity
                    },
                })
                .unwrap();
        }
        for epoch in 0..64_u32 {
            for index in 0..4_u64 {
                let source = index % 2 == 0;
                quality
                    .insert_span(QualitySpan {
                        epoch,
                        start_frame: u64::MAX - 1_000 + index * 2,
                        end_frame: u64::MAX - 999 + index * 2,
                        track: QualityTrack::Both,
                        cause: if source {
                            QualityCause::HostDiscontinuity
                        } else {
                            QualityCause::MicTapMismatch
                        },
                        class: if source {
                            QualityClass::SourceLoss
                        } else {
                            QualityClass::ProcessingLoss
                        },
                        stage: QualityStage::Capture,
                        resolution: if source {
                            QualityResolution::Outstanding
                        } else {
                            QualityResolution::RecoveredByBatch
                        },
                    })
                    .unwrap();
            }
        }
        quality.validate().unwrap();
        let full_bytes = serde_json::to_vec(&quality).unwrap().len();
        assert!(
            full_bytes > 64 * 1024,
            "fixture must exercise the existing prefix cap"
        );
        let mut provenance = TranscriptProvenance::legacy_unknown(GenerationMode::Live);
        attach_quality(
            &mut provenance,
            &quality,
            Some(crate::transcribe::RecordingStorageStatus::AudioDurable),
        )
        .unwrap();
        let projected: QualityRecord =
            serde_json::from_value(provenance.configuration["quality"].clone()).unwrap();
        assert_eq!(projected.epochs, quality.epochs);
        assert!(projected.spans.len() < quality.spans.len());
        let projected_bytes = serde_json::to_vec(&projected).unwrap().len();
        assert!(projected_bytes <= QUALITY_FRONTMATTER_BYTES);
        assert_eq!(
            provenance.configuration["quality_projection_truncated"],
            true
        );
        assert_eq!(provenance.configuration["quality_full_span_count"], 256);
        assert_eq!(
            projected.spans.len() as u64 + projected.overflow.omitted_spans.iter().sum::<u64>(),
            256
        );
        assert_eq!(
            projected.overflow.outstanding_omitted_spans[1], 0,
            "recovered processing history stays recovered"
        );
        assert!(projected.overflow.outstanding_omitted_spans[0] > 0);
        assert_eq!(
            provenance.configuration["source_completeness"],
            serde_json::to_value(quality.source_completeness()).unwrap()
        );

        let dir =
            std::env::temp_dir().join(format!("corti-full-quality-prefix-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let note = dir.join("note.md");
        let body = "\n# Synthetic\n\nState: transcribed \n\nSynthetic body unchanged.\n";
        let fresh = format!(
            "---\nsource: synthetic\n{}---\n{body}",
            provenance.frontmatter_line().unwrap()
        );
        assert!(fresh.len() - body.len() < 64 * 1024);
        std::fs::write(&note, &fresh).unwrap();
        let current = corti_vagus::note::CurrentNote::from_returned_path(note.clone()).unwrap();
        current
            .refresh_provenance(&TranscriptProvenance::legacy_unknown(GenerationMode::Live))
            .unwrap();
        current.refresh_provenance(&provenance).unwrap();
        assert_eq!(std::fs::read_to_string(&note).unwrap(), fresh);

        let audio = dir.join("recording.wav");
        let mut checkpoint = crate::checkpoint::FilingCheckpoint::new(
            corti_core::DiarizedTranscript::new(Vec::new()),
            None,
            None,
        );
        checkpoint.quality = Some(quality.clone());
        checkpoint.quality_storage = Some(crate::transcribe::RecordingStorageStatus::AudioDurable);
        checkpoint.set_provenance(provenance);
        checkpoint.store(&audio).unwrap();
        let mut restored = crate::checkpoint::FilingCheckpoint::load(&audio).unwrap();
        assert_eq!(restored.quality.as_ref(), Some(&quality));
        restored
            .provenance
            .configuration
            .insert("quality_projection_truncated".into(), Value::Bool(false));
        assert!(restored.store(&audio).is_err());
        let meta = corti_core::RecordingMeta {
            started_at: chrono::Local::now(),
            ended_at: None,
            owning_app: corti_core::OwningApp::from_bundle_id("synthetic.fixture"),
            audio_path: audio,
        };
        let marked = quality_body(
            &meta,
            &checkpoint.transcript,
            Some(&quality),
            checkpoint.quality_storage,
        );
        assert_eq!(marked.matches("> Audio/text availability:").count(), 256);
        println!(
            "quality prefix fixture: full_bytes={full_bytes} projected_bytes={projected_bytes} full_spans=256 epochs=64"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn source_epoch_display_preserves_words_and_marks_every_loss_and_overflow() {
        use corti_core::{
            CaptureEpoch, CaptureEpochReason, DiarizedTranscript, OwningApp, QualityCause,
            QualityClass, QualityRecord, QualityResolution, QualitySpan, QualityStage,
            QualityTrack, RecordingMeta, Speaker, TranscriptSegment,
        };
        let mut quality = QualityRecord::observed();
        quality
            .insert_epoch(CaptureEpoch {
                id: 0,
                source_start_ns: Some(0),
                source_rate_hz: 16_000,
                source_frame_base: 0,
                retained_start_frame: 0,
                retained_rate_hz: 16_000,
                mic_channels: 1,
                tap_channels: 1,
                reason: CaptureEpochReason::Start,
            })
            .unwrap();
        quality
            .insert_span(QualitySpan {
                epoch: 0,
                start_frame: 4_000,
                end_frame: 8_000,
                track: QualityTrack::Both,
                cause: QualityCause::RingOverflow,
                class: QualityClass::SourceLoss,
                stage: QualityStage::Capture,
                resolution: QualityResolution::Outstanding,
            })
            .unwrap();
        quality
            .insert_epoch(CaptureEpoch {
                id: 1,
                source_start_ns: Some(90_000_000_000),
                source_rate_hz: 16_000,
                source_frame_base: 0,
                retained_start_frame: 16_000,
                retained_rate_hz: 16_000,
                mic_channels: 1,
                tap_channels: 1,
                reason: CaptureEpochReason::HostDiscontinuity,
            })
            .unwrap();
        quality
            .record_omitted_spans(QualityClass::SourceLoss, QualityResolution::Outstanding, 2)
            .unwrap();
        let transcript = DiarizedTranscript::new(vec![
            TranscriptSegment {
                speaker: Speaker::Me,
                start: 0.8,
                end: 1.2,
                text: "Synthetic crossing once.".into(),
            },
            TranscriptSegment {
                speaker: Speaker::Me,
                start: 1.5,
                end: 1.9,
                text: "Synthetic later once.".into(),
            },
        ]);
        assert_eq!(source_endpoint(&quality, 1.5, false), Some((90.5, 1)));
        assert_eq!(source_endpoint(&quality, 1.0, true), Some((1.0, 0)));
        assert_eq!(source_endpoint(&quality, 1.0, false), Some((90.0, 1)));
        let meta = RecordingMeta {
            started_at: chrono::Local::now(),
            ended_at: None,
            owning_app: OwningApp::from_bundle_id("synthetic.fixture"),
            audio_path: "synthetic.wav".into(),
        };
        let body = quality_body(
            &meta,
            &transcript,
            Some(&quality),
            Some(crate::transcribe::RecordingStorageStatus::AudioDurable),
        );
        assert_eq!(body.matches("Synthetic crossing once.").count(), 1);
        assert_eq!(body.matches("Synthetic later once.").count(), 1);
        assert!(
            body.contains("**[01:30]"),
            "later turn must retain its source time"
        );
        assert!(body.contains("ring_overflow at 0.250–0.500 s"));
        assert!(body.contains("Unfilled capture epoch 1: source offset 90.000 s"));
        assert!(
            body.contains(
                "Quality accounting overflow: source/processing/degraded spans [2, 0, 0]"
            )
        );
        assert!(body.contains("1 transcript turn(s) cross an epoch boundary"));
        assert!(body.contains("--redo <id> --local"));
        assert_eq!(
            transcript.segments[1].start, 1.5,
            "retained canonical timings are immutable"
        );
    }

    #[test]
    fn aws_batch_provenance_is_secret_free_and_names_the_opaque_service_model() {
        let cfg = AppConfig {
            transcribe_backend: BackendChoice::Aws,
            aws_bucket: Some("private-bucket".into()),
            aws_profile: Some("secret-profile".into()),
            aws_region: Some("us-secret-1".into()),
            language: "en-GB".into(),
            ..AppConfig::default()
        };
        let provenance = from_config(&cfg, GenerationMode::Batch);
        let json = provenance.frontmatter_json().unwrap();
        assert_eq!(provenance.mode, GenerationMode::Batch);
        assert_eq!(provenance.backend, "aws");
        assert_eq!(provenance.models.asr.id, "aws/transcribe-default");
        assert_eq!(provenance.configuration["language"], "en-GB");
        assert_eq!(provenance.configuration["input"], "completed_recording");
        assert_eq!(provenance.configuration["segment_cleanup"]["rules"], 4);
        assert_eq!(
            provenance.configuration["segment_cleanup"]["echo_policy"],
            "acoustic_required_mic_only"
        );
        assert_eq!(
            provenance.configuration["segment_cleanup"]["text_only_duplicates"],
            "keep_suspected"
        );
        assert_eq!(
            provenance.configuration["segment_cleanup"]["critical_text_protection"],
            true
        );
        assert_eq!(
            provenance.configuration["segment_cleanup"]["strip_fillers"],
            true
        );
        assert!(
            provenance.configuration["segment_cleanup"]
                .get("lexicon")
                .is_some(),
            "the lexicon is always described, even when absent"
        );
        assert_eq!(
            provenance.configuration["segment_cleanup"]["echo_containment"],
            0.7
        );
        assert_eq!(
            provenance.configuration["segment_cleanup"]["echo_audio_margin_db"],
            3.0
        );
        assert_eq!(
            provenance.configuration["segment_cleanup"]["live_early_drop"], false,
            "a completed recording has no live publication to be early for"
        );
        // AWS has no canceller, so a batch note filed from it can never carry audio evidence.
        assert_eq!(
            provenance.configuration["segment_cleanup"]["audio_evidence"],
            false
        );
        assert!(!json.contains("private-bucket"));
        assert!(!json.contains("secret-profile"));
        assert!(!json.contains("us-secret-1"));
    }

    #[cfg(feature = "local")]
    #[test]
    fn live_local_provenance_names_models_representation_and_effective_settings() {
        let cfg = AppConfig {
            transcribe_backend: BackendChoice::Local,
            local_asr_engine: "ggml".into(),
            local_ggml_model: Some("/private/models/custom-parakeet.gguf".into()),
            local_threads: 6,
            local_diarize_far_end: true,
            local_embedding_model: "wespeaker-resnet34".into(),
            local_diarize_threshold: 0.65,
            live_buffer_minutes: 3,
            ..AppConfig::default()
        };
        let provenance = from_config(&cfg, GenerationMode::Live);
        let json = provenance.frontmatter_json().unwrap();
        assert_eq!(provenance.mode, GenerationMode::Live);
        assert_eq!(provenance.backend, "local");
        assert_eq!(provenance.models.asr.id, "nvidia/parakeet-tdt-0.6b-v3");
        assert_eq!(
            provenance.models.asr.artifact.as_deref(),
            Some("custom-parakeet.gguf")
        );
        assert_eq!(
            provenance.models.speaker_embedding.as_ref().unwrap().id,
            "wespeaker-resnet34"
        );
        assert_eq!(provenance.configuration["asr_engine"], "ggml");
        assert_eq!(provenance.configuration["threads"], 6);
        assert_eq!(provenance.configuration["live_buffer_minutes"], 3);
        assert_eq!(provenance.configuration["input"], "live_pcm_stream");
        assert_eq!(provenance.configuration["aec"]["mode"], "streaming");
        assert_eq!(provenance.configuration["segment_cleanup"]["rules"], 4);
        assert_eq!(
            provenance.configuration["segment_cleanup"]["echo_policy"],
            "acoustic_required_mic_only"
        );
        assert_eq!(
            provenance.configuration["segment_cleanup"]["text_only_duplicates"],
            "keep_suspected"
        );
        assert_eq!(
            provenance.configuration["segment_cleanup"]["critical_text_protection"],
            true
        );
        assert_eq!(
            provenance.configuration["segment_cleanup"]["live_early_drop"],
            true
        );
        // The live engine runs its own canceller alongside the transcriber, so its cleanup always has the
        // per-block record — unless AEC is off, in which case there is nothing to record.
        assert_eq!(
            provenance.configuration["segment_cleanup"]["audio_evidence"],
            true
        );
        let no_aec = AppConfig {
            aec_enabled: false,
            ..cfg.clone()
        };
        assert_eq!(
            from_config(&no_aec, GenerationMode::Live).configuration["segment_cleanup"]["audio_evidence"],
            false
        );
        assert!(
            !json.contains("/private/models"),
            "absolute path leaked: {json}"
        );
    }

    #[test]
    fn capture_provenance_uses_persisted_settings_and_reports_degradation() {
        let cfg = AppConfig::default();
        let mut captured = cfg.aec_config();
        captured.filter_len = 4_096;
        let processing = corti_capture::CaptureProcessing {
            schema_version: corti_capture::CAPTURE_PROCESSING_SCHEMA_VERSION,
            quality: None,
            aec: corti_capture::CaptureAecState::Degraded {
                config: captured,
                lookahead_seconds: 7.5,
            },
        };
        let provenance = from_config_with_aec(
            &cfg,
            GenerationMode::Batch,
            AecExecution::Capture(&processing),
            false,
        );
        assert_eq!(provenance.configuration["aec"]["mode"], "capture_streaming");
        assert_eq!(provenance.configuration["aec"]["outcome"], "degraded_mixed");
        assert_eq!(provenance.configuration["aec"]["filter_len"], 4_096);
        assert_eq!(provenance.configuration["aec"]["lookahead_seconds"], 7.5);
    }

    #[test]
    fn offline_provenance_distinguishes_applied_not_applicable_and_failed() {
        let cfg = AppConfig::default();
        let aec = cfg.aec_config();
        for (outcome, expected_mode, expected_outcome) in [
            (
                crate::transcribe::OfflineAecOutcome::Applied,
                "offline",
                "applied_far_reference_unknown",
            ),
            (
                crate::transcribe::OfflineAecOutcome::NotApplicable,
                "not_applicable",
                "not_applicable",
            ),
            (
                crate::transcribe::OfflineAecOutcome::Failed,
                "offline",
                "failed_raw_fallback",
            ),
        ] {
            let provenance = from_config_with_aec(
                &cfg,
                GenerationMode::Batch,
                AecExecution::Offline {
                    config: &aec,
                    lookahead_seconds: 5.0,
                    outcome,
                    capture_fallback: false,
                },
                false,
            );
            assert_eq!(provenance.configuration["aec"]["mode"], expected_mode);
            assert_eq!(provenance.configuration["aec"]["outcome"], expected_outcome);
        }
    }

    fn activity_quality(
        scope: corti_core::ActivityScope,
        reference: corti_core::FarReference,
    ) -> corti_core::QualityRecord {
        use corti_core::{
            ActivityBlock, ActivityInput, CaptureEpoch, CaptureEpochReason, ChannelActivity,
            FarReference, QualityRecord,
        };
        let mut quality = QualityRecord::observed();
        if reference == FarReference::Unknown {
            return quality;
        }
        let epoch = CaptureEpoch {
            id: 0,
            source_start_ns: Some(0),
            source_rate_hz: 8,
            source_frame_base: 0,
            retained_start_frame: 0,
            retained_rate_hz: 8,
            mic_channels: 1,
            tap_channels: 1,
            reason: CaptureEpochReason::Start,
        };
        quality.insert_epoch(epoch).unwrap();
        let mic = [0.01; 48];
        let mut tap = [0.0; 48];
        if reference == FarReference::Present {
            tap.fill(0.1);
        }
        if reference == FarReference::Intermittent {
            tap[0] = 0.1;
        }
        let mut activity =
            ChannelActivity::new(scope, ActivityInput::new([7; 32], 8).unwrap()).unwrap();
        activity
            .push(
                ActivityBlock {
                    epoch,
                    retained_start: 0,
                    mic: &mic,
                    tap: &tap,
                    unavailable: Some(&[]),
                },
                &mut quality,
            )
            .unwrap();
        activity.finish(Some(48), &mut quality).unwrap();
        assert_eq!(quality.activity.get(scope).far_reference(), reference);
        quality
    }

    #[test]
    fn every_aec_execution_arm_keeps_disposition_and_projects_only_its_input_scope() {
        use crate::transcribe::{OfflineAecOutcome, RecordingStorageStatus};
        use corti_capture::CaptureAecState;
        use corti_core::{ActivityScope, FarReference};
        let cfg = AppConfig::default();
        let config = cfg.aec_config();
        for (reference, label, applied) in [
            (
                FarReference::Unknown,
                "unknown",
                "applied_far_reference_unknown",
            ),
            (FarReference::Present, "present", "applied"),
            (
                FarReference::Intermittent,
                "intermittent",
                "applied_with_intermittent_far_reference",
            ),
            (FarReference::Absent, "absent", "no_far_reference"),
        ] {
            for (state, disposition) in [
                (CaptureAecState::Disabled, "disabled"),
                (CaptureAecState::NotApplicable, "not_applicable"),
                (
                    CaptureAecState::Applied {
                        config: config.clone(),
                        lookahead_seconds: 7.5,
                    },
                    "applied",
                ),
                (
                    CaptureAecState::RawFallback {
                        config: config.clone(),
                        lookahead_seconds: 7.5,
                    },
                    "raw_fallback",
                ),
                (
                    CaptureAecState::Degraded {
                        config: config.clone(),
                        lookahead_seconds: 7.5,
                    },
                    "degraded_mixed",
                ),
            ] {
                let mut quality = activity_quality(ActivityScope::Capture, reference);
                quality
                    .merge_snapshot(&activity_quality(
                        ActivityScope::Offline,
                        FarReference::Present,
                    ))
                    .unwrap();
                let processing = corti_capture::CaptureProcessing {
                    schema_version: corti_capture::CAPTURE_PROCESSING_SCHEMA_VERSION,
                    quality: Some(quality.clone()),
                    aec: state,
                };
                let mut provenance = from_config_with_aec(
                    &cfg,
                    GenerationMode::Batch,
                    AecExecution::Capture(&processing),
                    false,
                );
                attach_quality(
                    &mut provenance,
                    &quality,
                    Some(RecordingStorageStatus::AudioDurable),
                )
                .unwrap();
                let aec = &provenance.configuration["aec"];
                assert_eq!(aec["execution_outcome"], disposition);
                assert_eq!(
                    aec["outcome"],
                    if disposition == "applied" {
                        applied
                    } else {
                        disposition
                    }
                );
                assert_eq!(aec["far_reference"], label);
                assert_eq!(aec["observation_scope"], "capture");
                assert_eq!(
                    aec["suspected_silent_far_reference"],
                    quality.activity.get(ActivityScope::Capture).suspected()
                );
                assert!(!quality.requires_batch_recovery());
                assert_eq!(provenance.configuration["text_completeness"], "complete");
            }
            let mut quality = activity_quality(ActivityScope::Offline, reference);
            quality
                .merge_snapshot(&activity_quality(
                    ActivityScope::Capture,
                    FarReference::Present,
                ))
                .unwrap();
            for (outcome, disposition) in [
                (OfflineAecOutcome::Applied, "applied"),
                (OfflineAecOutcome::NotApplicable, "not_applicable"),
                (OfflineAecOutcome::Failed, "failed_raw_fallback"),
                (OfflineAecOutcome::NotRequested, "disabled"),
            ] {
                let mut provenance = from_config_with_aec(
                    &cfg,
                    GenerationMode::Batch,
                    AecExecution::Offline {
                        config: &config,
                        lookahead_seconds: 7.5,
                        outcome,
                        capture_fallback: true,
                    },
                    false,
                );
                attach_quality(&mut provenance, &quality, None).unwrap();
                assert_eq!(
                    provenance.configuration["aec"]["execution_outcome"],
                    disposition
                );
                assert_eq!(
                    provenance.configuration["aec"]["outcome"],
                    if disposition == "applied" {
                        applied
                    } else {
                        disposition
                    }
                );
                assert_eq!(provenance.configuration["aec"]["far_reference"], label);
                assert_eq!(
                    provenance.configuration["aec"]["observation_scope"],
                    "offline"
                );
            }
            for (execution, disposition) in [
                (AecExecution::Disabled, "disabled"),
                (AecExecution::PreprocessedUnknown, "unknown"),
            ] {
                let mut provenance =
                    from_config_with_aec(&cfg, GenerationMode::Batch, execution, false);
                attach_quality(&mut provenance, &quality, None).unwrap();
                assert_eq!(provenance.configuration["aec"]["outcome"], disposition);
                assert_eq!(provenance.configuration["aec"]["far_reference"], "unknown");
            }
        }
    }

    #[test]
    fn live_activity_projection_requires_actual_execution_and_keeps_short_lookahead() {
        use corti_core::{ActivityScope, FarReference};
        let cfg = AppConfig::default();
        let quality = activity_quality(ActivityScope::Live, FarReference::Absent);
        let mut provenance =
            from_config_with_aec(&cfg, GenerationMode::Live, AecExecution::Live, false);
        attach_quality(&mut provenance, &quality, None).unwrap();
        assert_eq!(provenance.configuration["aec"]["outcome"], "pending");
        assert_eq!(
            provenance.configuration["aec"]["lookahead_seconds"],
            float_value(0.1)
        );
        observe_live_aec_execution(&mut provenance, false, false);
        attach_quality(&mut provenance, &quality, None).unwrap();
        assert_eq!(provenance.configuration["aec"]["outcome"], "not_applicable");
        observe_live_aec_execution(&mut provenance, true, true);
        attach_quality(&mut provenance, &quality, None).unwrap();
        assert_eq!(
            provenance.configuration["aec"]["execution_outcome"],
            "applied"
        );
        assert_eq!(
            provenance.configuration["aec"]["outcome"],
            "no_far_reference"
        );
        let disabled = AppConfig {
            aec_enabled: false,
            ..cfg
        };
        let mut provenance =
            from_config_with_aec(&disabled, GenerationMode::Live, AecExecution::Live, false);
        observe_live_aec_execution(&mut provenance, true, true);
        attach_quality(&mut provenance, &quality, None).unwrap();
        assert_eq!(provenance.configuration["aec"]["outcome"], "disabled");
    }

    #[test]
    fn legacy_aec_disposition_survives_refresh_without_borrowing_new_capture_observations() {
        use corti_core::{ActivityScope, FarReference};
        let quality = activity_quality(ActivityScope::Capture, FarReference::Absent);
        for (prior, expected) in [
            ("applied", "applied_far_reference_unknown"),
            ("raw_fallback", "raw_fallback"),
            ("degraded_mixed", "degraded_mixed"),
            ("disabled", "disabled"),
        ] {
            let mut provenance = TranscriptProvenance::legacy_unknown(GenerationMode::Batch);
            provenance.configuration.insert(
                "aec".into(),
                serde_json::json!({"enabled":true,"mode":"offline","outcome":prior}),
            );
            attach_quality(&mut provenance, &quality, None).unwrap();
            assert_eq!(provenance.configuration["aec"]["execution_outcome"], prior);
            assert_eq!(provenance.configuration["aec"]["outcome"], expected);
            assert_eq!(provenance.configuration["aec"]["far_reference"], "unknown");
            assert_eq!(
                provenance.configuration["aec"]["suspected_silent_far_reference"],
                false
            );
            let once = provenance.clone();
            attach_quality(&mut provenance, &quality, None).unwrap();
            assert_eq!(provenance, once);
        }
    }

    /// A `[cleanup]` table that switches every pass off is recorded as `"off"` rather than as a set of
    /// inert thresholds, and a hand-edited threshold is clamped before it reaches the note.
    #[test]
    fn disabled_cleanup_is_recorded_as_off_and_nonsense_thresholds_are_clamped() {
        let mut cfg = AppConfig::default();
        cfg.cleanup.echo_drop = false;
        cfg.cleanup.drop_backchannels = false;
        cfg.cleanup.merge_gap_seconds = 0.0;
        cfg.cleanup.strip_fillers = false;
        let provenance = from_config(&cfg, GenerationMode::Batch);
        assert_eq!(provenance.configuration["segment_cleanup"], "off");

        // The live early drop is the echo rule, moved earlier: switching the echo pass off switches it off.
        let mut cfg = AppConfig::default();
        cfg.cleanup.echo_drop = false;
        let provenance = from_config(&cfg, GenerationMode::Live);
        assert_eq!(
            provenance.configuration["segment_cleanup"]["live_early_drop"],
            false
        );

        let mut cfg = AppConfig::default();
        cfg.cleanup.echo_containment = 9.0;
        cfg.cleanup.echo_window_seconds = f64::NAN;
        let provenance = from_config(&cfg, GenerationMode::Batch);
        assert_eq!(
            provenance.configuration["segment_cleanup"]["echo_containment"],
            1.0
        );
        assert_eq!(
            provenance.configuration["segment_cleanup"]["echo_window_seconds"],
            6.0
        );
        provenance.frontmatter_json().unwrap();
    }

    #[test]
    fn nonfinite_app_config_falls_back_before_provenance_serialization() {
        let cfg = AppConfig {
            aec_mu: Some(f32::NAN),
            ..AppConfig::default()
        };
        let provenance = from_config(&cfg, GenerationMode::Batch);
        let mu = provenance.configuration["aec"]["mu"].as_f64().unwrap();
        assert!((mu - 0.3).abs() < 1e-6);
        provenance.frontmatter_json().unwrap();
    }
}
