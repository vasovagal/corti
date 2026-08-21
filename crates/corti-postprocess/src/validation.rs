use std::{
    collections::{HashMap, HashSet},
    fmt,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{RowId, TranscriptRow};

pub const REWRITE_SCHEMA_VERSION: u32 = 1;
pub const QUESTION_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Replacement {
    pub row_id: RowId,
    pub text: String,
}

impl fmt::Debug for Replacement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Replacement")
            .field("row_id", &self.row_id)
            .field("text_bytes", &self.text.len())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RewriteOutput {
    pub schema: u32,
    pub replacements: Vec<Replacement>,
}

impl fmt::Debug for RewriteOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RewriteOutput")
            .field("schema", &self.schema)
            .field("replacement_count", &self.replacements.len())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionOutput {
    pub schema: u32,
    pub answer: String,
    pub cited_row_ids: Vec<RowId>,
    pub context_truncated: bool,
}

impl fmt::Debug for QuestionOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuestionOutput")
            .field("schema", &self.schema)
            .field("answer_bytes", &self.answer.len())
            .field("citation_count", &self.cited_row_ids.len())
            .field("context_truncated", &self.context_truncated)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RewriteValidationLimits {
    pub request_max_output_bytes: usize,
    pub catalog_max_output_bytes: usize,
}

impl RewriteValidationLimits {
    pub const fn effective_max_output_bytes(self) -> usize {
        if self.request_max_output_bytes < self.catalog_max_output_bytes {
            self.request_max_output_bytes
        } else {
            self.catalog_max_output_bytes
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ValidationError {
    #[error("output is not valid strict JSON/UTF-8")]
    MalformedJson,
    #[error("output schema version is unsupported")]
    UnsupportedSchema,
    #[error("request target ids are invalid or duplicated")]
    InvalidTargets,
    #[error("replacement references an unknown row id")]
    UnknownRow,
    #[error("replacement or citation contains a duplicate row id")]
    DuplicateRow,
    #[error("replacements reorder target rows")]
    ReorderedRows,
    #[error("replacement text is empty for a non-empty source row")]
    EmptyReplacement,
    #[error("output contains a control or bidirectional-control character")]
    ControlCharacter,
    #[error("output contains markup rather than plain text")]
    Markup,
    #[error("replacement exceeds its per-row expansion limit")]
    ExpansionLimit,
    #[error("aggregate output exceeds request or catalog bounds")]
    AggregateLimit,
    #[error("size arithmetic overflowed")]
    SizeOverflow,
    #[error("question answer is empty")]
    EmptyAnswer,
    #[error("question truncation marker does not match supplied context")]
    TruncationMismatch,
    #[error("final chunk target sets are not exact and disjoint")]
    NonDisjointTargets,
}

/// A rewrite that has passed all-or-nothing validation against one immutable target set.
#[derive(Clone, PartialEq, Eq)]
pub struct ValidatedRewrite {
    replacements: Vec<Replacement>,
    target_snapshot: Vec<TranscriptRow>,
}

impl fmt::Debug for ValidatedRewrite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ValidatedRewrite")
            .field("replacement_count", &self.replacements.len())
            .field("target_count", &self.target_snapshot.len())
            .finish()
    }
}

impl ValidatedRewrite {
    pub fn replacements(&self) -> &[Replacement] {
        &self.replacements
    }

    /// Apply validated replacements to a copy of the same target rows, preserving all non-text fields.
    pub fn apply_to(
        &self,
        targets: &[TranscriptRow],
    ) -> Result<Vec<TranscriptRow>, ValidationError> {
        // Fence checks remain mandatory at the coordinator, and this exact snapshot check prevents a
        // validated replacement from being accidentally applied to changed row content/metadata.
        if targets != self.target_snapshot {
            return Err(ValidationError::InvalidTargets);
        }
        let replacements: HashMap<&RowId, &str> = self
            .replacements
            .iter()
            .map(|replacement| (&replacement.row_id, replacement.text.as_str()))
            .collect();
        Ok(targets
            .iter()
            .cloned()
            .map(|mut row| {
                if let Some(text) = replacements.get(&row.row_id) {
                    row.text = (*text).to_owned();
                }
                row
            })
            .collect())
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ValidatedQuestion {
    answer: String,
    cited_row_ids: Vec<RowId>,
    context_truncated: bool,
}

impl fmt::Debug for ValidatedQuestion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ValidatedQuestion")
            .field("answer_bytes", &self.answer.len())
            .field("citation_count", &self.cited_row_ids.len())
            .field("context_truncated", &self.context_truncated)
            .finish()
    }
}

impl ValidatedQuestion {
    pub fn answer(&self) -> &str {
        &self.answer
    }

    pub fn cited_row_ids(&self) -> &[RowId] {
        &self.cited_row_ids
    }

    pub const fn context_truncated(&self) -> bool {
        self.context_truncated
    }
}

pub fn parse_and_validate_rewrite(
    bytes: &[u8],
    targets: &[TranscriptRow],
    limits: RewriteValidationLimits,
) -> Result<ValidatedRewrite, ValidationError> {
    let output: RewriteOutput =
        serde_json::from_slice(bytes).map_err(|_| ValidationError::MalformedJson)?;
    validate_rewrite(output, targets, limits)
}

fn validate_rewrite(
    output: RewriteOutput,
    targets: &[TranscriptRow],
    limits: RewriteValidationLimits,
) -> Result<ValidatedRewrite, ValidationError> {
    if output.schema != REWRITE_SCHEMA_VERSION {
        return Err(ValidationError::UnsupportedSchema);
    }

    let mut target_indices = HashMap::with_capacity(targets.len());
    for (index, target) in targets.iter().enumerate() {
        if target.end_ms < target.start_ms || target_indices.insert(&target.row_id, index).is_some()
        {
            return Err(ValidationError::InvalidTargets);
        }
    }

    let mut seen = HashSet::with_capacity(output.replacements.len());
    let mut previous_index = None;
    let mut aggregate_bytes = 0usize;
    for replacement in &output.replacements {
        let Some(&target_index) = target_indices.get(&replacement.row_id) else {
            return Err(ValidationError::UnknownRow);
        };
        if !seen.insert(&replacement.row_id) {
            return Err(ValidationError::DuplicateRow);
        }
        if previous_index.is_some_and(|previous| target_index <= previous) {
            return Err(ValidationError::ReorderedRows);
        }
        previous_index = Some(target_index);

        let raw = &targets[target_index].text;
        if !raw.is_empty() && replacement.text.trim().is_empty() {
            return Err(ValidationError::EmptyReplacement);
        }
        validate_plain_text(&replacement.text)?;
        if contains_markup(&replacement.text) {
            return Err(ValidationError::Markup);
        }

        let multiplied = raw
            .len()
            .checked_mul(3)
            .ok_or(ValidationError::SizeOverflow)?;
        let proportional = multiplied / 2;
        let additive = raw
            .len()
            .checked_add(256)
            .ok_or(ValidationError::SizeOverflow)?;
        let row_limit = proportional.max(additive);
        if replacement.text.len() > row_limit {
            return Err(ValidationError::ExpansionLimit);
        }
        aggregate_bytes = aggregate_bytes
            .checked_add(replacement.text.len())
            .ok_or(ValidationError::SizeOverflow)?;
    }
    if aggregate_bytes > limits.effective_max_output_bytes() {
        return Err(ValidationError::AggregateLimit);
    }

    Ok(ValidatedRewrite {
        replacements: output.replacements,
        target_snapshot: targets.to_vec(),
    })
}

pub fn parse_and_validate_question(
    bytes: &[u8],
    context: &[TranscriptRow],
    expected_context_truncated: bool,
    max_answer_bytes: usize,
) -> Result<ValidatedQuestion, ValidationError> {
    let output: QuestionOutput =
        serde_json::from_slice(bytes).map_err(|_| ValidationError::MalformedJson)?;
    if output.schema != QUESTION_SCHEMA_VERSION {
        return Err(ValidationError::UnsupportedSchema);
    }
    if output.answer.trim().is_empty() {
        return Err(ValidationError::EmptyAnswer);
    }
    if output.answer.len() > max_answer_bytes {
        return Err(ValidationError::AggregateLimit);
    }
    validate_plain_text(&output.answer)?;
    if contains_markup(&output.answer) {
        return Err(ValidationError::Markup);
    }
    if output.context_truncated != expected_context_truncated {
        return Err(ValidationError::TruncationMismatch);
    }

    let mut context_ids = HashSet::with_capacity(context.len());
    for row in context {
        if row.end_ms < row.start_ms || !context_ids.insert(&row.row_id) {
            return Err(ValidationError::InvalidTargets);
        }
    }
    let mut citations = HashSet::with_capacity(output.cited_row_ids.len());
    for citation in &output.cited_row_ids {
        if !context_ids.contains(citation) {
            return Err(ValidationError::UnknownRow);
        }
        if !citations.insert(citation) {
            return Err(ValidationError::DuplicateRow);
        }
    }

    Ok(ValidatedQuestion {
        answer: output.answer,
        cited_row_ids: output.cited_row_ids,
        context_truncated: output.context_truncated,
    })
}

/// Verify that final chunks cover every expected target exactly once and contain no other target.
pub fn validate_disjoint_target_chunks(
    expected: &[RowId],
    chunks: &[Vec<RowId>],
) -> Result<(), ValidationError> {
    let expected_set: HashSet<&RowId> = expected.iter().collect();
    if expected_set.len() != expected.len() {
        return Err(ValidationError::InvalidTargets);
    }
    let mut seen = HashSet::with_capacity(expected.len());
    for row_id in chunks.iter().flatten() {
        if !expected_set.contains(row_id) || !seen.insert(row_id) {
            return Err(ValidationError::NonDisjointTargets);
        }
    }
    if seen.len() != expected.len() {
        return Err(ValidationError::NonDisjointTargets);
    }
    Ok(())
}

fn validate_plain_text(text: &str) -> Result<(), ValidationError> {
    if text
        .chars()
        .any(|ch| ch.is_control() || is_bidi_control(ch))
    {
        return Err(ValidationError::ControlCharacter);
    }
    Ok(())
}

fn is_bidi_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

fn contains_markup(text: &str) -> bool {
    if text.contains('`')
        || text.contains("**")
        || text.contains("__")
        || text.contains("~~")
        || text.contains("](")
        || text.contains("![")
    {
        return true;
    }
    let trimmed = text.trim_start();
    if ["# ", "> ", "- ", "* ", "+ "]
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
        || trimmed.split_once(". ").is_some_and(|(prefix, _)| {
            !prefix.is_empty() && prefix.bytes().all(|b| b.is_ascii_digit())
        })
    {
        return true;
    }

    let bytes = text.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'<' {
            continue;
        }
        let Some(next) = bytes.get(index + 1) else {
            continue;
        };
        if (*next == b'/' || *next == b'!' || *next == b'?' || next.is_ascii_alphabetic())
            && bytes[index + 1..].contains(&b'>')
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, text: &str) -> TranscriptRow {
        TranscriptRow {
            row_id: RowId::new(id).unwrap(),
            speaker: "Speaker A".into(),
            start_ms: 1,
            end_ms: 2,
            text: text.into(),
        }
    }

    fn limits() -> RewriteValidationLimits {
        RewriteValidationLimits {
            request_max_output_bytes: 4_096,
            catalog_max_output_bytes: 8_192,
        }
    }

    #[test]
    fn valid_rewrite_may_omit_unchanged_rows_and_preserves_metadata() {
        let targets = [row("r-1", "Raw one."), row("r-2", "Raw two.")];
        let bytes = br#"{"schema":1,"replacements":[{"row_id":"r-2","text":"Clean two."}]}"#;
        let validated = parse_and_validate_rewrite(bytes, &targets, limits()).unwrap();
        let applied = validated.apply_to(&targets).unwrap();
        assert_eq!(applied[0].text, "Raw one.");
        assert_eq!(applied[1].text, "Clean two.");
        assert_eq!(applied[1].speaker, targets[1].speaker);
        assert_eq!(applied[1].start_ms, targets[1].start_ms);

        let mut stale_targets = targets.to_vec();
        stale_targets[1].text = "Changed after validation.".into();
        assert_eq!(
            validated.apply_to(&stale_targets),
            Err(ValidationError::InvalidTargets)
        );
    }

    #[test]
    fn rewrite_rejects_unknown_duplicate_reordered_and_extra_fields() {
        let targets = [row("r-1", "one"), row("r-2", "two")];
        let cases: &[(&[u8], ValidationError)] = &[
            (
                br#"{"schema":1,"replacements":[{"row_id":"r-3","text":"x"}]}"#,
                ValidationError::UnknownRow,
            ),
            (
                br#"{"schema":1,"replacements":[{"row_id":"r-1","text":"x"},{"row_id":"r-1","text":"y"}]}"#,
                ValidationError::DuplicateRow,
            ),
            (
                br#"{"schema":1,"replacements":[{"row_id":"r-2","text":"x"},{"row_id":"r-1","text":"y"}]}"#,
                ValidationError::ReorderedRows,
            ),
            (
                br#"{"schema":1,"replacements":[],"provider_body":"forbidden"}"#,
                ValidationError::MalformedJson,
            ),
        ];
        for (bytes, expected) in cases {
            assert_eq!(
                parse_and_validate_rewrite(bytes, &targets, limits()),
                Err(*expected)
            );
        }
    }

    #[test]
    fn rewrite_rejects_markup_controls_and_expansion() {
        let targets = [row("r-1", "x")];
        for text in ["<b>changed</b>", "**changed**", "changed\nforged"] {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "schema": 1,
                "replacements": [{"row_id": "r-1", "text": text}],
            }))
            .unwrap();
            assert!(parse_and_validate_rewrite(&bytes, &targets, limits()).is_err());
        }

        let too_large = "z".repeat(258);
        let bytes = serde_json::to_vec(&serde_json::json!({
            "schema": 1,
            "replacements": [{"row_id": "r-1", "text": too_large}],
        }))
        .unwrap();
        assert_eq!(
            parse_and_validate_rewrite(&bytes, &targets, limits()),
            Err(ValidationError::ExpansionLimit)
        );
    }

    #[test]
    fn invalid_utf8_and_partial_results_never_validate() {
        let targets = [row("r-1", "one")];
        assert_eq!(
            parse_and_validate_rewrite(&[0xff, 0xfe], &targets, limits()),
            Err(ValidationError::MalformedJson)
        );
        let aggregate_limits = RewriteValidationLimits {
            request_max_output_bytes: 1,
            catalog_max_output_bytes: 100,
        };
        assert_eq!(
            parse_and_validate_rewrite(
                br#"{"schema":1,"replacements":[{"row_id":"r-1","text":"two"}]}"#,
                &targets,
                aggregate_limits,
            ),
            Err(ValidationError::AggregateLimit)
        );
    }

    #[test]
    fn question_requires_known_unique_citations_and_truthful_truncation() {
        let context = [row("r-1", "Synthetic context")];
        let valid = br#"{"schema":1,"answer":"Grounded answer.","cited_row_ids":["r-1"],"context_truncated":true}"#;
        let answer = parse_and_validate_question(valid, &context, true, 1_024).unwrap();
        assert_eq!(answer.answer(), "Grounded answer.");

        let unknown =
            br#"{"schema":1,"answer":"Answer.","cited_row_ids":["r-9"],"context_truncated":true}"#;
        assert_eq!(
            parse_and_validate_question(unknown, &context, true, 1_024),
            Err(ValidationError::UnknownRow)
        );
        assert_eq!(
            parse_and_validate_question(valid, &context, false, 1_024),
            Err(ValidationError::TruncationMismatch)
        );
    }

    #[test]
    fn final_chunk_sets_must_be_exact_and_disjoint() {
        let expected = [RowId::new("r-1").unwrap(), RowId::new("r-2").unwrap()];
        assert!(
            validate_disjoint_target_chunks(
                &expected,
                &[vec![expected[0].clone()], vec![expected[1].clone()]]
            )
            .is_ok()
        );
        assert_eq!(
            validate_disjoint_target_chunks(
                &expected,
                &[vec![expected[0].clone()], vec![expected[0].clone()]]
            ),
            Err(ValidationError::NonDisjointTargets)
        );
    }
}
