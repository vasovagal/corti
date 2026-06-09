//! The one and only vagus touchpoint.
//!
//! corti never writes the iCloud vault or the vagus index directly (that would violate vagus's
//! guardrails). Instead it shells out to the installed `vagus` CLI exactly like vagus's own Claude Code
//! skills do:
//!
//! ```text
//! vagus add-note "<title>" --source "<source>" --print-path  < body-on-stdin
//! ```
//!
//! `--print-path` makes `vagus` skip the editor and print the created note path, which we capture.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use corti_core::{DiarizedTranscript, RecordingMeta};

/// A handle to the `vagus` binary.
pub struct Vagus {
    bin: PathBuf,
}

impl Vagus {
    /// Resolve the `vagus` binary: `$VAGUS_BIN` override, else `vagus` on `PATH`.
    ///
    /// Verifies it is runnable now so callers fail fast with a clear message rather than at filing time.
    pub fn discover() -> Result<Self> {
        let bin = std::env::var_os("VAGUS_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("vagus"));
        let v = Self { bin };
        v.check()?;
        Ok(v)
    }

    fn check(&self) -> Result<()> {
        match Command::new(&self.bin).arg("--version").output() {
            Ok(out) if out.status.success() => Ok(()),
            Ok(out) => bail!(
                "`{} --version` failed (exit {}): {}",
                self.bin.display(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
                "`vagus` not found (looked for `{}`). Install it (`brew install vagus`) or set $VAGUS_BIN.",
                self.bin.display()
            ),
            Err(e) => Err(e).with_context(|| format!("running `{}`", self.bin.display())),
        }
    }

    /// Create a note with `vagus add-note`, piping `body` on stdin. Returns the created note path.
    pub fn add_note(&self, title: &str, source: &str, body: &str) -> Result<PathBuf> {
        let mut child = Command::new(&self.bin)
            .args(["add-note", title, "--source", source, "--print-path"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawning `{} add-note`", self.bin.display()))?;

        child
            .stdin
            .take()
            .context("vagus stdin unavailable")?
            .write_all(body.as_bytes())
            .context("writing note body to vagus stdin")?;

        let out = child.wait_with_output().context("waiting for vagus")?;
        if !out.status.success() {
            bail!(
                "vagus add-note failed (exit {}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if path.is_empty() {
            bail!("vagus add-note printed no path");
        }
        Ok(PathBuf::from(path))
    }

    /// File a completed recording's transcript as a vagus note. Returns the created note path.
    pub fn file_recording(
        &self,
        meta: &RecordingMeta,
        transcript: &DiarizedTranscript,
    ) -> Result<PathBuf> {
        self.add_note(
            &meta.note_title(),
            &meta.source(),
            &recording_body(meta, transcript),
        )
    }
}

/// Compose the note body: a short auto-capture context line, then the diarized transcript. `pub` so the app
/// can render the same body when it writes a note to an explicit path (the `corti --input --output` CLI)
/// instead of filing through `vagus add-note`.
pub fn recording_body(meta: &RecordingMeta, transcript: &DiarizedTranscript) -> String {
    let mut body = format!(
        "> Auto-captured by corti from {}.\n\n",
        meta.owning_app.name
    );
    body.push_str("## Transcript\n\n");
    body.push_str(&transcript.to_markdown());
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use corti_core::{OwningApp, Speaker, TranscriptSegment};
    use std::path::PathBuf;

    fn meta() -> RecordingMeta {
        RecordingMeta {
            started_at: chrono::Local::now(),
            ended_at: None,
            owning_app: OwningApp::from_bundle_id("us.zoom.xos"),
            audio_path: PathBuf::from("/tmp/x.wav"),
        }
    }

    #[test]
    fn body_includes_context_and_transcript() {
        let t = DiarizedTranscript::new(vec![TranscriptSegment {
            speaker: Speaker::Me,
            start: 0.0,
            end: 1.0,
            text: "hello".into(),
        }]);
        let body = recording_body(&meta(), &t);
        assert!(body.contains("Auto-captured by corti from Zoom."));
        assert!(body.contains("## Transcript"));
        assert!(body.contains("**[00:00] Me:** hello"));
    }
}
