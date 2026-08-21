use std::{cmp::Ordering, collections::BTreeMap};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de, ser::SerializeStruct};
use sha2::{Digest, Sha256};
use thiserror::Error;
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;

use crate::{DigestKey, KeyedFingerprint};

pub const WORD_BANK_SCHEMA: u32 = 1;
pub const MAX_WORD_BANK_ENTRY_SCALARS: usize = 128;
pub const MAX_WORD_BANK_ENTRIES: usize = 5_000;
pub const MAX_WORD_BANK_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum WordBankError {
    #[error("word-bank schema is unsupported")]
    UnsupportedSchema,
    #[error("word-bank entry is empty after normalization")]
    EmptyEntry,
    #[error("word-bank entry contains a newline or control character")]
    ControlCharacter,
    #[error("word-bank entry contains a bidirectional control character")]
    BidiControl,
    #[error("word-bank entry exceeds 128 Unicode scalar values")]
    EntryTooLong,
    #[error("word bank exceeds 5000 canonical entries")]
    TooManyEntries,
    #[error("canonical word-bank document exceeds 256 KiB")]
    DocumentTooLarge,
    #[error("word-bank content digest does not match canonical content")]
    DigestMismatch,
    #[error("word-bank revision overflow")]
    RevisionOverflow,
    #[error("word-bank entry was not found")]
    EntryNotFound,
    #[error("edited entry conflicts with another case-fold-equivalent entry")]
    DuplicateEdit,
    #[error("canonical word-bank serialization failed")]
    Serialization,
}

/// Normalize one display spelling using the authoritative word-bank rules.
pub fn normalize_word_bank_entry(entry: &str) -> Result<String, WordBankError> {
    let nfc: String = entry.nfc().collect();
    for ch in nfc.chars() {
        if is_bidi_control(ch) {
            return Err(WordBankError::BidiControl);
        }
        // Newlines, tabs, NUL, and other controls are rejected rather than hidden by whitespace collapse.
        if ch.is_control() {
            return Err(WordBankError::ControlCharacter);
        }
    }

    let mut normalized = String::with_capacity(nfc.len());
    let mut pending_space = false;
    for ch in nfc.chars() {
        if ch.is_whitespace() {
            pending_space = !normalized.is_empty();
        } else {
            if pending_space {
                normalized.push(' ');
                pending_space = false;
            }
            normalized.push(ch);
        }
    }

    if normalized.is_empty() {
        return Err(WordBankError::EmptyEntry);
    }
    if normalized.chars().count() > MAX_WORD_BANK_ENTRY_SCALARS {
        return Err(WordBankError::EntryTooLong);
    }
    // Whitespace replacement cannot normally disturb NFC, but normalizing again makes that invariant explicit.
    Ok(normalized.nfc().collect())
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

fn folded_key(display: &str) -> String {
    display.case_fold().collect::<String>().nfc().collect()
}

/// Canonical, deterministically ordered spelling bank.
///
/// Fields are private so every instance satisfies normalization, deduplication, digest, and size invariants.
#[derive(Clone, PartialEq, Eq)]
pub struct WordBankDocument {
    schema: u32,
    revision: u64,
    entries: Vec<String>,
    content_digest: String,
}

impl std::fmt::Debug for WordBankDocument {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WordBankDocument")
            .field("schema", &self.schema)
            .field("revision", &self.revision)
            .field("entry_count", &self.entries.len())
            .field("content_digest", &self.content_digest)
            .finish()
    }
}

impl Default for WordBankDocument {
    fn default() -> Self {
        Self::empty()
    }
}

impl WordBankDocument {
    pub fn empty() -> Self {
        // Serialization of a fixed, in-memory struct cannot fail.
        Self::from_entries(0, std::iter::empty::<&str>()).expect("empty word bank is valid")
    }

