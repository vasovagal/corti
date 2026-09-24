//! Question subscriptions: saved questions the coordinator re-asks as the transcript grows.
//!
//! A subscription is the generalisation of the earlier single "pinned question". Each one carries its own
//! trigger policy (how much new speech, from whom, after how much quiet), its own context window and its
//! own answer format; the wire lane stays `Lane::PinnedQuestion` and the subscription id rides
//! `HostedRequest::target_id` so every call is attributable and fenced per subscription.
//!
//! Everything in this module is pure: presets, validation, the cheap `asked_of_me` pre-filter that decides
//! whether a batch of "Them" rows is worth a paid call, the context window over a row ledger, and the
//! extraction of a presentable partial answer from a streamed JSON prefix.

use corti_postprocess::TranscriptRow;
use serde::{Deserialize, Serialize};
use unicode_segmentation::UnicodeSegmentation;

/// Upper bound on saved subscriptions, mirrored by the hosted document validator.
pub const MAX_SUBSCRIPTIONS: usize = 16;
/// Upper bound on one template's UTF-8 length.
pub const MAX_TEMPLATE_BYTES: usize = 8 * 1024;
/// Upper bound on a streamed partial answer kept for display while a call runs.
pub const MAX_PARTIAL_ANSWER_BYTES: usize = 16 * 1024;
/// Default quiet period before an eligible subscription runs.
pub const DEFAULT_QUIET_MICROS: u64 = 750_000;
/// Default new-word threshold between runs.
pub const DEFAULT_MIN_NEW_WORDS: u64 = 40;
/// Default new-speech threshold between runs.
pub const DEFAULT_MIN_NEW_SPEECH_MS: u64 = 30_000;

/// Validated subscription id: `[a-z0-9-]{1,32}`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct SubscriptionId(String);

impl SubscriptionId {
    pub fn new(value: impl Into<String>) -> Result<Self, SubscriptionError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 32
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(SubscriptionError::InvalidId);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SubscriptionError {
    #[error("subscription id must match [a-z0-9-]{{1,32}}")]
    InvalidId,
    #[error("subscription template is empty, too long, or contains control characters")]
    InvalidTemplate,
    #[error("subscription title is too long or contains control characters")]
    InvalidTitle,
    #[error("duplicate subscription id")]
    DuplicateId,
    #[error("too many subscriptions")]
    TooMany,
    #[error("unknown preset, format, speaker filter, or context window")]
    UnknownVariant,
    #[error("unknown subscription id")]
    UnknownSubscription,
}

/// Built-in behaviours. `None` is a plain custom question with the default trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionPreset {
    None,
    /// "What are they asking me?" — Them rows only, quick quiet period, cheap question pre-filter.
    AskedOfMe,
    /// A running bullet list of what has been discussed.
    RunningSummary,
    /// What is being said about one topic named in the template.
    TopicWatch,
}

impl SubscriptionPreset {
    pub fn parse(value: &str) -> Result<Self, SubscriptionError> {
        match value {
            "none" => Ok(Self::None),
            "asked_of_me" => Ok(Self::AskedOfMe),
            "running_summary" => Ok(Self::RunningSummary),
            "topic_watch" => Ok(Self::TopicWatch),
            _ => Err(SubscriptionError::UnknownVariant),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::AskedOfMe => "asked_of_me",
            Self::RunningSummary => "running_summary",
            Self::TopicWatch => "topic_watch",
        }
    }

