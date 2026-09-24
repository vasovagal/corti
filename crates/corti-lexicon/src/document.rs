//! The persisted rule list: canonical JSON, a content digest, and bounded, validated rules.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

pub const LEXICON_SCHEMA: u32 = 1;
/// Upper bound on rules in one document (the word bank allows 5000 spellings; a lexicon is smaller).
pub const MAX_RULES: usize = 5_000;
/// Upper bound on `from`/`to` in characters.
pub const MAX_RULE_TEXT_CHARS: usize = 128;
/// Upper bound on the serialized document.
pub const MAX_LEXICON_BYTES: usize = 512 * 1024;
const MAX_NOTE_CHARS: usize = 256;
const MAX_EXAMPLES: usize = 8;
const MAX_EXAMPLE_CHARS: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LexiconError {
    #[error("rule text is empty, too long, or contains control characters")]
    InvalidText,
    #[error("a rule must change something: `from` and `to` are identical")]
    NoOpRule,
    #[error("too many rules")]
    TooManyRules,
    #[error("lexicon document is malformed: {0}")]
    Malformed(String),
    #[error("lexicon content digest does not match its rules")]
    DigestMismatch,
    #[error("unsupported lexicon schema {0}")]
    UnsupportedSchema(u32),
    #[error(
        "rule {0} matches its own replacement, so applying it twice would change the text again"
    )]
    SelfMatching(String),
    #[error("no rule with id {0}")]
    UnknownRule(String),
}

/// A rule matches one word or a run of words. Phrases are matched before words so the longest
/// correction wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleKind {
    Word,
    Phrase,
}

/// Where a rule came from, for the owner's own bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleSource {
    /// Accepted during `corti --review`.
    Review,
    /// Added by hand (`corti --lexicon add`).
    Manual,
    /// Imported from elsewhere.
    Import,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LexiconRule {
    /// Stable id derived from `from` + `to` (`r-` + 16 hex chars).
    pub id: String,
    pub kind: RuleKind,
    /// The text ASR produces (NFC, single spaces).
    pub from: String,
    /// What it should have been.
    pub to: String,
    /// Match only at word boundaries (always true for phrases; the only supported mode today).
    pub whole_word: bool,
    /// Match regardless of case, adapting the replacement's case to the match.
    pub case_insensitive: bool,
    pub source: RuleSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Transcript snippets that prompted the rule, for the owner's later reference.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<String>,
    /// RFC 3339 timestamp supplied by the caller (this crate has no clock).
    pub created_at: String,
}