    pub fn from_entries<I, S>(revision: u64, entries: I) -> Result<Self, WordBankError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let entries = canonicalize(entries)?;
        Self::from_canonical_entries(revision, entries)
    }

    pub const fn schema(&self) -> u32 {
        self.schema
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    pub fn content_digest(&self) -> &str {
        &self.content_digest
    }

    /// Canonical JSON in fixed field order: schema, revision, entries, content_digest.
    pub fn canonical_json(&self) -> Result<Vec<u8>, WordBankError> {
        serde_json::to_vec(self).map_err(|_| WordBankError::Serialization)
    }

    /// HMAC-derived external fingerprint. The raw content digest is intentionally not returned here.
    pub fn external_fingerprint(&self, key: &DigestKey) -> Result<KeyedFingerprint, WordBankError> {
        Ok(KeyedFingerprint::derive(
            key,
            b"corti-provenance-fingerprint-v1\0",
            &canonical_content_json(&self.entries)?,
        ))
    }

    /// Replace the desired set while preserving an existing display spelling for each folded key.
    /// Use [`Self::edit`] for an explicit spelling change.
    pub fn replace<I, S>(&self, desired: I) -> Result<Self, WordBankError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let desired = canonicalize(desired)?;
        let existing: BTreeMap<String, &String> = self
            .entries
            .iter()
            .map(|entry| (folded_key(entry), entry))
            .collect();
        let mut preserved = Vec::with_capacity(desired.len());
        for entry in desired {
            preserved.push(
                existing
                    .get(&folded_key(&entry))
                    .map_or(entry, |old| (*old).clone()),
            );
        }
        sort_entries(&mut preserved);
        self.updated(preserved)
    }

    /// Add entries, retaining existing display spellings for case-fold-equivalent input.
    pub fn add<I, S>(&self, additions: I) -> Result<Self, WordBankError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        // Canonicalize additions independently so multiple new display variants cannot make insertion
        // order affect the selected spelling. Existing document spellings still win below.
        let additions = canonicalize(additions)?;
        let mut by_fold: BTreeMap<String, String> = self
            .entries
            .iter()
            .map(|entry| (folded_key(entry), entry.clone()))
            .collect();
        for display in additions {
            by_fold.entry(folded_key(&display)).or_insert(display);
            if by_fold.len() > MAX_WORD_BANK_ENTRIES {
                return Err(WordBankError::TooManyEntries);
            }
        }
        let mut entries: Vec<String> = by_fold.into_values().collect();
        sort_entries(&mut entries);
        self.updated(entries)
    }

    /// Explicitly change one display spelling. The lookup is Unicode case-folded.
    pub fn edit(&self, existing: &str, replacement: &str) -> Result<Self, WordBankError> {
        let existing = normalize_word_bank_entry(existing)?;
        let old_key = folded_key(&existing);
        let replacement = normalize_word_bank_entry(replacement)?;
        let replacement_key = folded_key(&replacement);
        let Some(old_index) = self
            .entries
            .iter()
            .position(|entry| folded_key(entry) == old_key)
        else {
            return Err(WordBankError::EntryNotFound);
        };
        if self
            .entries
            .iter()
            .enumerate()
            .any(|(index, entry)| index != old_index && folded_key(entry) == replacement_key)
        {
            return Err(WordBankError::DuplicateEdit);
        }
        let mut entries = self.entries.clone();
        entries[old_index] = replacement;
        sort_entries(&mut entries);
        self.updated(entries)
    }

    pub fn remove(&self, entry: &str) -> Result<Self, WordBankError> {
        let entry = normalize_word_bank_entry(entry)?;
        let key = folded_key(&entry);
        let mut entries = self.entries.clone();
        let old_len = entries.len();
        entries.retain(|candidate| folded_key(candidate) != key);
        if entries.len() == old_len {
            return Err(WordBankError::EntryNotFound);
        }
        self.updated(entries)
    }

    pub fn clear(&self) -> Result<Self, WordBankError> {
        self.updated(Vec::new())
    }

    fn updated(&self, entries: Vec<String>) -> Result<Self, WordBankError> {
        if entries == self.entries {
            return Ok(self.clone());
        }
        let revision = self
            .revision
            .checked_add(1)
            .ok_or(WordBankError::RevisionOverflow)?;
        Self::from_canonical_entries(revision, entries)
    }

    fn from_canonical_entries(revision: u64, entries: Vec<String>) -> Result<Self, WordBankError> {
        if entries.len() > MAX_WORD_BANK_ENTRIES {
            return Err(WordBankError::TooManyEntries);
        }
        let content_digest = digest_entries(&entries)?;
        let document = Self {
            schema: WORD_BANK_SCHEMA,
            revision,
            entries,
            content_digest,
        };
        if document.canonical_json()?.len() > MAX_WORD_BANK_BYTES {
            return Err(WordBankError::DocumentTooLarge);
        }
        Ok(document)
    }
}

