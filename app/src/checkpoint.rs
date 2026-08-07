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
use corti_vagus::provenance::{GenerationMode, TranscriptProvenance};
use serde::{Deserialize, Serialize};

const VERSION: u32 = 1;
const AWS_STAGING_VERSION: u32 = 1;

fn legacy_batch_provenance() -> TranscriptProvenance {
    TranscriptProvenance::legacy_unknown(GenerationMode::Batch)
}

/// A note path whose ownership must survive a retry. Partial live notes may be rewritten/replaced; canonical
/// notes were already returned by vagus (or finalized live) and need completion only, even after they move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OwnedNote {
    pub(crate) path: PathBuf,
    pub(crate) canonical: bool,
}

impl OwnedNote {
    pub(crate) fn partial(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            canonical: false,
        }
    }

    pub(crate) fn canonical(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            canonical: true,
        }
    }
}

/// The exact AWS staging location that still needs privacy cleanup. Before ASR completes this is stored in
/// a separate marker; after ASR it is also carried by the transcript checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AwsStaging {
    pub(crate) bucket: String,
    pub(crate) key_prefix: String,
    pub(crate) job_name: String,
    pub(crate) region: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct AwsStagingMarker {
    version: u32,
    staging: AwsStaging,
}

impl AwsStaging {
    /// Publish cloud ownership before upload/start can create remote artifacts.
    pub(crate) fn store(&self, audio: &Path) -> Result<()> {
        atomic_store_json(
            &aws_staging_path_for(audio),
            &AwsStagingMarker {
                version: AWS_STAGING_VERSION,
                staging: self.clone(),
            },
            "AWS staging marker",
        )
    }

    pub(crate) fn load(audio: &Path) -> Result<Self> {
        let path = aws_staging_path_for(audio);
        let marker: AwsStagingMarker = load_json(&path, "AWS staging marker")?;
        if marker.version != AWS_STAGING_VERSION {
            bail!(
                "unsupported AWS staging marker version {} in {} (expected {})",
                marker.version,
                path.display(),
                AWS_STAGING_VERSION
            );
        }
        Ok(marker.staging)
    }

    pub(crate) fn remove(audio: &Path) -> Result<()> {
        match std::fs::remove_file(aws_staging_path_for(audio)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).context("removing AWS staging marker"),
        }
    }
}

/// The durable boundary between transcription and filing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct FilingCheckpoint {
    version: u32,
    pub(crate) transcript: DiarizedTranscript,
    /// Existing partial live note, or the path returned by `vagus add-note` before queue completion.
    pub(crate) note_path: Option<PathBuf>,
    /// `true` once the note body is canonical. Completion retries must trust this path even if vagus has
    /// reorganized it out of the inbox; absence is not permission to invoke `add-note` again.
    #[serde(default)]
    pub(crate) note_canonical: bool,
    /// Generation identity captured from the backend's exact config snapshot. Additive/defaulted so a
    /// pre-provenance v1 checkpoint remains readable without being mislabeled from current Settings.
    #[serde(default = "legacy_batch_provenance")]
    pub(crate) provenance: TranscriptProvenance,
    /// Present only for an AWS transcript whose staged input/output have not yet been deleted.
    pub(crate) aws_staging: Option<AwsStaging>,
}

impl FilingCheckpoint {
    pub(crate) fn new(
        transcript: DiarizedTranscript,
        note_path: Option<PathBuf>,
        aws_staging: Option<AwsStaging>,
    ) -> Self {
        Self {
            version: VERSION,
            transcript,
            note_path,
            note_canonical: false,
            provenance: legacy_batch_provenance(),
            aws_staging,
        }
    }

    pub(crate) fn owned_note(&self) -> Option<OwnedNote> {
        self.note_path.clone().map(|path| OwnedNote {
            path,
            canonical: self.note_canonical,
        })
    }

    pub(crate) fn set_canonical_note(&mut self, path: PathBuf) {
        self.note_path = Some(path);
        self.note_canonical = true;
    }

    pub(crate) fn set_provenance(&mut self, provenance: TranscriptProvenance) {
        self.provenance = provenance;
    }

    /// Atomically replace the checkpoint beside `audio` and fsync the file before publishing it.
    pub(crate) fn store(&self, audio: &Path) -> Result<()> {
        atomic_store_json(&path_for(audio), self, "filing checkpoint")
    }

    pub(crate) fn load(audio: &Path) -> Result<Self> {
        let path = path_for(audio);
        let checkpoint: Self = load_json(&path, "filing checkpoint")?;
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
    sibling_path(audio, "transcript")
}

/// `<recording-stem>.aws-staging.json`, published before a durable cloud attempt starts.
pub(crate) fn aws_staging_path_for(audio: &Path) -> PathBuf {
    sibling_path(audio, "aws-staging")
}

fn sibling_path(audio: &Path, kind: &str) -> PathBuf {
    let stem = audio
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "recording".to_string());
    audio.with_file_name(format!("{stem}.{kind}.json"))
}