impl LexiconRule {
    /// Build a validated rule; the kind is inferred (a `from` with internal whitespace is a phrase) and
    /// the id is derived from the normalized `from` and `to`.
    pub fn new(
        from: &str,
        to: &str,
        source: RuleSource,
        created_at: impl Into<String>,
    ) -> Result<Self, LexiconError> {
        let from = normalize_text(from)?;
        let to = normalize_text(to)?;
        if from == to {
            return Err(LexiconError::NoOpRule);
        }
        let kind = if from.contains(' ') {
            RuleKind::Phrase
        } else {
            RuleKind::Word
        };
        let id = rule_id(&from, &to);
        Ok(Self {
            id,
            kind,
            from,
            to,
            whole_word: true,
            case_insensitive: true,
            source,
            note: None,
            examples: Vec::new(),
            created_at: created_at.into(),
        })
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Result<Self, LexiconError> {
        let note = note.into();
        if note.chars().count() > MAX_NOTE_CHARS || note.chars().any(char::is_control) {
            return Err(LexiconError::InvalidText);
        }
        self.note = (!note.trim().is_empty()).then_some(note);
        Ok(self)
    }

    pub fn with_example(mut self, example: impl Into<String>) -> Result<Self, LexiconError> {
        let example: String = example
            .into()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if example.chars().count() > MAX_EXAMPLE_CHARS || example.chars().any(char::is_control) {
            return Err(LexiconError::InvalidText);
        }
        if !example.is_empty()
            && self.examples.len() < MAX_EXAMPLES
            && !self.examples.contains(&example)
        {
            self.examples.push(example);
        }
        Ok(self)
    }

    /// Case-sensitive match (for exact acronyms or names whose case is the correction).
    pub fn case_sensitive(mut self) -> Self {
        self.case_insensitive = false;
        self
    }

    fn validate(&self) -> Result<(), LexiconError> {
        let from = normalize_text(&self.from)?;
        let to = normalize_text(&self.to)?;
        if from != self.from || to != self.to {
            return Err(LexiconError::Malformed(
                "rule text is not normalized".to_owned(),
            ));
        }
        if from == to {
            return Err(LexiconError::NoOpRule);
        }
        if self.id != rule_id(&self.from, &self.to) {
            return Err(LexiconError::Malformed(format!(
                "rule id {} is stale",
                self.id
            )));
        }
        if (self.kind == RuleKind::Phrase) != self.from.contains(' ') {
            return Err(LexiconError::Malformed(format!(
                "rule {} kind does not match its text",
                self.id
            )));
        }
        if !self.whole_word {
            return Err(LexiconError::Malformed(
                "substring rules are reserved for a later schema".to_owned(),
            ));
        }
        if self.note.as_ref().is_some_and(|note| {
            note.chars().count() > MAX_NOTE_CHARS || note.chars().any(char::is_control)
        }) {
            return Err(LexiconError::InvalidText);
        }
        if self.examples.len() > MAX_EXAMPLES
            || self.examples.iter().any(|example| {
                example.chars().count() > MAX_EXAMPLE_CHARS || example.chars().any(char::is_control)
            })
        {
            return Err(LexiconError::InvalidText);
        }
        Ok(())
    }
}

/// NFC, single spaces, no control characters, bounded length.
fn normalize_text(text: &str) -> Result<String, LexiconError> {
    let normalized: String = text
        .nfc()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if normalized.is_empty()
        || normalized.chars().count() > MAX_RULE_TEXT_CHARS
        || normalized.chars().any(char::is_control)
    {
        return Err(LexiconError::InvalidText);
    }
    Ok(normalized)
}

fn rule_id(from: &str, to: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(from.to_lowercase().as_bytes());
    hasher.update([0]);
    hasher.update(to.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("r-{hex}")
}

/// The persisted document. `content_digest` covers the schema and the rules (not the revision), so a
/// hand edit that changes a rule without recomputing the digest is refused at load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LexiconDocument {
    schema: u32,
    revision: u64,
    rules: Vec<LexiconRule>,
    content_digest: String,
}

impl Default for LexiconDocument {
    fn default() -> Self {
        Self::empty()
    }
}

impl LexiconDocument {
    pub fn empty() -> Self {
        Self::from_rules(0, Vec::new()).expect("an empty rule list is valid")
    }

