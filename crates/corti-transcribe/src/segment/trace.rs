//! Bounded positioned edits emitted by the existing text algorithms. Never logs content.

use serde::{Deserialize, Serialize};

/// Maximum owned positioned-edit payload per row. Overflow preserves text output and explicitly
/// makes its trace incomplete. Filler origin scratch has a separate equal bound and is dropped.
pub const TEXT_TRACE_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteSpan {
    pub start: u32,
    pub end: u32,
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EditKind {
    Lexicon { rule_id: String },
    Filler,
    Stutter,
    Case,
    Whitespace,
}

/// UTF-8 byte offsets into this invocation's original/replacement rows, respectively.
/// ```compile_fail
/// fn requires_clone<T: Clone>() {}
/// requires_clone::<corti_transcribe::segment::TextEdit>();
/// ```
/// ```compile_fail
/// fn requires_debug<T: std::fmt::Debug>() {}
/// requires_debug::<corti_transcribe::segment::TextEdit>();
/// ```
#[derive(PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextEdit {
    pub before: ByteSpan,
    pub after: ByteSpan,
    pub action: EditKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceGap {
    UnsupportedRule,
    Capacity,
}

#[derive(Default, PartialEq, Eq)]
pub struct PositionedEdits {
    edits: Vec<TextEdit>,
    gap: Option<TraceGap>,
    rule_bytes: usize,
}

impl PositionedEdits {
    // Only the pre-existing CleanupDecision::clone compatibility path may duplicate this payload.
    // The new content-bearing trace/evidence types deliberately expose no Clone implementation.
    pub(super) fn legacy_copy(&self) -> Self {
        let edits = self
            .edits
            .iter()
            .map(|edit| TextEdit {
                before: edit.before,
                after: edit.after,
                action: match &edit.action {
                    EditKind::Lexicon { rule_id } => EditKind::Lexicon {
                        rule_id: rule_id.clone(),
                    },
                    EditKind::Filler => EditKind::Filler,
                    EditKind::Stutter => EditKind::Stutter,
                    EditKind::Case => EditKind::Case,
                    EditKind::Whitespace => EditKind::Whitespace,
                },
            })
            .collect();
        Self {
            edits,
            gap: self.gap,
            rule_bytes: self.rule_bytes,
        }
    }
    pub fn edits(&self) -> &[TextEdit] {
        &self.edits
    }
    pub fn gap(&self) -> Option<TraceGap> {
        self.gap
    }
    pub fn owned_bytes(&self) -> usize {
        self.edits.capacity() * std::mem::size_of::<TextEdit>() + self.rule_bytes
    }
    pub(crate) fn into_edits(self) -> Vec<TextEdit> {
        self.edits
    }
    pub(crate) fn unavailable(&mut self, cause: TraceGap) {
        self.gap.get_or_insert(cause);
        self.edits = Vec::new();
        self.rule_bytes = 0;
    }
    /// Called only at the selected rule branch, never by a candidate/dry-run matcher.
    pub fn lexicon(
        &mut self,
        before: std::ops::Range<usize>,
        after: std::ops::Range<usize>,
        rule_id: &str,
    ) {
        if self.gap.is_some() {
            return;
        }
        if rule_id.is_empty() || rule_id.len() > 128 || !self.room(rule_id.len()) {
            self.unavailable(TraceGap::Capacity);
            return;
        }
        self.push(
            before,
            after,
            EditKind::Lexicon {
                rule_id: rule_id.to_owned(),
            },
        );
    }
    fn room(&self, extra: usize) -> bool {
        (self.edits.len() + 1)
            .saturating_mul(std::mem::size_of::<TextEdit>())
            .saturating_add(self.rule_bytes)
            .saturating_add(extra)
            <= TEXT_TRACE_BYTES
    }
    pub(crate) fn push(
        &mut self,
        before: std::ops::Range<usize>,
        after: std::ops::Range<usize>,
        action: EditKind,
    ) {
        if self.gap.is_some() {
            return;
        }
        let extra = match &action {
            EditKind::Lexicon { rule_id } => rule_id.capacity(),
            _ => 0,
        };
        if before.end > u32::MAX as usize || after.end > u32::MAX as usize || !self.room(extra) {
            self.unavailable(TraceGap::Capacity);
            return;
        }
        self.edits.reserve_exact(1);
        if self.owned_bytes().saturating_add(extra) > TEXT_TRACE_BYTES {
            self.unavailable(TraceGap::Capacity);
            return;
        }
        self.rule_bytes += extra;
        self.edits.push(TextEdit {
            before: ByteSpan {
                start: before.start as u32,
                end: before.end as u32,
            },
            after: ByteSpan {
                start: after.start as u32,
                end: after.end as u32,
            },
            action,
        });
    }
}

pub struct TracedText {
    pub replacement: Option<String>,
    pub edits: PositionedEdits,
}

#[derive(Clone, Copy)]
enum TokenState {
    Kept,
    Filler,
    Stutter,
}
struct Origin {
    start: usize,
    end: usize,
    state: TokenState,
}

/// Tracks actual token choices, not an output-text join. No copied token strings are stored here.
pub(super) struct FillerTrace {
    origins: Vec<Origin>,
    kept: Vec<usize>,
    complete: bool,
}

impl FillerTrace {
    pub(super) fn new() -> Self {
        Self {
            origins: Vec::new(),
            kept: Vec::new(),
            complete: true,
        }
    }
    fn scratch_bytes(&self) -> usize {
        self.origins.capacity() * std::mem::size_of::<Origin>()
            + self.kept.capacity() * std::mem::size_of::<usize>()
    }
    fn stop(&mut self) {
        self.complete = false;
        self.origins = Vec::new();
        self.kept = Vec::new();
    }
    pub(super) fn token(&mut self, text: &str, raw: &str) {
        if !self.complete {
            return;
        }
        if self.scratch_bytes() + std::mem::size_of::<Origin>() + std::mem::size_of::<usize>()
            > TEXT_TRACE_BYTES
        {
            self.stop();
            return;
        }
        self.origins.reserve_exact(1);
        self.kept.reserve_exact(1);
        if self.scratch_bytes() > TEXT_TRACE_BYTES {
            self.stop();
            return;
        }
        let start = raw.as_ptr() as usize - text.as_ptr() as usize;
        self.origins.push(Origin {
            start,
            end: start + raw.len(),
            state: TokenState::Kept,
        });
    }
    pub(super) fn filler(&mut self) {
        if let Some(last) = self.origins.last_mut() {
            last.state = TokenState::Filler;
        }
    }
    pub(super) fn stutter(&mut self, replace_previous: bool, pop_previous: bool) {
        if !self.complete {
            return;
        }
        let current = self.origins.len() - 1;
        self.origins[current].state = TokenState::Stutter;
        if replace_previous {
            if let Some(previous) = self.kept.last_mut() {
                self.origins[*previous].state = TokenState::Stutter;
                *previous = current;
                self.origins[current].state = TokenState::Kept;
            }
        } else if pop_previous && let Some(previous) = self.kept.pop() {
            self.origins[previous].state = TokenState::Stutter;
        }
    }
    pub(super) fn keep(&mut self) {
        if self.complete {
            self.kept.push(self.origins.len() - 1);
        }
    }
    pub(super) fn finish(self, before: &str, after: &str) -> PositionedEdits {
        let mut edits = PositionedEdits::default();
        if !self.complete {
            edits.unavailable(TraceGap::Capacity);
            return edits;
        }
        let (mut left, mut right) = (0, 0);
        let mut output_tokens = after.split_whitespace();
        for origin in self.origins {
            let output = if matches!(origin.state, TokenState::Kept) {
                output_tokens.next()
            } else {
                None
            };
            let start = output.map_or(right, |token| {
                token.as_ptr() as usize - after.as_ptr() as usize
            });
            if before[left..origin.start] != after[right..start] {
                edits.push(left..origin.start, right..start, EditKind::Whitespace);
            }
            right = start;
            match output {
                Some(token) => {
                    trace_kept(
                        &mut edits,
                        &before[origin.start..origin.end],
                        token,
                        origin.start,
                        right,
                    );
                    right += token.len();
                }
                None => edits.push(
                    origin.start..origin.end,
                    right..right,
                    match origin.state {
                        TokenState::Stutter => EditKind::Stutter,
                        _ => EditKind::Filler,
                    },
                ),
            }
            left = origin.end;
        }
        if before[left..] != after[right..] {
            edits.push(left..before.len(), right..after.len(), EditKind::Whitespace);
        }
        edits
    }
}

fn trace_kept(edits: &mut PositionedEdits, before: &str, after: &str, left: usize, right: usize) {
    if before == after {
        return;
    }
    let (mut bs, mut os) = (0, 0);
    if let Some(first) = before.chars().next() {
        for mapped in [
            first.to_uppercase().collect::<String>(),
            first.to_lowercase().collect::<String>(),
        ] {
            if !mapped.is_empty()
                && mapped != before[..first.len_utf8()]
                && after.starts_with(&mapped)
            {
                bs = first.len_utf8();
                os = mapped.len();
                edits.push(left..left + bs, right..right + os, EditKind::Case);
                break;
            }
        }
    }
    // The algorithm's only other mutation of a kept token is its trailing comma/sentence mark.
    let common = before[bs..]
        .chars()
        .zip(after[os..].chars())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum::<usize>();
    bs += common;
    os += common;
    if bs != before.len() || os != after.len() {
        edits.push(
            left + bs..left + before.len(),
            right + os..right + after.len(),
            EditKind::Filler,
        );
    }
}
