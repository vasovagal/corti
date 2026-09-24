//! `corti-lexicon` — the learned correction lexicon.
//!
//! ASR keeps making the same mistakes for the same speaker: a product name it has never seen, a
//! colleague's surname, a phrase it always hears as something else. The lexicon is the owner's list of
//! those corrections, applied deterministically as the **last** cleanup pass on every transcript row
//! (batch and live, through `corti_transcribe::segment::TextRule`) and rendered into the hosted prompt
//! prefix so the paid rewrite applies them too.
//!
//! - [`LexiconDocument`]: the persisted, digest-verified rule list (`~/.local/share/corti/lexicon.json`).
//! - [`CompiledLexicon`]: one idempotent left-to-right pass, longest phrase first, case-preserving.
//! - [`heuristics`]: what a transcript review flags as worth a look (near-misses to known terms,
//!   spelling variants, unknown capitalised words, filler residue).
//! - [`review`]: the scripted-IO session that turns those flags into rules, one accepted rule at a time.
//!
//! Nothing here touches the filesystem or the network; the app owns persistence.

#![forbid(unsafe_code)]

mod compiled;
mod document;
pub mod heuristics;
pub mod review;

pub use compiled::{CompiledLexicon, Correction};
pub use document::{
    LEXICON_SCHEMA, LexiconDocument, LexiconError, LexiconRule, MAX_LEXICON_BYTES,
    MAX_RULE_TEXT_CHARS, MAX_RULES, RuleKind, RuleSource,
};
pub use heuristics::{Suspect, SuspectKind, find_suspects};
pub use review::{Decision, ReviewIo, ReviewSummary, run_review};