    /// A document from validated rules; duplicate `from` (case-insensitively) keeps the last one.
    pub fn from_rules(revision: u64, rules: Vec<LexiconRule>) -> Result<Self, LexiconError> {
        let mut deduped: Vec<LexiconRule> = Vec::with_capacity(rules.len());
        for rule in rules {
            rule.validate()?;
            deduped.retain(|existing| {
                !(existing.from.eq_ignore_ascii_case(&rule.from)
                    || existing.from.to_lowercase() == rule.from.to_lowercase())
            });
            deduped.push(rule);
        }
        if deduped.len() > MAX_RULES {
            return Err(LexiconError::TooManyRules);
        }
        deduped.sort_by(|left, right| {
            (right.kind, left.from.to_lowercase()).cmp(&(left.kind, right.from.to_lowercase()))
        });
        let content_digest = digest_of(&deduped);
        let document = Self {
            schema: LEXICON_SCHEMA,
            revision,
            rules: deduped,
            content_digest,
        };
        let bytes = document.canonical_json()?;
        if bytes.len() > MAX_LEXICON_BYTES {
            return Err(LexiconError::TooManyRules);
        }
        Ok(document)
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self, LexiconError> {
        if bytes.len() > MAX_LEXICON_BYTES {
            return Err(LexiconError::TooManyRules);
        }
        let document: Self = serde_json::from_slice(bytes)
            .map_err(|error| LexiconError::Malformed(error.to_string()))?;
        if document.schema != LEXICON_SCHEMA {
            return Err(LexiconError::UnsupportedSchema(document.schema));
        }
        for rule in &document.rules {
            rule.validate()?;
        }
        if document.rules.len() > MAX_RULES {
            return Err(LexiconError::TooManyRules);
        }
        if digest_of(&document.rules) != document.content_digest {
            return Err(LexiconError::DigestMismatch);
        }
        Ok(document)
    }

    pub fn canonical_json(&self) -> Result<Vec<u8>, LexiconError> {
        serde_json::to_vec_pretty(self).map_err(|error| LexiconError::Malformed(error.to_string()))
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub fn rules(&self) -> &[LexiconRule] {
        &self.rules
    }

    pub fn content_digest(&self) -> &str {
        &self.content_digest
    }

    pub fn rule(&self, id: &str) -> Option<&LexiconRule> {
        self.rules.iter().find(|rule| rule.id == id)
    }

    /// Whether `from` is already corrected by a rule (case-insensitively).
    pub fn has_from(&self, from: &str) -> bool {
        let wanted = from.to_lowercase();
        self.rules
            .iter()
            .any(|rule| rule.from.to_lowercase() == wanted)
    }

    /// A new document with `rule` added (or replacing the rule for the same `from`) at the next revision.
    pub fn with_rule(&self, rule: LexiconRule) -> Result<Self, LexiconError> {
        let mut rules = self.rules.clone();
        rules.push(rule);
        Self::from_rules(self.revision.saturating_add(1), rules)
    }

    /// A new document without the rule `id` at the next revision.
    pub fn without_rule(&self, id: &str) -> Result<Self, LexiconError> {
        if self.rule(id).is_none() {
            return Err(LexiconError::UnknownRule(id.to_owned()));
        }
        let rules = self
            .rules
            .iter()
            .filter(|rule| rule.id != id)
            .cloned()
            .collect();
        Self::from_rules(self.revision.saturating_add(1), rules)
    }
}

fn digest_of(rules: &[LexiconRule]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"corti-lexicon-v1\0");
    for rule in rules {
        hasher.update(rule.id.as_bytes());
        hasher.update([0]);
        hasher.update(rule.from.as_bytes());
        hasher.update([0]);
        hasher.update(rule.to.as_bytes());
        hasher.update([0]);
        hasher.update([u8::from(rule.case_insensitive), u8::from(rule.whole_word)]);
        hasher.update([0]);
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(from: &str, to: &str) -> LexiconRule {
        LexiconRule::new(from, to, RuleSource::Review, "2026-09-24T00:00:00Z").unwrap()
    }

    #[test]
    fn rules_are_normalized_validated_and_identified() {
        let phrase = rule("  settle ment   gateway ", "settlement gateway");
        assert_eq!(phrase.kind, RuleKind::Phrase);
        assert_eq!(phrase.from, "settle ment gateway");
        assert!(phrase.id.starts_with("r-"));
        assert_eq!(rule("Vagus", "vagus").kind, RuleKind::Word);
        assert_eq!(
            LexiconRule::new("same", "same", RuleSource::Manual, "t").unwrap_err(),
            LexiconError::NoOpRule
        );
        assert!(LexiconRule::new("", "x", RuleSource::Manual, "t").is_err());
        assert!(LexiconRule::new("a\u{7}", "x", RuleSource::Manual, "t").is_err());
        assert_eq!(
            rule("Corti", "corti").id,
            rule("CORTI", "corti").id,
            "ids ignore case of `from`"
        );
    }

    #[test]
    fn documents_round_trip_dedupe_and_verify_their_digest() {
        let document = LexiconDocument::from_rules(
            3,
            vec![
                rule("vagus", "Vagus"),
                rule("corty", "Corti"),
                rule("VAGUS", "vagus!"),
            ],
        )
        .unwrap();
        assert_eq!(
            document.rules().len(),
            2,
            "the later rule for the same `from` wins"
        );
        assert!(document.has_from("Corty"));
        let bytes = document.canonical_json().unwrap();
        let loaded = LexiconDocument::from_json(&bytes).unwrap();
        assert_eq!(loaded, document);

        let mut tampered: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        tampered["rules"][0]["to"] = "Something else".into();
        let tampered = serde_json::to_vec(&tampered).unwrap();
        assert!(matches!(
            LexiconDocument::from_json(&tampered),
            Err(LexiconError::Malformed(_) | LexiconError::DigestMismatch)
        ));

        let grown = document.with_rule(rule("rust lang", "Rust")).unwrap();
        assert_eq!(grown.revision(), 4);
        assert_eq!(
            grown.rules()[0].kind,
            RuleKind::Phrase,
            "phrases sort first"
        );
        let id = grown.rules()[0].id.clone();
        let shrunk = grown.without_rule(&id).unwrap();
        assert_eq!(shrunk.rules().len(), 2);
        assert!(matches!(
            shrunk.without_rule("r-missing"),
            Err(LexiconError::UnknownRule(_))
        ));
    }
}
