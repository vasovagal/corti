//! Private persistence boundary for the learned correction lexicon.
//!
//! Rules, digests, limits and the substitution pass live in runtime-free `corti-lexicon`. This module
//! only publishes the canonical JSON crash-safely as mode 0600 at `~/.local/share/corti/lexicon.json`,
//! decides whether the lexicon is switched on (hosted.toml `lexicon_enabled`), and never logs rule text.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use corti_lexicon::{CompiledLexicon, LexiconDocument, LexiconRule, MAX_LEXICON_BYTES};
use serde_json::{Map, Value};

use crate::private_file::{atomic_write_private, read_private};

pub(crate) fn lexicon_path() -> Result<PathBuf> {
    Ok(corti_queue::data_dir()?.join("lexicon.json"))
}

pub(crate) fn load() -> Result<LexiconDocument> {
    load_at(&lexicon_path()?)
}

pub(crate) fn load_at(path: &Path) -> Result<LexiconDocument> {
    let Some(bytes) = read_private(path, "lexicon", MAX_LEXICON_BYTES)? else {
        return Ok(LexiconDocument::empty());
    };
    LexiconDocument::from_json(&bytes)
        .with_context(|| format!("parsing lexicon {}", path.display()))
}

pub(crate) fn save_at(path: &Path, document: &LexiconDocument) -> Result<()> {
    let bytes = document
        .canonical_json()
        .context("serializing canonical lexicon")?;
    atomic_write_private(path, &bytes, "lexicon")
}

/// Whether hosted.toml switches the lexicon on. An unreadable document defaults to on: the switch
/// exists to turn corrections off deliberately, not to lose them to a typo elsewhere in the file.
pub(crate) fn enabled() -> bool {
    crate::postprocess_config::HostedPreferences::load()
        .map(|preferences| preferences.values().lexicon_enabled)
        .unwrap_or(true)
}

/// The compiled lexicon for the cleanup and prompt paths: `None` when it is switched off, absent, or
/// empty. An unreadable or self-matching document is logged (without rule text) and treated as absent
/// so transcription never fails because of a hand-edited corrections file.
pub(crate) fn load_compiled() -> Option<CompiledLexicon> {
    if !enabled() {
        return None;
    }
    let document = match load() {
        Ok(document) => document,
        Err(error) => {
            tracing::warn!(
                target: "corti::lexicon",
                error = %format!("{error:#}"),
                "lexicon is unreadable; transcripts run without learned corrections"
            );
            return None;
        }
    };
    if document.rules().is_empty() {
        return None;
    }
    match CompiledLexicon::compile(&document) {
        Ok(compiled) => Some(compiled),
        Err(error) => {
            tracing::warn!(
                target: "corti::lexicon",
                error = %error,
                "lexicon cannot be compiled; transcripts run without learned corrections"
            );
            None
        }
    }
}

/// The lexicon as a note-frontmatter provenance value: `null` when no rules apply, else its revision,
/// digest and rule count (never the rules themselves).
pub(crate) fn provenance_summary() -> Value {
    match load_compiled() {
        Some(lexicon) => {
            let mut map = Map::new();
            map.insert("revision".into(), Value::from(lexicon.revision()));
            map.insert("digest".into(), Value::String(lexicon.digest().to_owned()));
            map.insert(
                "rule_count".into(),
                Value::from(lexicon.rule_count() as u64),
            );
            Value::Object(map)
        }
        None => Value::Null,
    }
}

/// Add one rule to the document at `path` after re-reading it and checking it is still at
/// `expected_revision` (another process may have written in between). The new document is compiled
/// before it is written so a self-matching rule never lands on disk. Returns the saved document.
pub(crate) fn add_rule_checked(
    path: &Path,
    rule: LexiconRule,
    expected_revision: u64,
) -> Result<LexiconDocument> {
    let current = load_at(path)?;
    anyhow::ensure!(
        current.revision() == expected_revision,
        "the lexicon changed on disk (revision {} now, {} when the review started)",
        current.revision(),
        expected_revision
    );
    let next = current.with_rule(rule).context("adding the rule")?;
    CompiledLexicon::compile(&next).context("the rule set would not apply cleanly")?;
    save_at(path, &next)?;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use corti_lexicon::RuleSource;
    use std::os::unix::fs::PermissionsExt;

    fn test_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "corti-lexicon-persistence-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("lexicon.json")
    }

    fn rule(from: &str, to: &str) -> LexiconRule {
        LexiconRule::new(from, to, RuleSource::Manual, "2026-09-24T00:00:00Z").unwrap()
    }

    #[test]
    fn canonical_document_round_trips_at_mode_0600_and_missing_is_empty() {
        let path = test_path("round-trip");
        assert!(load_at(&path).unwrap().rules().is_empty());
        let document = LexiconDocument::from_rules(7, vec![rule("corty", "Corti")]).unwrap();
        save_at(&path, &document).unwrap();
        assert_eq!(load_at(&path).unwrap(), document);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn checked_add_refuses_a_stale_revision_and_a_self_matching_rule() {
        let path = test_path("checked-add");
        let document = LexiconDocument::from_rules(1, vec![rule("corty", "Corti")]).unwrap();
        save_at(&path, &document).unwrap();
        let saved = add_rule_checked(&path, rule("vagos", "Vagus"), 1).unwrap();
        assert_eq!(saved.revision(), 2);
        assert_eq!(saved.rules().len(), 2);
        assert!(
            add_rule_checked(&path, rule("k eight s", "k8s"), 1).is_err(),
            "stale"
        );
        assert!(
            add_rule_checked(&path, rule("gateway", "gateway service"), 2).is_err(),
            "self-matching"
        );
        assert_eq!(load_at(&path).unwrap().revision(), 2, "nothing was written");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn corrupt_digest_is_rejected() {
        let path = test_path("digest");
        let document = LexiconDocument::from_rules(1, vec![rule("corty", "Corti")]).unwrap();
        let mut value = serde_json::to_value(&document).unwrap();
        value["content_digest"] = "not-the-canonical-digest".into();
        atomic_write_private(&path, &serde_json::to_vec(&value).unwrap(), "lexicon").unwrap();
        assert!(load_at(&path).is_err());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