    /// The template a preset uses when the saved one is blank.
    pub const fn default_template(self) -> &'static str {
        match self {
            Self::None => "",
            Self::AskedOfMe => {
                "List the questions or requests the other speakers have directed at me (the \"Me\" speaker), most recent first. Quote each briefly and say who asked if the transcript shows it. If nothing was asked of me, say so."
            }
            Self::RunningSummary => {
                "Give a running bullet list of what has been discussed so far, one short bullet per topic or decision, in order."
            }
            Self::TopicWatch => {
                "What are they saying about the topic named below? Summarise the points made so far as short bullets and note any open questions."
            }
        }
    }

    pub const fn default_format(self) -> AnswerFormat {
        match self {
            Self::None => AnswerFormat::Paragraph,
            Self::AskedOfMe | Self::RunningSummary | Self::TopicWatch => AnswerFormat::Bullets,
        }
    }

    pub const fn default_trigger(self) -> TriggerPolicy {
        match self {
            Self::None | Self::RunningSummary | Self::TopicWatch => TriggerPolicy::DEFAULT,
            Self::AskedOfMe => TriggerPolicy {
                quiet_micros: 1_000_000,
                min_new_words: 1,
                min_new_speech_ms: 0,
                min_interval_micros: 0,
                on_speakers: SpeakerFilter::Them,
            },
        }
    }

    pub const fn default_context(self) -> ContextWindow {
        match self {
            Self::None | Self::RunningSummary | Self::TopicWatch => ContextWindow::Whole,
            Self::AskedOfMe => ContextWindow::LastMinutes(5),
        }
    }
}

/// How an answer is laid out. Only `Bullets` may contain newlines and list markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerFormat {
    Paragraph,
    Bullets,
}

impl AnswerFormat {
    pub fn parse(value: &str) -> Result<Self, SubscriptionError> {
        match value {
            "paragraph" => Ok(Self::Paragraph),
            "bullets" => Ok(Self::Bullets),
            _ => Err(SubscriptionError::UnknownVariant),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Paragraph => "paragraph",
            Self::Bullets => "bullets",
        }
    }

    /// The instruction appended to the question so the model answers in this layout.
    pub const fn instruction(self) -> &'static str {
        match self {
            Self::Paragraph => "Answer in one short plain-text paragraph.",
            Self::Bullets => {
                "Answer as a plain-text bullet list: one item per line, each line starting with \"- \". No headings, no emphasis markers."
            }
        }
    }
}

/// Which speakers' rows count as progress for a subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeakerFilter {
    All,
    Them,
    Me,
}

impl SpeakerFilter {
    pub fn parse(value: &str) -> Result<Self, SubscriptionError> {
        match value {
            "all" => Ok(Self::All),
            "them" => Ok(Self::Them),
            "me" => Ok(Self::Me),
            _ => Err(SubscriptionError::UnknownVariant),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Them => "them",
            Self::Me => "me",
        }
    }

    pub fn matches(self, row: &TranscriptRow) -> bool {
        match self {
            Self::All => true,
            Self::Them => !row_is_me(row),
            Self::Me => row_is_me(row),
        }
    }
}

/// Live rows are labelled `Me` (the owner's microphone) or `Them` (system audio); anything else is
/// treated as another party.
pub fn row_is_me(row: &TranscriptRow) -> bool {
    row.speaker.trim().eq_ignore_ascii_case("me")
}

/// How much of the ledger a subscription reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "window", rename_all = "snake_case")]
pub enum ContextWindow {
    Whole,
    LastMinutes(u32),
    LastRows(u32),
}

impl ContextWindow {
    pub fn parse(window: &str, minutes: u32, rows: u32) -> Result<Self, SubscriptionError> {
        match window {
            "whole" => Ok(Self::Whole),
            "last_minutes" => Ok(Self::LastMinutes(minutes.max(1))),
            "last_rows" => Ok(Self::LastRows(rows.max(1))),
            _ => Err(SubscriptionError::UnknownVariant),
        }
    }

    /// The suffix of `ledger` this window covers. Ledger rows are in arrival order.
    pub fn select(self, ledger: &[TranscriptRow]) -> &[TranscriptRow] {
        match self {
            Self::Whole => ledger,
            Self::LastRows(rows) => {
                let start = ledger.len().saturating_sub(rows as usize);
                &ledger[start..]
            }
            Self::LastMinutes(minutes) => {
                let Some(last) = ledger.last() else {
                    return ledger;
                };
                let horizon = last
                    .end_ms
                    .saturating_sub(u64::from(minutes).saturating_mul(60_000));
                let start = ledger
                    .iter()
                    .position(|row| row.end_ms >= horizon)
                    .unwrap_or(ledger.len());
                &ledger[start..]
            }
        }
    }
}

