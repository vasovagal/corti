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
    let mut out: Vec<TranscriptSegment> = Vec::new();
    let mut cur: Option<TranscriptSegment> = None;

    for w in words {
        if w.text.is_empty() {
            continue;
        }
        match cur.as_mut() {
            Some(seg) if continues_region(seg.end, w, gap) => {
                seg.text.push(' ');
                seg.text.push_str(&w.text);
                seg.end = w.end;
            }
            _ => {
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
    out
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
    let mut out: Vec<TranscriptSegment> = Vec::new();
    let mut cur: Option<TranscriptSegment> = None;

    for w in words {
        if w.text.is_empty() {
            continue;
        }
        let speaker = Speaker::Other(best_turn_label(w, turns, fallback_label));
        match cur.as_mut() {
            Some(seg) if seg.speaker == speaker && w.start - seg.end <= gap => {
                seg.text.push(' ');
                seg.text.push_str(&w.text);
                seg.end = w.end;
            }
            _ => {
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
    out
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
// local backend exposes none). Filler words and stutters are deliberately out of scope: they belong to the
// decoder, not to segmentation.

/// Version of the [`cleanup`] rule set, recorded in a note's `corti.configuration.segment_cleanup`
/// provenance. Bump it whenever a pass's behavior changes, so an old note is never read as if it had
/// today's rules; the individual thresholds are recorded alongside it and are not part of this number.
pub const CLEANUP_RULES_VERSION: u32 = 2;

/// Tuning for [`cleanup`]. Defaults are the shipping values; the app persists them under `[cleanup]` in
/// `config.toml` and overrides them with `CORTI_CLEANUP_*`.
#[derive(Debug, Clone, PartialEq)]
pub struct CleanupConfig {
    /// Run the cross-channel echo pass.
    pub echo_drop: bool,
    /// How long after a far-end utterance *starts* its echo may still be decoded on the other channel.
    pub echo_window_seconds: f64,
    /// Fraction of a segment's content tokens that must also appear in the candidate source before the
    /// segment is judged an echo of it (segments of three or more content tokens).
    pub echo_containment: f64,
    /// Largest silence (seconds) between two consecutive same-speaker segments that is still merged into
    /// one utterance. Non-positive disables the merge pass.
    pub merge_gap_seconds: f64,
    /// Run the backchannel pass ("Yeah." over the other side's speech).
    pub drop_backchannels: bool,
    /// How far (dB) a mic span's energy may exceed the AEC's echo estimate for that same span and still be
    /// judged "this was mostly echo". Only consulted when [`cleanup_with_evidence`] is given an
    /// [`AudioEvidence`] accessor; a very negative value switches the audio rule off without disturbing the
    /// text rules.
    pub echo_audio_margin_db: f32,
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
        }
    }
}

impl CleanupConfig {
    /// True when no pass would run, so callers can skip [`cleanup`] entirely and record `"off"` in
    /// provenance rather than a set of inert knobs.
    pub fn is_noop(&self) -> bool {
        let merge_off = self.merge_gap_seconds <= 0.0 || self.merge_gap_seconds.is_nan();
        !self.echo_drop && !self.drop_backchannels && merge_off
    }
}

/// What [`cleanup`] changed. Logged per run so a surprising transcript can be explained without rerunning
/// ASR; the echo count is split per direction because the two directions have different causes (mic bleed
/// vs. the far end re-decoding our own voice).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CleanupStats {
    /// `Me` segments dropped as echoes of far-end speech (the #107 phantom class).
    pub echo_dropped_me: usize,
    /// Far-end segments dropped as echoes of `Me` speech.
    pub echo_dropped_them: usize,
    /// Segments absorbed into the preceding same-speaker segment.
    pub merged: usize,
    /// Backchannel segments dropped over the other side's speech.
    pub backchannels_dropped: usize,
    /// `Me` segments dropped by the **audio** echo rule — the AEC said the mic block was mostly echo — of
    /// which the text rules would have kept some. A subset of nothing else: these are counted here and not
    /// in [`echo_dropped_me`](Self::echo_dropped_me), so the two signals stay separable in a sweep.
    pub echo_dropped_audio: usize,
}

impl CleanupStats {
    /// Total segments removed or absorbed — zero means the transcript came through untouched.
    pub fn changed(&self) -> usize {
        self.echo_dropped_me
            + self.echo_dropped_them
            + self.merged
            + self.backchannels_dropped
            + self.echo_dropped_audio
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

/// A span whose adaptation gate fired in at least this fraction of its blocks is not evidence of anything:
/// the canceller froze its filter update because it judged the near end to dominate, so its echo estimate
/// is stale for most of the span, and the comparison the audio rule makes is against a number that stopped
/// tracking the room. Such a span falls through to the text rules.
///
/// This is not merely a numerical scruple: "the gate fired" is the canceller's own statement that it heard
/// near-end speech, which is precisely the case in which a `Me` segment must be kept.
const AUDIO_DOUBLE_TALK_LIMIT: f32 = 0.5;

/// Clean one **time-sorted** timeline (the output of [`merge_by_time`]) in three passes, in this order:
/// echo → merge → backchannel. The order matters: dropping an echo first stops it from being merged into a
/// real turn, and merging before the backchannel pass protects a "Yeah." that is really the opening of a
/// longer sentence.
///
/// `carry` is a read-only set of segments from the previous live window. They are only ever consulted as
/// echo *sources*, never mutated, and never appear in the output — that is how the live path catches an
/// echo whose source landed just before the one-minute append boundary.
///
/// Only earlier, still-kept segments act as echo sources, so echoes never chain: a dropped copy cannot go
/// on to kill the real utterance that follows it.
pub fn cleanup(
    segments: Vec<TranscriptSegment>,
    cfg: &CleanupConfig,
    carry: &[TranscriptSegment],
) -> (Vec<TranscriptSegment>, CleanupStats) {
    cleanup_with_evidence(segments, cfg, carry, None)
}

/// [`cleanup`] with the acoustic canceller's per-block record available to the echo pass.
///
/// Text alone cannot separate two populations that look identical: a `Me` row that is residual speaker
/// bleed, and a `Me` row where both people genuinely used the same nouns. `evidence` answers the question
/// text cannot — *was this mic span mostly echo?* — so the echo pass can drop a ghost whose wording is
/// nowhere near the containment threshold.
///
/// The audio rule runs **inside the echo pass, before the text rules**, and only on `Me` segments. A
/// segment is dropped when all of these hold:
///
/// 1. it overlaps a far-end segment (`S.start < O.end && O.start < S.end`) — the far end had the floor;
/// 2. `evidence(S.start, S.end)` returns a span covering at least one block;
/// 3. that span's adaptation gate fired in fewer than [`AUDIO_DOUBLE_TALK_LIMIT`] of its blocks;
/// 4. `mic_db − echo_estimate_db ≤ echo_audio_margin_db` — the mic carried little more than what the
///    canceller was already subtracting as echo.
///
/// A span with **no blocks** falls through to the text rules unchanged. That is the AWS backend (no
/// canceller), a sidecar that does not cover this recording, and a stats ring that overflowed: in all
/// three, absence of evidence is not evidence of speech.
pub fn cleanup_with_evidence(
    segments: Vec<TranscriptSegment>,
    cfg: &CleanupConfig,
    carry: &[TranscriptSegment],
    evidence: Option<AudioEvidence<'_>>,
) -> (Vec<TranscriptSegment>, CleanupStats) {
    let mut stats = CleanupStats::default();
    let mut segments = segments;
    if cfg.echo_drop {
        segments = drop_echoes(segments, cfg, carry, evidence, &mut stats);
    }
    if cfg.merge_gap_seconds > 0.0 {
        segments = merge_fragments(segments, cfg, &mut stats);
    }
    if cfg.drop_backchannels {
        segments = drop_backchannel_turns(segments, &mut stats);
    }
    (segments, stats)
}

/// Filler noises. Stripped before any comparison so "Um, the gateway" and "the gateway" are the same
/// utterance to the echo detector.
const FILLERS: &[&str] = &[
    "um", "umm", "uh", "uhh", "ah", "aah", "er", "erm", "hm", "hmm", "mm", "mmm", "mhm", "huh",
];

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
#[derive(Debug, Clone, PartialEq)]
pub struct EchoCandidate {
    /// Near-end mic channel. Echo is only ever judged *across* this boundary.
    me: bool,
    start: f64,
    end: f64,
    tokens: BTreeSet<String>,
}

impl EchoCandidate {
    /// A candidate from a rendered segment — the batch and window rule.
    pub fn from_segment(segment: &TranscriptSegment) -> Self {
        Self {
            me: is_me(segment),
            start: segment.start,
            end: segment.end,
            tokens: content_tokens(&segment.text),
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

    /// True when this region repeats what `source` said on the **other** channel, inside the echo window.
    ///
    /// Three or more content tokens are an echo when `echo_containment` of them appear in the source. One
    /// or two are only enough when the match is total **and** this region's whole span sits inside the
    /// source's — that is the #107 phantom (`Me: Gateway` in the middle of a far-end monologue), and
    /// requiring containment of the span is what keeps a genuine one-word interjection.
    pub fn is_echo_of(&self, source: &Self, cfg: &CleanupConfig) -> bool {
        if source.me == self.me || self.tokens.is_empty() {
            return false;
        }
        // The source must have started first and still be inside the echo window.
        let in_window =
            source.start <= self.start && self.start <= source.end + cfg.echo_window_seconds;
        if !in_window {
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

/// Drop each segment that repeats what the other channel already said, inside the echo window. The text
/// rule itself lives on [`EchoCandidate::is_echo_of`], shared with the live early-drop path (#149 phase 2).
///
/// When `evidence` is available the **audio** rule is tried first, on `Me` segments only: a mic span the
/// canceller measured as carrying no more than `echo_audio_margin_db` above its own echo estimate is a
/// ghost whatever its wording. See [`cleanup_with_evidence`] for the full predicate.
fn drop_echoes(
    segments: Vec<TranscriptSegment>,
    cfg: &CleanupConfig,
    carry: &[TranscriptSegment],
    evidence: Option<AudioEvidence<'_>>,
    stats: &mut CleanupStats,
) -> Vec<TranscriptSegment> {
    let carry_sources: Vec<EchoCandidate> = carry.iter().map(EchoCandidate::from_segment).collect();
    let current: Vec<EchoCandidate> = segments.iter().map(EchoCandidate::from_segment).collect();
    let mut kept = vec![true; segments.len()];

    for (i, segment) in segments.iter().enumerate() {
        let subject = &current[i];
        // The audio rule needs no tokens at all — a ghost is often one garbled word — so it runs before
        // the text rules, which have nothing to say about a region whose wording matches nothing.
        if let Some(evidence) = evidence
            && subject.me
            && overlaps_far_end(subject.start, subject.end, &current, &carry_sources)
            && is_mostly_echo(evidence(subject.start, subject.end), cfg)
        {
            kept[i] = false;
            stats.echo_dropped_audio += 1;
            continue;
        }
        // Sources are the previous window's kept segments plus every earlier segment this pass has kept —
        // an already-dropped copy is never allowed to kill the utterance it copied.
        let dropped = carry_sources.iter().any(|o| subject.is_echo_of(o, cfg))
            || (0..i).any(|j| kept[j] && subject.is_echo_of(&current[j], cfg));
        if dropped {
            kept[i] = false;
            if is_me(segment) {
                stats.echo_dropped_me += 1;
            } else {
                stats.echo_dropped_them += 1;
            }
        }
    }

    // `retain` visits every element once, in order, so draining the decision list alongside it filters in
    // place without a second allocation.
    let mut kept = kept.into_iter();
    let mut out = segments;
    out.retain(|_| kept.next().unwrap_or(true));
    out
}

/// Whether `[start, end]` overlaps any far-end span on this timeline, current window or carried over. The
/// audio rule only fires while the far end had the floor: without that clause a quiet mic passage with no
/// echo at all (both energies near the floor, their difference small) would look exactly like a ghost.
///
/// Kept/dropped status is deliberately ignored. "The far end was speaking then" is a fact about the
/// timeline, and half the segments here have not been decided yet at this point in the pass.
fn overlaps_far_end(
    start: f64,
    end: f64,
    current: &[EchoCandidate],
    carry: &[EchoCandidate],
) -> bool {
    current
        .iter()
        .chain(carry)
        .any(|o| !o.me && start < o.end && o.start < end)
}

/// The audio verdict for one span: `Some(evidence)` covering at least one block, whose gate stayed mostly
/// quiet, and whose mic energy is within `echo_audio_margin_db` of the echo the canceller subtracted.
/// Every other case — no evidence, no blocks, a gate that fired for most of the span, a mic well above the
/// estimate — is `false`, and the segment goes on to the text rules.
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
    for b in segments {
        let mergeable = match out.last() {
            Some(a) => {
                let interrupted = if is_me(&b) { &them_starts } else { &me_starts };
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
            }
            _ => out.push(b),
        }
    }
    out
}

/// Drop pure backchannels spoken over the other channel — "Yeah." while the far end is mid-sentence is
/// listening, not content. A backchannel that answers a question is kept: if the nearest earlier
/// other-channel segment ends in `?`, this row is the answer to it.
fn drop_backchannel_turns(
    segments: Vec<TranscriptSegment>,
    stats: &mut CleanupStats,
) -> Vec<TranscriptSegment> {
    let mut kept = vec![true; segments.len()];
    for (i, s) in segments.iter().enumerate() {
        if s.text.split_whitespace().count() >= 4 || !is_backchannel(&s.text) {
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
    }

    let mut kept = kept.into_iter();
    let mut out = segments;
    out.retain(|_| kept.next().unwrap_or(true));
    out
}

/// Whether the whole utterance is backchannel: a listed phrase, or nothing but backchannel words and
/// fillers ("Yeah yeah.", "Um, okay.").
fn is_backchannel(text: &str) -> bool {
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
            ..CleanupConfig::default()
        }
    }

    /// Only the merge pass.
    fn merge_only() -> CleanupConfig {
        CleanupConfig {
            echo_drop: false,
            drop_backchannels: false,
            ..CleanupConfig::default()
        }
    }

    #[test]
    fn later_copy_of_a_far_end_utterance_is_dropped() {
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
            vec!["We should rotate the widget calibration before Friday."]
        );
        assert_eq!(stats.echo_dropped_me, 1);
        assert_eq!(stats.echo_dropped_them, 0);
    }

    #[test]
    fn one_source_kills_every_copy_inside_its_window() {
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
        assert_eq!(out.len(), 1);
        assert_eq!(stats.echo_dropped_me, 2);
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

        // …and it is genuinely a threshold, not an accident of the fixture.
        let loose = CleanupConfig {
            echo_containment: 0.6,
            ..echo_only()
        };
        let (out, _) = cleanup(near_miss_fixture(), &loose, &[]);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn one_word_phantom_inside_a_far_end_span_is_dropped_but_a_later_one_is_kept() {
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
        assert_eq!(stats.echo_dropped_me, 1, "only the contained span is echo");
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].start, 31.0);
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
            vec!["The dashboard still renders yesterday's totals."],
            "the carried source must not be re-emitted"
        );
        assert_eq!(stats.echo_dropped_me, 1);
        assert_eq!(carry.len(), 1, "carry is read-only");
    }

    /// A dropped copy must not become a source: the far end repeating itself right after our echo is real
    /// speech and has to survive.
    #[test]
    fn echoes_do_not_chain_through_a_dropped_copy() {
        let segments = vec![
            them(10.0, 12.0, "Rotate the widget calibration."),
            me(13.0, 14.0, "Rotate the widget calibration."),
            them(15.0, 17.0, "Rotate the widget calibration, yes."),
        ];
        let (out, stats) = cleanup(segments, &echo_only(), &[]);
        assert_eq!(out.len(), 2);
        assert_eq!(stats.echo_dropped_me, 1);
        assert_eq!(stats.echo_dropped_them, 0);
        assert!(!is_me(&out[1]));
    }

    #[test]
    fn stats_count_every_pass_and_both_echo_directions() {
        let segments = vec![
            // Me first, so this pair counts as a far-end echo of us.
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
        let (out, stats) = cleanup(segments, &CleanupConfig::default(), &[]);
        assert_eq!(
            stats,
            CleanupStats {
                echo_dropped_me: 1,
                echo_dropped_them: 1,
                merged: 1,
                backchannels_dropped: 1,
                echo_dropped_audio: 0,
            }
        );
        assert_eq!(
            texts(&out),
            vec![
                "The retention sweep runs hourly now.",
                "The gateway is what times out when the queue backs up.",
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
            ..CleanupConfig::default()
        };
        assert!(off.is_noop());
        assert!(!CleanupConfig::default().is_noop());

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
        let (out, stats) = cleanup(segments, &CleanupConfig::default(), &[]);
        assert_eq!(out.len(), 3, "no comparison against NaN can succeed");
        assert_eq!(stats.changed(), 0);
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
    fn a_span_with_no_blocks_falls_through_to_the_text_rules() {
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
        // The text echo rule still fires on the row that repeats the far end.
        assert_eq!(stats.echo_dropped_me, 1);
        assert_eq!(
            texts(&out),
            vec![
                "The calibration jig arrives from Toronto on Tuesday.",
                "Gateway harness.",
            ]
        );

        // An accessor that returns None for the span is the same story.
        let absent = |_s: f64, _e: f64| None;
        let (out, stats) = cleanup_with_evidence(segments, &echo_only(), &[], Some(&absent));
        assert_eq!(stats.echo_dropped_audio, 0);
        assert_eq!(stats.echo_dropped_me, 1);
        assert_eq!(out.len(), 2);
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
    fn a_very_negative_margin_switches_the_audio_rule_off_without_touching_the_text_rules() {
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
        assert_eq!(stats.echo_dropped_me, 1);
        assert_eq!(out.len(), 2);
    }
}
