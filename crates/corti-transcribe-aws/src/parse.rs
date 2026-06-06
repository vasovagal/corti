//! Parse AWS Transcribe output into a [`DiarizedTranscript`].
//!
//! For a 2-channel mic+tap WAV (ch0 = me/mic, ch1 = them/tap) corti runs the job with
//! `ChannelIdentification` on, so AWS transcribes each channel separately and emits
//! `results.channel_labels.channels[]`, each labelled `ch_0` / `ch_1` with its own word `items[]`. That
//! maps deterministically to speakers — `ch_0` ⇒ [`Speaker::Me`], anything else ⇒
//! [`Speaker::Other`]`("Them")` — with no energy-alignment heuristics
//! ([`parse_channel_transcript`]).
//!
//! For a 1-channel tap-only ("webinar") WAV, AWS can't do channel identification, so the job is plain and
//! the result is a single flat `results.items[]` stream; every utterance is the far-end "Them"
//! ([`parse_single_channel_transcript`]).
//!
//! This module is pure (no AWS, no IO) so the join logic is unit-tested against canned JSON.

use anyhow::{Context, Result};
use corti_core::{DiarizedTranscript, Speaker};
use corti_transcribe::segment::{SEGMENT_GAP, Word, merge_by_time, words_to_segments};
use serde::Deserialize;

// ---- AWS Transcribe result JSON (only the fields we use) ----

#[derive(Debug, Deserialize)]
struct Root {
    results: Results,
}

#[derive(Debug, Deserialize)]
struct Results {
    /// Present when the job ran with channel identification (2-track mic+tap).
    channel_labels: Option<ChannelLabels>,
    /// The flat word-item stream of a plain (single-channel / tap-only) job.
    #[serde(default)]
    items: Vec<Item>,
}

#[derive(Debug, Deserialize)]
struct ChannelLabels {
    channels: Vec<Channel>,
}

#[derive(Debug, Deserialize)]
struct Channel {
    channel_label: String,
    #[serde(default)]
    items: Vec<Item>,
}

#[derive(Debug, Deserialize)]
struct Item {
    /// Absent for punctuation.
    start_time: Option<String>,
    end_time: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    alternatives: Vec<Alternative>,
}

#[derive(Debug, Deserialize)]
struct Alternative {
    content: String,
}

/// Parse a channel-identification transcript JSON into a single, time-ordered [`DiarizedTranscript`].
pub fn parse_channel_transcript(json: &str) -> Result<DiarizedTranscript> {
    let root: Root = serde_json::from_str(json).context("parsing AWS Transcribe result JSON")?;
    let channels = root
        .results
        .channel_labels
        .context("transcript JSON has no channel_labels (was channel identification enabled?)")?
        .channels;

    let mut segments = Vec::new();
    for channel in &channels {
        let words = items_to_words(&channel.items);
        segments.extend(words_to_segments(
            &words,
            speaker_for(&channel.channel_label),
            SEGMENT_GAP,
        ));
    }
    // Interleave both channels into one timeline.
    Ok(DiarizedTranscript::new(merge_by_time(segments)))
}

/// Parse a plain (non-channel-identification) transcript JSON — a tap-only / mono recording — into a
/// [`DiarizedTranscript`] with every utterance attributed to the far-end ("Them") track.
pub fn parse_single_channel_transcript(json: &str) -> Result<DiarizedTranscript> {
    let root: Root = serde_json::from_str(json).context("parsing AWS Transcribe result JSON")?;
    let words = items_to_words(&root.results.items);
    let segments = words_to_segments(&words, Speaker::Other("Them".to_string()), SEGMENT_GAP);
    // Items are already time-ordered, but sort defensively to match the channel path.
    Ok(DiarizedTranscript::new(merge_by_time(segments)))
}

/// Map a channel label to its speaker: `ch_0` is the near-end mic (me); any other channel is the far end.
fn speaker_for(channel_label: &str) -> Speaker {
    if channel_label == "ch_0" {
        Speaker::Me
    } else {
        Speaker::Other("Them".to_string())
    }
}

/// Convert a channel's word/punctuation items into timestamped [`Word`]s. Punctuation items (no
/// timestamps) glue onto the preceding word with no leading space; a leading punctuation with no word to
/// attach to is dropped; pronunciation items without timestamps are skipped. The shared
/// [`words_to_segments`] then groups these into pause-split segments (keeping the historical behaviour).
fn items_to_words(items: &[Item]) -> Vec<Word> {
    let mut out: Vec<Word> = Vec::new();

    for item in items {
        let content = match item.alternatives.first() {
            Some(a) if !a.content.is_empty() => a.content.as_str(),
            _ => continue,
        };

        if item.kind == "punctuation" {
            // Attach to the current word; drop a leading punctuation with no word to attach to.
            if let Some(w) = out.last_mut() {
                w.text.push_str(content);
            }
            continue;
        }

        // Pronunciation: needs timestamps to place it.
        let (start, end) = match (
            item.start_time.as_deref().and_then(parse_time),
            item.end_time.as_deref().and_then(parse_time),
        ) {
            (Some(s), Some(e)) => (s, e),
            _ => continue,
        };

        out.push(Word {
            start,
            end,
            text: content.to_string(),
        });
    }
    out
}

