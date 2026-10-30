//! One idempotent substitution pass over text.

use corti_transcribe::segment::{PositionedEdits, TextRule, TracedText};

use crate::document::{LexiconDocument, LexiconError, LexiconRule};

/// A rule rendered for the hosted prompt: the pair the paid rewrite should apply as well.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Correction {
    pub from: String,
    pub to: String,
}

struct CompiledRule {
    id: String,
    /// The `from` text as chars, lowercased when the rule is case-insensitive.
    pattern: Vec<char>,
    to: String,
    case_insensitive: bool,
}

/// The lexicon ready to apply: rules sorted longest-first so a phrase beats the words inside it,
/// matched left to right at word boundaries, each match replaced once and never re-examined, so
/// applying the result again changes nothing. Compilation refuses a rule set under which some rule's
/// replacement would itself be rewritten.
pub struct CompiledLexicon {
    rules: Vec<CompiledRule>,
    revision: u64,
    digest: String,
}

impl std::fmt::Debug for CompiledLexicon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledLexicon")
            .field("rule_count", &self.rules.len())
            .field("revision", &self.revision)
            .field("digest", &self.digest)
            .finish()
    }
}

impl CompiledLexicon {
    pub fn compile(document: &LexiconDocument) -> Result<Self, LexiconError> {
        let mut rules: Vec<CompiledRule> = document
            .rules()
            .iter()
            .map(|rule| CompiledRule {
                id: rule.id.clone(),
                pattern: if rule.case_insensitive {
                    rule.from.chars().map(lower_char).collect()
                } else {
                    rule.from.chars().collect()
                },
                to: rule.to.clone(),
                case_insensitive: rule.case_insensitive,
            })
            .collect();
        rules.sort_by_key(|rule| std::cmp::Reverse(rule.pattern.len()));
        let compiled = Self {
            rules,
            revision: document.revision(),
            digest: document.content_digest().to_owned(),
        };
        // A replacement that some rule would rewrite again is refused: a second pass over the same
        // text must be a no-op. A rule whose pattern matches its own replacement without changing it
        // (`vagus` → `Vagus`) is a fixed point and fine.
        for rule in document.rules() {
            if compiled.apply(&rule.to).is_some() {
                return Err(LexiconError::SelfMatching(rule.id.clone()));
            }
        }
        Ok(compiled)
    }

