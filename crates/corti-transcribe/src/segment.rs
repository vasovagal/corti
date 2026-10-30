//! Shared reconciliation helpers: turn timestamped words into a [`DiarizedTranscript`]'s segments.
//!
//! Both backends produce per-word timing and need the same downstream shaping:
//! - group one speaker's words into pause-split utterances
//!   ([`words_to_segments`](crate::segment::words_to_segments)),
//! - merge multiple speakers onto one timeline ([`merge_by_time`](crate::segment::merge_by_time)),
//! - and, for the local backend, attribute far-end words to diarization turns and segment them in one pass
//!   ([`diarize_words`](crate::segment::diarize_words)).
//!
//! [`cleanup`](crate::segment::cleanup) then runs on that merged timeline: the one place in corti where the
//! two capture channels can see each other, so cross-channel echo, fragmentation and backchannels are
//! removed before anything downstream (note, LLM tier) sees a row.
//!
//! The AWS backend feeds channel-identified words (ch0 = me, ch1 = them); the local backend feeds Parakeet
//! words (ch0 = me) plus ch1 words attributed to pyannote speaker turns.

use std::collections::BTreeSet;

use corti_core::{Speaker, TranscriptSegment};

/// A single recognized word with absolute timestamps (seconds from the start of the recording).
#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

/// The actual rendered rows and ranges emitted by the same word loop. Ranges index the source
/// formed by joining nonempty words with single spaces. None means the bounded trace is unavailable;
/// rendered transcript output is still complete. Content deliberately has no Debug/Clone.
/// ```compile_fail
/// fn requires_debug<T: std::fmt::Debug>() {}
/// requires_debug::<corti_transcribe::segment::SegmentedRows>();
/// ```
/// ```compile_fail
/// fn requires_clone<T: Clone>() {}
/// requires_clone::<corti_transcribe::segment::SegmentedRows>();
/// ```
pub struct SegmentedRows {
    pub segments: Vec<TranscriptSegment>,
    pub source_ranges: Option<Vec<ByteSpan>>,
}

struct RangeTrace {
    ranges: Option<Vec<ByteSpan>>,
    offset: usize,
}
impl RangeTrace {
    fn new(enabled: bool) -> Self {
        Self {
            ranges: enabled.then(Vec::new),
            offset: 0,
        }
    }
    fn word(&mut self, bytes: usize, continues: bool) {
        let Some(ranges) = self.ranges.as_mut() else {
            return;
        };
        let Some(end) = self
            .offset
            .checked_add(bytes)
            .and_then(|end| u32::try_from(end).ok())
        else {
            self.ranges = None;
            return;
        };
        if continues {
            if let Some(last) = ranges.last_mut() {
                last.end = end;
            }
        } else if ranges.len() < trace::TEXT_TRACE_BYTES / std::mem::size_of::<ByteSpan>() {
            if ranges.len() == ranges.capacity() {
                let limit = trace::TEXT_TRACE_BYTES / std::mem::size_of::<ByteSpan>();
                let capacity = ranges.capacity().max(2).saturating_mul(2).min(limit);
                if ranges.try_reserve_exact(capacity - ranges.len()).is_err()
                    || ranges.capacity() > limit
                {
                    self.ranges = None;
                    return;
                }
            }
            ranges.push(ByteSpan {
                start: self.offset as u32,
                end,
            });
        } else {
            self.ranges = None;
            return;
        }
        if let Some(offset) = (end as usize).checked_add(1) {
            self.offset = offset;
        } else {
            self.ranges = None;
        }
    }
}

/// A diarization speaker turn (seconds), labelled with the display name to attribute overlapping words to.
#[derive(Debug, Clone, PartialEq)]
pub struct SpeakerTurn {
    pub start: f64,
    pub end: f64,
    pub label: String,
}

/// Start a new segment when the gap between consecutive words exceeds this (seconds), so a speaker's
/// stream breaks into readable utterances rather than one run-on blob. (Matches the AWS backend's
/// historical 1.5 s split.)
pub const SEGMENT_GAP: f64 = 1.5;

/// Whether `word` continues a region that ended at `prev_end` rather than opening a new one. The single
/// definition of the pause split, shared by [`words_to_segments`] and [`split_regions`]. A non-comparable
/// timestamp continues nothing, so it breaks the region rather than silently extending it.
fn continues_region(prev_end: f64, word: &Word, gap: f64) -> bool {
    word.start - prev_end <= gap
}

/// Split one speaker's time-ordered `words` into the groups [`words_to_segments`] would render as one
/// segment each — the same pause rule, without committing to a speaker or to rendered text. The live path
/// needs the words themselves: it decides region by region whether to publish or withhold, and a withheld
/// region is published later with its original words and timestamps.
pub fn split_regions(words: &[Word], gap: f64) -> Vec<Vec<Word>> {
    let mut out: Vec<Vec<Word>> = Vec::new();
    for w in words.iter().filter(|w| !w.text.is_empty()) {
        match out.last_mut() {
            Some(region) if continues_region(region_end(region), w, gap) => region.push(w.clone()),
            _ => out.push(vec![w.clone()]),
        }
    }
    out
}

/// The end timestamp of a non-empty region (its last word's end).
fn region_end(region: &[Word]) -> f64 {
    region.last().map_or(f64::NAN, |w| w.end)
}

/// Group one speaker's time-ordered `words` into [`TranscriptSegment`]s, starting a new segment on a pause
/// longer than `gap`. Words are joined with single spaces (punctuation should already be glued onto the
/// word by the caller). Empty words are skipped; empty input yields no segments.
pub fn words_to_segments(words: &[Word], speaker: Speaker, gap: f64) -> Vec<TranscriptSegment> {
    words_to_segments_impl(words, speaker, gap, false).segments
}

/// Optional bounded lineage emitted by the actual grouping loop, without another grouping pass.
pub fn words_to_segments_with_ranges(words: &[Word], speaker: Speaker, gap: f64) -> SegmentedRows {
    words_to_segments_impl(words, speaker, gap, true)
}

fn words_to_segments_impl(
    words: &[Word],
    speaker: Speaker,
    gap: f64,
    traced: bool,
) -> SegmentedRows {
    let mut out: Vec<TranscriptSegment> = Vec::new();
    let mut cur: Option<TranscriptSegment> = None;
    let mut trace = RangeTrace::new(traced);

    for w in words {
        if w.text.is_empty() {
            continue;
        }
        match cur.as_mut() {
            Some(seg) if continues_region(seg.end, w, gap) => {
                trace.word(w.text.len(), true);
                seg.text.push(' ');
                seg.text.push_str(&w.text);
                seg.end = w.end;
            }
            _ => {
                trace.word(w.text.len(), false);
                if let Some(seg) = cur.take() {
                    out.push(seg);
                }
                cur = Some(TranscriptSegment {
                    speaker: speaker.clone(),
                    start: w.start,
                    end: w.end,
                    text: w.text.clone(),
                });
            }
        }
    }
    if let Some(seg) = cur.take() {
        out.push(seg);
    }
    SegmentedRows {
        segments: out,
        source_ranges: trace.ranges,
    }
}

/// Merge segments from multiple speakers into one timeline, sorted by start time. `total_cmp` keeps it
/// panic-free on any odd NaN.
pub fn merge_by_time(mut segments: Vec<TranscriptSegment>) -> Vec<TranscriptSegment> {
    segments.sort_by(|a, b| a.start.total_cmp(&b.start));
    segments
}

/// Attribute each time-ordered word to the diarization turn it overlaps most, then segment in a single
/// pass: start a new segment whenever the attributed speaker changes **or** the pause exceeds `gap`. The
/// result is already a merged, time-ordered timeline of far-end speakers (each labelled
/// [`Speaker::Other`]). A word overlapping no turn takes the nearest turn's label; with no turns at all,
/// every word is attributed to `fallback_label`.
pub fn diarize_words(
    words: &[Word],
    turns: &[SpeakerTurn],
    gap: f64,
    fallback_label: &str,
) -> Vec<TranscriptSegment> {
    diarize_words_impl(words, turns, gap, fallback_label, false).segments
}

/// Optional bounded lineage emitted by the actual speaker-attribution loop, without rerunning it.
pub fn diarize_words_with_ranges(
    words: &[Word],
    turns: &[SpeakerTurn],
    gap: f64,
    fallback_label: &str,
) -> SegmentedRows {
    diarize_words_impl(words, turns, gap, fallback_label, true)
}

fn diarize_words_impl(
    words: &[Word],
    turns: &[SpeakerTurn],
    gap: f64,
    fallback_label: &str,
    traced: bool,
) -> SegmentedRows {
    let mut out: Vec<TranscriptSegment> = Vec::new();
    let mut cur: Option<TranscriptSegment> = None;
    let mut trace = RangeTrace::new(traced);

    for w in words {
        if w.text.is_empty() {
            continue;
        }
        let speaker = Speaker::Other(best_turn_label(w, turns, fallback_label));
        match cur.as_mut() {
            Some(seg) if seg.speaker == speaker && w.start - seg.end <= gap => {
                trace.word(w.text.len(), true);
                seg.text.push(' ');
                seg.text.push_str(&w.text);
                seg.end = w.end;
            }
            _ => {
                trace.word(w.text.len(), false);
                if let Some(seg) = cur.take() {
                    out.push(seg);
                }
                cur = Some(TranscriptSegment {
                    speaker,
                    start: w.start,
                    end: w.end,
                    text: w.text.clone(),
                });
            }
        }
    }
    if let Some(seg) = cur.take() {
        out.push(seg);
    }
    SegmentedRows {
        segments: out,
        source_ranges: trace.ranges,
    }
}

/// The label of the turn a word overlaps most; ties/no-overlap fall back to the nearest turn (by the gap
/// between the word's midpoint and the turn), and no turns at all yields `fallback`.
fn best_turn_label(w: &Word, turns: &[SpeakerTurn], fallback: &str) -> String {
    if turns.is_empty() {
        return fallback.to_string();
    }
    // Prefer the turn with the largest temporal overlap.
    let best_overlap = turns
        .iter()
        .max_by(|a, b| overlap(w, a).total_cmp(&overlap(w, b)));
    if let Some(t) = best_overlap
        && overlap(w, t) > 0.0
    {
        return t.label.clone();
    }
    // No overlap with any turn → nearest turn to the word's midpoint.
    let mid = (w.start + w.end) / 2.0;
    turns
        .iter()
        .min_by(|a, b| turn_distance(mid, a).total_cmp(&turn_distance(mid, b)))
        .map(|t| t.label.clone())
        .unwrap_or_else(|| fallback.to_string())
}

/// Seconds of temporal overlap between a word and a turn (0 if disjoint).
fn overlap(w: &Word, t: &SpeakerTurn) -> f64 {
    (w.end.min(t.end) - w.start.max(t.start)).max(0.0)
}

/// Distance (seconds) from a time point to a turn interval (0 if inside).
fn turn_distance(t: f64, turn: &SpeakerTurn) -> f64 {
    if t < turn.start {
        turn.start - t
    } else if t > turn.end {
        t - turn.end
    } else {
        0.0
    }
}

// ---------------------------------------------------------------------------------------------------
// Deterministic segment cleanup (#149; #107 mitigation #5)
// ---------------------------------------------------------------------------------------------------
//
// Two defects survive ASR and cannot be fixed downstream: the LLM post-processing tier is contractually
// forbidden from dropping or reordering rows (`corti-postprocess`), and the two capture channels never see
// each other before [`merge_by_time`]. So the fix belongs here, on the merged timeline:
//
// 1. **Cross-channel echo.** Residual speaker bleed the AEC could not remove is decoded as a short "Me"
//    utterance sitting inside far-end speech, 1–6 s after the far end said the same words (issue #107).
// 2. **Fragmentation and backchannels.** `words_to_segments` only joins word gaps ≤ [`SEGMENT_GAP`], so an
//    utterance with a longer breath break becomes several one- and two-word rows, and pure "Yeah." rows
//    over the other side's speech add nothing to a transcript.
//
// Everything here is deterministic text/timing arithmetic — no audio, no model, no confidence score (the
// local backend exposes none). Filler and stutter removal (#154) is a per-segment token walk, and the
// learned lexicon is applied through the [`TextRule`] seam so this crate never depends on it.

/// Version of the [`cleanup`] rule set, recorded in a note's `corti.configuration.segment_cleanup`
/// provenance. Bump it whenever a pass's behavior changes, so an old note is never read as if it had
/// today's rules; the individual thresholds are recorded alongside it and are not part of this number.
///
/// - 2: echo → merge → backchannel.
/// - 3: sort by start first (#157), then echo → merge → backchannel → fillers/stutters (#154) → lexicon.
/// - 4: acoustic-only mic echo deletion with critical-word protection and bounded decisions (#161).
pub const CLEANUP_RULES_VERSION: u32 = 4;

/// A deterministic text substitution applied to every surviving segment after the structural passes —
/// the learned correction lexicon in practice. Implementations return `Some(new_text)` only when
/// something changed, and must be idempotent (applying the result again changes nothing).
pub trait TextRule {
    fn apply(&self, text: &str) -> Option<String>;

    /// Additive trace seam. Legacy implementations keep their output and explicitly mark missing
    /// positional/rule evidence; callers must never invent a winning rule ID from a dry run.
    fn apply_traced(&self, text: &str) -> TracedText {
        let replacement = self.apply(text);
        let mut edits = PositionedEdits::default();
        if replacement.is_some() {
            edits.unavailable(TraceGap::UnsupportedRule);
        }
        TracedText { replacement, edits }
    }
}

#[path = "segment/trace.rs"]
mod trace;
pub use trace::{
    ByteSpan, EditKind, PositionedEdits, TEXT_TRACE_BYTES, TextEdit, TraceGap, TracedText,
};

/// Tuning for [`cleanup`]. Defaults are the shipping values; the app persists them under `[cleanup]` in
/// `config.toml` and overrides them with `CORTI_CLEANUP_*`.
#[derive(Debug, Clone, PartialEq)]
pub struct CleanupConfig {
    /// Run the cross-channel echo pass.
    pub echo_drop: bool,
    /// How long after a far-end utterance *starts* its echo may still be decoded on the other channel.
    pub echo_window_seconds: f64,
    /// Fraction of a segment's content tokens that must also appear in the candidate source before the
    /// segment becomes a text-match candidate (segments of three or more content tokens). A candidate
    /// alone never authorizes deletion.
    pub echo_containment: f64,
    /// Largest silence (seconds) between two consecutive same-speaker segments that is still merged into
    /// one utterance. Non-positive disables the merge pass.
    pub merge_gap_seconds: f64,
    /// Run the backchannel pass ("Yeah." over the other side's speech).
    pub drop_backchannels: bool,
    /// How far (dB) a mic span's energy may exceed the AEC's echo estimate for that same span and still be
    /// judged "this was mostly echo". Only consulted when [`cleanup_with_evidence`] is given an
    /// [`AudioEvidence`] accessor; a very negative value switches echo deletion off while retaining
    /// text-match decisions.
    pub echo_audio_margin_db: f32,
    /// Strip marked filler noises (`um`, `uh`, `ah`, `er`, `hmm`) and collapse repeated words and bigrams
    /// (`the the` → `the`, `I think I think` → `I think`) in every surviving segment, repairing the
    /// sentence start and a dangling comma afterwards (#154). A segment left empty is dropped.
    pub strip_fillers: bool,
}

