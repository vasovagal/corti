//! What a transcript review flags: cheap, deterministic, no model.

use std::collections::{BTreeMap, HashSet};

use corti_core::TranscriptSegment;
use unicode_segmentation::UnicodeSegmentation;

use crate::compiled::CompiledLexicon;

/// Upper bound on suspects one review reports, so a long call stays reviewable.
pub const MAX_SUSPECTS: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuspectKind {
    /// An unknown token within an edit or two of a known term (word bank or a lexicon `to`).
    NearMiss { term: String },
    /// Two unknown capitalised spellings within one edit of each other in the same note.
    SpellingVariant { other: String },
    /// A capitalised word that is not at a sentence start and not known, seen more than once.
    UnknownCapitalized,
    /// Filler noise still present (the filler pass was off or predates the note).
    FillerResidue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suspect {
    /// Index into the reviewed segments where the token was first seen.
    pub segment_index: usize,
    /// The token as it appears in the transcript.
    pub token: String,
    pub kind: SuspectKind,
    /// How many times the token appears across the note.
    pub occurrences: usize,
    /// The first segment's text, for the prompt.
    pub context: String,
}

const FILLERS: &[&str] = &["um", "umm", "uh", "uhh", "ah", "er", "erm", "hmm"];

const COMMON_CAPITALISED: &[&str] = &[
    "i",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
    "ok",
    "okay",
    "yeah",
    "yes",
    "no",
    "thanks",
    "hi",
    "hello",
    "hey",
    "bye",
    "good",
    "great",
    "right",
    "sure",
    "well",
    "so",
    "and",
    "but",
    "the",
    "a",
    "an",
    "we",
    "you",
    "they",
    "it",
    "he",
    "she",
    "this",
    "that",
    "there",
    "here",
    "what",
    "how",
    "why",
    "when",
    "where",
    "who",
    "which",
    "if",
    "then",
    "also",
    "just",
    "like",
    "let's",
    "let",
    "oh",
    "um",
    "uh",
];

/// Optimal string alignment distance (Damerau–Levenshtein with adjacent transpositions).
pub fn damerau_levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = best;
        }
    }
    d[n][m]
}

/// Lowercase letters/digits plus internal `-`/`'`; empty when the token has no letters.
pub fn normalize_word(raw: &str) -> String {
    let kept: String = raw
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '\'' || *c == '’')
        .flat_map(char::to_lowercase)
        .collect();
    let trimmed = kept.trim_matches(|c| c == '-' || c == '\'' || c == '’');
    if trimmed.chars().any(char::is_alphabetic) {
        trimmed.to_owned()
    } else {
        String::new()
    }
}

struct Seen {
    first_segment: usize,
    display: String,
    count: usize,
    capitalised_mid_sentence: usize,
    context: String,
}

