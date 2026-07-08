//! In-place note writes for live inbox filing (ADR 0010).
//!
//! ADR 0001 confines corti to `vagus add-note`; ADR 0010 amends that with exactly three writes, all
//! against a path `--print-path` returned: **append** body content while a call is live, **flip** the
//! state line in place when the transcript is final, and **rewrite** the body when the batch path
//! supersedes a partial live note. (The fourth permitted action — deleting the note when its recording
//! is discarded — is a plain `remove_file` at the call site.)
//!
//! ## The state-line contract (issue #87)
//! The first corti-authored body line (right under vagus's `# <title>` heading) is exactly
//! [`STATE_TRANSCRIBING`] while segments are streaming in and exactly [`STATE_TRANSCRIBED`] — padded
//! with one trailing space to the **same byte width** — once final. Inbox agents key off this line.
//! [`flip_state`] seeks and overwrites only those bytes: no rename, no truncation, no full-file
//! rewrite, so a `tail -f` follower keeps its inode and never observes the file shrink mid-stream.

use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{Context, Result};

/// State line while live segments are still being appended.
pub const STATE_TRANSCRIBING: &str = "State: transcribing";
/// Final state line. The trailing space pads it to the byte width of [`STATE_TRANSCRIBING`] so the
/// flip is a same-width in-place overwrite.
pub const STATE_TRANSCRIBED: &str = "State: transcribed ";

/// Append `text` to the note, flushed so an external follower (`tail -f`) sees it immediately.
pub fn append(path: &Path, text: &str) -> Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .with_context(|| format!("opening {} for append", path.display()))?;
    f.write_all(text.as_bytes())
        .with_context(|| format!("appending to {}", path.display()))?;
    f.flush()?;
    Ok(())
}

/// Flip the state line to [`STATE_TRANSCRIBED`] in place: seek to the line and overwrite exactly its
/// bytes (same width — no rename, no truncation; the inode survives). Idempotent.
pub fn flip_state(path: &Path) -> Result<()> {
    const _: () = assert!(STATE_TRANSCRIBING.len() == STATE_TRANSCRIBED.len());
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading {} to flip its state line", path.display()))?;
    let off = state_line_offset(&content)
        .with_context(|| format!("{} has no state line to flip", path.display()))?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("opening {} to flip its state line", path.display()))?;
    f.seek(SeekFrom::Start(off as u64))?;
    f.write_all(STATE_TRANSCRIBED.as_bytes())?;
    f.flush()?;
    Ok(())
}

/// Replace everything from the state line onward with `new_body` (which carries its own state line),
/// keeping the frontmatter + title prefix. Used when the batch path supersedes a partial live note —
/// truncate + write on the same file, never a rename, so the inode survives. (A follower may see the
/// file shrink here; the strict no-shrink guarantee only covers the mid-stream [`flip_state`].)
pub fn rewrite_body(path: &Path, new_body: &str) -> Result<()> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading {} to rewrite its body", path.display()))?;
    // Our live notes always start their body with the state line; the frontmatter fallback only
    // covers a hand-edited note (the title heading is lost then — acceptable for a repair path).
    let keep = state_line_offset(&content).unwrap_or_else(|| frontmatter_end(&content));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("opening {} to rewrite its body", path.display()))?;
    f.write_all(content[..keep].as_bytes())?;
    f.write_all(new_body.as_bytes())?;
    f.flush()?;
    Ok(())
}

/// Byte offset of the first line that is exactly a state line (either form). `None` if absent.
fn state_line_offset(content: &str) -> Option<usize> {
    let mut off = 0usize;
    for line in content.split_inclusive('\n') {
        let text = line.trim_end_matches('\n').trim_end_matches('\r');
        if text == STATE_TRANSCRIBING || text == STATE_TRANSCRIBED {
            return Some(off);
        }
        off += line.len();
    }
    None
}