impl Default for CleanupConfig {
    fn default() -> Self {
        Self {
            echo_drop: true,
            // Measured echo lag on real captures is 1–6 s (#107); the window is the outer bound, not a
            // typical value, because containment still has to hold.
            echo_window_seconds: 6.0,
            echo_containment: 0.7,
            // Longer than SEGMENT_GAP (1.5 s) — every fragment visible in a note already has a gap above
            // it — but short enough that a genuine new utterance after a real pause stays its own row.
            merge_gap_seconds: 2.5,
            drop_backchannels: true,
            // Real captures top out around 10.6 dB ERLE (#107), so a mic block that is genuinely nothing
            // but residual echo sits *below* the echo estimate, not above it. +3 dB leaves room for the
            // estimate being one block stale without admitting a span that carries real near-end speech.
            echo_audio_margin_db: 3.0,
            // The largest single noise source in the corpus (#154: ~30 fillers and ~15 stutters per
            // thousand words) and the cheapest rule to apply, so it ships on.
            strip_fillers: true,
        }
    }
}

impl CleanupConfig {
    /// True when no pass would run, so callers can skip [`cleanup`] entirely and record `"off"` in
    /// provenance rather than a set of inert knobs. A [`TextRule`] handed to [`cleanup_with_rules`]
    /// still runs regardless.
    pub fn is_noop(&self) -> bool {
        let merge_off = self.merge_gap_seconds <= 0.0 || self.merge_gap_seconds.is_nan();
        !self.echo_drop && !self.drop_backchannels && merge_off && !self.strip_fillers
    }
}

/// What [`cleanup`] changed. Only counts, safe to log without transcript content. Version 4 retains the
/// old text-deletion counters for source compatibility; they remain zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CleanupStats {
    /// Legacy text-only mic deletion count; always zero under version 4.
    pub echo_dropped_me: usize,
    /// Legacy text-only far-end deletion count; always zero under version 4.
    pub echo_dropped_them: usize,
    /// Segments absorbed into the preceding same-speaker segment.
    pub merged: usize,
    /// Backchannel segments dropped over the other side's speech.
    pub backchannels_dropped: usize,
    /// `Me` segments dropped by the **audio** echo rule — the AEC said the mic block was mostly echo — of
    /// which the text rules would have kept some. A subset of nothing else: these are counted here and not
    /// in [`echo_dropped_me`](Self::echo_dropped_me), so the two signals stay separable in a sweep.
    pub echo_dropped_audio: usize,
    /// Filler tokens removed by the filler pass (#154); a segment emptied by it counts its tokens here
    /// and is dropped.
    pub fillers_removed: usize,
    /// Repeated words and bigrams collapsed by the filler pass.
    pub stutters_collapsed: usize,
    /// Segments changed by the [`TextRule`] (the lexicon).
    pub lexicon_applied: usize,
}

impl CleanupStats {
    /// Total segments removed or absorbed plus tokens and rows rewritten — zero means the transcript
    /// came through untouched.
    pub fn changed(&self) -> usize {
        self.echo_dropped_me
            + self.echo_dropped_them
            + self.merged
            + self.backchannels_dropped
            + self.echo_dropped_audio
            + self.fillers_removed
            + self.stutters_collapsed
            + self.lexicon_applied
    }
}

/// What the acoustic echo canceller measured over one span of the **cleaned** mic timeline — the same
/// timeline a [`TranscriptSegment`]'s `start`/`end` are on.
///
/// This crate deliberately does **not** depend on `corti-aec`: transcription must not pull in a DSP crate,
/// and the AWS backend has no canceller at all. The caller (the app's live loop, or the batch path reading
/// a `-aec-stats.json` sidecar) folds `corti_aec::SpanStats` into this four-field summary.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpanEvidence {
    /// Mean block mic energy over the span, in dB.
    pub mic_db: f32,
    /// Mean block energy of the echo estimate the canceller subtracted over the span, in dB.
    pub echo_estimate_db: f32,
    /// Fraction of the span's blocks in which the adaptation gate fired (the canceller judged the near end
    /// to dominate and froze its filter update).
    pub double_talk_fraction: f32,
    /// How many canceller blocks the span covers. Zero means "no measurement", which is not the same as
    /// "measured silence" — see [`cleanup_with_evidence`].
    pub blocks: usize,
}

/// Accessor handed to [`cleanup_with_evidence`]: given `(start, end)` in seconds on the transcript
/// timeline, the canceller's summary for that span, or `None` where nothing was recorded.
pub type AudioEvidence<'a> = &'a dyn Fn(f64, f64) -> Option<SpanEvidence>;

/// Conservative echo policy shared by batch cleanup and live early-drop. Text alone never deletes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EchoVerdict {
    #[default]
    Keep,
    KeepCriticalDifference,
    SuspectedDuplicate,
    DropAcoustic,
}

/// Content-free source attribution. Times are on the same recording timeline as the subject.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EchoSource {
    pub me: bool,
    pub start: f64,
    pub end: f64,
}

