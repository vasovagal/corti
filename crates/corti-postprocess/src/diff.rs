use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;

const DEFAULT_MAX_MATRIX_CELLS: usize = 4_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffSpan {
    Equal(String),
    Insert(String),
    Delete(String),
    Replace { deleted: String, inserted: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextDiff {
    spans: Vec<DiffSpan>,
}

impl TextDiff {
    pub fn spans(&self) -> &[DiffSpan] {
        &self.spans
    }

    pub fn is_changed(&self) -> bool {
        self.spans
            .iter()
            .any(|span| !matches!(span, DiffSpan::Equal(_)))
    }

    /// Reconstruct the exact old input; useful for asserting that no code point was lost.
    pub fn old_text(&self) -> String {
        let mut text = String::new();
        for span in &self.spans {
            match span {
                DiffSpan::Equal(value) | DiffSpan::Delete(value) => text.push_str(value),
                DiffSpan::Replace { deleted, .. } => text.push_str(deleted),
                DiffSpan::Insert(_) => {}
            }
        }
        text
    }

    /// Reconstruct the exact new input; applying a diff never needs access to provider/runtime state.
    pub fn new_text(&self) -> String {
        let mut text = String::new();
        for span in &self.spans {
            match span {
                DiffSpan::Equal(value) | DiffSpan::Insert(value) => text.push_str(value),
                DiffSpan::Replace { inserted, .. } => text.push_str(inserted),
                DiffSpan::Delete(_) => {}
            }
        }
        text
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum DiffError {
    #[error("diff input exceeds the deterministic in-memory cell limit")]
    TooLarge,
}

/// Unicode-aware deterministic diff with a bounded in-memory LCS table.
pub fn diff(old: &str, new: &str) -> Result<TextDiff, DiffError> {
    diff_with_limit(old, new, DEFAULT_MAX_MATRIX_CELLS)
}

pub fn diff_with_limit(
    old: &str,
    new: &str,
    max_matrix_cells: usize,
) -> Result<TextDiff, DiffError> {
    if old == new {
        return Ok(TextDiff {
            spans: (!old.is_empty())
                .then(|| DiffSpan::Equal(old.to_owned()))
                .into_iter()
                .collect(),
        });
    }

    let old_tokens = tokenize(old);
    let new_tokens = tokenize(new);
    let rows = old_tokens.len().checked_add(1).ok_or(DiffError::TooLarge)?;
    let columns = new_tokens.len().checked_add(1).ok_or(DiffError::TooLarge)?;
    let cells = rows.checked_mul(columns).ok_or(DiffError::TooLarge)?;
    if cells > max_matrix_cells {
        return Err(DiffError::TooLarge);
    }

    let mut lcs = vec![0u32; cells];
    for old_index in (0..old_tokens.len()).rev() {
        for new_index in (0..new_tokens.len()).rev() {
            let index = old_index * columns + new_index;
            lcs[index] = if old_tokens[old_index] == new_tokens[new_index] {
                lcs[(old_index + 1) * columns + new_index + 1].saturating_add(1)
            } else {
                lcs[(old_index + 1) * columns + new_index]
                    .max(lcs[old_index * columns + new_index + 1])
            };
        }
    }

    // On an LCS tie, delete first. This one rule makes ambiguous diffs stable across runs/platforms.
    let mut operations = Vec::with_capacity(old_tokens.len() + new_tokens.len());
    let (mut old_index, mut new_index) = (0, 0);
    while old_index < old_tokens.len() || new_index < new_tokens.len() {
        if old_index < old_tokens.len()
            && new_index < new_tokens.len()
            && old_tokens[old_index] == new_tokens[new_index]
        {
            operations.push(TokenOperation::Equal(old_tokens[old_index]));
            old_index += 1;
            new_index += 1;
        } else if old_index < old_tokens.len()
            && (new_index == new_tokens.len()
                || lcs[(old_index + 1) * columns + new_index]
                    >= lcs[old_index * columns + new_index + 1])
        {
            operations.push(TokenOperation::Delete(old_tokens[old_index]));
            old_index += 1;
        } else {
            operations.push(TokenOperation::Insert(new_tokens[new_index]));
            new_index += 1;
        }
    }

    Ok(TextDiff {
        spans: coalesce(operations),
    })
}

fn tokenize(text: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    for boundary in text.split_word_bounds() {
        if boundary.is_empty() {
            continue;
        }
        if boundary.chars().any(char::is_alphanumeric) || boundary.chars().all(char::is_whitespace)
        {
            tokens.push(boundary);
        } else {
            // Punctuation, symbols, and emoji are compared at extended-grapheme granularity.
            tokens.extend(boundary.graphemes(true));
        }
    }
    tokens
}

#[derive(Debug, Clone, Copy)]
enum TokenOperation<'a> {
    Equal(&'a str),
    Insert(&'a str),
    Delete(&'a str),
}

fn coalesce(operations: Vec<TokenOperation<'_>>) -> Vec<DiffSpan> {
    let mut spans = Vec::new();
    let mut deleted = String::new();
    let mut inserted = String::new();

    let flush_change = |spans: &mut Vec<DiffSpan>, deleted: &mut String, inserted: &mut String| {
        if deleted.is_empty() && inserted.is_empty() {
            return;
        }
        let deleted_value = std::mem::take(deleted);
        let inserted_value = std::mem::take(inserted);
        spans.push(
            match (deleted_value.is_empty(), inserted_value.is_empty()) {
                (false, false) => DiffSpan::Replace {
                    deleted: deleted_value,
                    inserted: inserted_value,
                },
                (false, true) => DiffSpan::Delete(deleted_value),
                (true, false) => DiffSpan::Insert(inserted_value),
                (true, true) => unreachable!(),
            },
        );
    };

    for operation in operations {
        match operation {
            TokenOperation::Equal(value) => {
                flush_change(&mut spans, &mut deleted, &mut inserted);
                match spans.last_mut() {
                    Some(DiffSpan::Equal(existing)) => existing.push_str(value),
                    _ => spans.push(DiffSpan::Equal(value.to_owned())),
                }
            }
            TokenOperation::Delete(value) => deleted.push_str(value),
            TokenOperation::Insert(value) => inserted.push_str(value),
        }
    }
    flush_change(&mut spans, &mut deleted, &mut inserted);
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separates_unicode_words_punctuation_and_whitespace() {
        let result = diff("Hello, café!", "Hello; café?").unwrap();
        assert_eq!(result.old_text(), "Hello, café!");
        assert_eq!(result.new_text(), "Hello; café?");
        assert!(result.is_changed());
        assert!(result.spans().iter().any(|span| matches!(
            span,
            DiffSpan::Replace { deleted, inserted }
                if deleted.contains(',') && inserted.contains(';')
        )));
    }

    #[test]
    fn grapheme_clusters_are_never_split() {
        let result = diff("Status 👍🏽", "Status ✅").unwrap();
        assert!(result.spans().iter().any(|span| matches!(
            span,
            DiffSpan::Replace { deleted, inserted }
                if deleted == "👍🏽" && inserted == "✅"
        )));
    }

    #[test]
    fn property_style_reconstruction_and_determinism() {
        let samples = [
            "",
            "Alpha",
            "Alpha beta",
            "Alpha, beta!",
            "  spaced  text ",
            "Cafe\u{301}",
            "🧪 test",
            "line-like — punctuation",
        ];
        for old in samples {
            for new in samples {
                let first = diff(old, new).unwrap();
                let second = diff(old, new).unwrap();
                assert_eq!(first, second, "non-deterministic pair");
                assert_eq!(first.old_text(), old);
                assert_eq!(first.new_text(), new);
                assert_eq!(first.is_changed(), old != new);
            }
        }
    }

    #[test]
    fn memory_bound_fails_before_allocating_matrix() {
        assert_eq!(
            diff_with_limit("one two three", "four five six", 4),
            Err(DiffError::TooLarge)
        );
    }
}
