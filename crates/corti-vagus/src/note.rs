//! In-place note writes for live inbox filing (ADR 0010).
//!
//! ADR 0001 confines corti to `vagus add-note`; ADR 0010 amends that with exactly four operations,
//! all against a path `--print-path` returned: **append** body content (live segments, or a failure
//! annotation), **flip** the state line in place when the transcript is final, **rewrite** the body
//! when the batch path supersedes a partial live note, and **delete** the note when its recording is
//! discarded (a plain `remove_file` at the call site — the first three live here).
//!
//! ## The state-line contract (issue #87)
//! The first corti-authored body line (right under vagus's `# <title>` heading) is exactly
//! [`STATE_TRANSCRIBING`] while segments are streaming in and exactly [`STATE_TRANSCRIBED`] — padded
//! with one trailing space to the **same byte width** — once final. Inbox agents key off this line.
//! [`flip_state`] seeks and overwrites only those bytes: no rename, no truncation, no full-file
//! rewrite, so a `tail -f` follower keeps its inode and never observes the file shrink mid-stream.

use std::fmt;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use crate::provenance::TranscriptProvenance;

/// State line while live segments are still being appended.
pub const STATE_TRANSCRIBING: &str = "State: transcribing";
/// Final state line. The trailing space pads it to the byte width of [`STATE_TRANSCRIBING`] so the
/// flip is a same-width in-place overwrite.
pub const STATE_TRANSCRIBED: &str = "State: transcribed ";

/// Opaque authority for the one note path returned for the current recording.
///
/// The handle intentionally exposes no vault discovery, sibling traversal, general read, or path accessor.
/// It can only perform Corti's bounded-prefix, same-inode transcript/provenance rewrite. Provider adapters
/// accept typed transcript rows and therefore have no reason or API surface to receive this handle.
#[derive(Clone)]
pub struct CurrentNote {
    path: PathBuf,
}

impl CurrentNote {
    /// Bind the exact path returned by `vagus add-note --print-path` or recovered from this recording's
    /// durable checkpoint. Callers remain responsible for preserving that ownership chain.
    pub fn from_returned_path(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        ensure!(
            !path.as_os_str().is_empty(),
            "current-note path must not be empty"
        );
        Ok(Self { path })
    }

    /// Replace this recording's transcript body and Corti provenance while preserving the note inode and
    /// bounded frontmatter/title prefix. The helper reads only that prefix; it never returns vault text.
    pub fn rewrite_transcript(
        &self,
        new_body: &str,
        provenance: &TranscriptProvenance,
    ) -> Result<()> {
        rewrite_body_with_provenance(&self.path, new_body, provenance)
    }
}

impl fmt::Debug for CurrentNote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CurrentNote(<opaque-current-path>)")
    }
}

/// Append `text` to the note as one durable chunk. [`File::sync_all`](std::fs::File::sync_all) is the
/// explicit crash boundary: after this returns, an app or macOS crash may lose a later in-memory chunk but
/// must not lose this one. A plain `Write::flush` is insufficient because `File` is unbuffered in userspace
/// and the dirty pages may still exist only in the kernel cache.
pub fn append(path: &Path, text: &str) -> Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .with_context(|| format!("opening {} for append", path.display()))?;
    f.write_all(text.as_bytes())
        .with_context(|| format!("appending to {}", path.display()))?;
    f.sync_all()
        .with_context(|| format!("syncing appended chunk in {}", path.display()))?;
    Ok(())
}

/// Sync a note just created by `vagus add-note` and its parent directory. The file sync makes the initial
/// `State: transcribing` body durable; the directory sync makes the new name durable. Call this once before
/// publishing the path to the queue or appending the first transcript chunk.
pub fn sync_created(path: &Path) -> Result<()> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening newly-created note {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing newly-created note {}", path.display()))?;
    if let Some(parent) = path.parent() {
        let dir = std::fs::File::open(parent)
            .with_context(|| format!("opening note directory {}", parent.display()))?;
        dir.sync_all()
            .with_context(|| format!("syncing note directory {}", parent.display()))?;
    }
    Ok(())
}

/// Flip the state line to [`STATE_TRANSCRIBED`] in place: seek to the line and overwrite exactly its
/// bytes (same width — no rename, no truncation; the inode survives). Idempotent.
pub fn flip_state(path: &Path) -> Result<()> {
    const _: () = assert!(STATE_TRANSCRIBING.len() == STATE_TRANSCRIBED.len());
    let off = state_line_offset_in_file(path)?
        .with_context(|| format!("{} has no state line to flip", path.display()))?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("opening {} to flip its state line", path.display()))?;
    f.seek(SeekFrom::Start(off))?;
    f.write_all(STATE_TRANSCRIBED.as_bytes())?;
    f.sync_all()
        .with_context(|| format!("syncing state line in {}", path.display()))?;
    Ok(())
}

