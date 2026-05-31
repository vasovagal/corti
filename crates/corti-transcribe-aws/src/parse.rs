//! Parse AWS Transcribe **channel-identification** output into a [`DiarizedTranscript`].
//!
//! corti uploads a 2-channel WAV (ch0 = me/mic, ch1 = them/tap) with `ChannelIdentification` on, so AWS
//! transcribes each channel separately and emits `results.channel_labels.channels[]`, each labelled
//! `ch_0` / `ch_1` with its own word `items[]`. That maps deterministically to speakers — `ch_0` ⇒
//! [`Speaker::Me`], anything else ⇒ [`Speaker::Other`]`("Them")` — with no energy-alignment heuristics.
//!
//! This module is pure (no AWS, no IO) so the join logic is unit-tested against canned JSON.

use anyhow::{Context, Result};
use corti_core::{DiarizedTranscript, Speaker, TranscriptSegment};
use serde::Deserialize;

/// Start a new segment when the gap between consecutive words in a channel exceeds this (seconds), so a
/// channel's stream is broken into readable utterances rather than one run-on blob.
const SEGMENT_GAP: f64 = 1.5;

// ---- AWS Transcribe result JSON (only the fields we use) ----

#[derive(Debug, Deserialize)]
struct Root {
    results: Results,
}

#[derive(Debug, Deserialize)]
struct Results {
    /// Present when the job ran with channel identification.
    channel_labels: Option<ChannelLabels>,
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

    let mut segments: Vec<TranscriptSegment> = Vec::new();
    for channel in &channels {
        segments.extend(segments_for_channel(channel));
    }
    // Interleave both channels into one timeline. `total_cmp` keeps it panic-free on any odd NaN.
    segments.sort_by(|a, b| a.start.total_cmp(&b.start));
    Ok(DiarizedTranscript::new(segments))
}

/// Map a channel label to its speaker: `ch_0` is the near-end mic (me); any other channel is the far end.
fn speaker_for(channel_label: &str) -> Speaker {
    if channel_label == "ch_0" {
        Speaker::Me
    } else {
        Speaker::Other("Them".to_string())
    }
}

/// Group one channel's word items into segments, splitting on a pause longer than [`SEGMENT_GAP`].
/// Punctuation items (no timestamps) glue onto the current word with no leading space.
fn segments_for_channel(channel: &Channel) -> Vec<TranscriptSegment> {
    let speaker = speaker_for(&channel.channel_label);
    let mut out: Vec<TranscriptSegment> = Vec::new();
    let mut cur: Option<TranscriptSegment> = None;

    for item in &channel.items {
        let content = match item.alternatives.first() {
            Some(a) if !a.content.is_empty() => a.content.as_str(),
            _ => continue,
        };

        if item.kind == "punctuation" {
            // Attach to the current segment; drop a leading punctuation with no word to attach to.
            if let Some(seg) = cur.as_mut() {
                seg.text.push_str(content);
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

        match cur.as_mut() {
            Some(seg) if start - seg.end <= SEGMENT_GAP => {
                seg.text.push(' ');
                seg.text.push_str(content);
                seg.end = end;
            }
            _ => {
                if let Some(seg) = cur.take() {
                    out.push(seg);
                }
                cur = Some(TranscriptSegment {
                    speaker: speaker.clone(),
                    start,
                    end,
                    text: content.to_string(),
                });
            }
        }
    }
    if let Some(seg) = cur.take() {
        out.push(seg);
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
}