fn canonicalize<I, S>(entries: I) -> Result<Vec<String>, WordBankError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut by_fold = BTreeMap::<String, String>::new();
    for entry in entries {
        let display = normalize_word_bank_entry(entry.as_ref())?;
        // With no pre-existing document spelling, choose the lowest UTF-8 display bytes for a folded key.
        // This deterministic tie-break keeps bulk-paste/insertion order out of cache identity.
        by_fold
            .entry(folded_key(&display))
            .and_modify(|existing| {
                if display.as_bytes() < existing.as_bytes() {
                    existing.clone_from(&display);
                }
            })
            .or_insert(display);
        if by_fold.len() > MAX_WORD_BANK_ENTRIES {
            return Err(WordBankError::TooManyEntries);
        }
    }
    let mut entries: Vec<String> = by_fold.into_values().collect();
    sort_entries(&mut entries);
    Ok(entries)
}

fn sort_entries(entries: &mut [String]) {
    entries.sort_by(|left, right| {
        let folded = folded_key(left).cmp(&folded_key(right));
        if folded == Ordering::Equal {
            left.as_bytes().cmp(right.as_bytes())
        } else {
            folded
        }
    });
}

#[derive(Serialize)]
struct CanonicalContent<'a> {
    schema: u32,
    entries: &'a [String],
}

fn canonical_content_json(entries: &[String]) -> Result<Vec<u8>, WordBankError> {
    serde_json::to_vec(&CanonicalContent {
        schema: WORD_BANK_SCHEMA,
        entries,
    })
    .map_err(|_| WordBankError::Serialization)
}

fn digest_entries(entries: &[String]) -> Result<String, WordBankError> {
    let digest = Sha256::digest(canonical_content_json(entries)?);
    Ok(URL_SAFE_NO_PAD.encode(digest))
}

impl Serialize for WordBankDocument {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("WordBankDocument", 4)?;
        state.serialize_field("schema", &self.schema)?;
        state.serialize_field("revision", &self.revision)?;
        state.serialize_field("entries", &self.entries)?;
        state.serialize_field("content_digest", &self.content_digest)?;
        state.end()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWordBankDocument {
    schema: u32,
    revision: u64,
    entries: Vec<String>,
    content_digest: String,
}

impl<'de> Deserialize<'de> for WordBankDocument {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawWordBankDocument::deserialize(deserializer)?;
        if raw.schema != WORD_BANK_SCHEMA {
            return Err(de::Error::custom(WordBankError::UnsupportedSchema));
        }
        let document = Self::from_entries(raw.revision, raw.entries).map_err(de::Error::custom)?;
        if raw.content_digest != document.content_digest {
            return Err(de::Error::custom(WordBankError::DigestMismatch));
        }
        Ok(document)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_is_nfc_trimmed_and_unicode_whitespace_collapsed() {
        assert_eq!(
            normalize_word_bank_entry("  Cafe\u{301}\u{2003}Name  ").unwrap(),
            "Café Name"
        );
    }