/// When a subscription becomes eligible to run again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TriggerPolicy {
    /// Quiet after the last relevant row before a run.
    pub quiet_micros: u64,
    /// New words from the relevant speakers since the last run (0 = any new row).
    pub min_new_words: u64,
    /// New covered speech since the last run; either threshold suffices.
    pub min_new_speech_ms: u64,
    /// Minimum spacing between dispatches (0 = none).
    pub min_interval_micros: u64,
    pub on_speakers: SpeakerFilter,
}

impl TriggerPolicy {
    pub const DEFAULT: Self = Self {
        quiet_micros: DEFAULT_QUIET_MICROS,
        min_new_words: DEFAULT_MIN_NEW_WORDS,
        min_new_speech_ms: DEFAULT_MIN_NEW_SPEECH_MS,
        min_interval_micros: 0,
        on_speakers: SpeakerFilter::All,
    };
}

impl Default for TriggerPolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Words / covered speech / rows, counted for one speaker class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProgressCounters {
    pub words: u64,
    pub speech_ms: u64,
    pub rows: u64,
}

impl ProgressCounters {
    pub fn add_row(&mut self, row: &TranscriptRow) {
        self.words = self
            .words
            .saturating_add(row.text.unicode_words().count() as u64);
        self.speech_ms = self
            .speech_ms
            .saturating_add(row.end_ms.saturating_sub(row.start_ms));
        self.rows = self.rows.saturating_add(1);
    }
}

/// Progress counters per speaker class plus when each class last advanced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProgressLedger {
    pub all: ProgressCounters,
    pub them: ProgressCounters,
    pub me: ProgressCounters,
    pub last_all_at_micros: u64,
    pub last_them_at_micros: u64,
    pub last_me_at_micros: u64,
}

impl ProgressLedger {
    pub fn observe(&mut self, rows: &[TranscriptRow], now_micros: u64) {
        for row in rows {
            self.all.add_row(row);
            self.last_all_at_micros = now_micros;
            if row_is_me(row) {
                self.me.add_row(row);
                self.last_me_at_micros = now_micros;
            } else {
                self.them.add_row(row);
                self.last_them_at_micros = now_micros;
            }
        }
    }

    pub fn counters(&self, filter: SpeakerFilter) -> ProgressCounters {
        match filter {
            SpeakerFilter::All => self.all,
            SpeakerFilter::Them => self.them,
            SpeakerFilter::Me => self.me,
        }
    }

    pub fn last_progress_at(&self, filter: SpeakerFilter) -> u64 {
        match filter {
            SpeakerFilter::All => self.last_all_at_micros,
            SpeakerFilter::Them => self.last_them_at_micros,
            SpeakerFilter::Me => self.last_me_at_micros,
        }
    }
}

/// Whether the progress since `baseline` satisfies `policy` (rows must have arrived, and either the
/// word or the speech threshold must be met).
pub fn meaningful_progress(
    policy: &TriggerPolicy,
    baseline: &ProgressLedger,
    current: &ProgressLedger,
) -> bool {
    let before = baseline.counters(policy.on_speakers);
    let after = current.counters(policy.on_speakers);
    after.rows > before.rows
        && (after.words.saturating_sub(before.words) >= policy.min_new_words
            || after.speech_ms.saturating_sub(before.speech_ms) >= policy.min_new_speech_ms)
}

/// One saved subscription as the coordinator sees it. `template` may be blank for a preset, in which
/// case the preset's default is used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionSpec {
    pub id: SubscriptionId,
    pub title: String,
    pub template: String,
    pub enabled: bool,
    pub preset: SubscriptionPreset,
    pub format: AnswerFormat,
    pub trigger: TriggerPolicy,
    pub context: ContextWindow,
    pub name_hints: Vec<String>,
}

