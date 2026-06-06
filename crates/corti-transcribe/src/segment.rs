//! Shared reconciliation helpers: turn timestamped words into a [`DiarizedTranscript`]'s segments.
//!
//! Both backends produce per-word timing and need the same downstream shaping:
//! - group one speaker's words into pause-split utterances ([`words_to_segments`]),
//! - merge multiple speakers onto one timeline ([`merge_by_time`]),
//! - and, for the local backend, attribute far-end words to diarization turns and segment them in one pass
//!   ([`diarize_words`]).
//!
//! The AWS backend feeds channel-identified words (ch0 = me, ch1 = them); the local backend feeds Parakeet
//! words (ch0 = me) plus ch1 words attributed to pyannote speaker turns.

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
            Some(seg) if w.start - seg.end <= gap => {
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
}