/// An echo verdict and the source that explains it (critical match, suspected match, or overlap).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EchoAssessment {
    pub verdict: EchoVerdict,
    pub source: Option<EchoSource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupStage {
    Sort,
    Echo,
    Merge,
    Backchannel,
    Fillers,
    Lexicon,
    EarlyEcho,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupReason {
    TimeOrder,
    Echo(EchoVerdict),
    SameSpeakerContinuation,
    OverlappingBackchannel,
    FillersOrStutters,
    TextRule,
}

/// One reversible stage decision. Indices refer to this stage's input sequence, not the original
/// recording's sequence; EarlyEcho uses the caller's monotonically increasing region index.
/// Original/replacement rows preserve exact text, speaker, and span. No Debug: this contains text.
#[derive(PartialEq)]
pub struct CleanupDecision {
    pub stage: CleanupStage,
    pub input_indices: Vec<usize>,
    pub originals: Vec<TranscriptSegment>,
    pub replacements: Vec<TranscriptSegment>,
    pub reason: CleanupReason,
    pub evidence: Option<SpanEvidence>,
    pub source: Option<EchoSource>,
    pub edits: PositionedEdits,
}

// Preserve the legacy decision API; new evidence/trace owners deliberately have no Clone API.
impl Clone for CleanupDecision {
    fn clone(&self) -> Self {
        Self {
            stage: self.stage,
            input_indices: self.input_indices.clone(),
            originals: self.originals.clone(),
            replacements: self.replacements.clone(),
            reason: self.reason,
            evidence: self.evidence,
            source: self.source,
            edits: self.edits.legacy_copy(),
        }
    }
}

/// Maximum retained decision payload, including row/record metadata and owned text/speaker bytes.
/// This is a logical payload cap, excluding allocator bookkeeping and transient cleanup output.
pub const CLEANUP_DECISION_BYTES: usize = 16 * 1024 * 1024;

/// Bounded, content-bearing decisions. Overflow omits whole records and increments `omitted`;
/// it never changes cleanup output. Append enforces the same cap across multiple live windows.
#[derive(Clone, Default)]
pub struct CleanupDecisions {
    entries: Vec<CleanupDecision>,
    pub omitted: usize,
    bytes: usize,
}

fn segment_bytes(segment: &TranscriptSegment) -> usize {
    std::mem::size_of::<TranscriptSegment>()
        .saturating_add(segment.text.len())
        .saturating_add(match &segment.speaker {
            Speaker::Me => 0,
            Speaker::Other(label) => label.len(),
        })
}

impl CleanupDecisions {
    pub fn entries(&self) -> &[CleanupDecision] {
        &self.entries
    }

    pub fn into_entries(self) -> Vec<CleanupDecision> {
        self.entries
    }

    /// Move the existing owners into evidence; retain the upstream omission count explicitly.
    pub fn into_parts(self) -> (Vec<CleanupDecision>, usize) {
        (self.entries, self.omitted)
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn append(&mut self, other: Self) {
        self.omitted = self.omitted.saturating_add(other.omitted);
        for entry in other.entries {
            let bytes = std::mem::size_of::<CleanupDecision>()
                .saturating_add(entry.edits.owned_bytes())
                .saturating_add(
                    entry
                        .input_indices
                        .len()
                        .saturating_mul(std::mem::size_of::<usize>()),
                )
                .saturating_add(
                    entry
                        .originals
                        .iter()
                        .chain(&entry.replacements)
                        .map(segment_bytes)
                        .fold(0usize, usize::saturating_add),
                );
            if self.reserve_record(bytes) {
                self.entries.reserve_exact(1);
                self.entries.push(entry);
            }
        }
    }

    fn reserve_record(&mut self, bytes: usize) -> bool {
        if bytes > CLEANUP_DECISION_BYTES.saturating_sub(self.bytes) {
            self.omitted = self.omitted.saturating_add(1);
            false
        } else {
            self.bytes += bytes;
            true
        }
    }

    // Borrow first and check the cap before cloning any content.
    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        stage: CleanupStage,
        indices: &[usize],
        originals: &[&TranscriptSegment],
        replacements: &[&TranscriptSegment],
        reason: CleanupReason,
        evidence: Option<SpanEvidence>,
        source: Option<EchoSource>,
    ) {
        let bytes = std::mem::size_of::<CleanupDecision>()
            .saturating_add(indices.len().saturating_mul(std::mem::size_of::<usize>()))
            .saturating_add(
                originals
                    .iter()
                    .chain(replacements)
                    .map(|s| segment_bytes(s))
                    .fold(0usize, usize::saturating_add),
            );
        if self.reserve_record(bytes) {
            self.entries.reserve_exact(1);
            self.entries.push(CleanupDecision {
                stage,
                input_indices: indices.to_vec(),
                originals: originals.iter().map(|s| (*s).clone()).collect(),
                replacements: replacements.iter().map(|s| (*s).clone()).collect(),
                reason,
                evidence,
                source,
                edits: PositionedEdits::default(),
            });
        }
    }

    fn record_text(
        &mut self,
        stage: CleanupStage,
        index: usize,
        original: &TranscriptSegment,
        replacement: Option<&TranscriptSegment>,
        reason: CleanupReason,
        edits: PositionedEdits,
    ) {
        let needed = std::mem::size_of::<CleanupDecision>()
            + std::mem::size_of::<usize>()
            + segment_bytes(original)
            + replacement.map_or(0, segment_bytes)
            + edits.owned_bytes();
        if needed > CLEANUP_DECISION_BYTES.saturating_sub(self.bytes) {
            self.omitted = self.omitted.saturating_add(1);
            return;
        }
        match replacement {
            Some(value) => self.record(stage, &[index], &[original], &[value], reason, None, None),
            None => self.record(stage, &[index], &[original], &[], reason, None, None),
        }
        self.bytes += edits.owned_bytes();
        if let Some(last) = self.entries.last_mut() {
            last.edits = edits;
        }
    }

    pub fn record_echo(
        &mut self,
        stage: CleanupStage,
        input_index: usize,
        segment: &TranscriptSegment,
        verdict: EchoVerdict,
        evidence: Option<SpanEvidence>,
    ) {
        self.record_echo_assessment(
            stage,
            input_index,
            segment,
            EchoAssessment {
                verdict,
                source: None,
            },
            evidence,
        );
    }

    pub fn record_echo_assessment(
        &mut self,
        stage: CleanupStage,
        input_index: usize,
        segment: &TranscriptSegment,
        assessment: EchoAssessment,
        evidence: Option<SpanEvidence>,
    ) {
        let kept = [segment];
        let replacements: &[&TranscriptSegment] = if assessment.verdict == EchoVerdict::DropAcoustic
        {
            &[]
        } else {
            &kept
        };
        self.record(
            stage,
            &[input_index],
            &[segment],
            replacements,
            CleanupReason::Echo(assessment.verdict),
            evidence,
            assessment.source,
        );
    }
}

/// Cleanup output and its bounded, in-memory explanation. No Debug: decisions contain transcript text.
pub struct CleanupResult {
    pub segments: Vec<TranscriptSegment>,
    pub stats: CleanupStats,
    pub decisions: CleanupDecisions,
}

/// A span whose adaptation gate fired in at least this fraction of its blocks is not evidence of anything:
/// the canceller froze its filter update because it judged the near end to dominate, so its echo estimate
/// is stale for most of the span, and the comparison the audio rule makes is against a number that stopped
/// tracking the room. Such a span is kept regardless of text similarity.
///
/// This is not merely a numerical scruple: "the gate fired" is the canceller's own statement that it heard
/// near-end speech, which is precisely the case in which a `Me` segment must be kept.
const AUDIO_DOUBLE_TALK_LIMIT: f32 = 0.5;

/// Clean one timeline in this order: sort by start (#157: the live path splices `Me` batches into the
/// `Them` stream, and every rule below assumes neighbours in sequence are neighbours in time) → echo →
/// merge → backchannel → fillers/stutters → lexicon. The order matters: dropping an echo first stops it
/// from being merged into a real turn, merging before the backchannel pass protects a "Yeah." that is
/// really the opening of a longer sentence, and the text rewrites run last so the structural passes
/// compare what was actually said.
///
/// `carry` is a read-only set of segments from the previous live window. They are only ever consulted as
/// echo *sources*, never mutated, and never appear in the output — that is how the live path catches an
/// echo whose source landed just before the one-minute append boundary.
///
/// All far-end overlaps can supply acoustic context, and all text matches can protect a correction.
/// Echo deletion cannot chain onto the far-end channel, which is never eligible for acoustic deletion.
pub fn cleanup(
    segments: Vec<TranscriptSegment>,
    cfg: &CleanupConfig,
    carry: &[TranscriptSegment],
) -> (Vec<TranscriptSegment>, CleanupStats) {
    cleanup_with_rules(segments, cfg, carry, None, None)
}

/// [`cleanup`] with the acoustic canceller's per-block record available to the echo pass.
///
/// Text alone cannot separate two populations that look identical: a `Me` row that is residual speaker
/// bleed, and a `Me` row where both people genuinely used the same nouns. `evidence` answers the question
/// text cannot — *was this mic span mostly echo?* — so the echo pass can drop a ghost whose wording is
/// nowhere near the containment threshold.
///
/// Critical raw-text differences in any matched source protect the segment first. Otherwise a segment
/// is dropped only when it is `Me` and all of these hold:
///
/// 1. it overlaps a far-end segment (`S.start < O.end && O.start < S.end`) — the far end had the floor;
/// 2. `evidence(S.start, S.end)` returns a span covering at least one block;
/// 3. that span's adaptation gate fired in fewer than [`AUDIO_DOUBLE_TALK_LIMIT`] of its blocks;
/// 4. `mic_db − echo_estimate_db ≤ echo_audio_margin_db` — the mic carried little more than what the
///    canceller was already subtracting as echo.
///
/// A span with **no blocks** is kept, as is a text-only duplicate. That is the AWS backend (no
/// canceller), a sidecar that does not cover this recording, and a stats ring that overflowed: in all
/// three, missing evidence cannot authorize deletion.
pub fn cleanup_with_evidence(
    segments: Vec<TranscriptSegment>,
    cfg: &CleanupConfig,
    carry: &[TranscriptSegment],
    evidence: Option<AudioEvidence<'_>>,
) -> (Vec<TranscriptSegment>, CleanupStats) {
    cleanup_with_rules(segments, cfg, carry, evidence, None)
}

/// [`cleanup_with_evidence`] plus a [`TextRule`] (the learned lexicon) applied to every surviving segment
/// as the last pass. The rule runs even when `cfg.is_noop()`; callers that want nothing at all simply
/// pass `None`.
pub fn cleanup_with_rules(
    segments: Vec<TranscriptSegment>,
    cfg: &CleanupConfig,
    carry: &[TranscriptSegment],
    evidence: Option<AudioEvidence<'_>>,
    rule: Option<&dyn TextRule>,
) -> (Vec<TranscriptSegment>, CleanupStats) {
    let result = cleanup_with_decisions(segments, cfg, carry, evidence, rule);
    (result.segments, result.stats)
}

/// Rich counterpart of [`cleanup_with_rules`], retaining bounded decisions for every changed stage and
/// every echo candidate. Original spans/text are retained in memory only; callers control persistence.
pub fn cleanup_with_decisions(
    segments: Vec<TranscriptSegment>,
    cfg: &CleanupConfig,
    carry: &[TranscriptSegment],
    evidence: Option<AudioEvidence<'_>>,
    rule: Option<&dyn TextRule>,
) -> CleanupResult {
    cleanup_with_protected_decisions(segments, cfg, carry, evidence, rule, &[])
}

/// Preserve raw text for input rows already protected by an earlier echo assessment. Indices refer to
/// `segments` before sorting, not text or timestamps; equal rows remain distinct. Protection survives
/// merging and automatic backchannel/filler cleanup independently of the bounded decision ledger.
/// Explicit caller-supplied text rules still run last.
pub fn cleanup_with_protected_decisions(
    segments: Vec<TranscriptSegment>,
    cfg: &CleanupConfig,
    carry: &[TranscriptSegment],
    evidence: Option<AudioEvidence<'_>>,
    rule: Option<&dyn TextRule>,
    protected_input_indices: &[usize],
) -> CleanupResult {
    cleanup_impl(
        segments,
        cfg,
        carry,
        evidence,
        rule,
        protected_input_indices,
        false,
    )
}

pub(crate) fn cleanup_with_traced_decisions(
    segments: Vec<TranscriptSegment>,
    cfg: &CleanupConfig,
    carry: &[TranscriptSegment],
    evidence: Option<AudioEvidence<'_>>,
    rule: Option<&dyn TextRule>,
    protected: &[usize],
) -> CleanupResult {
    cleanup_impl(segments, cfg, carry, evidence, rule, protected, true)
}

fn cleanup_impl(
    segments: Vec<TranscriptSegment>,
    cfg: &CleanupConfig,
    carry: &[TranscriptSegment],
    evidence: Option<AudioEvidence<'_>>,
    rule: Option<&dyn TextRule>,
    protected_input_indices: &[usize],
    traced: bool,
) -> CleanupResult {
    let mut stats = CleanupStats::default();
    let mut decisions = CleanupDecisions::default();
    let protected_inputs: BTreeSet<_> = protected_input_indices.iter().copied().collect();
    let mut indexed: Vec<_> = segments
        .into_iter()
        .enumerate()
        .map(|(index, segment)| (index, segment, protected_inputs.contains(&index)))
        .collect();
    indexed.sort_by(|a, b| a.1.start.total_cmp(&b.1.start));
    for (output_index, (input_index, segment, _)) in indexed.iter().enumerate() {
        if output_index != *input_index {
            decisions.record(
                CleanupStage::Sort,
                &[*input_index],
                &[segment],
                &[segment],
                CleanupReason::TimeOrder,
                None,
                None,
            );
        }
    }
    let (mut segments, mut preserve_raw): (Vec<_>, Vec<_>) = indexed
        .into_iter()
        .map(|(_, segment, protected)| (segment, protected))
        .unzip();
    if cfg.echo_drop {
        segments = drop_echoes(
            segments,
            cfg,
            carry,
            evidence,
            &mut stats,
            &mut decisions,
            &mut preserve_raw,
        );
    }
    if cfg.merge_gap_seconds > 0.0 {
        segments = merge_fragments(segments, cfg, &mut stats, &mut decisions, &mut preserve_raw);
    }
    if cfg.drop_backchannels {
        segments = drop_backchannel_turns(segments, &mut stats, &mut decisions, &mut preserve_raw);
    }
    if cfg.strip_fillers {
        segments = strip_fillers_pass(segments, &mut stats, &mut decisions, &preserve_raw, traced);
    }
    if let Some(rule) = rule {
        for (i, segment) in segments.iter_mut().enumerate() {
            let TracedText { replacement, edits } = if traced {
                rule.apply_traced(&segment.text)
            } else {
                let replacement = rule.apply(&segment.text);
                let mut edits = PositionedEdits::default();
                if replacement.is_some() {
                    edits.unavailable(TraceGap::UnsupportedRule);
                }
                TracedText { replacement, edits }
            };
            if let Some(text) = replacement {
                let replacement = TranscriptSegment {
                    speaker: segment.speaker.clone(),
                    start: segment.start,
                    end: segment.end,
                    text,
                };
                decisions.record_text(
                    CleanupStage::Lexicon,
                    i,
                    segment,
                    Some(&replacement),
                    CleanupReason::TextRule,
                    edits,
                );
                *segment = replacement;
                stats.lexicon_applied += 1;
            }
        }
    }
    CleanupResult {
        segments,
        stats,
        decisions,
    }
}

/// Possible filler noises, omitted only from loose candidate matching. Their raw spelling remains in
/// the critical signature: `um` and `mm` may be units even without a neighboring numeric token.
const FILLERS: &[&str] = &[
    "um", "umm", "uh", "uhh", "ah", "aah", "er", "erm", "hm", "hmm", "mm", "mmm", "mhm", "huh",
];

/// The fillers the [`CleanupConfig::strip_fillers`] pass deletes from text. Deliberately narrower than
/// [`FILLERS`]: `mm`/`mhm`/`huh` can be a whole (affirmative or questioning) turn, and a backchannel
/// row is the backchannel pass's business, not this one's.
const STRIP_FILLERS: &[&str] = &[
    "um", "umm", "ummm", "uh", "uhh", "uhhh", "ah", "aah", "er", "erm", "hm", "hmm", "hmmm",
];

/// Strip marked fillers and collapse stutters in one segment's text (#154). Unmarked medial filler
/// spellings are retained because they may be units or identifiers. Returns the new text (possibly
/// empty) and how many filler tokens and repeated words/bigrams went. The walk is token-by-token so
/// punctuation glued to a filler is not lost: a filler's sentence-ending mark moves to the word before
/// it, a comma before a filler is dropped so "I was, uh, thinking" reads "I was thinking", the sentence
/// start is re-capitalised when the opening filler went, and a capital the decoder left on the word
/// after a mid-sentence filler ("I think, um, We should") is lowered.
pub fn strip_fillers_text(text: &str) -> (String, usize, usize) {
    strip_fillers_inner(text, None)
}

/// The existing filler/stutter pass plus positions from its actual token choices. Trace overflow
/// never changes the returned text/counts and is explicit in PositionedEdits::gap.
pub fn strip_fillers_text_traced(text: &str) -> (String, usize, usize, PositionedEdits) {
    let mut trace = trace::FillerTrace::new();
    let (output, fillers, stutters) = strip_fillers_inner(text, Some(&mut trace));
    let edits = trace.finish(text, &output);
    (output, fillers, stutters, edits)
}

fn strip_fillers_inner(
    text: &str,
    mut trace: Option<&mut trace::FillerTrace>,
) -> (String, usize, usize) {
    let mut kept: Vec<String> = Vec::new();
    let mut kept_norm: Vec<String> = Vec::new();
    let mut contexts = raw_token_context(text).peekable();
    let mut first_can_recase = true;
    let mut fillers = 0usize;
    let mut stutters = 0usize;
    let mut after_filler = false;
    for (index, raw) in text.split_whitespace().enumerate() {
        if let Some(trace) = &mut trace {
            trace.token(text, raw);
        }
        let (protected, can_recase) = contexts
            .next_if(|context| context.index == index)
            .map_or((false, true), |context| {
                (context.protected, context.can_recase)
            });
        let norm = normalize_token(raw);
        if !protected && STRIP_FILLERS.contains(&norm.as_str()) {
            if let Some(trace) = &mut trace {
                trace.filler();
            }
            fillers += 1;
            let trailing = raw.chars().last().filter(|c| matches!(c, '.' | '?' | '!'));
            if let Some(previous) = kept.last_mut() {
                if previous.ends_with(',') {
                    previous.pop();
                }
                if let Some(mark) = trailing
                    && !previous.ends_with(['.', '?', '!', ';', ':'])
                {
                    previous.push(mark);
                }
            }
            after_filler = true;
            continue;
        }
        let mut token = raw.to_string();
        if !norm.is_empty() {
            let n = kept_norm.len();
            if n > 0 && kept_norm[n - 1] == norm && same_stutter_value(&kept[n - 1], raw) {
                if let Some(trace) = &mut trace {
                    trace.stutter(kept.last().is_some_and(|p| p.ends_with(',')), false);
                }
                stutters += 1;
                // "we, we roll" keeps the later copy so the stutter's comma goes with it.
                if let Some(previous) = kept.last_mut()
                    && previous.ends_with(',')
                {
                    *previous = raw.to_string();
                }
                if n == 1 {
                    first_can_recase &= can_recase;
                }
                continue;
            }
            if n >= 3
                && kept_norm[n - 3] == kept_norm[n - 1]
                && kept_norm[n - 2] == norm
                && same_stutter_value(&kept[n - 3], &kept[n - 1])
                && same_stutter_value(&kept[n - 2], raw)
            {
                if let Some(trace) = &mut trace {
                    trace.stutter(false, true);
                }
                kept.pop();
                kept_norm.pop();
                stutters += 1;
                continue;
            }
            if after_filler
                && can_recase
                && let Some(previous) = kept.last()
                && !previous.ends_with(['.', '?', '!'])
                && norm != "i"
                && is_capitalised_word(&token)
            {
                token = lower_first(&token);
            }
        }
        after_filler = false;
        if kept.is_empty() {
            first_can_recase = can_recase;
        }
        kept.push(token);
        kept_norm.push(norm);
        if let Some(trace) = &mut trace {
            trace.keep();
        }
    }
    let mut out = kept.join(" ");
    if fillers > 0
        && first_can_recase
        && text.chars().next().is_some_and(char::is_uppercase)
        && out.chars().next().is_some_and(char::is_lowercase)
    {
        out = upper_first(&out);
    }
    (out, fillers, stutters)
}

fn same_stutter_value(first: &str, second: &str) -> bool {
    let first = trim_critical_token(first);
    let second = trim_critical_token(second);
    if first == second {
        return true;
    }
    // Only closed grammar/pronouns and the explicit intensifier acceptance "Very very" allow
    // sentence-case equivalence. Unknown title-case words may be names ("May may", "Foo foo").
    let lower = first.to_lowercase();
    first == upper_first(&lower)
        && second == lower
        && (ECHO_GRAMMAR.contains(&lower.as_str())
            || FILLER_RECASE_PRONOUNS.contains(&lower.as_str())
            || lower == "very")
}

fn is_capitalised_word(token: &str) -> bool {
    let mut chars = token.chars();
    let first_upper = chars.next().is_some_and(char::is_uppercase);
    let rest_lower = chars.all(|c| !c.is_uppercase());
    first_upper && rest_lower && token.chars().filter(|c| c.is_alphabetic()).count() > 1
}

fn lower_first(token: &str) -> String {
    let mut chars = token.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn upper_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The filler pass over a whole timeline: rewrite every segment, drop the ones it empties.
fn strip_fillers_pass(
    segments: Vec<TranscriptSegment>,
    stats: &mut CleanupStats,
    decisions: &mut CleanupDecisions,
    preserve_raw: &[bool],
    traced: bool,
) -> Vec<TranscriptSegment> {
    let mut out = Vec::with_capacity(segments.len());
    for (i, mut segment) in segments.into_iter().enumerate() {
        if preserve_raw[i] {
            out.push(segment);
            continue;
        }
        let (text, fillers, stutters, edits) = if traced {
            strip_fillers_text_traced(&segment.text)
        } else {
            let (text, fillers, stutters) = strip_fillers_text(&segment.text);
            let mut edits = PositionedEdits::default();
            if text != segment.text {
                edits.unavailable(TraceGap::UnsupportedRule);
            }
            (text, fillers, stutters, edits)
        };
        stats.fillers_removed += fillers;
        stats.stutters_collapsed += stutters;
        if text.trim().is_empty() {
            decisions.record_text(
                CleanupStage::Fillers,
                i,
                &segment,
                None,
                CleanupReason::FillersOrStutters,
                edits,
            );
            continue;
        }
        if fillers > 0 || stutters > 0 {
            let replacement = TranscriptSegment {
                speaker: segment.speaker.clone(),
                start: segment.start,
                end: segment.end,
                text,
            };
            decisions.record_text(
                CleanupStage::Fillers,
                i,
                &segment,
                Some(&replacement),
                CleanupReason::FillersOrStutters,
                edits,
            );
            segment = replacement;
        }
        out.push(segment);
    }
    out
}

/// Single-word backchannels. They are both excluded from content tokens (they carry no information, so
/// they must not prop up a containment score) and used to recognize a whole backchannel turn.
const BACKCHANNELS: &[&str] = &[
    "yeah",
    "yep",
    "yup",
    "yes",
    "okay",
    "ok",
    "sure",
    "right",
    "uh-huh",
    "mm-hmm",
    "uhhuh",
    "cool",
    "alright",
    "exactly",
    "absolutely",
    "totally",
    "definitely",
    "gotcha",
];

/// Multi-word backchannel turns, matched on the whole normalized text.
const BACKCHANNEL_PHRASES: &[&str] = &[
    "all right",
    "there we go",
    "go ahead",
    "got it",
    "makes sense",
    "that makes sense",
    "i see",
    "sounds good",
    "for sure",
    "of course",
    "no worries",
];

/// Closed-class words that appear in nearly every utterance. Excluded from content tokens so containment
/// measures shared *content*, not shared grammar.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "do", "for", "from", "had", "has",
    "have", "he", "her", "his", "i", "if", "in", "is", "it", "its", "just", "like", "me", "my",
    "no", "not", "of", "oh", "on", "or", "our", "out", "she", "so", "that", "the", "their", "them",
    "then", "there", "these", "they", "this", "to", "up", "was", "we", "well", "were", "with",
    "you", "your", "i'm", "it's", "that's", "we're", "you're", "don't", "we've", "i've",
];

/// Lowercase one raw token, keeping only letters plus word-internal `-`/`'` (so `uh-huh` and `don't`
/// survive) and dropping the punctuation glued on by the decoder. Returns an empty string for a token with
/// no letters at all (a bare number, an ellipsis).
fn normalize_token(raw: &str) -> String {
    let kept: String = raw
        .chars()
        .filter(|c| c.is_alphabetic() || *c == '-' || *c == '\'')
        .flat_map(char::to_lowercase)
        .collect();
    let trimmed = kept.trim_matches(|c| c == '-' || c == '\'');
    if trimmed.chars().any(char::is_alphabetic) {
        trimmed.to_string()
    } else {
        String::new()
    }
}

/// The segment's text as normalized tokens, punctuation stripped, in order.
fn normalized_tokens(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(normalize_token)
        .filter(|t| !t.is_empty())
        .collect()
}

/// The **set** of content tokens: normalized tokens minus fillers, stopwords and backchannels. A set (not a
/// bag) so a stutter-repeated word cannot inflate a containment score.
fn content_tokens(text: &str) -> BTreeSet<String> {
    normalized_tokens(text)
        .into_iter()
        .filter(|t| {
            let t = t.as_str();
            !FILLERS.contains(&t) && !STOPWORDS.contains(&t) && !BACKCHANNELS.contains(&t)
        })
        .collect()
}

/// Fraction of `subject`'s content tokens that also appear in `source`. Empty subject ⇒ 0.
fn containment(subject: &BTreeSet<String>, source: &BTreeSet<String>) -> f64 {
    if subject.is_empty() {
        return 0.0;
    }
    let shared = subject.iter().filter(|t| source.contains(*t)).count();
    shared as f64 / subject.len() as f64
}

/// The near-end mic track is one channel; every [`Speaker::Other`] label is the far-end channel. Echo and
/// backchannel rules only ever compare *across* that boundary.
fn is_me(segment: &TranscriptSegment) -> bool {
    matches!(segment.speaker, Speaker::Me)
}

/// One side of the echo comparison: a closed region's channel, span, and content-token set, computed once.
///
/// The live early-drop path (#149 phase 2) asks the same question of one mic region against a rolling ring
/// of far-end regions **before** the region is published, so both paths ask it through this one type
/// instead of through two copies of the thresholds.
#[derive(Clone, PartialEq)]
pub struct EchoCandidate {
    /// Near-end mic channel. Echo is only ever judged *across* this boundary.
    me: bool,
    start: f64,
    end: f64,
    tokens: BTreeSet<String>,
    normalized: Vec<String>,
    critical: Vec<String>,
}

impl EchoCandidate {
    /// A candidate from a rendered segment — the batch and window rule.
    pub fn from_segment(segment: &TranscriptSegment) -> Self {
        Self {
            me: is_me(segment),
            start: segment.start,
            end: segment.end,
            tokens: content_tokens(&segment.text),
            normalized: normalized_tokens(&segment.text),
            critical: critical_tokens(&segment.text),
        }
    }

    /// A candidate from one closed region's words, before anything has rendered them as a segment — the
    /// live rule. An empty region carries no tokens, so it is never an echo of anything.
    pub fn from_words(me: bool, words: &[Word]) -> Self {
        let text = words
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        Self {
            me,
            start: words.first().map_or(0.0, |w| w.start),
            end: words.last().map_or(0.0, |w| w.end),
            tokens: content_tokens(&text),
            normalized: normalized_tokens(&text),
            critical: critical_tokens(&text),
        }
    }

    /// Seconds from call start at which this region begins.
    pub fn start(&self) -> f64 {
        self.start
    }

    /// Seconds from call start at which this region ends.
    pub fn end(&self) -> f64 {
        self.end
    }

    /// How many distinct content tokens the region carries — the unit "short" is measured in.
    pub fn content_tokens(&self) -> usize {
        self.tokens.len()
    }

    /// Text-only candidate match on the **other** channel, inside the echo window. This is NOT a
    /// deletion verdict: callers must use [`echo_verdict`] or [`echo_assessment`] to decide publication.
    ///
    /// Three or more content tokens match when `echo_containment` of them appear in the source. One
    /// or two are only enough when the match is total **and** this region's whole span sits inside the
    /// source's — that is the #107 phantom (`Me: Gateway` in the middle of a far-end monologue), and
    /// requiring containment of the span is what keeps a genuine one-word interjection.
    /// An identical nonempty normalized sequence also supports a candidate, including pure
    /// backchannels whose case differs. It still cannot authorize deletion without the shared verdict.
    pub fn is_echo_of(&self, source: &Self, cfg: &CleanupConfig) -> bool {
        if source.me == self.me {
            return false;
        }
        // The source must have started first and still be inside the echo window.
        let in_window =
            source.start <= self.start && self.start <= source.end + cfg.echo_window_seconds;
        if !in_window {
            return false;
        }
        // Case-only backchannels can have no content tokens ("Okay" / "OKAY"). A nonempty
        // normalized sequence still establishes a text match inside the same time/channel gates.
        if !self.normalized.is_empty() && self.normalized == source.normalized {
            return true;
        }
        if self.tokens.is_empty() {
            return false;
        }
        let c = containment(&self.tokens, &source.tokens);
        if self.tokens.len() >= 3 {
            c >= cfg.echo_containment
        } else {
            c >= 1.0 && self.start >= source.start && self.end <= source.end
        }
    }
}

/// A deliberately small grammar vocabulary. Every unknown token is retained, rather than guessing
/// which tokens are names, identifiers, units, or negators. Order, case, digits, and internal punctuation
/// survive. Grammar exemptions apply only with ordinary casing and outside numeric context, so
/// `A`, `Ah`, `mm`, or `um` cannot disappear from a quantity. In particular, the candidate detector's
/// STOPWORDS must never define this signature.
const ECHO_GRAMMAR: &[&str] = &[
    "a", "an", "the", "am", "is", "are", "was", "were", "be", "been", "being",
];

fn critical_tokens(text: &str) -> Vec<String> {
    raw_token_context(text)
        .filter_map(|context| {
            let lower = context.token.to_lowercase();
            (context.protected || !ECHO_GRAMMAR.contains(&lower.as_str()))
                .then(|| context.token.to_owned())
        })
        .collect()
}

fn trim_critical_token(raw: &str) -> &str {
    // Preserve a leading decimal point, signs, currency, %, /, # and @. A terminal full stop is
    // sentence punctuation; internal decimal/version dots are retained.
    raw.trim_start_matches([
        '\"', '\'', '“', '”', '‘', '’', '(', ')', '[', ']', '{', '}', ',', '!', '?', ';', ':', '¿',
        '¡',
    ])
    .trim_end_matches([
        '\"', '\'', '“', '”', '‘', '’', '(', ')', '[', ']', '{', '}', ',', '.', '!', '?', ';', ':',
        '¿', '¡',
    ])
}

struct RawTokenContext<'a> {
    index: usize,
    token: &'a str,
    protected: bool,
    can_recase: bool,
}

