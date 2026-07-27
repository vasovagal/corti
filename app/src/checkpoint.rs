//! Durable post-ASR filing checkpoint.
//!
//! Once this file exists, the expensive/backend-specific transcription is complete and the only remaining
//! work is filing the transcript. It lives beside the raw recording (outside any vault), is written
//! atomically, and is removed only after the queue durably reaches `Done`.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use corti_core::DiarizedTranscript;
use serde::{Deserialize, Serialize};

const VERSION: u32 = 1;

/// The durable boundary between transcription and filing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct FilingCheckpoint {
    version: u32,
    pub(crate) transcript: DiarizedTranscript,
    /// Existing partial live note, or the path returned by `vagus add-note` before queue completion.
    pub(crate) note_path: Option<PathBuf>,
}

impl FilingCheckpoint {
    pub(crate) fn new(transcript: DiarizedTranscript, note_path: Option<PathBuf>) -> Self {
        Self {
            version: VERSION,
            transcript,
            note_path,
        }
    }

    /// Atomically replace the checkpoint beside `audio` and fsync the file before publishing it.
    pub(crate) fn store(&self, audio: &Path) -> Result<()> {
        let path = path_for(audio);
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating checkpoint directory {}", parent.display()))?;

        let bytes = serde_json::to_vec(self).context("serializing filing checkpoint")?;
        let tmp = temp_path(&path);
        let write_result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)
                .with_context(|| format!("creating temporary checkpoint {}", tmp.display()))?;
            file.write_all(&bytes)
                .with_context(|| format!("writing temporary checkpoint {}", tmp.display()))?;
            file.sync_all()
                .with_context(|| format!("syncing temporary checkpoint {}", tmp.display()))?;
            std::fs::rename(&tmp, &path).with_context(|| {
                format!(
                    "publishing filing checkpoint {} -> {}",
                    tmp.display(),
                    path.display()
                )
            })?;
            // Best-effort directory sync closes the rename durability window on filesystems that support it.
            if let Ok(dir) = File::open(parent) {
                let _ = dir.sync_all();
            }
            Ok(())
        })();
        if write_result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        write_result
    }

    pub(crate) fn load(audio: &Path) -> Result<Self> {
        let path = path_for(audio);
        let mut bytes = Vec::new();
        File::open(&path)
            .with_context(|| format!("opening filing checkpoint {}", path.display()))?
            .read_to_end(&mut bytes)
            .with_context(|| format!("reading filing checkpoint {}", path.display()))?;
        let checkpoint: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing filing checkpoint {}", path.display()))?;
        if checkpoint.version != VERSION {
            bail!(
                "unsupported filing checkpoint version {} in {} (expected {})",
                checkpoint.version,
                path.display(),
                VERSION
            );
        }
        Ok(checkpoint)
    }
}

/// `<recording-stem>.transcript.json`, beside the raw recording.
pub(crate) fn path_for(audio: &Path) -> PathBuf {
    let stem = audio
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "recording".to_string());
    audio.with_file_name(format!("{stem}.transcript.json"))
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name: OsString = path.as_os_str().to_os_string();
    name.push(format!(".tmp-{}", std::process::id()));
    PathBuf::from(name)
}

/// Temporary checkpoint files left by a process death before the atomic rename.
///
/// These are never recovery inputs: only the canonical path returned by [`path_for`] is published. The
/// retention sweep discovers every PID-suffixed sibling so plaintext transcript debris cannot outlive its
/// recording row.
pub(crate) fn temporary_paths(audio: &Path) -> std::io::Result<Vec<PathBuf>> {
    let checkpoint = path_for(audio);
    let parent = checkpoint
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let Some(file_name) = checkpoint.file_name() else {
        return Ok(Vec::new());
    };
    let prefix = format!("{}.tmp-", file_name.to_string_lossy());

    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use corti_core::{Speaker, TranscriptSegment};

    fn dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("corti-checkpoint-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn checkpoint(note: Option<PathBuf>) -> FilingCheckpoint {
        FilingCheckpoint::new(
            DiarizedTranscript::new(vec![TranscriptSegment {
                speaker: Speaker::Me,
                start: 1.0,
                end: 2.0,
                text: "durable words".into(),
            }]),
            note,
        )
    }

    #[test]
    fn path_is_a_sibling_of_the_raw_recording() {
        assert_eq!(
            path_for(Path::new("/cache/20260708-120000-zoom.wav")),
            PathBuf::from("/cache/20260708-120000-zoom.transcript.json")
        );
    }

    #[test]
    fn atomically_round_trips_transcript_and_note_path() {
        let dir = dir("roundtrip");
        let audio = dir.join("recording.wav");
        let expected = checkpoint(Some(dir.join("note.md")));
        expected.store(&audio).unwrap();
        assert_eq!(FilingCheckpoint::load(&audio).unwrap(), expected);
        assert!(!temp_path(&path_for(&audio)).exists());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn rejects_unknown_checkpoint_versions() {
        let dir = dir("version");
        let audio = dir.join("recording.wav");
        let path = path_for(&audio);
        std::fs::write(
            &path,
            r#"{"version":999,"transcript":{"segments":[]},"note_path":null}"#,
        )
        .unwrap();
        let err = FilingCheckpoint::load(&audio).unwrap_err().to_string();
        assert!(
            err.contains("unsupported filing checkpoint version 999"),
            "{err}"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn discovers_all_pid_suffixed_temporary_checkpoints() {
        let dir = dir("temporary-paths");
        let audio = dir.join("recording.wav");
        let checkpoint = path_for(&audio);
        let stale_a = PathBuf::from(format!("{}.tmp-12", checkpoint.display()));
        let stale_b = PathBuf::from(format!("{}.tmp-34", checkpoint.display()));
        std::fs::write(&stale_b, b"b").unwrap();
        std::fs::write(&stale_a, b"a").unwrap();
        std::fs::write(dir.join("other.transcript.json.tmp-12"), b"other").unwrap();

        assert_eq!(temporary_paths(&audio).unwrap(), vec![stale_a, stale_b]);
        std::fs::remove_dir_all(dir).ok();
    }
}