/// Flag what is worth the owner's attention in `segments`. `known_terms` are the word bank entries;
/// the lexicon's own targets are known too, and anything the lexicon already corrects is skipped
/// (it is not a suspect, it is a fix).
pub fn find_suspects(
    segments: &[TranscriptSegment],
    known_terms: &[String],
    lexicon: &CompiledLexicon,
) -> Vec<Suspect> {
    let corrected: Vec<String> = lexicon
        .corrections()
        .iter()
        .map(|correction| correction.to.clone())
        .collect();
    let known: Vec<(String, String)> = known_terms
        .iter()
        .chain(corrected.iter())
        .filter_map(|term| {
            let norm = normalize_word(term);
            (!norm.is_empty() && !norm.contains(' ')).then(|| (norm, term.clone()))
        })
        .collect();
    let known_set: HashSet<&str> = known.iter().map(|(norm, _)| norm.as_str()).collect();
    let corrected_from: HashSet<String> = lexicon
        .corrections()
        .iter()
        .map(|correction| correction.from.to_lowercase())
        .collect();

    let mut seen: BTreeMap<String, Seen> = BTreeMap::new();
    let mut filler_count = 0usize;
    let mut filler_first: Option<(usize, String, String)> = None;
    for (index, segment) in segments.iter().enumerate() {
        let mut sentence_start = true;
        for raw in segment.text.unicode_words() {
            let norm = normalize_word(raw);
            if norm.is_empty() {
                continue;
            }
            if FILLERS.contains(&norm.as_str()) {
                filler_count += 1;
                filler_first.get_or_insert_with(|| (index, raw.to_owned(), segment.text.clone()));
                continue;
            }
            let entry = seen.entry(norm.clone()).or_insert_with(|| Seen {
                first_segment: index,
                display: raw.to_owned(),
                count: 0,
                capitalised_mid_sentence: 0,
                context: segment.text.clone(),
            });
            entry.count += 1;
            if !sentence_start && raw.chars().next().is_some_and(char::is_uppercase) {
                entry.capitalised_mid_sentence += 1;
            }
            sentence_start = false;
        }
        // A segment ends a sentence for the next segment's purposes; inside a segment the decoder's
        // own terminal punctuation marks the boundary.
        let _ = sentence_start;
    }
    // Re-walk to honour in-segment sentence boundaries for the capitalisation signal: `unicode_words`
    // strips punctuation, so use raw whitespace tokens for that one bit.
    for entry in seen.values_mut() {
        entry.capitalised_mid_sentence = 0;
    }
    for segment in segments {
        let mut sentence_start = true;
        for raw in segment.text.split_whitespace() {
            let norm = normalize_word(raw);
            if norm.is_empty() {
                continue;
            }
            if let Some(entry) = seen.get_mut(&norm)
                && !sentence_start
                && raw.chars().next().is_some_and(char::is_uppercase)
            {
                entry.capitalised_mid_sentence += 1;
            }
            sentence_start = raw.ends_with(['.', '?', '!']);
        }
    }

    let mut suspects: Vec<Suspect> = Vec::new();
    let unknown: Vec<(&String, &Seen)> = seen
        .iter()
        .filter(|(norm, _)| !known_set.contains(norm.as_str()) && !corrected_from.contains(*norm))
        .collect();
    for (norm, entry) in &unknown {
        if norm.chars().count() < 4 {
            continue;
        }
        let limit = if norm.chars().count() < 6 { 1 } else { 2 };
        if let Some((_, term)) = known
            .iter()
            .filter(|(candidate, _)| candidate != *norm)
            .map(|(candidate, term)| (damerau_levenshtein(norm, candidate), term))
            .filter(|(distance, _)| *distance <= limit)
            .min_by_key(|(distance, _)| *distance)
        {
            suspects.push(Suspect {
                segment_index: entry.first_segment,
                token: entry.display.clone(),
                kind: SuspectKind::NearMiss { term: term.clone() },
                occurrences: entry.count,
                context: entry.context.clone(),
            });
        }
    }
    let flagged: HashSet<String> = suspects.iter().map(|s| normalize_word(&s.token)).collect();
    let name_like: Vec<(&String, &Seen)> = unknown
        .iter()
        .filter(|(norm, entry)| {
            norm.chars().count() >= 5
                && entry.capitalised_mid_sentence > 0
                && !COMMON_CAPITALISED.contains(&norm.as_str())
        })
        .copied()
        .collect();
    for (index, (norm, entry)) in name_like.iter().enumerate() {
        if flagged.contains(*norm) {
            continue;
        }
        let variant = name_like
            .iter()
            .enumerate()
            .filter(|(other_index, _)| *other_index != index)
            .map(|(_, (other_norm, other_entry))| {
                (damerau_levenshtein(norm, other_norm), other_entry)
            })
            .filter(|(distance, _)| *distance == 1)
            .min_by_key(|(distance, _)| *distance);
        if let Some((_, other)) = variant {
            if other.count >= entry.count {
                suspects.push(Suspect {
                    segment_index: entry.first_segment,
                    token: entry.display.clone(),
                    kind: SuspectKind::SpellingVariant {
                        other: other.display.clone(),
                    },
                    occurrences: entry.count,
                    context: entry.context.clone(),
                });
            }
            continue;
        }
        if entry.capitalised_mid_sentence >= 2 {
            suspects.push(Suspect {
                segment_index: entry.first_segment,
                token: entry.display.clone(),
                kind: SuspectKind::UnknownCapitalized,
                occurrences: entry.count,
                context: entry.context.clone(),
            });
        }
    }
    if let Some((segment_index, token, context)) = filler_first {
        suspects.push(Suspect {
            segment_index,
            token,
            kind: SuspectKind::FillerResidue,
            occurrences: filler_count,
            context,
        });
    }
    suspects.sort_by_key(|suspect| (suspect.segment_index, suspect.token.to_lowercase()));
    suspects.truncate(MAX_SUSPECTS);
    suspects
}