/// Replace everything from the state line onward with `new_body` (which carries its own state line),
/// keeping the frontmatter + title prefix. Used when the batch path supersedes a partial live note —
/// truncate + write on the same file, never a rename, so the inode survives. (A follower may see the
/// file shrink here; the strict no-shrink guarantee only covers the mid-stream [`flip_state`].)
pub fn rewrite_body(path: &Path, new_body: &str) -> Result<()> {
    rewrite_body_inner(path, new_body, None)
}

/// Replace the body and upsert the Corti-owned provenance field in the same bounded-prefix, same-inode
/// rewrite. A partial live note that falls back to batch must never retain stale `mode: live` metadata over
/// its canonical batch transcript. Provenance is written into the prefix before the final body bytes.
pub fn rewrite_body_with_provenance(
    path: &Path,
    new_body: &str,
    provenance: &TranscriptProvenance,
) -> Result<()> {
    rewrite_body_inner(path, new_body, Some(provenance))
}

fn rewrite_body_inner(
    path: &Path,
    new_body: &str,
    provenance: Option<&TranscriptProvenance>,
) -> Result<()> {
    // Scan with a reusable line buffer: final live notes can be arbitrarily long, but their state line is
    // near the top and a state flip/body rewrite must not clone the whole transcript into RAM.
    let keep = match state_line_offset_in_file(path)? {
        Some(offset) => offset,
        // The fallback only covers a hand-edited note (the title heading is lost — acceptable for repair).
        None => frontmatter_end_in_file(path)?,
    };
    const MAX_PREFIX_BYTES: u64 = 64 * 1024;
    ensure!(
        keep <= MAX_PREFIX_BYTES,
        "refusing to retain an unexpectedly large note prefix ({keep} bytes) while rewriting {}",
        path.display()
    );
    let mut prefix = Vec::with_capacity(keep as usize);
    std::fs::File::open(path)?
        .take(keep)
        .read_to_end(&mut prefix)
        .with_context(|| format!("reading prefix of {} to rewrite its body", path.display()))?;
    if let Some(provenance) = provenance {
        prefix = upsert_provenance(&prefix, provenance)
            .with_context(|| format!("updating transcript provenance in {}", path.display()))?;
        ensure!(
            prefix.len() as u64 <= MAX_PREFIX_BYTES,
            "refusing an unexpectedly large note prefix ({} bytes) after adding provenance to {}",
            prefix.len(),
            path.display()
        );
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("opening {} to rewrite its body", path.display()))?;
    f.write_all(&prefix)?;
    f.write_all(new_body.as_bytes())?;
    f.sync_all()
        .with_context(|| format!("syncing rewritten body in {}", path.display()))?;
    Ok(())
}

/// Replace or insert the top-level `corti:` field in a bounded note prefix. Current Vagus writes the value
/// on one JSON-flow line; the indented-line removal also handles a user reformatting that object as block
/// YAML before a fallback rewrite.
fn upsert_provenance(prefix: &[u8], provenance: &TranscriptProvenance) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(prefix).context("note prefix is not UTF-8")?;
    let lines: Vec<(usize, usize, &str)> = text
        .split_inclusive('\n')
        .scan(0usize, |offset, line| {
            let start = *offset;
            *offset += line.len();
            Some((start, *offset, line))
        })
        .collect();
    ensure!(
        lines
            .first()
            .is_some_and(|(_, _, line)| line.trim_end_matches(['\r', '\n']) == "---"),
        "note has no leading YAML frontmatter"
    );
    let close = lines
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, (_, _, line))| line.trim_end_matches(['\r', '\n']) == "---")
        .map(|(index, _)| index)
        .context("note has unterminated YAML frontmatter")?;
    let replacement = provenance
        .frontmatter_line()
        .context("serializing transcript provenance")?;

    let existing = lines[..close]
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, (_, _, line))| line.starts_with("corti:"));
    let (start, end) = if let Some((index, (start, initial_end, _))) = existing {
        let mut end = *initial_end;
        for (_, line_end, line) in &lines[index + 1..close] {
            if line.starts_with(' ') || line.starts_with('\t') {
                end = *line_end;
            } else {
                break;
            }
        }
        (*start, end)
    } else {
        (lines[close].0, lines[close].0)
    };

    let mut out = Vec::with_capacity(prefix.len() + replacement.len());
    out.extend_from_slice(&prefix[..start]);
    out.extend_from_slice(replacement.as_bytes());
    out.extend_from_slice(&prefix[end..]);
    Ok(out)
}