/// Subject pronouns may recover ordinary mid-sentence case after a filler ("um, We should"). This is
/// solely a sentence-repair allowance: it never removes a token from the raw critical signature.
const FILLER_RECASE_PRONOUNS: &[&str] = &["i", "you", "he", "she", "it", "we", "they"];

/// Raw nonempty tokens and whether context, case or punctuation protects them from lossy normalization.
/// Indices retain the original whitespace-token positions so a caller can also preserve punctuation.
fn raw_token_context(text: &str) -> impl Iterator<Item = RawTokenContext<'_>> {
    let mut tokens = text
        .split_whitespace()
        .enumerate()
        .map(|(index, raw)| (index, raw, trim_critical_token(raw)))
        .filter(|(_, _, token)| !token.is_empty())
        .peekable();
    let mut previous_numeric = false;
    let mut previous_comma = false;
    let mut first = true;
    std::iter::from_fn(move || {
        let (index, raw, token) = tokens.next()?;
        // Units and identifiers may precede or follow a number. Protect both neighbors before
        // considering exemptions; no vocabulary can safely enumerate all such critical tokens.
        let numeric_context = previous_numeric
            || tokens
                .peek()
                .is_some_and(|(_, _, next)| next.chars().any(char::is_numeric));
        let lower = token.to_lowercase();
        let sentence_case = token == upper_first(&lower);
        let ordinary_case = token == lower || (first && sentence_case);
        let normalization_loss = lower != normalize_token(token);
        // Unmarked medial fillers are ambiguous units/content. Preserve them without guessing a
        // unit or numeral vocabulary. Leading fillers and comma-marked hesitations remain eligible
        // for ordinary cleanup; cross-channel critical protection overrides even those removals.
        let hesitation_context = first || raw.ends_with(',') || previous_comma;
        let ambiguous_filler = FILLERS.contains(&lower.as_str()) && !hesitation_context;
        previous_numeric = token.chars().any(char::is_numeric);
        previous_comma = raw.ends_with(',');
        first = false;
        Some(RawTokenContext {
            index,
            token,
            protected: previous_numeric
                || numeric_context
                || !ordinary_case
                || normalization_loss
                || ambiguous_filler,
            can_recase: !previous_numeric
                && !numeric_context
                && !normalization_loss
                && !ambiguous_filler
                && (ordinary_case
                    || (sentence_case && FILLER_RECASE_PRONOUNS.contains(&lower.as_str()))),
        })
    })
}

/// The shared echo deletion decision, without attribution. See [`echo_assessment`] for the policy.
pub fn echo_verdict<'a>(
    subject: &EchoCandidate,
    sources: impl IntoIterator<Item = &'a EchoCandidate>,
    cfg: &CleanupConfig,
    evidence: Option<SpanEvidence>,
) -> EchoVerdict {
    echo_assessment(subject, sources, cfg, evidence).verdict
}

/// Assess all relevant opposite-channel sources before deleting anything. Any text-matched critical
/// difference protects the subject, including a non-overlapping earlier source in the echo window or
/// a later-starting overlapping source. Text matches without acoustic proof are only suspected.
/// Only a mic subject with overlapping far-end speech and the existing quiet-mic/double-talk predicate
/// can be dropped. Far-end subjects are never acoustically measured and are never echo-deleted.
pub fn echo_assessment<'a>(
    subject: &EchoCandidate,
    sources: impl IntoIterator<Item = &'a EchoCandidate>,
    cfg: &CleanupConfig,
    evidence: Option<SpanEvidence>,
) -> EchoAssessment {
    if !cfg.echo_drop {
        return EchoAssessment::default();
    }
    let mut matched = None;
    let mut overlap = None;
    for source in sources {
        if source.me == subject.me {
            continue;
        }
        let descriptor = EchoSource {
            me: source.me,
            start: source.start,
            end: source.end,
        };
        let overlaps = subject.start < source.end && source.start < subject.end;
        let text_matches =
            subject.is_echo_of(source, cfg) || (overlaps && source.is_echo_of(subject, cfg));
        if text_matches {
            if subject.critical != source.critical {
                return EchoAssessment {
                    verdict: EchoVerdict::KeepCriticalDifference,
                    source: Some(descriptor),
                };
            }
            matched.get_or_insert(descriptor);
        }
        if overlaps && !source.me {
            overlap.get_or_insert(descriptor);
        }
    }
    if subject.me && overlap.is_some() && is_mostly_echo(evidence, cfg) {
        EchoAssessment {
            verdict: EchoVerdict::DropAcoustic,
            source: overlap,
        }
    } else if matched.is_some() {
        EchoAssessment {
            verdict: EchoVerdict::SuspectedDuplicate,
            source: matched,
        }
    } else {
        EchoAssessment::default()
    }
}

/// Apply the shared conservative verdict and retain decisions before removing acoustically supported
/// mic echoes. Text candidates and protected corrections remain in the output.
fn drop_echoes(
    segments: Vec<TranscriptSegment>,
    cfg: &CleanupConfig,
    carry: &[TranscriptSegment],
    evidence: Option<AudioEvidence<'_>>,
    stats: &mut CleanupStats,
    decisions: &mut CleanupDecisions,
    preserve_raw: &mut Vec<bool>,
) -> Vec<TranscriptSegment> {
    let carry_sources: Vec<EchoCandidate> = carry.iter().map(EchoCandidate::from_segment).collect();
    let current: Vec<EchoCandidate> = segments.iter().map(EchoCandidate::from_segment).collect();
    let mut kept = vec![true; segments.len()];

    for (i, segment) in segments.iter().enumerate() {
        let subject = &current[i];
        let reading = evidence.and_then(|get| get(subject.start, subject.end));
        let mut assessment =
            echo_assessment(subject, carry_sources.iter().chain(&current), cfg, reading);
        if preserve_raw[i] && assessment.verdict != EchoVerdict::KeepCriticalDifference {
            assessment = EchoAssessment {
                verdict: EchoVerdict::KeepCriticalDifference,
                source: None,
            };
        }
        preserve_raw[i] |= assessment.verdict == EchoVerdict::KeepCriticalDifference;
        if assessment.verdict != EchoVerdict::Keep {
            decisions.record_echo_assessment(CleanupStage::Echo, i, segment, assessment, reading);
        }
        if assessment.verdict == EchoVerdict::DropAcoustic {
            kept[i] = false;
            stats.echo_dropped_audio += 1;
        }
    }

    let mut kept_flags = kept.iter();
    preserve_raw.retain(|_| *kept_flags.next().unwrap());
    let mut kept = kept.into_iter();
    let mut out = segments;
    out.retain(|_| kept.next().unwrap_or(true));
    out
}

/// The audio verdict for one span: `Some(evidence)` covering at least one block, whose gate stayed mostly
/// quiet, and whose mic energy is within `echo_audio_margin_db` of the echo the canceller subtracted.
/// Every other case — no evidence, no blocks, a gate that fired for most of the span, a mic well above the
/// estimate — is `false` and cannot authorize echo deletion.
fn is_mostly_echo(evidence: Option<SpanEvidence>, cfg: &CleanupConfig) -> bool {
    let Some(e) = evidence else {
        return false;
    };
    // Spelled out rather than negated: `!(a < b)` trips `neg_cmp_op_on_partial_ord`, and a NaN fraction
    // must fail the guard rather than pass it.
    let gate_stayed_quiet = e.double_talk_fraction < AUDIO_DOUBLE_TALK_LIMIT;
    if e.blocks == 0 || !gate_stayed_quiet {
        return false;
    }
    let excess = e.mic_db - e.echo_estimate_db;
    excess.is_finite() && excess <= cfg.echo_audio_margin_db
}

/// Join consecutive same-speaker segments separated by at most `merge_gap_seconds` **when the other channel
/// did not start a turn in between**. That last clause is the whole point: VAD tuning can lengthen the
/// silence it tolerates, but it cannot know that the far end took the floor, and merging across a real
/// exchange would rewrite who said what to whom.
fn merge_fragments(
    segments: Vec<TranscriptSegment>,
    cfg: &CleanupConfig,
    stats: &mut CleanupStats,
    decisions: &mut CleanupDecisions,
    preserve_raw: &mut Vec<bool>,
) -> Vec<TranscriptSegment> {
    let me_starts: Vec<f64> = segments
        .iter()
        .filter(|s| is_me(s))
        .map(|s| s.start)
        .collect();
    let them_starts: Vec<f64> = segments
        .iter()
        .filter(|s| !is_me(s))
        .map(|s| s.start)
        .collect();

    let mut out: Vec<TranscriptSegment> = Vec::with_capacity(segments.len());
    let mut merged_protection: Vec<bool> = Vec::with_capacity(segments.len());
    let mut group_start = 0;
    for (i, b) in segments.iter().enumerate() {
        let mergeable = match out.last() {
            Some(a) => {
                let interrupted = if is_me(b) { &them_starts } else { &me_starts };
                a.speaker == b.speaker
                    && b.start - a.end <= cfg.merge_gap_seconds
                    && !interrupted
                        .iter()
                        .any(|start| a.start <= *start && *start < b.start)
            }
            None => false,
        };
        match out.last_mut() {
            Some(a) if mergeable => {
                a.text = format!("{} {}", a.text.trim_end(), b.text.trim_start());
                a.end = b.end;
                stats.merged += 1;
                *merged_protection.last_mut().unwrap() |= preserve_raw[i];
            }
            _ => {
                if i - group_start > 1 {
                    record_merge(&segments, group_start, i, out.last().unwrap(), decisions);
                }
                group_start = i;
                out.push(b.clone());
                merged_protection.push(preserve_raw[i]);
            }
        }
    }
    if segments.len() - group_start > 1 {
        record_merge(
            &segments,
            group_start,
            segments.len(),
            out.last().unwrap(),
            decisions,
        );
    }
    *preserve_raw = merged_protection;
    out
}

fn record_merge(
    input: &[TranscriptSegment],
    start: usize,
    end: usize,
    replacement: &TranscriptSegment,
    decisions: &mut CleanupDecisions,
) {
    let bytes = std::mem::size_of::<CleanupDecision>()
        .saturating_add((end - start).saturating_mul(std::mem::size_of::<usize>()))
        .saturating_add(
            input[start..end]
                .iter()
                .map(segment_bytes)
                .fold(0usize, usize::saturating_add),
        )
        .saturating_add(segment_bytes(replacement));
    if bytes > CLEANUP_DECISION_BYTES.saturating_sub(decisions.bytes) {
        decisions.omitted = decisions.omitted.saturating_add(1);
        return;
    }
    let indices: Vec<_> = (start..end).collect();
    let originals: Vec<_> = input[start..end].iter().collect();
    decisions.record(
        CleanupStage::Merge,
        &indices,
        &originals,
        &[replacement],
        CleanupReason::SameSpeakerContinuation,
        None,
        None,
    );
}