#[cfg(test)]
mod tests {
    use super::*;
    use corti_core::Speaker;

    fn seg(text: &str) -> TranscriptSegment {
        TranscriptSegment {
            speaker: Speaker::Other("Them".into()),
            start: 0.0,
            end: 1.0,
            text: text.into(),
        }
    }

    #[test]
    fn distance_counts_transpositions_as_one() {
        assert_eq!(damerau_levenshtein("vagus", "vagus"), 0);
        assert_eq!(damerau_levenshtein("vagus", "vaugs"), 1);
        assert_eq!(damerau_levenshtein("corti", "corty"), 1);
        assert_eq!(damerau_levenshtein("corti", "cortex"), 2);
        assert_eq!(damerau_levenshtein("", "abc"), 3);
    }

    #[test]
    fn near_misses_variants_capitalised_unknowns_and_fillers_are_flagged() {
        let segments = vec![
            seg("We shipped Corty yesterday and Vagos indexed it."),
            seg("Um, the Zephyrine team asked about Zephyrina again."),
            seg("Zephyrine owns the rollout, Zephyrine said so."),
            seg("The gateway is fine."),
        ];
        let known = vec!["Corti".to_owned(), "Vagus".to_owned()];
        let suspects = find_suspects(&segments, &known, &CompiledLexicon::empty());
        let kinds: Vec<(&str, &SuspectKind)> = suspects
            .iter()
            .map(|s| (s.token.as_str(), &s.kind))
            .collect();
        assert!(kinds.iter().any(|(token, kind)| *token == "Corty"
            && matches!(kind, SuspectKind::NearMiss { term } if term == "Corti")));
        assert!(kinds.iter().any(|(token, kind)| *token == "Vagos"
            && matches!(kind, SuspectKind::NearMiss { term } if term == "Vagus")));
        assert!(
            kinds.iter().any(|(token, kind)| *token == "Zephyrina"
                && matches!(kind, SuspectKind::SpellingVariant { other } if other == "Zephyrine")),
            "{kinds:?}"
        );
        assert!(
            kinds
                .iter()
                .any(|(token, kind)| *token == "Um" && matches!(kind, SuspectKind::FillerResidue))
        );
        assert!(
            !kinds.iter().any(|(token, _)| *token == "gateway"),
            "an ordinary lowercase word is not a suspect"
        );
        assert!(
            !kinds.iter().any(|(token, _)| *token == "We"),
            "a sentence-initial capital is not a name"
        );
    }

    #[test]
    fn corrected_tokens_are_no_longer_suspects() {
        let document = crate::LexiconDocument::from_rules(
            1,
            vec![
                crate::LexiconRule::new("corty", "Corti", crate::RuleSource::Review, "t").unwrap(),
            ],
        )
        .unwrap();
        let lexicon = CompiledLexicon::compile(&document).unwrap();
        let suspects = find_suspects(&[seg("Corty shipped.")], &[], &lexicon);
        assert!(suspects.is_empty(), "{suspects:?}");
    }
}
