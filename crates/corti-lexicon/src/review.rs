//! The review session: walk the suspects, ask, and persist each accepted rule immediately.
//!
//! The session is pure over an injected [`ReviewIo`] so it is testable with scripted answers; the CLI
//! supplies a terminal implementation and a `persist` closure that re-reads the document, checks the
//! revision it last saw, and writes atomically.

use crate::document::{LexiconRule, RuleSource};
use crate::heuristics::{Suspect, SuspectKind};

/// What the owner decided about one suspect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The transcript is right (or not worth a rule).
    Keep,
    /// Add a rule replacing the suspect token with this text.
    Rule { to: String },
    /// Add a rule with an explicit `from` (a phrase around the token) and `to`.
    RuleFrom { from: String, to: String },
    /// Skip every remaining suspect of the same kind.
    SkipKind,
    /// Stop the session.
    Quit,
}

pub trait ReviewIo {
    /// Present one suspect (`index` of `total`) and return the decision.
    fn decide(&mut self, suspect: &Suspect, index: usize, total: usize) -> Decision;
    /// Show a one-line message (a saved rule, a refused one).
    fn notify(&mut self, message: &str);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReviewSummary {
    pub reviewed: usize,
    pub kept: usize,
    pub rules_added: usize,
    pub skipped: usize,
    pub refused: usize,
    pub quit: bool,
}

/// Run the session. `persist` receives each accepted rule and returns `Err(reason)` when it could not
/// be saved (a stale revision, a self-matching rule); the session reports it and continues.
pub fn run_review(
    suspects: &[Suspect],
    io: &mut dyn ReviewIo,
    now: &str,
    mut persist: impl FnMut(LexiconRule) -> Result<(), String>,
) -> ReviewSummary {
    let mut summary = ReviewSummary::default();
    let mut skipped_kinds: Vec<std::mem::Discriminant<SuspectKind>> = Vec::new();
    let total = suspects.len();
    for (index, suspect) in suspects.iter().enumerate() {
        if skipped_kinds.contains(&std::mem::discriminant(&suspect.kind)) {
            summary.skipped += 1;
            continue;
        }
        summary.reviewed += 1;
        match io.decide(suspect, index + 1, total) {
            Decision::Keep => summary.kept += 1,
            Decision::Rule { to } => {
                accept(
                    &mut summary,
                    io,
                    &suspect.token,
                    &to,
                    suspect,
                    now,
                    &mut persist,
                );
            }
            Decision::RuleFrom { from, to } => {
                accept(&mut summary, io, &from, &to, suspect, now, &mut persist);
            }
            Decision::SkipKind => {
                skipped_kinds.push(std::mem::discriminant(&suspect.kind));
                summary.skipped += 1;
            }
            Decision::Quit => {
                summary.quit = true;
                summary.skipped += total - index - 1;
                break;
            }
        }
    }
    summary
}

fn accept(
    summary: &mut ReviewSummary,
    io: &mut dyn ReviewIo,
    from: &str,
    to: &str,
    suspect: &Suspect,
    now: &str,
    persist: &mut impl FnMut(LexiconRule) -> Result<(), String>,
) {
    let rule = LexiconRule::new(from, to, RuleSource::Review, now)
        .and_then(|rule| rule.with_example(suspect.context.clone()));
    match rule {
        Ok(rule) => match persist(rule) {
            Ok(()) => {
                summary.rules_added += 1;
                io.notify(&format!("saved: {from} → {to}"));
            }
            Err(reason) => {
                summary.refused += 1;
                io.notify(&format!("not saved ({reason}): {from} → {to}"));
            }
        },
        Err(error) => {
            summary.refused += 1;
            io.notify(&format!("not saved ({error}): {from} → {to}"));
        }
    }
}

/// A one-line description of a suspect for terminal prompts.
pub fn describe(suspect: &Suspect) -> String {
    let what = match &suspect.kind {
        SuspectKind::NearMiss { term } => {
            format!("looks like a misspelling of known term \"{term}\"")
        }
        SuspectKind::SpellingVariant { other } => format!("also spelled \"{other}\" in this note"),
        SuspectKind::UnknownCapitalized => "capitalised mid-sentence but unknown".to_owned(),
        SuspectKind::FillerResidue => "filler noise still present".to_owned(),
    };
    format!("\"{}\" ×{} — {what}", suspect.token, suspect.occurrences)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scripted {
        answers: Vec<Decision>,
        notices: Vec<String>,
    }

    impl ReviewIo for Scripted {
        fn decide(&mut self, _suspect: &Suspect, _index: usize, _total: usize) -> Decision {
            self.answers.remove(0)
        }

        fn notify(&mut self, message: &str) {
            self.notices.push(message.to_owned());
        }
    }

    fn suspect(token: &str, kind: SuspectKind) -> Suspect {
        Suspect {
            segment_index: 0,
            token: token.into(),
            kind,
            occurrences: 1,
            context: format!("context with {token}"),
        }
    }

    #[test]
    fn decisions_persist_rules_immediately_skip_kinds_and_quit() {
        let suspects = vec![
            suspect(
                "Corty",
                SuspectKind::NearMiss {
                    term: "Corti".into(),
                },
            ),
            suspect("Um", SuspectKind::FillerResidue),
            suspect("Uh", SuspectKind::FillerResidue),
            suspect(
                "Vagos",
                SuspectKind::NearMiss {
                    term: "Vagus".into(),
                },
            ),
            suspect("Zephyrina", SuspectKind::UnknownCapitalized),
            suspect("Never", SuspectKind::UnknownCapitalized),
        ];
        let mut io = Scripted {
            answers: vec![
                Decision::Rule { to: "Corti".into() },
                Decision::SkipKind,
                Decision::RuleFrom {
                    from: "vagos index".into(),
                    to: "Vagus index".into(),
                },
                Decision::Keep,
                Decision::Quit,
            ],
            notices: Vec::new(),
        };
        let mut saved = Vec::new();
        let summary = run_review(&suspects, &mut io, "2026-09-24T00:00:00Z", |rule| {
            if rule.from == "vagos index" {
                return Err("stale revision".into());
            }
            saved.push(rule);
            Ok(())
        });
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].from, "Corty");
        assert_eq!(saved[0].to, "Corti");
        assert_eq!(saved[0].examples, vec!["context with Corty".to_owned()]);
        assert_eq!(
            summary,
            ReviewSummary {
                reviewed: 5,
                kept: 1,
                rules_added: 1,
                skipped: 2,
                refused: 1,
                quit: true,
            },
            "{summary:?}"
        );
        assert!(io.notices[0].starts_with("saved: Corty → Corti"));
        assert!(io.notices[1].starts_with("not saved (stale revision)"));
        assert!(describe(&suspects[0]).contains("misspelling of known term \"Corti\""));
    }
}