/// Byte offset just past the closing `---` of a leading YAML frontmatter block (0 if there is none).
fn frontmatter_end(content: &str) -> usize {
    if let Some(rest) = content.strip_prefix("---\n")
        && let Some(i) = rest.find("\n---\n")
    {
        return "---\n".len() + i + "\n---\n".len();
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use std::path::PathBuf;

    fn test_note(name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("corti-note-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("note.md");
        std::fs::write(&p, content).unwrap();
        p
    }

    /// A live note exactly as vagus + corti produce it: frontmatter, title heading, then the
    /// corti-authored body starting with the state line.
    fn live_note_content() -> String {
        format!(
            "---\ncreated: 2026-07-08T10:00\nstatus: inbox\nsource: Zoom · 2026-07-08 10:00\n---\n\n\
             # Zoom call — 2026-07-08 10:00\n\n\
             {STATE_TRANSCRIBING}\n\n\
             > Auto-captured by corti from Zoom.\n\n## Transcript\n\n"
        )
    }

    #[test]
    fn state_lines_have_equal_byte_width() {
        assert_eq!(STATE_TRANSCRIBING.len(), STATE_TRANSCRIBED.len());
        // The padding is exactly one trailing space — the contract inbox agents match on.
        assert_eq!(STATE_TRANSCRIBED, "State: transcribed ");
    }

    #[test]
    fn append_adds_exactly_the_text() {
        let p = test_note("append", &live_note_content());
        append(&p, "**[00:00] Me:** hello\n\n").unwrap();
        append(&p, "**[00:03] Them:** hi\n\n").unwrap();
        let got = std::fs::read_to_string(&p).unwrap();
        assert!(got.ends_with("**[00:00] Me:** hello\n\n**[00:03] Them:** hi\n\n"));
        assert!(got.starts_with("---\n")); // prefix untouched
    }

    #[test]
    fn flip_is_same_width_in_place_and_keeps_the_inode() {
        let p = test_note("flip", &live_note_content());
        append(&p, "**[00:00] Me:** hello\n\n").unwrap();
        let before = std::fs::read_to_string(&p).unwrap();
        let ino = std::fs::metadata(&p).unwrap().ino();

        flip_state(&p).unwrap();

        let after = std::fs::read_to_string(&p).unwrap();
        assert_eq!(
            before.len(),
            after.len(),
            "flip must not change the byte length"
        );
        assert_eq!(
            std::fs::metadata(&p).unwrap().ino(),
            ino,
            "flip must keep the inode"
        );
        assert!(after.contains(&format!("\n{STATE_TRANSCRIBED}\n")));
        assert!(!after.contains(STATE_TRANSCRIBING));
        // Everything except the flipped line is byte-identical.
        assert_eq!(before.replace(STATE_TRANSCRIBING, STATE_TRANSCRIBED), after);
        // Idempotent.
        flip_state(&p).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), after);
    }

    #[test]
    fn flip_without_a_state_line_errors() {
        let p = test_note("flip-missing", "---\na: b\n---\n\n# T\n\nno state here\n");
        let err = flip_state(&p).unwrap_err().to_string();
        assert!(err.contains("no state line"), "got: {err}");
    }

    #[test]
    fn rewrite_replaces_from_the_state_line_and_keeps_the_inode() {
        let p = test_note("rewrite", &live_note_content());
        append(&p, "**[00:00] Me:** partial words\n\n").unwrap();
        let ino = std::fs::metadata(&p).unwrap().ino();

        let new_body = format!(
            "{STATE_TRANSCRIBED}\n\n> Auto-captured by corti from Zoom.\n\n## Transcript\n\n\
             **[00:00] Me:** the full batch transcript\n\n"
        );
        rewrite_body(&p, &new_body).unwrap();

        let got = std::fs::read_to_string(&p).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().ino(), ino);
        assert!(got.starts_with("---\ncreated:"), "frontmatter kept");
        assert!(got.contains("# Zoom call — 2026-07-08 10:00"), "title kept");
        assert!(got.contains("the full batch transcript"));
        assert!(!got.contains("partial words"), "old body replaced");
        assert!(got.contains(&format!("\n{STATE_TRANSCRIBED}\n")));
    }

    #[test]
    fn rewrite_without_a_state_line_keeps_frontmatter_only() {
        let p = test_note("rewrite-fm", "---\na: b\n---\nold body\n");
        rewrite_body(&p, "new body\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "---\na: b\n---\nnew body\n"
        );
    }

    #[test]
    fn state_line_offset_finds_the_exact_line_only() {
        let content = live_note_content();
        let off = state_line_offset(&content).unwrap();
        assert!(content[off..].starts_with(STATE_TRANSCRIBING));
        // A transcript line merely mentioning the phrase is not a state line.
        assert!(state_line_offset("**[00:00] Me:** State: transcribing sounds odd\n").is_none());
    }
}