/// Parse an AWS timestamp string (seconds, e.g. `"4.86"`) into an `f64`.
fn parse_time(s: &str) -> Option<f64> {
    s.parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Two channels: ch_0 (me) with a pause that splits two segments + trailing punctuation;
    // ch_1 (them) speaking in between so the merged timeline interleaves.
    const SAMPLE: &str = r#"
    {
      "jobName": "demo",
      "results": {
        "transcripts": [{ "transcript": "ignored" }],
        "channel_labels": {
          "number_of_channels": 2,
          "channels": [
            {
              "channel_label": "ch_0",
              "items": [
                { "start_time": "0.0", "end_time": "0.4", "type": "pronunciation",
                  "alternatives": [{ "content": "Morning" }] },
                { "start_time": "0.4", "end_time": "0.8", "type": "pronunciation",
                  "alternatives": [{ "content": "team" }] },
                { "type": "punctuation", "alternatives": [{ "content": "." }] },
                { "start_time": "6.0", "end_time": "6.5", "type": "pronunciation",
                  "alternatives": [{ "content": "Thanks" }] }
              ]
            },
            {
              "channel_label": "ch_1",
              "items": [
                { "start_time": "2.0", "end_time": "2.5", "type": "pronunciation",
                  "alternatives": [{ "content": "Hello" }] },
                { "start_time": "2.5", "end_time": "2.9", "type": "pronunciation",
                  "alternatives": [{ "content": "there" }] }
              ]
            }
          ]
        }
      }
    }"#;

    #[test]
    fn maps_channels_to_me_and_them_and_interleaves_by_time() {
        let t = parse_channel_transcript(SAMPLE).unwrap();
        // Three segments: ch0 [0.0-0.8], ch1 [2.0-2.9], ch0 [6.0-6.5], sorted by start.
        assert_eq!(t.segments.len(), 3);

        assert_eq!(t.segments[0].speaker, Speaker::Me);
        assert_eq!(t.segments[0].text, "Morning team."); // gap-joined + glued punctuation
        assert_eq!(t.segments[0].start, 0.0);
        assert_eq!(t.segments[0].end, 0.8);

        assert_eq!(t.segments[1].speaker, Speaker::Other("Them".into()));
        assert_eq!(t.segments[1].text, "Hello there");
        assert_eq!(t.segments[1].start, 2.0);

        assert_eq!(t.segments[2].speaker, Speaker::Me);
        assert_eq!(t.segments[2].text, "Thanks"); // split from segment 0 by the >1.5s gap
        assert_eq!(t.segments[2].start, 6.0);
    }

    #[test]
    fn renders_to_markdown_via_core() {
        let t = parse_channel_transcript(SAMPLE).unwrap();
        let md = t.to_markdown();
        assert!(md.contains("**[00:00] Me:** Morning team."));
        assert!(md.contains("**[00:02] Them:** Hello there"));
        assert!(md.contains("**[00:06] Me:** Thanks"));
    }

    #[test]
    fn empty_channels_yield_empty_transcript() {
        let json = r#"{ "results": { "channel_labels": { "channels": [
            { "channel_label": "ch_0", "items": [] },
            { "channel_label": "ch_1", "items": [] }
        ] } } }"#;
        let t = parse_channel_transcript(json).unwrap();
        assert!(t.segments.is_empty());
        assert_eq!(t.to_markdown(), "_(no speech transcribed)_\n");
    }

    #[test]
    fn missing_channel_labels_is_an_error() {
        let json = r#"{ "results": { "transcripts": [{ "transcript": "hi" }] } }"#;
        assert!(parse_channel_transcript(json).is_err());
    }

    #[test]
    fn leading_punctuation_without_a_word_is_dropped() {
        let json = r#"{ "results": { "channel_labels": { "channels": [
            { "channel_label": "ch_0", "items": [
                { "type": "punctuation", "alternatives": [{ "content": "," }] },
                { "start_time": "1.0", "end_time": "1.3", "type": "pronunciation",
                  "alternatives": [{ "content": "Hi" }] }
            ] }
        ] } } }"#;
        let t = parse_channel_transcript(json).unwrap();
        assert_eq!(t.segments.len(), 1);
        assert_eq!(t.segments[0].text, "Hi");
    }

    #[test]
    fn single_channel_attributes_everything_to_them() {
        // A plain (tap-only / mono) job: a flat results.items stream with a >1.5s gap splitting two
        // segments and trailing punctuation glued on, every segment attributed to "Them".
        let json = r#"{ "results": {
            "transcripts": [{ "transcript": "ignored" }],
            "items": [
                { "start_time": "0.0", "end_time": "0.4", "type": "pronunciation",
                  "alternatives": [{ "content": "Hello" }] },
                { "start_time": "0.4", "end_time": "0.8", "type": "pronunciation",
                  "alternatives": [{ "content": "world" }] },
                { "type": "punctuation", "alternatives": [{ "content": "." }] },
                { "start_time": "5.0", "end_time": "5.3", "type": "pronunciation",
                  "alternatives": [{ "content": "Bye" }] }
            ]
        } }"#;
        let t = parse_single_channel_transcript(json).unwrap();
        assert_eq!(t.segments.len(), 2);
        assert_eq!(t.segments[0].speaker, Speaker::Other("Them".into()));
        assert_eq!(t.segments[0].text, "Hello world.");
        assert_eq!(t.segments[1].speaker, Speaker::Other("Them".into()));
        assert_eq!(t.segments[1].text, "Bye");
        assert_eq!(t.segments[1].start, 5.0);
    }

    #[test]
    fn single_channel_with_no_items_is_empty() {
        // No `items` (and no `channel_labels`) → empty transcript, not an error.
        let t = parse_single_channel_transcript(r#"{ "results": { "transcripts": [] } }"#).unwrap();
        assert!(t.segments.is_empty());
    }
}
