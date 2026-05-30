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

    /// Render as a Markdown note body: one line per segment,
    /// `**[mm:ss] Speaker:** text`. Empty transcripts render a placeholder line.
    pub fn to_markdown(&self) -> String {
        if self.segments.is_empty() {
            return "_(no speech transcribed)_\n".to_string();
        }
        let mut out = String::new();
        for seg in &self.segments {
            out.push_str(&format!(
                "**[{}] {}:** {}\n\n",
                fmt_timestamp(seg.start),
                seg.speaker.display(),
                seg.text.trim()
            ));
        }
        out
    }
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
}
