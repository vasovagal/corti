//! Private persistence boundary for the canonical hosted spelling bank.
//!
//! Normalization, revisioning, digest verification, and size limits live in runtime-free
//! `corti-postprocess`. This module only publishes that canonical JSON crash-safely as mode 0600 and never
//! logs entries or serialized bytes.

// This phase intentionally lands the boundary before coordinator/Settings wiring.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use corti_postprocess::{MAX_WORD_BANK_BYTES, WordBankDocument};

use crate::private_file::{atomic_write_private, read_private};

pub(crate) fn word_bank_path() -> Result<PathBuf> {
    Ok(corti_queue::data_dir()?.join("word-bank.json"))
}

pub(crate) fn load() -> Result<WordBankDocument> {
    load_at(&word_bank_path()?)
}

pub(crate) fn save(document: &WordBankDocument) -> Result<()> {
    save_at(&word_bank_path()?, document)
}

fn load_at(path: &Path) -> Result<WordBankDocument> {
    let Some(bytes) = read_private(path, "word bank", MAX_WORD_BANK_BYTES)? else {
        return Ok(WordBankDocument::empty());
    };
    serde_json::from_slice(&bytes).with_context(|| format!("parsing word bank {}", path.display()))
}

fn save_at(path: &Path, document: &WordBankDocument) -> Result<()> {
    let bytes = document
        .canonical_json()
        .context("serializing canonical word bank")?;
    atomic_write_private(path, &bytes, "word bank")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn test_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "corti-word-bank-persistence-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("word-bank.json")
    }

    #[test]
    fn canonical_document_round_trips_at_mode_0600() {
        let path = test_path("round-trip");
        let document =
            WordBankDocument::from_entries(7, ["Synthetic Alpha", "Synthetic Beta"]).unwrap();
        save_at(&path, &document).unwrap();

        assert_eq!(load_at(&path).unwrap(), document);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            document.canonical_json().unwrap()
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn missing_document_is_empty_at_revision_zero() {
        let path = test_path("missing");
        std::fs::remove_file(&path).ok();
        let loaded = load_at(&path).unwrap();
        assert!(loaded.entries().is_empty());
        assert_eq!(loaded.revision(), 0);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn corrupt_digest_is_rejected_instead_of_becoming_prompt_input() {
        let path = test_path("digest");
        let mut value =
            serde_json::to_value(WordBankDocument::from_entries(1, ["Synthetic term"]).unwrap())
                .unwrap();
        value["content_digest"] = "not-the-canonical-digest".into();
        atomic_write_private(&path, &serde_json::to_vec(&value).unwrap(), "word bank").unwrap();

        assert!(load_at(&path).is_err());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