impl SubscriptionSpec {
    /// A spec with a preset's defaults and no custom template.
    pub fn from_preset(id: SubscriptionId, preset: SubscriptionPreset) -> Self {
        Self {
            id,
            title: preset_title(preset).to_owned(),
            template: String::new(),
            enabled: true,
            preset,
            format: preset.default_format(),
            trigger: preset.default_trigger(),
            context: preset.default_context(),
            name_hints: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<(), SubscriptionError> {
        if self.title.len() > 128 || self.title.chars().any(char::is_control) {
            return Err(SubscriptionError::InvalidTitle);
        }
        if self.template.len() > MAX_TEMPLATE_BYTES
            || self
                .template
                .chars()
                .any(|ch| ch.is_control() && ch != '\n')
        {
            return Err(SubscriptionError::InvalidTemplate);
        }
        if self.effective_template().trim().is_empty() {
            return Err(SubscriptionError::InvalidTemplate);
        }
        Ok(())
    }

    /// The saved template, or the preset's default when the saved one is blank.
    pub fn effective_template(&self) -> String {
        if self.template.trim().is_empty() {
            self.preset.default_template().to_owned()
        } else {
            self.template.trim().to_owned()
        }
    }

    /// The full question sent to the model: the template plus the layout instruction, and for
    /// `asked_of_me` the names the owner answers to.
    pub fn question_text(&self) -> String {
        let mut text = self.effective_template();
        if self.preset == SubscriptionPreset::AskedOfMe && !self.name_hints.is_empty() {
            text.push_str("\nI also answer to: ");
            text.push_str(&self.name_hints.join(", "));
            text.push('.');
        }
        text.push('\n');
        text.push_str(self.format.instruction());
        text
    }

    /// Whether these changes need in-flight work canceled and the progress baseline reset.
    pub fn changes_question(&self, other: &Self) -> bool {
        self.question_text() != other.question_text()
            || self.context != other.context
            || self.preset != other.preset
    }
}

pub const fn preset_title(preset: SubscriptionPreset) -> &'static str {
    match preset {
        SubscriptionPreset::None => "Custom question",
        SubscriptionPreset::AskedOfMe => "Asked of me",
        SubscriptionPreset::RunningSummary => "Running summary",
        SubscriptionPreset::TopicWatch => "Topic watch",
    }
}

/// Validate a whole set: ids unique, count bounded, every spec valid.
pub fn validate_set(specs: &[SubscriptionSpec]) -> Result<(), SubscriptionError> {
    if specs.len() > MAX_SUBSCRIPTIONS {
        return Err(SubscriptionError::TooMany);
    }
    let mut seen = std::collections::HashSet::with_capacity(specs.len());
    for spec in specs {
        if !seen.insert(&spec.id) {
            return Err(SubscriptionError::DuplicateId);
        }
        spec.validate()?;
    }
    Ok(())
}

const INTERROGATIVES: &[&str] = &[
    "what", "why", "how", "when", "where", "who", "which", "whose", "can", "could", "would",
    "should", "will", "do", "does", "did", "is", "are", "was", "were", "have", "has", "any",
    "tell", "walk", "remind",
];

/// Cheap pre-filter for `asked_of_me`: is there a question here that is plausibly directed at the
/// owner? A question mark counts on its own; otherwise an interrogative opening must be paired with a
/// second-person reference or one of the owner's name hints. Kept deliberately loose — false positives
/// cost one bounded call, false negatives hide a question.
pub fn looks_asked_of_me(text: &str, name_hints: &[String]) -> bool {
    let lowered = text.to_lowercase();
    if lowered.contains('?') {
        return true;
    }
    let words: Vec<&str> = lowered.unicode_words().collect();
    if words.is_empty() {
        return false;
    }
    // An interrogative within the first three words: "what do you…", "so could you…", "Xavier can
    // you…". Later interrogatives ("I wonder what you…") are left to the question mark rule.
    let opens_with_interrogative = words
        .iter()
        .take(3)
        .any(|word| INTERROGATIVES.contains(word));
    if !opens_with_interrogative {
        return false;
    }
    words.iter().any(|word| {
        matches!(*word, "you" | "your" | "yours" | "yourself")
            || name_hints
                .iter()
                .any(|hint| !hint.is_empty() && hint.to_lowercase() == *word)
    })
}

/// Whether any of `rows` is a Them row that passes the pre-filter.
pub fn rows_contain_question_for_me(rows: &[TranscriptRow], name_hints: &[String]) -> bool {
    rows.iter()
        .any(|row| !row_is_me(row) && looks_asked_of_me(&row.text, name_hints))
}

/// Extract the readable part of a streamed question answer from the JSON prefix received so far. The
/// providers stream the raw `{"schema":1,"answer":"…` object, so the display text is the decoded
/// `answer` string up to the last complete escape. Returns `None` until the answer field has started.
pub fn partial_answer_from_json_prefix(prefix: &str) -> Option<String> {
    let start = prefix.find("\"answer\"")?;
    let after_key = &prefix[start + "\"answer\"".len()..];
    let colon = after_key.find(':')?;
    let value = after_key[colon + 1..].trim_start();
    let body = value.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = body.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => break,
            '\\' => {
                let Some(escaped) = chars.next() else {
                    break;
                };
                match escaped {
                    'n' => out.push('\n'),
                    't' => out.push(' '),
                    'r' => {}
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    '/' => out.push('/'),
                    'u' => {
                        let hex: String = chars.by_ref().take(4).collect();
                        if hex.len() < 4 {
                            break;
                        }
                        if let Some(decoded) =
                            u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
                        {
                            out.push(decoded);
                        }
                    }
                    _ => {}
                }
            }
            other if other.is_control() => {}
            other => out.push(other),
        }
    }
    let trimmed = out.trim_end();
    if trimmed.is_empty() {
        return None;
    }
    let mut kept = trimmed.to_owned();
    if kept.len() > MAX_PARTIAL_ANSWER_BYTES {
        let mut cut = MAX_PARTIAL_ANSWER_BYTES;
        while !kept.is_char_boundary(cut) {
            cut -= 1;
        }
        kept.truncate(cut);
    }
    Some(kept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use corti_postprocess::RowId;

    fn row(id: u64, speaker: &str, text: &str, start_ms: u64, end_ms: u64) -> TranscriptRow {
        TranscriptRow {
            row_id: RowId::new(format!("r-{id}")).unwrap(),
            speaker: speaker.into(),
            start_ms,
            end_ms,
            text: text.into(),
        }
    }

    #[test]
    fn ids_and_specs_are_validated() {
        assert!(SubscriptionId::new("asked-of-me").is_ok());
        assert!(SubscriptionId::new("Not Valid").is_err());
        assert!(SubscriptionId::new("").is_err());
        assert!(SubscriptionId::new("a".repeat(33)).is_err());

        let mut custom = SubscriptionSpec::from_preset(
            SubscriptionId::new("custom").unwrap(),
            SubscriptionPreset::None,
        );
        assert_eq!(
            custom.validate(),
            Err(SubscriptionError::InvalidTemplate),
            "a custom question needs its own template"
        );
        custom.template = "What did we decide?".into();
        assert!(custom.validate().is_ok());
        custom.template = "bad\u{7}".into();
        assert!(custom.validate().is_err());

        let preset = SubscriptionSpec::from_preset(
            SubscriptionId::new("summary").unwrap(),
            SubscriptionPreset::RunningSummary,
        );
        assert!(
            preset.validate().is_ok(),
            "presets carry a default template"
        );
        assert!(preset.question_text().contains("bullet"));

        let duplicate = vec![preset.clone(), preset.clone()];
        assert_eq!(
            validate_set(&duplicate),
            Err(SubscriptionError::DuplicateId)
        );
    }

    #[test]
    fn asked_of_me_prefilter_accepts_directed_questions_only() {
        let hints = vec!["Xavier".to_owned()];
        assert!(looks_asked_of_me(
            "What do you think about the rollout?",
            &[]
        ));
        assert!(looks_asked_of_me(
            "Xavier can you take the action item",
            &hints
        ));
        assert!(looks_asked_of_me("so what would you do here", &[]));
        assert!(!looks_asked_of_me("What we did last quarter was fine", &[]));
        assert!(!looks_asked_of_me(
            "I think the rollout is on track",
            &hints
        ));
        assert!(!looks_asked_of_me("", &hints));

        let rows = vec![
            row(1, "Me", "Are we done here?", 0, 1_000),
            row(2, "Them", "Let's move to the next item", 1_000, 2_000),
        ];
        assert!(
            !rows_contain_question_for_me(&rows, &hints),
            "the owner's own question is not a question for the owner"
        );
        let rows = vec![row(
            3,
            "Them",
            "Xavier, could you own the migration",
            2_000,
            3_000,
        )];
        assert!(rows_contain_question_for_me(&rows, &hints));
    }

    #[test]
    fn progress_ledger_counts_per_speaker_and_thresholds_apply_per_filter() {
        let mut ledger = ProgressLedger::default();
        let baseline = ledger;
        ledger.observe(
            &[
                row(1, "Me", &"w ".repeat(30), 0, 10_000),
                row(2, "Them", &"w ".repeat(15), 10_000, 20_000),
            ],
            5_000_000,
        );
        assert_eq!(ledger.all.words, 45);
        assert_eq!(ledger.them.words, 15);
        assert_eq!(ledger.me.words, 30);
        assert_eq!(ledger.last_them_at_micros, 5_000_000);

        let all = TriggerPolicy::DEFAULT;
        assert!(meaningful_progress(&all, &baseline, &ledger));
        let them = TriggerPolicy {
            on_speakers: SpeakerFilter::Them,
            ..TriggerPolicy::DEFAULT
        };
        assert!(
            !meaningful_progress(&them, &baseline, &ledger),
            "15 Them words are under the 40-word threshold"
        );
        let any_them = SubscriptionPreset::AskedOfMe.default_trigger();
        assert!(meaningful_progress(&any_them, &baseline, &ledger));
    }

    #[test]
    fn context_windows_select_a_suffix() {
        let ledger: Vec<TranscriptRow> = (0..10)
            .map(|index| {
                row(
                    index,
                    "Them",
                    "text",
                    index * 60_000,
                    index * 60_000 + 1_000,
                )
            })
            .collect();
        assert_eq!(ContextWindow::Whole.select(&ledger).len(), 10);
        assert_eq!(ContextWindow::LastRows(3).select(&ledger).len(), 3);
        assert_eq!(ContextWindow::LastRows(50).select(&ledger).len(), 10);
        let last_two_minutes = ContextWindow::LastMinutes(2).select(&ledger);
        assert_eq!(
            last_two_minutes.len(),
            3,
            "rows ending within two minutes of the newest"
        );
        assert_eq!(last_two_minutes[0].row_id.as_str(), "r-7");
        assert!(ContextWindow::LastMinutes(1).select(&[]).is_empty());
    }

    #[test]
    fn partial_answers_are_decoded_from_the_streamed_json_prefix() {
        assert_eq!(partial_answer_from_json_prefix(r#"{"schema":1,"#), None);
        assert_eq!(
            partial_answer_from_json_prefix(r#"{"schema":1,"answer":"- First point\n- Sec"#),
            Some("- First point\n- Sec".to_owned())
        );
        assert_eq!(
            partial_answer_from_json_prefix(r#"{"schema":1,"answer":"Done.","cited_row_ids":["#),
            Some("Done.".to_owned())
        );
        assert_eq!(
            partial_answer_from_json_prefix(r#"{"answer": "Quote \"here\" and é"#),
            Some("Quote \"here\" and é".to_owned())
        );
        assert_eq!(
            partial_answer_from_json_prefix(r#"{"answer":"trailing \"#),
            Some("trailing".to_owned()),
            "a dangling escape is dropped rather than shown"
        );
    }
}