    pub fn empty() -> Self {
        Self {
            rules: Vec::new(),
            revision: 0,
            digest: LexiconDocument::empty().content_digest().to_owned(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// The rules as prompt corrections (in application order).
    pub fn corrections(&self) -> Vec<Correction> {
        self.rules
            .iter()
            .map(|rule| Correction {
                from: rule.pattern.iter().collect(),
                to: rule.to.clone(),
            })
            .collect()
    }

    /// Apply every rule once, left to right. `Some` only when the text changed.
    pub fn apply(&self, text: &str) -> Option<String> {
        let (out, applied) = self.apply_counting(text);
        (applied > 0 && out != text).then_some(out)
    }

    /// [`apply`](Self::apply) plus how many matches were replaced.
    pub fn apply_counting(&self, text: &str) -> (String, usize) {
        self.apply_counting_inner(text, None)
    }

    /// Actual winning replacements, with input/output UTF-8 offsets and selected rule IDs.
    /// A capped trace can be incomplete; the substitution output remains identical to apply.
    pub fn apply_traced(&self, text: &str) -> TracedText {
        let mut edits = PositionedEdits::default();
        let (out, applied) = self.apply_counting_inner(text, Some(&mut edits));
        TracedText {
            replacement: (applied > 0 && out != text).then_some(out),
            edits,
        }
    }

    fn apply_counting_inner(
        &self,
        text: &str,
        mut edits: Option<&mut PositionedEdits>,
    ) -> (String, usize) {
        if self.rules.is_empty() || text.is_empty() {
            return (text.to_owned(), 0);
        }
        let chars: Vec<char> = text.chars().collect();
        let lower: Vec<char> = chars.iter().map(|c| lower_char(*c)).collect();
        let mut out = String::with_capacity(text.len());
        let mut applied = 0usize;
        let mut index = 0usize;
        let mut source_byte = 0usize;
        while index < chars.len() {
            let at_boundary = index == 0 || !is_word_char(chars[index - 1]);
            let matched = at_boundary
                .then(|| {
                    self.rules.iter().find(|rule| {
                        let end = index + rule.pattern.len();
                        if end > chars.len() {
                            return false;
                        }
                        let haystack = if rule.case_insensitive {
                            &lower[index..end]
                        } else {
                            &chars[index..end]
                        };
                        haystack == rule.pattern.as_slice()
                            && (end == chars.len() || !is_word_char(chars[end]))
                    })
                })
                .flatten();
            match matched {
                Some(rule) => {
                    let end = index + rule.pattern.len();
                    let source_end = source_byte
                        + chars[index..end]
                            .iter()
                            .map(|c| c.len_utf8())
                            .sum::<usize>();
                    let output_start = out.len();
                    out.push_str(&adapt_case(
                        &rule.to,
                        &chars[index..end],
                        rule.case_insensitive,
                    ));
                    if let Some(edits) = &mut edits
                        && text[source_byte..source_end] != out[output_start..]
                    {
                        edits.lexicon(source_byte..source_end, output_start..out.len(), &rule.id);
                    }
                    source_byte = source_end;
                    applied += 1;
                    index = end;
                }
                None => {
                    out.push(chars[index]);
                    source_byte += chars[index].len_utf8();
                    index += 1;
                }
            }
        }
        (out, applied)
    }

    /// The ids of the rules that would fire on `text`, for the review tool's dry runs.
    pub fn matching_rule_ids(&self, text: &str) -> Vec<&str> {
        let mut ids = Vec::new();
        for rule in &self.rules {
            let one = CompiledLexicon {
                rules: vec![CompiledRule {
                    id: rule.id.clone(),
                    pattern: rule.pattern.clone(),
                    to: rule.to.clone(),
                    case_insensitive: rule.case_insensitive,
                }],
                revision: self.revision,
                digest: self.digest.clone(),
            };
            if one.apply(text).is_some() {
                ids.push(rule.id.as_str());
            }
        }
        ids
    }
}

impl TextRule for CompiledLexicon {
    fn apply(&self, text: &str) -> Option<String> {
        CompiledLexicon::apply(self, text)
    }

    fn apply_traced(&self, text: &str) -> TracedText {
        CompiledLexicon::apply_traced(self, text)
    }
}

impl TryFrom<&LexiconDocument> for CompiledLexicon {
    type Error = LexiconError;

    fn try_from(document: &LexiconDocument) -> Result<Self, Self::Error> {
        Self::compile(document)
    }
}

impl From<&LexiconRule> for Correction {
    fn from(rule: &LexiconRule) -> Self {
        Self {
            from: rule.from.clone(),
            to: rule.to.clone(),
        }
    }
}

fn lower_char(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '\'' || c == '’'
}

/// Shape the replacement like the matched text when the rule is case-insensitive: an all-caps match
/// yields an all-caps replacement, a capitalised match capitalises the replacement, anything else is
/// used as written (so a rule can deliberately fix case, `vagus` → `Vagus`).
fn adapt_case(to: &str, matched: &[char], case_insensitive: bool) -> String {
    if !case_insensitive {
        return to.to_owned();
    }
    let letters: Vec<char> = matched
        .iter()
        .copied()
        .filter(|c| c.is_alphabetic())
        .collect();
    if letters.len() > 1 && letters.iter().all(|c| c.is_uppercase()) {
        return to.to_uppercase();
    }
    let matched_capitalised = letters.first().is_some_and(|c| c.is_uppercase())
        && letters.iter().skip(1).all(|c| !c.is_uppercase());
    let to_starts_lower = to.chars().next().is_some_and(char::is_lowercase);
    if matched_capitalised && to_starts_lower {
        let mut chars = to.chars();
        return match chars.next() {
            Some(first) => first.to_uppercase().chain(chars).collect(),
            None => String::new(),
        };
    }
    to.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::RuleSource;

    fn rule(from: &str, to: &str) -> LexiconRule {
        LexiconRule::new(from, to, RuleSource::Review, "t").unwrap()
    }

    fn lexicon(rules: Vec<LexiconRule>) -> CompiledLexicon {
        CompiledLexicon::compile(&LexiconDocument::from_rules(1, rules).unwrap()).unwrap()
    }

    #[test]
    fn phrases_beat_words_and_replacements_adapt_case_at_word_boundaries() {
        let lexicon = lexicon(vec![
            rule("settle ment", "settlement"),
            rule("settle ment gateway", "SettlementGateway"),
            rule("vagus", "Vagus"),
            rule("corty", "Corti"),
        ]);
        assert_eq!(
            lexicon.apply("The settle ment gateway talks to vagus and corty."),
            Some("The SettlementGateway talks to Vagus and Corti.".to_owned())
        );
        assert_eq!(
            lexicon.apply("CORTY is loud; Corty is capitalised; corty is not."),
            Some("CORTI is loud; Corti is capitalised; Corti is not.".to_owned()),
            "a rule whose `to` fixes case applies it to the lowercase match too"
        );
        assert_eq!(lexicon.apply("The vagusnerve is not the product."), None);
        assert_eq!(lexicon.apply("unchanged text"), None);
        let (out, count) = lexicon.apply_counting("vagus vagus");
        assert_eq!((out.as_str(), count), ("Vagus Vagus", 2));
    }

    #[test]
    fn a_pass_is_idempotent_and_self_matching_sets_are_refused() {
        let lexicon = lexicon(vec![rule("mac book", "MacBook"), rule("the the", "the")]);
        let once = lexicon.apply("the the mac book and the mac book").unwrap();
        assert_eq!(once, "the MacBook and the MacBook");
        assert_eq!(lexicon.apply(&once), None, "a second pass changes nothing");

        let cyclic =
            LexiconDocument::from_rules(1, vec![rule("alpha", "beta"), rule("beta", "alpha")])
                .unwrap();
        assert!(matches!(
            CompiledLexicon::compile(&cyclic),
            Err(LexiconError::SelfMatching(_))
        ));
        let self_hit =
            LexiconDocument::from_rules(1, vec![rule("gateway", "gateway service")]).unwrap();
        assert!(
            matches!(
                CompiledLexicon::compile(&self_hit),
                Err(LexiconError::SelfMatching(_))
            ),
            "`gateway service` contains `gateway` again"
        );
    }

    #[test]
    fn corrections_and_matching_ids_are_reported() {
        let document = LexiconDocument::from_rules(2, vec![rule("k eight s", "k8s")]).unwrap();
        let lexicon = CompiledLexicon::compile(&document).unwrap();
        assert_eq!(
            lexicon.corrections(),
            vec![Correction {
                from: "k eight s".into(),
                to: "k8s".into()
            }]
        );
        assert_eq!(
            lexicon.matching_rule_ids("we run on K eight S"),
            vec![document.rules()[0].id.as_str()]
        );
        assert!(lexicon.matching_rule_ids("we run on k8s").is_empty());
        assert_eq!(lexicon.revision(), 2);
        assert_eq!(lexicon.digest(), document.content_digest());
        assert!(CompiledLexicon::empty().is_empty());
    }

    #[test]
    fn trace_records_selected_phrase_winners_and_utf8_output_offsets() {
        let document = LexiconDocument::from_rules(
            1,
            vec![
                rule("settle ment", "settlement"),
                rule("settle ment gateway", "SettlementGateway"),
                rule("corty", "Corti"),
            ],
        )
        .unwrap();
        let lexicon = CompiledLexicon::compile(&document).unwrap();
        let before = "é settle ment gateway and CORTY";
        let trace = lexicon.apply_traced(before);
        assert_eq!(trace.replacement, lexicon.apply(before));
        assert_eq!(trace.edits.gap(), None);
        assert_eq!(trace.edits.edits().len(), 2);
        let mut left = 0;
        let mut right = 0;
        let after = trace.replacement.as_ref().unwrap();
        for edit in trace.edits.edits() {
            assert_eq!(
                &before[left..edit.before.start as usize],
                &after[right..edit.after.start as usize]
            );
            let corti_transcribe::segment::EditKind::Lexicon { rule_id } = &edit.action else {
                panic!("unexpected category")
            };
            let expected = if edit.before.start == 3 {
                "settle ment gateway"
            } else {
                "corty"
            };
            assert_eq!(
                rule_id,
                &document
                    .rules()
                    .iter()
                    .find(|r| r.from == expected)
                    .unwrap()
                    .id
            );
            left = edit.before.end as usize;
            right = edit.after.end as usize;
        }
        assert_eq!(&before[left..], &after[right..]);
        assert!(lexicon.apply_traced(after).edits.edits().is_empty());
    }

    #[test]
    fn actual_correct_and_incorrect_rust_hint_reconstruct_the_original_input() {
        use corti_core::{Speaker, TranscriptSegment};
        use corti_transcribe::evidence::*;
        use corti_transcribe::segment::CleanupConfig;
        let document = LexiconDocument::from_rules(1, vec![rule("rust", "REST")]).unwrap();
        let lexicon = CompiledLexicon::compile(&document).unwrap();
        for before in ["Use the rust endpoint.", "We write this service in rust."] {
            let config = CleanupConfig {
                echo_drop: false,
                merge_gap_seconds: 0.0,
                drop_backchannels: false,
                strip_fillers: false,
                ..CleanupConfig::default()
            };
            let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
            let original = TranscriptSegment {
                speaker: Speaker::Me,
                start: 1.234,
                end: 2.987,
                text: before.into(),
            };
            let result = ledger.cleanup(
                RecordingIdentity {
                    window: 8,
                    kind: StageKind::TrueAsr,
                    identity: Identity {
                        configuration_hash: Some(lexicon.digest().into()),
                        model_hash: None,
                        runtime_hash: None,
                    },
                    signal: SignalIdentity {
                        source: SignalSource::OfflineInput,
                        aec: AecOrigin::None,
                        source_rate_hz: Some(48_000),
                        lookahead_frames: None,
                    },
                },
                vec![original],
                &config,
                CleanupInput {
                    origins: &[],
                    source_links: None,
                    carry: &[],
                    carry_links: None,
                    protected_input_indices: &[],
                },
                None,
                Some(&lexicon),
            );
            let recorded = result.evidence.unwrap();
            assert_eq!(recorded.retained, Append::Retained);
            assert_eq!(result.segments[0].text, before.replace("rust", "REST"));
            let mut bytes = Vec::new();
            ledger.write_json(&mut bytes).unwrap();
            let loaded = Ledger::read_json(&bytes, DEFAULT_CAP).unwrap();
            assert_eq!(
                loaded
                    .reconstruct_input(recorded.final_stage.unwrap())
                    .unwrap()[0]
                    .segment
                    .text,
                before
            );
            let edit = &loaded.groups().last().unwrap().stages[0].decisions[0].edits[0];
            let corti_transcribe::segment::EditKind::Lexicon { rule_id } = &edit.action else {
                panic!("unexpected category")
            };
            assert_eq!(rule_id, &document.rules()[0].id);
            assert_eq!(
                &before[edit.before.start as usize..edit.before.end as usize],
                "rust"
            );
        }
    }
}