/// Drop pure backchannels spoken over the other channel — "Yeah." while the far end is mid-sentence is
/// listening, not content. A backchannel that answers a question is kept: if the nearest earlier
/// other-channel segment ends in `?`, this row is the answer to it.
fn drop_backchannel_turns(
    segments: Vec<TranscriptSegment>,
    stats: &mut CleanupStats,
    decisions: &mut CleanupDecisions,
    preserve_raw: &mut Vec<bool>,
) -> Vec<TranscriptSegment> {
    let mut kept = vec![true; segments.len()];
    for (i, s) in segments.iter().enumerate() {
        if preserve_raw[i] || s.text.split_whitespace().count() >= 4 || !is_backchannel(&s.text) {
            continue;
        }
        let over_other_speech = segments
            .iter()
            .enumerate()
            .any(|(j, o)| j != i && is_me(o) != is_me(s) && s.start < o.end && o.start < s.end);
        if !over_other_speech {
            continue;
        }
        // Time-sorted input ⇒ the last other-channel segment at or before this start is the nearest one.
        let answers_a_question = segments
            .iter()
            .rfind(|o| is_me(o) != is_me(s) && o.start <= s.start)
            .is_some_and(|o| o.text.trim_end().ends_with('?'));
        if answers_a_question {
            continue;
        }
        kept[i] = false;
        stats.backchannels_dropped += 1;
        decisions.record(
            CleanupStage::Backchannel,
            &[i],
            &[s],
            &[],
            CleanupReason::OverlappingBackchannel,
            None,
            None,
        );
    }

    let mut kept_flags = kept.iter();
    preserve_raw.retain(|_| *kept_flags.next().unwrap());
    let mut kept = kept.into_iter();
    let mut out = segments;
    out.retain(|_| kept.next().unwrap_or(true));
    out
}

