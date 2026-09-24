//! A diarized, timestamped transcript and its Markdown rendering.
//!
//! This is the common output shape every transcription backend produces (`corti-transcribe`), and the
//! input to note filing (`corti-vagus`). The type renders its own Markdown so that filing depends only on
//! `corti-core`, not on any particular backend.

use serde::{Deserialize, Serialize};

/// Who spoke a segment. The near-end mic track is always `Me`; everyone else is an `Other` with a display
/// label (e.g. AWS speaker `spk_0` → `Speaker 1`, or simply `Them` for the far-end track).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "label")]
pub enum Speaker {
    /// The user (the near-end mic track).
    Me,
    /// Any other speaker, with a display label.
    Other(String),
}

impl Speaker {
    /// The label shown in rendered Markdown.
    pub fn display(&self) -> &str {
        match self {
            Speaker::Me => "Me",
            Speaker::Other(label) => label,
        }
    }
}

/// One contiguous utterance by a single speaker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub speaker: Speaker,
    /// Seconds from the start of the recording.
    pub start: f64,
    /// Seconds from the start of the recording.
    pub end: f64,
    pub text: String,
}

/// A full diarized transcript: an ordered list of segments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DiarizedTranscript {
    pub segments: Vec<TranscriptSegment>,
}

impl DiarizedTranscript {
    pub fn new(segments: Vec<TranscriptSegment>) -> Self {
        Self { segments }
    }

    /// Render as a Markdown note body: one line per segment, `**[mm:ss] Speaker:** text`, in start
    /// order (#157: the live path splices `Me` batches into the `Them` stream, so segment order is not
    /// time order; a stable sort keeps same-start segments as they arrived). Empty transcripts render a
    /// placeholder line.
    pub fn to_markdown(&self) -> String {
        if self.segments.is_empty() {
            return "_(no speech transcribed)_\n".to_string();
        }
        let mut ordered: Vec<&TranscriptSegment> = self.segments.iter().collect();
        ordered.sort_by(|a, b| a.start.total_cmp(&b.start));
        let mut out = String::new();
        for seg in ordered {
            out.push_str(&format!(
                "**[{}] {}:** {}\n\n",
                fmt_timestamp(seg.start),
                seg.speaker.display(),
                seg.text.trim()
            ));
        }
        out
    }

    /// The tolerant inverse of [`to_markdown`](Self::to_markdown): every line of the form
    /// `**[mm:ss] Speaker:** text` (or `h:mm:ss`) becomes a segment whose `end` equals its `start`
    /// (the note carries no end times); anything else — frontmatter, headings, blank lines, prose — is
    /// ignored. Segments keep file order; render with `to_markdown` to sort them.
    pub fn parse_markdown(text: &str) -> Self {
        let segments = text.lines().filter_map(parse_turn_line).collect();
        Self { segments }
    }
}

fn parse_turn_line(line: &str) -> Option<TranscriptSegment> {
    let rest = line.trim_start().strip_prefix("**[")?;
    let (stamp, rest) = rest.split_once("] ")?;
    let start = parse_timestamp(stamp)?;
    let (speaker, text) = rest.split_once(":**")?;
    let speaker = speaker.trim();
    if speaker.is_empty() {
        return None;
    }
    let speaker = if speaker == "Me" {
        Speaker::Me
    } else {
        Speaker::Other(speaker.to_owned())
    };
    Some(TranscriptSegment {
        speaker,
        start,
        end: start,
        text: text.trim().to_owned(),
    })
}

fn parse_timestamp(stamp: &str) -> Option<f64> {
    let parts: Vec<&str> = stamp.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }
    let mut total = 0u64;
    for part in &parts {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        total = total
            .checked_mul(60)?
            .checked_add(part.parse::<u64>().ok()?)?;
    }
    Some(total as f64)
}

/// Format seconds as `mm:ss`, or `h:mm:ss` past an hour.
fn fmt_timestamp(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_formatting() {
        assert_eq!(fmt_timestamp(0.0), "00:00");
        assert_eq!(fmt_timestamp(5.4), "00:05");
        assert_eq!(fmt_timestamp(72.0), "01:12");
        assert_eq!(fmt_timestamp(3661.0), "1:01:01");
        assert_eq!(fmt_timestamp(-3.0), "00:00");
    }

    #[test]
    fn markdown_renders_speaker_and_time() {
        let t = DiarizedTranscript::new(vec![
            TranscriptSegment {
                speaker: Speaker::Me,
                start: 0.0,
                end: 2.5,
                text: "Hey, can you hear me?".into(),
            },
            TranscriptSegment {
                speaker: Speaker::Other("Speaker 1".into()),
                start: 3.0,
                end: 6.0,
                text: "  Yep, loud and clear.  ".into(),
            },
        ]);
        let md = t.to_markdown();
        assert!(md.contains("**[00:00] Me:** Hey, can you hear me?"));
        assert!(md.contains("**[00:03] Speaker 1:** Yep, loud and clear."));
    }

    #[test]
    fn empty_transcript_has_placeholder() {
        assert_eq!(
            DiarizedTranscript::default().to_markdown(),
            "_(no speech transcribed)_\n"
        );
    }

    #[test]
    fn markdown_is_rendered_in_start_order_and_parses_back() {
        let out_of_order = DiarizedTranscript::new(vec![
            TranscriptSegment {
                speaker: Speaker::Other("Them".into()),
                start: 314.0,
                end: 320.0,
                text: "Later far-end turn.".into(),
            },
            TranscriptSegment {
                speaker: Speaker::Me,
                start: 41.0,
                end: 43.0,
                text: "Earlier mic turn.".into(),
            },
            TranscriptSegment {
                speaker: Speaker::Me,
                start: 3661.0,
                end: 3662.0,
                text: "Past the hour.".into(),
            },
        ]);
        let md = out_of_order.to_markdown();
        assert_eq!(
            md,
            "**[00:41] Me:** Earlier mic turn.\n\n**[05:14] Them:** Later far-end turn.\n\n**[1:01:01] Me:** Past the hour.\n\n"
        );
        let with_noise =
            format!("---\ntitle: x\n---\n# Heading\n\n{md}\nnot a turn\n**[broken Me:** nope\n");
        let parsed = DiarizedTranscript::parse_markdown(&with_noise);
        assert_eq!(parsed.segments.len(), 3);
        assert_eq!(parsed.segments[0].speaker, Speaker::Me);
        assert_eq!(parsed.segments[0].start, 41.0);
        assert_eq!(parsed.segments[0].end, 41.0);
        assert_eq!(parsed.segments[0].text, "Earlier mic turn.");
        assert_eq!(parsed.segments[1].speaker, Speaker::Other("Them".into()));
        assert_eq!(parsed.segments[1].start, 314.0);
        assert_eq!(parsed.segments[2].start, 3661.0);
        assert_eq!(parsed.to_markdown(), md, "round trip is stable");
    }
}