/// Streaming state-line scanner. It retains one line rather than the complete, growing note.
fn state_line_offset_in_file(path: &Path) -> Result<Option<u64>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {} to find its state line", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut offset = 0u64;
    loop {
        line.clear();
        let bytes = reader
            .read_line(&mut line)
            .with_context(|| format!("reading {} to find its state line", path.display()))?;
        if bytes == 0 {
            return Ok(None);
        }
        let text = line.trim_end_matches('\n').trim_end_matches('\r');
        if text == STATE_TRANSCRIBING || text == STATE_TRANSCRIBED {
            return Ok(Some(offset));
        }
        offset = offset.saturating_add(bytes as u64);
    }
}

/// Byte offset immediately after a leading YAML frontmatter block, without reading the body. Zero when the
/// first line is not `---` or the block is unterminated.
fn frontmatter_end_in_file(path: &Path) -> Result<u64> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {} to find its frontmatter", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut offset = 0u64;
    if reader.read_line(&mut line)? == 0 || line.trim_end_matches(['\r', '\n']) != "---" {
        return Ok(0);
    }
    offset += line.len() as u64;
    loop {
        line.clear();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            return Ok(0);
        }
        offset += bytes as u64;
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return Ok(offset);
        }
    }
}

/// Byte offset of the first line that is exactly a state line (either form). `None` if absent.
#[cfg(test)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provenance::GenerationMode;
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
    fn state_operations_do_not_read_the_growing_transcript_tail() {
        let p = test_note("bounded-state-scan", &live_note_content());
        // Invalid UTF-8 after the state line proves flip/rewrite stop at the bounded prefix; the old
        // read_to_string implementation failed here and allocated in proportion to the whole note.
        let mut append = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        append.write_all(&[0xff; 4096]).unwrap();
        flip_state(&p).unwrap();
        let bytes = std::fs::read(&p).unwrap();
        assert!(
            bytes
                .windows(STATE_TRANSCRIBED.len())
                .any(|window| window == STATE_TRANSCRIBED.as_bytes())
        );

        rewrite_body(&p, "new bounded body\n").unwrap();
        assert!(
            std::fs::read_to_string(&p)
                .unwrap()
                .ends_with("new bounded body\n")
        );
    }

    #[test]
    fn flip_without_a_state_line_errors() {
        let p = test_note("flip-missing", "---\na: b\n---\n\n# T\n\nno state here\n");
        let err = flip_state(&p).unwrap_err().to_string();
        assert!(err.contains("no state line"), "got: {err}");
    }

    #[test]
    fn current_note_handle_rewrites_only_the_owned_note_and_redacts_its_path() {
        let p = test_note("current-handle", &live_note_content());
        let ino = std::fs::metadata(&p).unwrap().ino();
        let current = CurrentNote::from_returned_path(p.clone()).unwrap();
        let provenance =
            crate::provenance::TranscriptProvenance::legacy_unknown(GenerationMode::Batch);

        current
            .rewrite_transcript("State: transcribed \n\nsynthetic final body\n", &provenance)
            .unwrap();

        let got = std::fs::read_to_string(&p).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().ino(), ino);
        assert!(got.ends_with("State: transcribed \n\nsynthetic final body\n"));
        let debug = format!("{current:?}");
        assert!(!debug.contains(p.to_string_lossy().as_ref()));
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
    fn fallback_rewrite_upserts_batch_provenance_without_reading_the_tail() {
        let live = crate::provenance::TranscriptProvenance::legacy_unknown(GenerationMode::Live);
        let mut content = live_note_content();
        content = content.replacen(
            "---\n\n#",
            &format!("{}---\n\n#", live.frontmatter_line().unwrap()),
            1,
        );
        let p = test_note("rewrite-provenance", &content);
        let mut append = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        append.write_all(&[0xff; 4096]).unwrap();
        let ino = std::fs::metadata(&p).unwrap().ino();
        let batch = crate::provenance::TranscriptProvenance::legacy_unknown(GenerationMode::Batch);

        rewrite_body_with_provenance(&p, "canonical batch body\n", &batch).unwrap();

        let got = std::fs::read_to_string(&p).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().ino(), ino);
        assert_eq!(
            got.matches("\ncorti: ").count(),
            1,
            "old value was replaced"
        );
        assert!(got.contains(r#""mode":"batch""#), "got: {got}");
        assert!(!got.contains(r#""mode":"live""#), "got: {got}");
        assert!(got.ends_with("canonical batch body\n"));
    }

    #[test]
    fn fallback_rewrite_inserts_provenance_when_an_older_vagus_omitted_it() {
        let p = test_note("insert-provenance", &live_note_content());
        let batch = crate::provenance::TranscriptProvenance::legacy_unknown(GenerationMode::Batch);
        rewrite_body_with_provenance(&p, "canonical batch body\n", &batch).unwrap();
        let got = std::fs::read_to_string(&p).unwrap();
        assert!(got.contains("\ncorti: {"), "got: {got}");
        assert!(got.find("corti:").unwrap() < got.find("\n---\n\n#").unwrap());
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