/// Whether the whole utterance is backchannel: a listed phrase, or nothing but backchannel words and
/// fillers ("Yeah yeah.", "Um, okay."). Raw values must survive normalization before any turn can be
/// discarded: "Okay, 15000." is an answer containing a number, not an empty acknowledgement.
fn is_backchannel(text: &str) -> bool {
    let single_token = raw_token_context(text).count() == 1;
    if raw_token_context(text).any(|context| {
        let lower = context.token.to_lowercase();
        let single_uppercase_backchannel = single_token
            && BACKCHANNELS.contains(&lower.as_str())
            && context.token == lower.to_uppercase();
        lower != normalize_token(context.token)
            || (context.protected && !single_uppercase_backchannel)
    }) {
        return false;
    }
    let tokens = normalized_tokens(text);
    if tokens.is_empty() {
        return false;
    }
    if BACKCHANNEL_PHRASES.contains(&tokens.join(" ").as_str()) {
        return true;
    }
    tokens
        .iter()
        .all(|t| BACKCHANNELS.contains(&t.as_str()) || FILLERS.contains(&t.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(start: f64, end: f64, text: &str) -> Word {
        Word {
            start,
            end,
            text: text.to_string(),
        }
    }

    #[test]
    fn groups_words_and_splits_on_long_pause() {
        let words = [
            word(0.0, 0.4, "Morning"),
            word(0.4, 0.8, "team."),
            word(6.0, 6.5, "Thanks"),
        ];
        let segs = words_to_segments(&words, Speaker::Me, SEGMENT_GAP);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].speaker, Speaker::Me);
        assert_eq!(segs[0].text, "Morning team.");
        assert_eq!(segs[0].start, 0.0);
        assert_eq!(segs[0].end, 0.8);
        assert_eq!(segs[1].text, "Thanks");
        assert_eq!(segs[1].start, 6.0);
    }

    #[test]
    fn empty_words_yield_no_segments() {
        assert!(words_to_segments(&[], Speaker::Me, SEGMENT_GAP).is_empty());
    }

    #[test]
    fn merge_interleaves_speakers_by_start() {
        let me = words_to_segments(&[word(0.0, 0.8, "Morning")], Speaker::Me, SEGMENT_GAP);
        let them = words_to_segments(
            &[word(2.0, 2.9, "Hello there")],
            Speaker::Other("Them".into()),
            SEGMENT_GAP,
        );
        let merged = merge_by_time([me, them].concat());
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].speaker, Speaker::Me);
        assert_eq!(merged[1].speaker, Speaker::Other("Them".into()));
        assert!(merged[0].start < merged[1].start);
    }

    #[test]
    fn diarize_attributes_words_by_overlap_and_breaks_on_speaker_change() {
        // Two far-end speakers alternating; words land inside each turn.
        let words = [
            word(0.0, 0.5, "Hi"),
            word(0.5, 1.0, "there"),
            word(2.0, 2.5, "Hello"),
            word(2.5, 3.0, "back"),
        ];
        let turns = [
            SpeakerTurn {
                start: 0.0,
                end: 1.2,
                label: "Them 1".into(),
            },
            SpeakerTurn {
                start: 1.8,
                end: 3.2,
                label: "Them 2".into(),
            },
        ];
        let segs = diarize_words(&words, &turns, SEGMENT_GAP, "Them");
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].speaker, Speaker::Other("Them 1".into()));
        assert_eq!(segs[0].text, "Hi there");
        assert_eq!(segs[1].speaker, Speaker::Other("Them 2".into()));
        assert_eq!(segs[1].text, "Hello back");
    }

    #[test]
    fn diarize_with_no_turns_uses_fallback_label() {
        let words = [word(0.0, 0.5, "Hello"), word(0.6, 1.0, "world")];
        let segs = diarize_words(&words, &[], SEGMENT_GAP, "Them");
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].speaker, Speaker::Other("Them".into()));
        assert_eq!(segs[0].text, "Hello world");
    }

    #[test]
    fn diarize_word_outside_all_turns_takes_nearest() {
        // A word just after the only turn ends → attributed to that turn.
        let words = [word(5.0, 5.4, "late")];
        let turns = [SpeakerTurn {
            start: 0.0,
            end: 4.0,
            label: "Them 1".into(),
        }];
        let segs = diarize_words(&words, &turns, SEGMENT_GAP, "Them");
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].speaker, Speaker::Other("Them 1".into()));
    }

    // ---- cleanup (#149) ----------------------------------------------------------------------------
    //
    // Fixtures are synthetic. This repository is public and the transcripts that motivated the rules are
    // private meetings, so the shapes are reproduced (an echo 1–5 s late, a fragment split by a breath, a
    // backchannel over the far end) with invented words.

    fn seg(speaker: Speaker, start: f64, end: f64, text: &str) -> TranscriptSegment {
        TranscriptSegment {
            speaker,
            start,
            end,
            text: text.to_string(),
        }
    }

    fn me(start: f64, end: f64, text: &str) -> TranscriptSegment {
        seg(Speaker::Me, start, end, text)
    }

    fn them(start: f64, end: f64, text: &str) -> TranscriptSegment {
        seg(Speaker::Other("Them".into()), start, end, text)
    }

    fn texts(segments: &[TranscriptSegment]) -> Vec<&str> {
        segments.iter().map(|s| s.text.as_str()).collect()
    }

    /// Only the echo pass, so a fixture's fragments/backchannels don't confuse what is being asserted.
    fn echo_only() -> CleanupConfig {
        CleanupConfig {
            merge_gap_seconds: 0.0,
            drop_backchannels: false,
            strip_fillers: false,
            ..CleanupConfig::default()
        }
    }

    /// Only the merge pass.
    fn merge_only() -> CleanupConfig {
        CleanupConfig {
            echo_drop: false,
            drop_backchannels: false,
            strip_fillers: false,
            ..CleanupConfig::default()
        }
    }

    /// The structural passes without the filler pass, for fixtures whose wording is the assertion.
    fn structural() -> CleanupConfig {
        CleanupConfig {
            strip_fillers: false,
            ..CleanupConfig::default()
        }
    }

    #[test]
    fn genuine_corrections_and_repeats_survive_both_channel_orders() {
        let cases = [
            (
                "english_negation",
                "We should deploy the payment gateway today.",
                "We should not deploy the payment gateway today.",
            ),
            (
                "amount",
                "The invoice total is 1500 euros today.",
                "The invoice total is 15000 euros today.",
            ),
            (
                "spanish_negation",
                "Debemos publicar la nueva versión mañana.",
                "No debemos publicar la nueva versión mañana.",
            ),
            (
                "repeated_command",
                "Please restart the production gateway now.",
                "Please restart the production gateway now.",
            ),
            (
                "short_no",
                "We should deploy the payment gateway today.",
                "No.",
            ),
            (
                "short_number",
                "The invoice total is 1500 euros today.",
                "15.",
            ),
            (
                "unrelated",
                "The gateway remains available today.",
                "The orchard grows purple plums.",
            ),
        ];
        let double_talk = evidence_of(8, 0.0, 0.75);
        let mut false_positives = 0;
        for (label, first, second) in cases {
            for me_first in [false, true] {
                for evidence in [None, Some(&double_talk as AudioEvidence<'_>)] {
                    let segments = if me_first {
                        vec![me(10.0, 15.0, first), them(11.0, 14.0, second)]
                    } else {
                        vec![them(10.0, 15.0, first), me(11.0, 14.0, second)]
                    };
                    let (out, _) = cleanup_with_evidence(segments, &echo_only(), &[], evidence);
                    let lost = 2 - out.len();
                    false_positives += lost;
                    println!(
                        "case={label} me_first={me_first} evidence={} false_positives={lost}",
                        evidence.is_some()
                    );
                }
            }
        }
        println!("genuine_cases false_positives={false_positives} false_negatives=0");
        assert_eq!(false_positives, 0);
    }

    #[test]
    fn quiet_evidence_never_overrides_a_matched_critical_correction() {
        let quiet = evidence_of(8, 0.0, 0.0);
        for (first, second) in [
            (
                "We should deploy the payment gateway today.",
                "We should not deploy the payment gateway today.",
            ),
            (
                "The invoice total is 1500 euros today.",
                "The invoice total is 15000 euros today.",
            ),
            (
                "Debemos publicar la nueva versión mañana.",
                "No debemos publicar la nueva versión mañana.",
            ),
            (
                "Ship gateway build v1.5 to Ada today.",
                "Ship gateway build v1.6 to Bea today.",
            ),
            (
                "The gateway uses 15 mW today.",
                "The gateway uses 15 MW today.",
            ),
            (
                "The gateway is ready today.",
                "The gateway is never ready today.",
            ),
        ] {
            let (out, stats) = cleanup_with_evidence(
                vec![them(10.0, 15.0, first), me(11.0, 14.0, second)],
                &echo_only(),
                &[],
                Some(&quiet),
            );
            assert_eq!(out.len(), 2);
            assert_eq!(stats.changed(), 0);
        }
    }

    fn assert_raw_unit_corrections_survive(
        cfg: &CleanupConfig,
        evidence: Option<AudioEvidence<'_>>,
    ) {
        let cases = [
            ("amperes_capacity", "15 A", "15 Ah"),
            ("millimetres_missing", "15 mm", "15"),
            ("millimetres_millimolar", "15 mm", "15 mM"),
            ("attached_millimetres_missing", "15mm", "15"),
            ("attached_millimetres_millimolar", "15mm", "15mM"),
            ("ascii_micrometres_missing", "15 um", "15"),
            ("attached_ascii_micrometres_missing", "15um", "15"),
            ("attached_amperes_capacity", "15A", "15Ah"),
            ("leading_unit", "A 15", "Ah 15"),
            ("capitalized_identifier", "Ah", "A"),
        ];
        let mut failures = Vec::new();
        for (label, first, second) in cases {
            for reverse_values in [false, true] {
                let (first, second) = if reverse_values {
                    (second, first)
                } else {
                    (first, second)
                };
                let first = format!("The gateway battery uses {first} today.");
                let second = format!("The gateway battery uses {second} today.");
                for me_first in [false, true] {
                    let segments = if me_first {
                        vec![me(10.0, 15.0, &first), them(11.0, 14.0, &second)]
                    } else {
                        vec![them(10.0, 15.0, &first), me(11.0, 14.0, &second)]
                    };
                    let result = cleanup_with_decisions(segments.clone(), cfg, &[], evidence, None);
                    let mic = segments.iter().find(|segment| is_me(segment)).unwrap();
                    let far = segments.iter().find(|segment| !is_me(segment)).unwrap();
                    let words: Vec<_> = mic
                        .text
                        .split_whitespace()
                        .map(|text| word(mic.start, mic.end, text))
                        .collect();
                    let live_verdict = echo_verdict(
                        &EchoCandidate::from_words(true, &words),
                        [&EchoCandidate::from_segment(far)],
                        &echo_only(),
                        evidence.and_then(|get| get(mic.start, mic.end)),
                    );
                    if result.segments != segments
                        || result.stats.changed() != 0
                        || live_verdict != EchoVerdict::KeepCriticalDifference
                        || !result.decisions.entries().iter().any(|decision| {
                            decision.reason
                                == CleanupReason::Echo(EchoVerdict::KeepCriticalDifference)
                                && decision.originals.iter().any(is_me)
                        })
                    {
                        failures.push(format!(
                            "{label} reverse_values={reverse_values} me_first={me_first} \
                             kept={} live_verdict={live_verdict:?}",
                            result.segments.len()
                        ));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn raw_unit_corrections_survive_quiet_evidence_in_both_channel_orders() {
        let quiet = evidence_of(8, 0.0, 0.0);
        assert_raw_unit_corrections_survive(&echo_only(), Some(&quiet));
    }

    #[test]
    fn raw_unit_corrections_survive_absent_and_double_talk_evidence() {
        let double_talk = evidence_of(8, 0.0, 0.75);
        for evidence in [None, Some(&double_talk as AudioEvidence<'_>)] {
            assert_raw_unit_corrections_survive(&echo_only(), evidence);
        }
    }

    #[test]
    fn raw_unit_corrections_survive_the_full_cleanup_pipeline() {
        let quiet = evidence_of(8, 0.0, 0.0);
        let double_talk = evidence_of(8, 0.0, 0.75);
        for evidence in [
            None,
            Some(&double_talk as AudioEvidence<'_>),
            Some(&quiet as AudioEvidence<'_>),
        ] {
            assert_raw_unit_corrections_survive(&CleanupConfig::default(), evidence);
        }
    }

    #[test]
    fn unit_corrections_without_adjacent_digits_survive_echo_and_full_cleanup() {
        let quiet = evidence_of(8, 0.0, 0.0);
        let double_talk = evidence_of(8, 0.0, 0.75);
        let mut failures = Vec::new();
        for (first, second) in [
            (
                "Report the gateway dimensions in mm today.",
                "Report the gateway dimensions in um today.",
            ),
            (
                "The plate area is 15 square mm today.",
                "The plate area is 15 square um today.",
            ),
            (
                "The gateway thickness is fifteen mm today.",
                "The gateway thickness is fifteen um today.",
            ),
            (
                "The plate area is 15 square mm, today.",
                "The plate area is 15 square um, today.",
            ),
        ] {
            for (first, second) in [(first, second), (second, first)] {
                for me_first in [false, true] {
                    let segments = if me_first {
                        vec![me(10.0, 15.0, first), them(11.0, 14.0, second)]
                    } else {
                        vec![them(10.0, 15.0, first), me(11.0, 14.0, second)]
                    };
                    for (mode, cfg) in [("echo", echo_only()), ("full", CleanupConfig::default())] {
                        for (label, evidence) in [
                            ("absent", None),
                            ("double_talk", Some(&double_talk as AudioEvidence<'_>)),
                            ("quiet", Some(&quiet as AudioEvidence<'_>)),
                        ] {
                            let result =
                                cleanup_with_decisions(segments.clone(), &cfg, &[], evidence, None);
                            if result.segments != segments
                                || result.stats.changed() != 0
                                || !result.decisions.entries().iter().any(|decision| {
                                    decision.reason
                                        == CleanupReason::Echo(EchoVerdict::KeepCriticalDifference)
                                })
                            {
                                failures.push(format!(
                                    "{first:?} / {second:?} me_first={me_first} \
                                     mode={mode} evidence={label} kept={} changed={}",
                                    result.segments.len(),
                                    result.stats.changed()
                                ));
                            }
                        }
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn backchannel_case_differences_survive_full_cleanup() {
        let double_talk = evidence_of(8, 0.0, 0.75);
        let mut failures = Vec::new();
        for (first, second) in [
            ("Got it.", "Got IT."),
            ("I see.", "I SEE."),
            ("Go ahead.", "Go Ahead."),
            ("Okay.", "OKAY."),
        ] {
            for (first, second) in [(first, second), (second, first)] {
                for me_first in [false, true] {
                    for evidence in [None, Some(&double_talk as AudioEvidence<'_>)] {
                        let segments = if me_first {
                            vec![me(10.0, 15.0, first), them(11.0, 14.0, second)]
                        } else {
                            vec![them(10.0, 15.0, first), me(11.0, 14.0, second)]
                        };
                        let (out, stats) = cleanup_with_evidence(
                            segments.clone(),
                            &CleanupConfig::default(),
                            &[],
                            evidence,
                        );
                        if out != segments || stats.changed() != 0 {
                            failures.push(format!(
                                "{first:?} / {second:?} me_first={me_first} \
                                 evidence={} kept={}",
                                evidence.is_some(),
                                out.len()
                            ));
                        }
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn sentence_initial_names_are_not_case_only_stutters() {
        let mut failures = Vec::new();
        for text in [
            "May may approve the gateway today.",
            "Foo foo updates the gateway today.",
        ] {
            let segments = vec![me(10.0, 15.0, text)];
            let (out, _) = cleanup(segments.clone(), &CleanupConfig::default(), &[]);
            if out != segments {
                failures.push(format!("{text:?} became {:?}", texts(&out)));
            }
        }
        let (out, stats) = cleanup(
            vec![me(10.0, 15.0, "Very very important.")],
            &CleanupConfig::default(),
            &[],
        );
        assert_eq!(texts(&out), vec!["Very important."]);
        assert_eq!(stats.stutters_collapsed, 1);
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn unmarked_medial_units_survive_without_a_matching_source() {
        for text in [
            "The gateway thickness is fifteen um today.",
            "The plate area is 15 square um today.",
        ] {
            for me_first in [false, true] {
                let other = "The orchard grows purple plums.";
                let segments = if me_first {
                    vec![me(10.0, 15.0, text), them(11.0, 14.0, other)]
                } else {
                    vec![them(10.0, 15.0, other), me(11.0, 14.0, text)]
                };
                for echo_drop in [false, true] {
                    let cfg = CleanupConfig {
                        echo_drop,
                        ..CleanupConfig::default()
                    };
                    let (out, stats) = cleanup(segments.clone(), &cfg, &[]);
                    assert_eq!(out, segments);
                    assert_eq!(stats.changed(), 0);
                }
            }
        }
    }

    #[test]
    fn normalized_sequence_fallback_respects_channel_time_and_nonempty_gates() {
        let cfg = echo_only();
        let source = EchoCandidate::from_segment(&them(0.0, 5.0, "Okay."));
        let case_variant = EchoCandidate::from_segment(&me(1.0, 2.0, "OKAY."));
        assert!(case_variant.is_echo_of(&source, &cfg));
        assert_eq!(
            echo_verdict(&case_variant, [&source], &cfg, None),
            EchoVerdict::KeepCriticalDifference
        );
        for segment in [
            me(1.0, 2.0, "Sure."),
            me(12.0, 13.0, "OKAY."),
            them(1.0, 2.0, "OKAY."),
        ] {
            assert!(!EchoCandidate::from_segment(&segment).is_echo_of(&source, &cfg));
        }
        assert!(
            !EchoCandidate::from_segment(&me(1.0, 2.0, "15000."))
                .is_echo_of(&EchoCandidate::from_segment(&them(0.0, 5.0, "1500.")), &cfg)
        );
        let (out, stats) = cleanup(
            vec![
                them(0.0, 5.0, "The orchard grows purple plums."),
                me(1.0, 2.0, "OKAY!"),
            ],
            &CleanupConfig::default(),
            &[],
        );
        assert_eq!(out.len(), 1);
        assert_eq!(stats.backchannels_dropped, 1);
    }

    #[test]
    fn protected_input_indices_survive_sorting_duplicate_text_and_equal_times() {
        let duplicate = me(10.0, 11.0, "Um, ready.");
        let result = cleanup_with_protected_decisions(
            vec![duplicate.clone(), them(0.0, 1.0, "Earlier."), duplicate],
            &CleanupConfig {
                echo_drop: false,
                merge_gap_seconds: 0.0,
                ..CleanupConfig::default()
            },
            &[],
            None,
            None,
            &[2],
        );
        assert_eq!(
            texts(&result.segments),
            vec!["Earlier.", "Ready.", "Um, ready."]
        );
        assert_eq!(result.stats.fillers_removed, 1);
        assert!(result.decisions.entries().iter().any(|decision| {
            decision.stage == CleanupStage::Sort && decision.input_indices == [0]
        }));
    }

    #[test]
    fn merged_rows_preserve_protection_from_either_input() {
        for protected_index in [0, 1] {
            let result = cleanup_with_protected_decisions(
                vec![me(0.0, 1.0, "Um,"), me(1.1, 2.0, "15 square um.")],
                &CleanupConfig {
                    echo_drop: false,
                    ..CleanupConfig::default()
                },
                &[],
                None,
                None,
                &[protected_index],
            );
            assert_eq!(texts(&result.segments), vec!["Um, 15 square um."]);
            assert_eq!(result.stats.merged, 1);
            assert_eq!(result.stats.fillers_removed, 0);
        }
    }

    #[test]
    fn raw_protection_survives_omitted_echo_decisions() {
        let prefix = "x".repeat(CLEANUP_DECISION_BYTES / 2 + 1);
        let segments = vec![
            them(10.0, 15.0, &format!("{prefix} unit mm.")),
            me(11.0, 14.0, &format!("{prefix} unit um.")),
        ];
        let quiet = evidence_of(8, 0.0, 0.0);
        let result = cleanup_with_decisions(
            segments.clone(),
            &CleanupConfig::default(),
            &[],
            Some(&quiet),
            None,
        );
        assert_eq!(result.segments, segments);
        assert_eq!(result.stats.changed(), 0);
        assert!(result.decisions.entries().is_empty());
        assert_eq!(result.decisions.omitted, 2);
        assert_eq!(result.decisions.bytes(), 0);
    }

    #[test]
    fn full_cleanup_keeps_distinct_raw_values_during_stutter_removal() {
        for text in [
            "The gateway uses 15mm 15mM today.",
            "The gateway uses v1.5 v15 today.",
            "The gateway uses AB-15 AB-150 today.",
            "The gateway uses mW MW today.",
            "The gateway uses 15mm wide 15mM wide today.",
        ] {
            let segments = vec![me(10.0, 15.0, text)];
            let (out, stats) = cleanup(segments.clone(), &CleanupConfig::default(), &[]);
            assert_eq!(out, segments, "{text}");
            assert_eq!(stats.changed(), 0, "{text}");
        }
    }

    #[test]
    fn full_cleanup_preserves_raw_values_through_filler_sentence_repairs() {
        let mut failures = Vec::new();
        for (text, expected) in [
            (
                "The gateway uses um Ah 15 today.",
                "The gateway uses um Ah 15 today.",
            ),
            ("Um, mM 15.", "mM 15."),
            ("Um, mm 5.", "mm 5."),
            ("Marker um, Ah.", "Marker Ah."),
            ("Contact um, Ada.", "Contact Ada."),
            ("Contact um, Will.", "Contact Will."),
            ("Contact um, May.", "Contact May."),
            ("Um, @ready.", "@ready."),
            ("The ticket is @um.", "The ticket is @um."),
            ("The ticket is #ah.", "The ticket is #ah."),
            ("The ticket is um/.", "The ticket is um/."),
            ("Read @ready ready now.", "Read @ready ready now."),
            ("Read foo/bar foobar now.", "Read foo/bar foobar now."),
        ] {
            let (out, _) = cleanup(vec![me(10.0, 15.0, text)], &CleanupConfig::default(), &[]);
            if out != vec![me(10.0, 15.0, expected)] {
                failures.push(format!("{text:?} became {:?}", texts(&out)));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn full_cleanup_keeps_critical_values_in_backchannel_shaped_turns() {
        let double_talk = evidence_of(8, 0.0, 0.75);
        let mut failures = Vec::new();
        for text in [
            "Okay, 15000.",
            "I see 15000.",
            "Okay, 1.2.4.",
            "I see #15000.",
            "Okay1.",
            "@Okay.",
            "#Okay.",
            "Okay mM.",
            "Okay Ah.",
            "Okay 15mm.",
        ] {
            for me_first in [false, true] {
                for evidence in [None, Some(&double_talk as AudioEvidence<'_>)] {
                    let source = "The gateway remains available today.";
                    let segments = if me_first {
                        vec![me(10.0, 15.0, source), them(11.0, 14.0, text)]
                    } else {
                        vec![them(10.0, 15.0, source), me(11.0, 14.0, text)]
                    };
                    let (out, stats) = cleanup_with_evidence(
                        segments.clone(),
                        &CleanupConfig::default(),
                        &[],
                        evidence,
                    );
                    if out != segments || stats.changed() != 0 {
                        failures.push(format!(
                            "{text} me_first={me_first} evidence={} kept={}",
                            evidence.is_some(),
                            out.len()
                        ));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn every_matched_source_is_checked_before_acoustic_deletion() {
        let quiet = evidence_of(8, 0.0, 0.0);
        let subject = me(
            12.0,
            14.0,
            "We should not deploy the payment gateway today.",
        );
        let earlier = them(10.0, 11.0, "We should deploy the payment gateway today.");
        let overlapping = them(11.5, 15.0, &subject.text);
        for carry in [
            vec![earlier.clone(), overlapping.clone()],
            vec![overlapping, earlier],
        ] {
            let result = cleanup_with_decisions(
                vec![subject.clone()],
                &echo_only(),
                &carry,
                Some(&quiet),
                None,
            );
            assert_eq!(result.segments.len(), 1);
            let decision = &result.decisions.entries()[0];
            assert_eq!(
                decision.reason,
                CleanupReason::Echo(EchoVerdict::KeepCriticalDifference)
            );
            assert_eq!(decision.source.unwrap().start, 10.0);
        }
        // A source beginning later still overlaps the mic and must not bypass critical protection.
        let result = cleanup_with_decisions(
            vec![
                subject,
                them(13.0, 15.0, "We should deploy the payment gateway today."),
            ],
            &echo_only(),
            &[],
            Some(&quiet),
            None,
        );
        assert_eq!(result.segments.len(), 2);
        assert_eq!(result.stats.echo_dropped_audio, 0);
    }

    #[test]
    fn raw_critical_signature_preserves_unknown_words_and_numeric_forms() {
        for (a, b) in [
            ("gateway build 1.5", "gateway build 15"),
            ("gateway draw .5", "gateway draw 5"),
            ("gateway draw 15 mW", "gateway draw 15 MW"),
            ("gateway owner Ada", "gateway owner Bea"),
            ("gateway ticket AB-15", "gateway ticket AB-150"),
            ("gateway cost $15", "gateway cost 15"),
            ("gateway nunca listo", "gateway listo"),
            ("gateway jamás listo", "gateway listo"),
            ("gateway without retry", "gateway with retry"),
            ("gateway no longer ready", "gateway ready"),
            ("gateway alpha beta", "gateway beta alpha"),
        ] {
            assert_ne!(critical_tokens(a), critical_tokens(b));
        }
        assert_eq!(
            critical_tokens("The gateway is ready."),
            critical_tokens("gateway ready")
        );
        assert_eq!(
            critical_tokens("Um, gateway ready!"),
            vec!["Um", "gateway", "ready"]
        );
    }

    #[test]
    fn generated_attenuated_echo_and_grammar_paraphrase_are_removed_with_real_aec_evidence() {
        use corti_aec::{AecConfig, StreamingAec, span_stats};
        let rate = 8_000;
        let count = rate as usize * 4;
        let mut seed = 19_u32;
        let far: Vec<f32> = (0..count)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((seed >> 8) as f32 / 16_777_216.0 - 0.5) * 0.8
            })
            .collect();
        let delay = 24;
        let mic: Vec<f32> = (0..count)
            .map(|i| {
                if i >= delay {
                    0.3 * far[i - delay]
                } else {
                    0.0
                }
            })
            .collect();
        let cfg = AecConfig {
            filter_len: 128,
            ..AecConfig::default()
        };
        let mut aec = StreamingAec::new_with_lookahead(rate, cfg, rate as usize);
        let mut clean = aec.push(&mic, &far);
        let mut blocks = aec.block_stats();
        let tail = aec.finish_with_stats();
        clean.extend(tail.audio);
        blocks.extend(tail.stats);
        assert_eq!(clean.len(), mic.len());
        let evidence = |start, end| {
            span_stats(&blocks, start, end).map(|s| SpanEvidence {
                mic_db: s.mean_mic_db,
                echo_estimate_db: s.mean_echo_estimate_db,
                double_talk_fraction: s.double_talk_fraction,
                blocks: s.blocks,
            })
        };
        let mut false_negatives = 0;
        for (label, text, carry) in [
            ("delayed_copy", "The gateway is ready today.", false),
            ("grammar_paraphrase", "gateway ready today.", false),
            ("window_boundary", "The gateway is ready today.", true),
        ] {
            let source = them(0.5, 3.0, "The gateway is ready today.");
            let subject = me(1.0, 2.5, text);
            let (input, carry) = if carry {
                (vec![subject.clone()], vec![source])
            } else {
                (vec![source, subject.clone()], vec![])
            };
            let result = cleanup_with_decisions(input, &echo_only(), &carry, Some(&evidence), None);
            false_negatives += usize::from(result.segments.iter().any(is_me));
            assert_eq!(result.stats.echo_dropped_audio, 1);
            let decision = result
                .decisions
                .entries()
                .iter()
                .find(|d| d.reason == CleanupReason::Echo(EchoVerdict::DropAcoustic))
                .unwrap();
            assert_eq!(decision.originals, vec![subject]);
            assert!(decision.replacements.is_empty());
            assert_eq!(decision.source.unwrap().start, 0.5);
            assert!(decision.evidence.unwrap().blocks > 0);
            println!("case={label} false_positives=0 false_negatives={false_negatives}");
        }
        println!("acoustic_cases false_positives=0 false_negatives={false_negatives}");
        assert_eq!(false_negatives, 0);
    }

    #[test]
    fn decisions_preserve_every_stage_input_span_text_and_replacement() {
        struct RouterRule;
        impl TextRule for RouterRule {
            fn apply(&self, text: &str) -> Option<String> {
                text.contains("gateway")
                    .then(|| text.replace("gateway", "router"))
            }
        }
        let result = cleanup_with_decisions(
            vec![
                me(22.0, 23.0, "responds."),
                them(0.0, 10.0, "Our orchard holds purple plums."),
                me(20.0, 21.0, "Um, the the gateway"),
                me(1.0, 1.5, "Yeah."),
            ],
            &CleanupConfig {
                echo_drop: false,
                ..CleanupConfig::default()
            },
            &[],
            None,
            Some(&RouterRule),
        );
        assert_eq!(
            texts(&result.segments),
            vec!["Our orchard holds purple plums.", "The router responds."]
        );
        let entries = result.decisions.entries();
        assert!(entries.iter().any(|d| d.stage == CleanupStage::Sort));
        let merged = entries
            .iter()
            .find(|d| d.stage == CleanupStage::Merge)
            .unwrap();
        assert_eq!(merged.input_indices, vec![2, 3]);
        assert_eq!(
            texts(&merged.originals),
            vec!["Um, the the gateway", "responds."]
        );
        assert_eq!(
            (merged.replacements[0].start, merged.replacements[0].end),
            (20.0, 23.0)
        );
        let backchannel = entries
            .iter()
            .find(|d| d.stage == CleanupStage::Backchannel)
            .unwrap();
        assert_eq!(backchannel.input_indices, vec![1]);
        assert_eq!(texts(&backchannel.originals), vec!["Yeah."]);
        assert!(backchannel.replacements.is_empty());
        let filler = entries
            .iter()
            .find(|d| d.stage == CleanupStage::Fillers)
            .unwrap();
        assert_eq!(filler.input_indices, vec![1]);
        assert_eq!(texts(&filler.replacements), vec!["The gateway responds."]);
        let lexicon = entries
            .iter()
            .find(|d| d.stage == CleanupStage::Lexicon)
            .unwrap();
        assert_eq!(texts(&lexicon.originals), vec!["The gateway responds."]);
        assert_eq!(texts(&lexicon.replacements), vec!["The router responds."]);
    }

    #[test]
    fn suspected_and_early_kept_regions_are_recorded_without_losing_originals() {
        let text = "Please restart the production gateway now.";
        let result = cleanup_with_decisions(
            vec![them(0.0, 5.0, text), me(1.0, 4.0, text)],
            &echo_only(),
            &[],
            None,
            None,
        );
        assert_eq!(result.segments.len(), 2);
        let decision = &result.decisions.entries()[1];
        assert_eq!(
            decision.reason,
            CleanupReason::Echo(EchoVerdict::SuspectedDuplicate)
        );
        assert_eq!(decision.input_indices, vec![1]);
        assert_eq!(decision.originals, decision.replacements);
        assert_eq!(
            decision.source,
            Some(EchoSource {
                me: false,
                start: 0.0,
                end: 5.0
            })
        );
        let mut decisions = CleanupDecisions::default();
        decisions.record_echo(
            CleanupStage::EarlyEcho,
            42,
            &result.segments[1],
            EchoVerdict::Keep,
            None,
        );
        assert_eq!(decisions.entries()[0].input_indices, vec![42]);
        assert_eq!(
            decisions.entries()[0].originals,
            decisions.entries()[0].replacements
        );
    }

    #[test]
    fn decision_payload_is_bounded_before_cloning_and_across_appends() {
        let mut decisions = CleanupDecisions::default();
        let oversized = me(0.0, 1.0, &"x".repeat(CLEANUP_DECISION_BYTES + 1));
        decisions.record_echo(
            CleanupStage::EarlyEcho,
            0,
            &oversized,
            EchoVerdict::Keep,
            None,
        );
        assert!(decisions.entries().is_empty());
        assert_eq!(decisions.bytes(), 0);
        assert_eq!(decisions.omitted, 1);
        let large = me(0.0, 1.0, &"x".repeat(CLEANUP_DECISION_BYTES / 4));
        decisions.record_echo(CleanupStage::EarlyEcho, 1, &large, EchoVerdict::Keep, None);
        let mut next = CleanupDecisions::default();
        next.record_echo(CleanupStage::EarlyEcho, 2, &large, EchoVerdict::Keep, None);
        next.record_echo(
            CleanupStage::EarlyEcho,
            3,
            &oversized,
            EchoVerdict::Keep,
            None,
        );
        decisions.append(next);
        assert_eq!(decisions.entries().len(), 1);
        assert_eq!(decisions.omitted, 3);
        assert!(decisions.bytes() <= CLEANUP_DECISION_BYTES);
        // A large record does not prevent a subsequent small decision from being retained.
        decisions.record_echo(
            CleanupStage::EarlyEcho,
            4,
            &me(1.0, 2.0, "No."),
            EchoVerdict::Keep,
            None,
        );
        assert_eq!(decisions.entries().len(), 2);
        assert_eq!(decisions.omitted, 3);
    }

    #[test]
    fn later_copy_of_a_far_end_utterance_is_kept_without_evidence() {
        let segments = vec![
            them(
                10.0,
                14.0,
                "We should rotate the widget calibration before Friday.",
            ),
            me(15.0, 16.5, "Rotate the widget calibration."),
        ];
        let (out, stats) = cleanup(segments, &echo_only(), &[]);
        assert_eq!(
            texts(&out),
            vec![
                "We should rotate the widget calibration before Friday.",
                "Rotate the widget calibration."
            ]
        );
        assert_eq!(stats.echo_dropped_me, 0);
        assert_eq!(stats.echo_dropped_them, 0);
    }

    #[test]
    fn one_source_keeps_every_copy_without_evidence() {
        let segments = vec![
            them(
                10.0,
                14.0,
                "We should rotate the widget calibration before Friday.",
            ),
            me(15.0, 16.5, "Rotate the widget calibration."),
            me(18.0, 19.0, "Widget calibration rotate."),
        ];
        let (out, stats) = cleanup(segments, &echo_only(), &[]);
        assert_eq!(out.len(), 3);
        assert_eq!(stats.echo_dropped_me, 0);
    }

    /// Two of three content tokens shared is 0.67 — below the 0.7 threshold, so a genuinely different
    /// sentence that happens to reuse the topic's nouns survives.
    fn near_miss_fixture() -> Vec<TranscriptSegment> {
        vec![
            them(
                10.0,
                14.0,
                "We should rotate the widget calibration before Friday.",
            ),
            me(15.0, 16.5, "Rotate the widget schedule."),
        ]
    }

    #[test]
    fn near_miss_below_the_containment_threshold_is_kept() {
        let (out, stats) = cleanup(near_miss_fixture(), &echo_only(), &[]);
        assert_eq!(out.len(), 2, "0.67 containment must not be an echo");
        assert_eq!(stats.echo_dropped_me, 0);

        // Lowering the text-match threshold still cannot authorize deletion.
        let loose = CleanupConfig {
            echo_containment: 0.6,
            ..echo_only()
        };
        let (out, _) = cleanup(near_miss_fixture(), &loose, &[]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn one_word_candidates_inside_and_after_a_far_end_span_are_kept_without_evidence() {
        let segments = vec![
            them(
                20.0,
                30.0,
                "The gateway is what times out when the queue backs up.",
            ),
            me(22.0, 22.4, "Gateway."),
            me(31.0, 31.5, "Gateway."),
        ];
        let (out, stats) = cleanup(segments, &echo_only(), &[]);
        assert_eq!(stats.echo_dropped_me, 0);
        assert_eq!(out.len(), 3);
        assert_eq!(out[2].start, 31.0);
    }

    #[test]
    fn backchannel_over_far_end_speech_is_dropped_but_an_answer_to_a_question_survives() {
        let segments = vec![
            them(
                40.0,
                50.0,
                "So the plan is to rebuild the index overnight and swap it in.",
            ),
            me(42.0, 42.4, "Yeah."),
            them(60.0, 64.0, "Do you want me to go first?"),
            me(63.0, 63.6, "Sure."),
        ];
        let cfg = CleanupConfig {
            echo_drop: false,
            merge_gap_seconds: 0.0,
            ..CleanupConfig::default()
        };
        let (out, stats) = cleanup(segments, &cfg, &[]);
        assert_eq!(stats.backchannels_dropped, 1);
        assert_eq!(
            texts(&out),
            vec![
                "So the plan is to rebuild the index overnight and swap it in.",
                "Do you want me to go first?",
                "Sure.",
            ]
        );
    }

    #[test]
    fn a_backchannel_with_nobody_else_speaking_is_kept() {
        let segments = vec![them(40.0, 41.0, "Right."), me(50.0, 50.4, "Yeah.")];
        let cfg = CleanupConfig {
            echo_drop: false,
            merge_gap_seconds: 0.0,
            ..CleanupConfig::default()
        };
        let (out, stats) = cleanup(segments, &cfg, &[]);
        assert_eq!(out.len(), 2);
        assert_eq!(stats.backchannels_dropped, 0);
    }

    #[test]
    fn fragments_merge_only_within_the_gap() {
        let close = vec![
            me(0.0, 1.0, "The migration finished"),
            me(3.0, 4.0, "about an hour ago."),
        ];
        let (out, stats) = cleanup(close, &merge_only(), &[]);
        assert_eq!(
            texts(&out),
            vec!["The migration finished about an hour ago."]
        );
        assert_eq!(out[0].start, 0.0);
        assert_eq!(out[0].end, 4.0);
        assert_eq!(stats.merged, 1);

        let far = vec![
            me(0.0, 1.0, "The migration finished"),
            me(4.0, 5.0, "about an hour ago."),
        ];
        let (out, stats) = cleanup(far, &merge_only(), &[]);
        assert_eq!(out.len(), 2, "a 3.0 s gap is a new utterance");
        assert_eq!(stats.merged, 0);
    }

    #[test]
    fn an_intervening_far_end_turn_blocks_the_merge() {
        let segments = vec![
            me(0.0, 1.0, "The migration finished"),
            them(1.2, 1.6, "Which migration?"),
            me(2.0, 3.0, "about an hour ago."),
        ];
        let (out, stats) = cleanup(segments, &merge_only(), &[]);
        assert_eq!(out.len(), 3, "the far end took the floor in between");
        assert_eq!(stats.merged, 0);
    }

    #[test]
    fn separate_far_end_speakers_do_not_merge_into_each_other() {
        let segments = vec![
            seg(Speaker::Other("Them 1".into()), 0.0, 1.0, "I can take that"),
            seg(Speaker::Other("Them 2".into()), 1.5, 2.5, "or I can."),
        ];
        let (out, stats) = cleanup(segments, &merge_only(), &[]);
        assert_eq!(out.len(), 2);
        assert_eq!(stats.merged, 0);
    }

    #[test]
    fn carry_segments_are_echo_sources_and_never_output() {
        let carry = vec![them(
            10.0,
            14.0,
            "We should rotate the widget calibration before Friday.",
        )];
        let segments = vec![
            me(15.0, 16.5, "Rotate the widget calibration."),
            me(
                18.0,
                19.0,
                "The dashboard still renders yesterday's totals.",
            ),
        ];
        let (out, stats) = cleanup(segments, &echo_only(), &carry);
        assert_eq!(
            texts(&out),
            vec![
                "Rotate the widget calibration.",
                "The dashboard still renders yesterday's totals."
            ],
            "the carried source must not be re-emitted"
        );
        assert_eq!(stats.echo_dropped_me, 0);
        assert_eq!(carry.len(), 1, "carry is read-only");
    }

    /// A text match cannot start a deletion chain; all repeats survive without acoustic evidence.
    #[test]
    fn text_only_candidates_never_start_a_deletion_chain() {
        let segments = vec![
            them(10.0, 12.0, "Rotate the widget calibration."),
            me(13.0, 14.0, "Rotate the widget calibration."),
            them(15.0, 17.0, "Rotate the widget calibration, yes."),
        ];
        let (out, stats) = cleanup(segments, &echo_only(), &[]);
        assert_eq!(out.len(), 3);
        assert_eq!(stats.echo_dropped_me, 0);
        assert_eq!(stats.echo_dropped_them, 0);
        assert!(!is_me(&out[2]));
    }

    #[test]
    fn stats_count_structural_changes_without_text_only_echo_deletions() {
        let segments = vec![
            // Me first; the far-end repetition must survive.
            me(0.0, 3.0, "The retention sweep runs hourly now."),
            them(4.0, 5.5, "Retention sweep hourly."),
            // A far-end utterance echoed back onto the mic.
            them(
                20.0,
                30.0,
                "The gateway is what times out when the queue backs up.",
            ),
            me(22.0, 22.4, "Gateway."),
            // A pure backchannel over that same far-end turn.
            me(26.0, 26.3, "Yeah."),
            // Two mic fragments with nobody else speaking in between.
            me(40.0, 41.0, "I will send the summary"),
            me(42.5, 43.5, "after this call."),
        ];
        let (out, stats) = cleanup(segments, &structural(), &[]);
        assert_eq!(
            stats,
            CleanupStats {
                echo_dropped_me: 0,
                echo_dropped_them: 0,
                merged: 1,
                backchannels_dropped: 1,
                echo_dropped_audio: 0,
                fillers_removed: 0,
                stutters_collapsed: 0,
                lexicon_applied: 0,
            }
        );
        assert_eq!(
            texts(&out),
            vec![
                "The retention sweep runs hourly now.",
                "Retention sweep hourly.",
                "The gateway is what times out when the queue backs up.",
                "Gateway.",
                "I will send the summary after this call.",
            ]
        );
    }

    #[test]
    fn every_pass_is_individually_switchable_and_a_dead_config_is_a_noop() {
        let off = CleanupConfig {
            echo_drop: false,
            merge_gap_seconds: 0.0,
            drop_backchannels: false,
            strip_fillers: false,
            ..CleanupConfig::default()
        };
        assert!(off.is_noop());
        assert!(!CleanupConfig::default().is_noop());
        assert!(
            !CleanupConfig {
                strip_fillers: true,
                ..off.clone()
            }
            .is_noop(),
            "the filler pass alone is still a pass"
        );

        let segments = vec![
            them(
                20.0,
                30.0,
                "The gateway is what times out when the queue backs up.",
            ),
            me(22.0, 22.4, "Gateway."),
            me(26.0, 26.3, "Yeah."),
        ];
        let (out, stats) = cleanup(segments.clone(), &off, &[]);
        assert_eq!(out, segments);
        assert_eq!(stats.changed(), 0);
    }

    /// Timing arithmetic must stay panic-free on a NaN a decoder should never produce but might.
    #[test]
    fn nan_timings_are_passed_through_rather_than_panicking() {
        let segments = vec![
            them(f64::NAN, f64::NAN, "The gateway is what times out."),
            me(f64::NAN, f64::NAN, "Gateway."),
            me(0.0, 1.0, "Gateway."),
        ];
        let (out, stats) = cleanup(segments, &structural(), &[]);
        assert_eq!(out.len(), 3, "no comparison against NaN can succeed");
        assert_eq!(stats.changed(), 0);
    }

    #[test]
    fn segments_are_sorted_by_start_before_any_pass_so_spliced_batches_see_true_adjacency() {
        // #157: the live path emits `Me` in batches spliced into the `Them` stream, so file order is
        // not time order and an echo pair may not be adjacent in the input.
        let segments = vec![
            them(
                20.0,
                30.0,
                "The gateway is what times out when the queue backs up.",
            ),
            me(60.0, 61.0, "I will send the summary"),
            me(22.0, 22.4, "Gateway."),
            me(62.5, 63.5, "after this call."),
            them(5.0, 6.0, "Let's start."),
        ];
        let (out, stats) = cleanup(segments, &structural(), &[]);
        assert_eq!(
            stats.echo_dropped_me, 0,
            "the out-of-order candidate is kept without evidence"
        );
        assert_eq!(stats.merged, 1, "the out-of-order fragments still merge");
        assert_eq!(
            texts(&out),
            vec![
                "Let's start.",
                "The gateway is what times out when the queue backs up.",
                "Gateway.",
                "I will send the summary after this call.",
            ]
        );
        let starts: Vec<f64> = out.iter().map(|s| s.start).collect();
        assert_eq!(starts, vec![5.0, 20.0, 22.0, 60.0]);
    }

    #[test]
    fn fillers_and_stutters_are_stripped_with_sentence_repairs() {
        for (input, expected, fillers, stutters) in [
            ("Um, the gateway is down.", "The gateway is down.", 1, 0),
            ("the the gateway", "the gateway", 0, 1),
            ("15mm 15mm", "15mm", 0, 1),
            ("15mm wide 15mm wide", "15mm wide", 0, 1),
            (
                "I think I think we should ship",
                "I think we should ship",
                0,
                1,
            ),
            (
                "I was, uh, thinking about it.",
                "I was thinking about it.",
                1,
                0,
            ),
            ("I think, um. We should go.", "I think. We should go.", 1, 0),
            ("I think, um, We should go.", "I think we should go.", 1, 0),
            ("So um, uh, yeah the queue.", "So yeah the queue.", 2, 0),
            ("Hmm.", "", 1, 0),
            ("No fillers here.", "No fillers here.", 0, 0),
            ("Uh, I, I mean it.", "I mean it.", 1, 1),
            ("Mm-hmm, right.", "Mm-hmm, right.", 0, 0),
        ] {
            let (out, f, s) = strip_fillers_text(input);
            assert_eq!(out, expected, "{input:?}");
            assert_eq!((f, s), (fillers, stutters), "{input:?}");
        }
    }

    #[test]
    fn filler_pass_drops_emptied_rows_and_reports_counts_and_the_knob_switches_it_off() {
        let segments = vec![
            me(0.0, 1.0, "Um, the the gateway is down."),
            them(2.0, 2.5, "Uh."),
            them(3.0, 5.0, "Right, so we, we roll back."),
        ];
        let (out, stats) = cleanup(
            segments.clone(),
            &CleanupConfig {
                echo_drop: false,
                merge_gap_seconds: 0.0,
                drop_backchannels: false,
                ..CleanupConfig::default()
            },
            &[],
        );
        assert_eq!(
            texts(&out),
            vec!["The gateway is down.", "Right, so we roll back."]
        );
        assert_eq!(stats.fillers_removed, 2);
        assert_eq!(stats.stutters_collapsed, 2);

        let (untouched, stats) = cleanup(
            segments.clone(),
            &CleanupConfig {
                echo_drop: false,
                merge_gap_seconds: 0.0,
                drop_backchannels: false,
                strip_fillers: false,
                ..CleanupConfig::default()
            },
            &[],
        );
        assert_eq!(texts(&untouched), texts(&segments));
        assert_eq!(stats.fillers_removed + stats.stutters_collapsed, 0);
    }

    struct Upper;

    impl TextRule for Upper {
        fn apply(&self, text: &str) -> Option<String> {
            text.contains("widget")
                .then(|| text.replace("widget", "Widget"))
        }
    }

    #[test]
    fn a_text_rule_runs_last_on_every_surviving_row_and_is_counted() {
        let segments = vec![
            them(0.0, 5.0, "The widget calibration drifted."),
            me(3.0, 3.5, "Yeah."),
            me(10.0, 12.0, "Um, ship the widget fix."),
        ];
        let (out, stats) = cleanup_with_rules(
            segments,
            &CleanupConfig {
                echo_drop: false,
                merge_gap_seconds: 0.0,
                ..CleanupConfig::default()
            },
            &[],
            None,
            Some(&Upper),
        );
        assert_eq!(
            texts(&out),
            vec!["The Widget calibration drifted.", "Ship the Widget fix."]
        );
        assert_eq!(stats.lexicon_applied, 2);
        assert_eq!(stats.backchannels_dropped, 1);
    }

    #[test]
    fn content_tokens_ignore_fillers_stopwords_and_punctuation() {
        assert_eq!(
            content_tokens("Um, so the WIDGET calibration — widget! — drifted."),
            ["calibration", "drifted", "widget"]
                .into_iter()
                .map(str::to_string)
                .collect::<BTreeSet<_>>()
        );
        assert!(content_tokens("Yeah, yeah. Okay.").is_empty());
    }

    #[test]
    fn backchannel_recognition_covers_phrases_and_combinations() {
        for text in [
            "Yeah.",
            "OKAY!",
            "Yeah, yeah.",
            "Okay yeah.",
            "Uh-huh.",
            "There we go.",
            "Go ahead.",
            "Got it.",
            "Makes sense.",
            "All right.",
            "Um, sure.",
        ] {
            assert!(is_backchannel(text), "{text} should read as backchannel");
        }
        for text in [
            "Yeah, the migration finished.",
            "Right after the standup.",
            "Sure thing, I will send it.",
        ] {
            assert!(!is_backchannel(text), "{text} carries content");
        }
    }

    /// `split_regions` splits exactly where `words_to_segments` does, and keeps the words themselves so a
    /// withheld region can be published later unchanged.
    #[test]
    fn regions_split_on_the_same_pause_segments_do() {
        let words = [
            word(0.0, 0.4, "Morning"),
            word(0.4, 0.8, "team."),
            word(6.0, 6.5, "Thanks"),
        ];
        let regions = split_regions(&words, SEGMENT_GAP);
        let segments = words_to_segments(&words, Speaker::Me, SEGMENT_GAP);
        assert_eq!(regions.len(), segments.len());
        assert_eq!(regions[0], words[0..2]);
        assert_eq!(regions[1], words[2..3]);
        assert!(split_regions(&[], SEGMENT_GAP).is_empty());
        assert!(split_regions(&[word(0.0, 0.1, "")], SEGMENT_GAP).is_empty());
    }

    /// The candidate built from a region's words asks the same question as the one built from the rendered
    /// segment, so the live path and the window path cannot drift apart.
    #[test]
    fn echo_candidate_from_words_matches_the_rendered_segment() {
        let cfg = CleanupConfig::default();
        let them = TranscriptSegment {
            speaker: Speaker::Other("Them".into()),
            start: 10.0,
            end: 18.0,
            text: "the invoice reconciliation moves to the settlement gateway".into(),
        };
        let ghost = [word(13.0, 13.4, "settlement"), word(13.4, 13.9, "gateway.")];

        let source = EchoCandidate::from_segment(&them);
        let subject = EchoCandidate::from_words(true, &ghost);
        assert_eq!(subject.start(), 13.0);
        assert_eq!(subject.end(), 13.9);
        assert_eq!(subject.content_tokens(), 2);
        assert!(subject.is_echo_of(&source, &cfg));
        // Same verdict through the rendered segment.
        assert!(
            EchoCandidate::from_segment(&TranscriptSegment {
                speaker: Speaker::Me,
                start: 13.0,
                end: 13.9,
                text: "settlement gateway.".into(),
            })
            .is_echo_of(&source, &cfg)
        );
        // Same channel is never an echo, and an empty region has nothing to match.
        assert!(!subject.is_echo_of(&subject, &cfg));
        assert!(!EchoCandidate::from_words(true, &[]).is_echo_of(&source, &cfg));
    }

    /// A short genuine answer is not an echo: it shares no content with the far-end region it follows.
    #[test]
    fn echo_candidate_keeps_a_short_answer_that_shares_no_content() {
        let cfg = CleanupConfig::default();
        let source = EchoCandidate::from_segment(&TranscriptSegment {
            speaker: Speaker::Other("Them".into()),
            start: 10.0,
            end: 12.0,
            text: "which region is the replica in?".into(),
        });
        let answer = EchoCandidate::from_words(true, &[word(12.4, 13.0, "Frankfurt.")]);
        assert!(!answer.is_echo_of(&source, &cfg));
    }

    // ---- audio evidence (#149 phase 3b) --------------------------------------------------------------

    /// A canned canceller reading for every span: `blocks` non-zero and the mic sitting `excess` dB above
    /// the echo estimate.
    fn evidence_of(
        blocks: usize,
        excess: f32,
        double_talk_fraction: f32,
    ) -> impl Fn(f64, f64) -> Option<SpanEvidence> {
        move |_start, _end| {
            Some(SpanEvidence {
                mic_db: -30.0 + excess,
                echo_estimate_db: -30.0,
                double_talk_fraction,
                blocks,
            })
        }
    }

    #[test]
    fn audio_evidence_drops_a_ghost_the_text_rule_would_keep() {
        // Nothing in common with the far-end turn: containment is 0, so the text rules keep this row.
        let segments = vec![
            them(
                10.0,
                20.0,
                "The calibration jig arrives from Toronto on Tuesday.",
            ),
            me(12.0, 12.6, "Gateway harness."),
        ];
        let (kept, kept_stats) = cleanup(segments.clone(), &echo_only(), &[]);
        assert_eq!(kept.len(), 2, "the text rules alone keep the ghost");
        assert_eq!(kept_stats.echo_dropped_audio, 0);

        let quiet_mic = evidence_of(4, 1.0, 0.0);
        let (out, stats) = cleanup_with_evidence(segments, &echo_only(), &[], Some(&quiet_mic));
        assert_eq!(
            texts(&out),
            vec!["The calibration jig arrives from Toronto on Tuesday."]
        );
        assert_eq!(stats.echo_dropped_audio, 1);
        // The audio drop is counted on its own, never folded into the text counters.
        assert_eq!(stats.echo_dropped_me, 0);
        assert_eq!(stats.changed(), 1);
    }

    #[test]
    fn audio_rule_keeps_a_mic_span_louder_than_the_echo_estimate() {
        let segments = vec![
            them(
                10.0,
                20.0,
                "The calibration jig arrives from Toronto on Tuesday.",
            ),
            me(12.0, 12.6, "Gateway harness."),
        ];
        // 9 dB above the estimate is real near-end speech riding on top of the residual echo.
        let loud_mic = evidence_of(4, 9.0, 0.0);
        let (out, stats) = cleanup_with_evidence(segments, &echo_only(), &[], Some(&loud_mic));
        assert_eq!(out.len(), 2);
        assert_eq!(stats.echo_dropped_audio, 0);
    }

    #[test]
    fn a_span_with_no_blocks_is_kept_even_when_text_matches() {
        let segments = vec![
            them(
                10.0,
                20.0,
                "The calibration jig arrives from Toronto on Tuesday.",
            ),
            me(12.0, 12.6, "Gateway harness."),
            me(21.0, 22.0, "Calibration jig from Toronto."),
        ];
        // AWS, or a recording with no sidecar: the accessor answers, but with nothing measured.
        let empty = evidence_of(0, -40.0, 0.0);
        let (out, stats) = cleanup_with_evidence(segments.clone(), &echo_only(), &[], Some(&empty));
        assert_eq!(stats.echo_dropped_audio, 0);
        // A text match is recorded but cannot delete the repeated row.
        assert_eq!(stats.echo_dropped_me, 0);
        assert_eq!(
            texts(&out),
            vec![
                "The calibration jig arrives from Toronto on Tuesday.",
                "Gateway harness.",
                "Calibration jig from Toronto.",
            ]
        );

        // An accessor that returns None for the span is the same story.
        let absent = |_s: f64, _e: f64| None;
        let (out, stats) = cleanup_with_evidence(segments, &echo_only(), &[], Some(&absent));
        assert_eq!(stats.echo_dropped_audio, 0);
        assert_eq!(stats.echo_dropped_me, 0);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn audio_rule_needs_far_end_speech_under_the_mic_span() {
        // A quiet mic passage with no far end talking is not a ghost — both energies just sit near the
        // floor, and their difference is small for the uninteresting reason.
        let segments = vec![
            them(
                0.0,
                5.0,
                "The calibration jig arrives from Toronto on Tuesday.",
            ),
            me(30.0, 31.0, "Gateway harness."),
        ];
        let quiet_mic = evidence_of(6, 0.0, 0.0);
        let (out, stats) = cleanup_with_evidence(segments, &echo_only(), &[], Some(&quiet_mic));
        assert_eq!(out.len(), 2);
        assert_eq!(stats.echo_dropped_audio, 0);
    }

    #[test]
    fn a_far_end_span_carried_from_the_previous_window_still_grounds_the_audio_rule() {
        let carry = vec![them(50.0, 62.0, "The calibration jig arrives on Tuesday.")];
        let segments = vec![me(60.5, 61.0, "Gateway harness.")];
        let quiet_mic = evidence_of(3, 1.0, 0.0);
        let (out, stats) = cleanup_with_evidence(segments, &echo_only(), &carry, Some(&quiet_mic));
        assert!(out.is_empty());
        assert_eq!(stats.echo_dropped_audio, 1);
    }

    #[test]
    fn a_span_the_gate_froze_is_not_evidence() {
        let segments = vec![
            them(
                10.0,
                20.0,
                "The calibration jig arrives from Toronto on Tuesday.",
            ),
            me(12.0, 12.6, "Gateway harness."),
        ];
        // The adaptation gate fired for most of the span: the canceller itself says the near end
        // dominated, and its echo estimate stopped adapting. Keep the row.
        let frozen = evidence_of(4, 0.0, 0.75);
        let (out, stats) =
            cleanup_with_evidence(segments.clone(), &echo_only(), &[], Some(&frozen));
        assert_eq!(out.len(), 2);
        assert_eq!(stats.echo_dropped_audio, 0);

        // Exactly at the limit is still a freeze.
        let borderline = evidence_of(4, 0.0, AUDIO_DOUBLE_TALK_LIMIT);
        let (out, _) =
            cleanup_with_evidence(segments.clone(), &echo_only(), &[], Some(&borderline));
        assert_eq!(out.len(), 2);

        // Just under it is not.
        let mostly_quiet = evidence_of(4, 0.0, 0.49);
        let (out, stats) = cleanup_with_evidence(segments, &echo_only(), &[], Some(&mostly_quiet));
        assert_eq!(out.len(), 1);
        assert_eq!(stats.echo_dropped_audio, 1);
    }

    #[test]
    fn the_audio_rule_never_fires_on_a_far_end_segment() {
        // The short row sits inside the other channel's turn and its span reads as pure echo — but it is
        // the far-end track, which the canceller never measured. Only `Me` is the mic.
        let segments = vec![
            me(
                10.0,
                20.0,
                "The calibration jig arrives from Toronto on Tuesday.",
            ),
            them(12.0, 12.6, "Gateway harness."),
        ];
        let echo_only_in_the_short_span = |start: f64, end: f64| {
            let mostly_echo = start >= 11.0 && end <= 13.0;
            Some(SpanEvidence {
                mic_db: if mostly_echo { -30.0 } else { -6.0 },
                echo_estimate_db: -30.0,
                double_talk_fraction: 0.0,
                blocks: 4,
            })
        };
        let (out, stats) = cleanup_with_evidence(
            segments,
            &echo_only(),
            &[],
            Some(&echo_only_in_the_short_span),
        );
        assert_eq!(out.len(), 2);
        assert_eq!(stats.echo_dropped_audio, 0);
    }

    #[test]
    fn a_very_negative_margin_switches_echo_deletion_off() {
        let segments = vec![
            them(
                10.0,
                20.0,
                "The calibration jig arrives from Toronto on Tuesday.",
            ),
            me(12.0, 12.6, "Gateway harness."),
            me(21.0, 22.0, "Calibration jig from Toronto."),
        ];
        let cfg = CleanupConfig {
            echo_audio_margin_db: -1000.0,
            ..echo_only()
        };
        let quiet_mic = evidence_of(4, 0.0, 0.0);
        let (out, stats) = cleanup_with_evidence(segments, &cfg, &[], Some(&quiet_mic));
        assert_eq!(stats.echo_dropped_audio, 0);
        assert_eq!(stats.echo_dropped_me, 0);
        assert_eq!(out.len(), 3);
    }
}