    #[test]
    fn casefold_dedup_is_unicode_aware_and_existing_display_wins() {
        let bank = WordBankDocument::from_entries(17, ["Straße", "Vagus"]).unwrap();
        let merged = bank.add(["STRASSE", "strasse"]).unwrap();
        assert_eq!(merged.entries(), &["Straße", "Vagus"]);
        assert_eq!(merged.revision(), 17);

        let edited = bank.edit("STRASSE", "STRASSE").unwrap();
        assert_eq!(edited.entries(), &["STRASSE", "Vagus"]);
        assert_eq!(edited.revision(), 18);
    }

    #[test]
    fn insertion_order_never_changes_document_or_digest() {
        let permutations = [
            ["Gamma", "alpha", "ALPHA", "Beta"],
            ["ALPHA", "Beta", "alpha", "Gamma"],
            ["Beta", "Gamma", "ALPHA", "alpha"],
            ["alpha", "Gamma", "Beta", "ALPHA"],
        ];
        let expected = WordBankDocument::from_entries(4, permutations[0]).unwrap();
        for permutation in permutations {
            let actual = WordBankDocument::from_entries(4, permutation).unwrap();
            assert_eq!(actual, expected);
            assert_eq!(
                actual.canonical_json().unwrap(),
                expected.canonical_json().unwrap()
            );
        }
    }

    #[test]
    fn revision_changes_only_with_canonical_content() {
        let bank = WordBankDocument::from_entries(8, ["Alpha"]).unwrap();
        assert_eq!(bank.replace([" alpha "]).unwrap().revision(), 8);
        assert_eq!(bank.add(["Beta"]).unwrap().revision(), 9);
        assert_eq!(bank.clear().unwrap().revision(), 9);
    }

    #[test]
    fn rejects_injection_and_limits() {
        assert_eq!(
            normalize_word_bank_entry("line\nbreak"),
            Err(WordBankError::ControlCharacter)
        );
        assert_eq!(
            normalize_word_bank_entry("safe\u{202e}spoof"),
            Err(WordBankError::BidiControl)
        );
        assert_eq!(
            normalize_word_bank_entry(&"x".repeat(MAX_WORD_BANK_ENTRY_SCALARS + 1)),
            Err(WordBankError::EntryTooLong)
        );

        let oversized: Vec<String> = (0..MAX_WORD_BANK_ENTRIES)
            .map(|index| format!("entry-{index:04}-{}", "x".repeat(52)))
            .collect();
        assert_eq!(
            WordBankDocument::from_entries(1, oversized),
            Err(WordBankError::DocumentTooLarge)
        );
    }

    #[test]
    fn deserialize_verifies_digest_and_recanonicalizes_order() {
        let bank = WordBankDocument::from_entries(2, ["Beta", "Alpha"]).unwrap();
        let mut value = serde_json::to_value(&bank).unwrap();
        value["entries"] = serde_json::json!(["Beta", "Alpha"]);
        let loaded: WordBankDocument = serde_json::from_value(value).unwrap();
        assert_eq!(loaded, bank);

        let mut corrupt = serde_json::to_value(&bank).unwrap();
        corrupt["content_digest"] = serde_json::json!("not-the-digest");
        assert!(serde_json::from_value::<WordBankDocument>(corrupt).is_err());
    }

    #[test]
    fn external_fingerprint_is_keyed_and_contains_no_entry_text() {
        let bank = WordBankDocument::from_entries(1, ["SyntheticUniqueTerm"]).unwrap();
        let first = bank.external_fingerprint(&DigestKey::new([1; 32])).unwrap();
        let second = bank.external_fingerprint(&DigestKey::new([2; 32])).unwrap();
        assert_ne!(first, second);
        assert!(!first.as_str().contains("SyntheticUniqueTerm"));
    }
}
