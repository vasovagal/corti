//! In-place note writes for live inbox filing (ADR 0010).
//!
//! ADR 0001 confines corti to `vagus add-note`; ADR 0010 amends that with exactly four operations,
//! all against a path `--print-path` returned: **append** body content (live segments, or a failure
//! annotation), **flip** the state line in place when the transcript is final, **rewrite** the body
//! when the batch path supersedes a partial live note, and **delete** the note when its recording is
//! discarded (a plain `remove_file` at the call site — the first three live here).
//! The capture-quality amendment also permits refreshing only Corti's provenance on that same owned
//! note, preserving its existing body and inode.
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

const MAX_PREFIX_BYTES: usize = 64 * 1024;
const BODY_MOVE_BYTES: usize = 64 * 1024;

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

    /// Refresh only Corti's frontmatter on this owned note, keeping its title, state and arbitrary body
    /// bytes intact. Prefix validation is bounded to 64 KiB and finishes before mutation. If the prefix
    /// changes length, the body is moved internally with one 64 KiB buffer; no text is returned or logged.
    ///
    /// This shares the existing same-inode rewrite's durability window: a write/sync failure or crash
    /// during the move can leave an incomplete rewrite. It is not an atomic replacement. The caller must
    /// serialize this operation with appends/state flips for this recording.
    pub fn refresh_provenance(&self, provenance: &TranscriptProvenance) -> Result<()> {
        refresh_provenance_in_place(&self.path, provenance)
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
    ensure!(
        keep <= MAX_PREFIX_BYTES as u64,
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
            prefix.len() <= MAX_PREFIX_BYTES,
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

/// Read through the closing frontmatter delimiter only. `Take` bounds even a malformed, newline-free
/// prefix; unlike a state-line search, this can never walk the recording's growing transcript body.
fn bounded_frontmatter_prefix(file: &mut std::fs::File) -> Result<Vec<u8>> {
    let mut reader = BufReader::new(file.take(MAX_PREFIX_BYTES as u64));
    let mut prefix = Vec::with_capacity(MAX_PREFIX_BYTES);
    let mut first = true;
    loop {
        let start = prefix.len();
        ensure!(
            reader.read_until(b'\n', &mut prefix)? != 0,
            "current-note frontmatter is missing, unterminated or exceeds the prefix limit"
        );
        let line = &prefix[start..];
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if first {
            ensure!(line == b"---", "current-note frontmatter is missing");
            first = false;
        } else if line == b"---" {
            return Ok(prefix);
        }
    }
}

fn refresh_provenance_in_place(path: &Path, provenance: &TranscriptProvenance) -> Result<()> {
    refresh_provenance_with_io(path, provenance, &mut RefreshIo::default())
}

#[derive(Default)]
struct RefreshIo {
    #[cfg(test)]
    writes_before_failure: Option<usize>,
}

impl RefreshIo {
    fn before_write(&mut self) -> std::io::Result<()> {
        #[cfg(test)]
        if let Some(remaining) = &mut self.writes_before_failure {
            if *remaining == 0 {
                return Err(std::io::Error::other("injected current-note write failure"));
            }
            *remaining -= 1;
        }
        Ok(())
    }
}

fn refresh_provenance_with_io(
    path: &Path,
    provenance: &TranscriptProvenance,
    io: &mut RefreshIo,
) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .context("opening current note for provenance refresh")?;
    let old_len = file
        .metadata()
        .context("reading current-note length")?
        .len();
    let prefix = bounded_frontmatter_prefix(&mut file)
        .context("validating current-note prefix before provenance refresh")?;
    let replacement = upsert_provenance(&prefix, provenance)
        .map_err(|_| anyhow::anyhow!("invalid_current_note_provenance_prefix"))?;
    ensure!(
        replacement.len() <= MAX_PREFIX_BYTES,
        "current-note provenance exceeds the prefix limit"
    );
    let old_prefix = u64::try_from(prefix.len()).context("current-note prefix length overflow")?;
    let new_prefix =
        u64::try_from(replacement.len()).context("current-note prefix length overflow")?;
    let body_len = old_len
        .checked_sub(old_prefix)
        .context("current-note prefix exceeds its file length")?;
    let new_len = new_prefix
        .checked_add(body_len)
        .context("current-note length overflow")?;
    if replacement == prefix {
        return Ok(());
    }

    // Every validation and final-length calculation above completes before the first write. Moving
    // backwards on growth and forwards on shrinkage prevents an overlapping write from consuming bytes
    // that a later chunk still needs. Only this fixed buffer ever contains body bytes.
    let mut buffer = [0_u8; BODY_MOVE_BYTES];
    if new_prefix > old_prefix {
        let mut remaining = body_len;
        while remaining != 0 {
            let chunk_len = remaining.min(BODY_MOVE_BYTES as u64);
            let offset = remaining - chunk_len;
            move_body_chunk(
                &mut file,
                &mut buffer[..chunk_len as usize],
                old_prefix,
                new_prefix,
                offset,
                io,
            )?;
            remaining = offset;
        }
    } else if new_prefix < old_prefix {
        let mut copied = 0_u64;
        while copied < body_len {
            let chunk_len = (body_len - copied).min(BODY_MOVE_BYTES as u64);
            move_body_chunk(
                &mut file,
                &mut buffer[..chunk_len as usize],
                old_prefix,
                new_prefix,
                copied,
                io,
            )?;
            copied = copied
                .checked_add(chunk_len)
                .context("current-note body offset overflow")?;
        }
    }
    file.seek(SeekFrom::Start(0))?;
    io.before_write()?;
    file.write_all(&replacement)
        .context("writing current-note provenance prefix")?;
    file.set_len(new_len)
        .context("setting refreshed current-note length")?;
    file.sync_all()
        .context("syncing current-note provenance refresh")
}

fn move_body_chunk(
    file: &mut std::fs::File,
    buffer: &mut [u8],
    old_prefix: u64,
    new_prefix: u64,
    offset: u64,
    io: &mut RefreshIo,
) -> Result<()> {
    let source = old_prefix
        .checked_add(offset)
        .context("current-note source offset overflow")?;
    let target = new_prefix
        .checked_add(offset)
        .context("current-note target offset overflow")?;
    file.seek(SeekFrom::Start(source))?;
    file.read_exact(buffer)
        .context("reading current-note body during provenance refresh")?;
    file.seek(SeekFrom::Start(target))?;
    io.before_write()?;
    file.write_all(buffer)
        .context("moving current-note body during provenance refresh")
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
    // Preflight without allocating the serialized value. The subsequent string and rewritten prefix
    // are each at most 64 KiB, even if a caller supplies an oversized configuration object.
    serde_json::to_writer(&mut PrefixSizeLimit(8), provenance)
        .context("transcript provenance exceeds the prefix limit")?;
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

    let output_len = prefix
        .len()
        .checked_sub(end - start)
        .and_then(|retained| retained.checked_add(replacement.len()))
        .context("note prefix length overflow")?;
    ensure!(
        output_len <= MAX_PREFIX_BYTES,
        "transcript provenance exceeds the prefix limit"
    );
    let mut out = Vec::with_capacity(output_len);
    out.extend_from_slice(&prefix[..start]);
    out.extend_from_slice(replacement.as_bytes());
    out.extend_from_slice(&prefix[end..]);
    Ok(out)
}

struct PrefixSizeLimit(usize);

impl Write for PrefixSizeLimit {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .filter(|total| *total <= MAX_PREFIX_BYTES)
            .ok_or_else(|| std::io::Error::other("note prefix limit exceeded"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
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

    fn body_bytes(path: &Path) -> Vec<u8> {
        let bytes = std::fs::read(path).unwrap();
        let prefix = frontmatter_end_in_file(path).unwrap() as usize;
        bytes[prefix..].to_vec()
    }

    #[test]
    fn provenance_refresh_grows_shrinks_and_preserves_large_utf8_and_binary_body() {
        let path = test_note("refresh-moves", &live_note_content());
        let mut tail = "SYNTHETIC_BODY_SENTINEL — café 日本語\n"
            .repeat(8_000)
            .into_bytes();
        tail.extend_from_slice(&[0, 0xff, 0xfe, b'\n']);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&tail)
            .unwrap();
        let original_body = body_bytes(&path);
        assert!(original_body.len() > 3 * BODY_MOVE_BYTES);
        let original_len = std::fs::metadata(&path).unwrap().len();
        let inode = std::fs::metadata(&path).unwrap().ino();
        let current = CurrentNote::from_returned_path(path.clone()).unwrap();
        let mut large = TranscriptProvenance::legacy_unknown(GenerationMode::Live);
        large.configuration.insert(
            "fixture".into(),
            serde_json::json!("metadata".repeat(2_000)),
        );

        current.refresh_provenance(&large).unwrap();
        let grown = std::fs::read(&path).unwrap();
        assert!(grown.len() as u64 > original_len);
        assert_eq!(body_bytes(&path), original_body);
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        // An identical replacement does not move the body or change any byte.
        current.refresh_provenance(&large).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), grown);

        let small = TranscriptProvenance::legacy_unknown(GenerationMode::Batch);
        current.refresh_provenance(&small).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() < grown.len() as u64);
        assert_eq!(body_bytes(&path), original_body);
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        let mut file = std::fs::File::open(&path).unwrap();
        let prefix = bounded_frontmatter_prefix(&mut file).unwrap();
        let prefix = String::from_utf8(prefix).unwrap();
        assert!(prefix.contains("created: 2026-07-08T10:00"));
        assert!(prefix.contains("source: Zoom · 2026-07-08 10:00"));
        assert_eq!(prefix.matches("\ncorti:").count(), 1);
        assert!(prefix.contains(r#""mode":"batch""#));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn same_size_provenance_refresh_preserves_every_body_byte_and_inode() {
        let path = test_note("refresh-same-size", &live_note_content());
        let current = CurrentNote::from_returned_path(path.clone()).unwrap();
        let mut provenance = TranscriptProvenance::legacy_unknown(GenerationMode::Live);
        provenance
            .configuration
            .insert("fixture".into(), serde_json::json!("first"));
        current.refresh_provenance(&provenance).unwrap();
        let original = std::fs::read(&path).unwrap();
        let body = body_bytes(&path);
        let inode = std::fs::metadata(&path).unwrap().ino();
        provenance
            .configuration
            .insert("fixture".into(), serde_json::json!("other"));
        current.refresh_provenance(&provenance).unwrap();
        let updated = std::fs::read(&path).unwrap();
        assert_eq!(updated.len(), original.len());
        assert_ne!(updated, original);
        assert_eq!(body_bytes(&path), body);
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn refresh_rejects_oversized_or_malformed_prefixes_without_mutation_or_text_exposure() {
        let provenance = TranscriptProvenance::legacy_unknown(GenerationMode::Live);
        for (index, bytes) in [
            b"SYNTHETIC_PREFIX_SENTINEL\nno frontmatter\n".to_vec(),
            b"---\nSYNTHETIC_PREFIX_SENTINEL\nunterminated\n".to_vec(),
            b"---\ninvalid: \xff\n---\nSYNTHETIC_BODY_SENTINEL".to_vec(),
            format!(
                "---\nSYNTHETIC_PREFIX_SENTINEL:{}\n---\nbody",
                "x".repeat(MAX_PREFIX_BYTES)
            )
            .into_bytes(),
            format!(
                "---\nnear_limit: {}\n---\nbody",
                "x".repeat(MAX_PREFIX_BYTES - 32)
            )
            .into_bytes(),
        ]
        .into_iter()
        .enumerate()
        {
            let path = test_note(&format!("refresh-invalid-{index}"), "");
            std::fs::write(&path, &bytes).unwrap();
            let inode = std::fs::metadata(&path).unwrap().ino();
            let current = CurrentNote::from_returned_path(path.clone()).unwrap();
            let error = current.refresh_provenance(&provenance).unwrap_err();
            // No content-bearing logging API is called by refresh. These are the only diagnostics its
            // caller can log, and neither reveals the private prefix/body sentinel.
            for diagnostic in [format!("{error:#}"), format!("{current:?}")] {
                assert!(!diagnostic.contains("SYNTHETIC_PREFIX_SENTINEL"));
                assert!(!diagnostic.contains("SYNTHETIC_BODY_SENTINEL"));
            }
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
            std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
        let path = test_note("refresh-oversized-provenance", &live_note_content());
        let original = std::fs::read(&path).unwrap();
        let mut oversized = provenance;
        oversized.configuration.insert(
            "fixture".into(),
            serde_json::json!("SYNTHETIC_METADATA_SENTINEL".repeat(MAX_PREFIX_BYTES)),
        );
        let error = CurrentNote::from_returned_path(path.clone())
            .unwrap()
            .refresh_provenance(&oversized)
            .unwrap_err();
        assert!(!format!("{error:#}").contains("SYNTHETIC_METADATA_SENTINEL"));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn interrupted_provenance_move_reports_io_failure_and_can_be_rewritten_from_authority() {
        let path = test_note("refresh-io-failure", &live_note_content());
        append(&path, &"SYNTHETIC_BODY_SENTINEL\n".repeat(12_000)).unwrap();
        let inode = std::fs::metadata(&path).unwrap().ino();
        let authority = path.with_file_name("recording-quality.json");
        std::fs::write(&authority, b"synthetic durable quality authority").unwrap();
        let mut provenance = TranscriptProvenance::legacy_unknown(GenerationMode::Batch);
        provenance.configuration.insert(
            "fixture".into(),
            serde_json::json!("metadata".repeat(2_000)),
        );
        let mut io = RefreshIo {
            writes_before_failure: Some(1),
        };
        let error = refresh_provenance_with_io(&path, &provenance, &mut io).unwrap_err();
        assert!(format!("{error:#}").contains("injected current-note write failure"));
        assert!(!format!("{error:#}").contains("SYNTHETIC_BODY_SENTINEL"));
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(
            std::fs::read(&authority).unwrap(),
            b"synthetic durable quality authority"
        );
        // A partial move is intentionally not claimed atomic. The existing canonical fallback rewrite
        // repairs this owned note using its separately durable transcript/quality authority.
        let recovered = "State: transcribed \n\nSynthetic recovered transcript.\n";
        CurrentNote::from_returned_path(path.clone())
            .unwrap()
            .rewrite_transcript(recovered, &provenance)
            .unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().ends_with(recovered));
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn maximum_width_quality_requires_a_bounded_same_record_projection() {
        let epochs = (0..64_u64)
            .map(|index| {
                serde_json::json!({
                    "id": index,
                    "source_start_ns": u64::MAX - 64_000_000_000 + index * 1_000_000_000,
                    "source_rate_hz": u32::MAX, "source_frame_base": u64::MAX - 1_000,
                    "retained_start_frame": u64::MAX - 1_000 + index * 10,
                    "retained_rate_hz": u32::MAX, "mic_channels": 1, "tap_channels": 1,
                    "reason": if index == 0 { "start" } else { "host_discontinuity" }
                })
            })
            .collect::<Vec<_>>();
        let spans =
            (0..64_u64)
                .flat_map(|epoch| {
                    (0..4_u64).map(move |index| serde_json::json!({
            "epoch": epoch, "start_frame": u64::MAX - 1_000 + index * 2,
            "end_frame": u64::MAX - 999 + index * 2, "track":"both",
            "cause":"mic_tap_mismatch", "class":"processing_loss", "stage":"capture",
            "resolution":"recovered_by_batch"
        }))
                })
                .collect::<Vec<_>>();
        let full: corti_core::QualityRecord = serde_json::from_value(serde_json::json!({
            "schema_version":1, "coverage":"observed", "epochs":epochs, "spans":spans,
            "overflow": corti_core::QualityOverflow::default()
        }))
        .unwrap();
        full.validate().unwrap();
        let full_bytes = serde_json::to_vec(&full).unwrap().len();
        assert!(full_bytes > MAX_PREFIX_BYTES);
        let path = test_note("maximum-quality-prefix", &live_note_content());
        let current = CurrentNote::from_returned_path(path.clone()).unwrap();
        let original = std::fs::read(&path).unwrap();
        let body = body_bytes(&path);
        let mut provenance = TranscriptProvenance::legacy_unknown(GenerationMode::Live);
        provenance
            .configuration
            .insert("quality".into(), serde_json::to_value(&full).unwrap());
        assert!(current.refresh_provenance(&provenance).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);

        // Mirror the app's explicitly labelled display budget, without changing the complete authority.
        let mut projection = full.clone();
        while serde_json::to_vec(&projection).unwrap().len() > 48 * 1024 {
            let span = projection.spans.pop().unwrap();
            projection
                .record_omitted_spans(span.class, span.resolution, 1)
                .unwrap();
        }
        let projected_bytes = serde_json::to_vec(&projection).unwrap().len();
        assert_eq!(projection.epochs, full.epochs);
        assert_eq!(projection.overflow.outstanding_omitted_spans[1], 0);
        provenance
            .configuration
            .insert("quality".into(), serde_json::to_value(&projection).unwrap());
        provenance.configuration.insert(
            "quality_projection_truncated".into(),
            serde_json::json!(true),
        );
        provenance
            .configuration
            .insert("quality_full_span_count".into(), serde_json::json!(256));
        current.refresh_provenance(&provenance).unwrap();
        assert_eq!(body_bytes(&path), body);
        assert_eq!(full.spans.len(), 256);
        assert_eq!(full.epochs.len(), 64);
        let fresh = format!(
            "---\n{}---\nSynthetic fresh body.\n",
            provenance.frontmatter_line().unwrap()
        );
        let fresh_path = test_note("maximum-quality-fresh", &fresh);
        CurrentNote::from_returned_path(fresh_path.clone())
            .unwrap()
            .refresh_provenance(&provenance)
            .unwrap();
        assert_eq!(std::fs::read_to_string(&fresh_path).unwrap(), fresh);
        println!(
            "quality prefix fixture: full_bytes={full_bytes} projected_bytes={projected_bytes} full_spans=256 epochs=64"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        std::fs::remove_dir_all(fresh_path.parent().unwrap()).unwrap();
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