/// Conservative retention gate: an unreadable canonical checkpoint might be the last copy of an AWS owner,
/// so preserve it just like an explicit pre-ASR marker rather than trading a small local leak for cloud PHI.
pub(crate) fn has_unresolved_aws_staging(audio: &Path) -> bool {
    if aws_staging_path_for(audio).is_file() {
        return true;
    }
    match FilingCheckpoint::load(audio) {
        Ok(checkpoint) => checkpoint.aws_staging.is_some(),
        Err(_) => path_for(audio).is_file(),
    }
}

fn atomic_store_json<T: Serialize + ?Sized>(path: &Path, value: &T, label: &str) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating {label} directory {}", parent.display()))?;

    let bytes = serde_json::to_vec(value).with_context(|| format!("serializing {label}"))?;
    let tmp = temp_path(path);
    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .with_context(|| format!("creating temporary {label} {}", tmp.display()))?;
        file.write_all(&bytes)
            .with_context(|| format!("writing temporary {label} {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing temporary {label} {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| {
            format!("publishing {label} {} -> {}", tmp.display(), path.display())
        })?;
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

fn load_json<T: for<'de> Deserialize<'de>>(path: &Path, label: &str) -> Result<T> {
    let mut bytes = Vec::new();
    File::open(path)
        .with_context(|| format!("opening {label} {}", path.display()))?
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {label} {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {label} {}", path.display()))
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
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let prefixes: Vec<String> = [checkpoint, aws_staging_path_for(audio)]
        .into_iter()
        .filter_map(|path| {
            path.file_name()
                .map(|name| format!("{}.tmp-", name.to_string_lossy()))
        })
        .collect();

    let entries = match std::fs::read_dir(&parent) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if prefixes
            .iter()
            .any(|prefix| name.to_string_lossy().starts_with(prefix))
        {
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
            Some(AwsStaging {
                bucket: "old-bucket".into(),
                key_prefix: "corti/".into(),
                job_name: "recording".into(),
                region: Some("us-east-1".into()),
            }),
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
        let mut expected = checkpoint(Some(dir.join("note.md")));
        expected.set_canonical_note(dir.join("note.md"));
        let mut captured = TranscriptProvenance::legacy_unknown(GenerationMode::Batch);
        captured.version = "captured-before-settings-changed".into();
        captured.backend = "local".into();
        expected.set_provenance(captured);
        expected.store(&audio).unwrap();
        assert_eq!(FilingCheckpoint::load(&audio).unwrap(), expected);
        assert!(expected.owned_note().unwrap().canonical);

        // Once staged-object deletion succeeds, clearing that ownership must itself survive a filing retry.
        let mut cleaned = expected.clone();
        cleaned.aws_staging = None;
        cleaned.store(&audio).unwrap();
        assert_eq!(FilingCheckpoint::load(&audio).unwrap(), cleaned);
        assert!(!temp_path(&path_for(&audio)).exists());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn pre_asr_aws_staging_marker_round_trips_and_gates_retention() {
        let dir = dir("aws-staging");
        let audio = dir.join("recording.wav");
        let staging = checkpoint(None).aws_staging.unwrap();

        staging.store(&audio).unwrap();
        assert_eq!(AwsStaging::load(&audio).unwrap(), staging);
        assert!(has_unresolved_aws_staging(&audio));
        AwsStaging::remove(&audio).unwrap();
        assert!(!aws_staging_path_for(&audio).exists());
        assert!(!has_unresolved_aws_staging(&audio));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn pre_provenance_checkpoint_loads_as_explicit_unknown() {
        let dir = dir("legacy-provenance");
        let audio = dir.join("recording.wav");
        std::fs::write(
            path_for(&audio),
            r#"{"version":1,"transcript":{"segments":[]},"note_path":null,"aws_staging":null}"#,
        )
        .unwrap();
        let loaded = FilingCheckpoint::load(&audio).unwrap();
        assert_eq!(loaded.provenance.version, "unknown");
        assert_eq!(loaded.provenance.backend, "unknown");
        assert_eq!(loaded.provenance.mode, GenerationMode::Batch);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn rejects_unknown_checkpoint_versions() {
        let dir = dir("version");
        let audio = dir.join("recording.wav");
        let path = path_for(&audio);
        std::fs::write(
            &path,
            r#"{"version":999,"transcript":{"segments":[]},"note_path":null,"aws_staging":null}"#,
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
        let stale_aws = PathBuf::from(format!("{}.tmp-56", aws_staging_path_for(&audio).display()));
        std::fs::write(&stale_b, b"b").unwrap();
        std::fs::write(&stale_a, b"a").unwrap();
        std::fs::write(&stale_aws, b"aws").unwrap();
        std::fs::write(dir.join("other.transcript.json.tmp-12"), b"other").unwrap();

        let mut expected = vec![stale_a, stale_b, stale_aws];
        expected.sort();
        assert_eq!(temporary_paths(&audio).unwrap(), expected);
        std::fs::remove_dir_all(dir).ok();
    }
}
