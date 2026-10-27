//! Mandatory capture-quality storage, independent of optional text evidence.
//!
//! This is an off-HAL, single-writer API. Call [`QualityStore::begin`] before creating the WAV,
//! persist mappings before admitting their dependent PCM, and prepare the exact final snapshot
//! before finalizing the WAV. Only an explicit successful completion call promotes storage state;
//! dropping either this store or a WAV writer cannot do so. Storage durability never recovers loss.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::QualityRecord;

pub const QUALITY_STORE_VERSION: u32 = 1;
pub const MAX_QUALITY_SIDECAR_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityStorageState {
    /// Processing history for a recording without capture authority. This state never promotes.
    LegacyUnknown,
    InProgress,
    Finalizing,
    AudioDurable,
}

/// A storage envelope around the shared quality record, not another quality model.
/// Fields are read-only so callers cannot manufacture a store phase transition.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualitySnapshot {
    storage_version: u32,
    recording_id: String,
    audio_file_name: String,
    state: QualityStorageState,
    #[serde(deserialize_with = "deserialize_retained_frames")]
    expected_retained_frames: Option<u64>,
    quality: QualityRecord,
    /// Processing history retained after transient jobs/checkpoints are removed. Capture facts and
    /// storage certification above remain immutable after the capture writer finishes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    settled_quality: Option<QualityRecord>,
}

// Require the field even for legacy envelopes, where its explicit null means unknown, not zero.
fn deserialize_retained_frames<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    Option::<u64>::deserialize(deserializer)
}

// Do not expose recording identifiers, file names, or serialized payloads through diagnostic output.
impl fmt::Debug for QualitySnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QualitySnapshot")
            .field("storage_version", &self.storage_version)
            .field("state", &self.state)
            .field("expected_retained_frames", &self.expected_retained_frames)
            .finish_non_exhaustive()
    }
}

impl QualitySnapshot {
    pub fn state(&self) -> QualityStorageState {
        self.state
    }

    pub fn expected_retained_frames(&self) -> Option<u64> {
        self.expected_retained_frames
    }

    pub fn quality(&self) -> &QualityRecord {
        &self.quality
    }

    pub fn settled_quality(&self) -> Option<&QualityRecord> {
        self.settled_quality.as_ref()
    }
}

/// Completion eligibility of a read. Partial states retain the exact known capture history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityReadStatus {
    /// Absent older sidecar or history-only envelope: never a clean-capture certificate. A present
    /// snapshot retains its settled record for compare-and-swap publication on later attempts.
    LegacyUnknown,
    InProgress,
    Finalizing,
    AudioUnverified,
    FrameCountMismatch,
    InterruptedPublication,
    AudioDurable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualityReadout {
    pub status: QualityReadStatus,
    pub snapshot: Option<QualitySnapshot>,
}

impl QualityReadout {
    pub fn permits_completion(&self) -> bool {
        self.status == QualityReadStatus::AudioDurable
            && self
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.state == QualityStorageState::AudioDurable)
    }

    pub fn quality(&self) -> Option<&QualityRecord> {
        self.snapshot.as_ref().map(QualitySnapshot::quality)
    }
}

/// Content-free categories. Underlying I/O/JSON messages and recording paths are not retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityStoreError {
    InvalidPath,
    InvalidIdentity,
    AlreadyExists,
    InvalidQuality,
    HistoryRegression,
    InvalidTransition,
    FrameCountMismatch,
    BindingMismatch,
    UnsupportedVersion,
    TooLarge,
    Corrupt,
    Read,
    Write,
    FileSync,
    Rename,
    DirectorySync,
    Remove,
    InterruptedPublication,
    Rollback,
    AudioUnavailable,
}

impl fmt::Display for QualityStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidPath => "invalid quality storage path",
            Self::InvalidIdentity => "invalid quality recording identity",
            Self::AlreadyExists => "quality storage already exists",
            Self::InvalidQuality => "invalid quality snapshot",
            Self::HistoryRegression => "quality history regression",
            Self::InvalidTransition => "invalid quality storage transition",
            Self::FrameCountMismatch => "quality frame count mismatch",
            Self::BindingMismatch => "quality recording binding mismatch",
            Self::UnsupportedVersion => "unsupported quality storage version",
            Self::TooLarge => "quality storage size limit exceeded",
            Self::Corrupt => "corrupt quality storage",
            Self::Read => "quality storage read failed",
            Self::Write => "quality storage write failed",
            Self::FileSync => "quality storage file sync failed",
            Self::Rename => "quality storage rename failed",
            Self::DirectorySync => "quality storage directory sync failed",
            Self::Remove => "quality storage removal failed",
            Self::InterruptedPublication => "quality storage publication interrupted",
            Self::Rollback => "quality storage rollback incomplete",
            Self::AudioUnavailable => "quality storage audio unavailable",
        })
    }
}

impl std::error::Error for QualityStoreError {}

struct StorePaths {
    audio: PathBuf,
    sidecar: PathBuf,
    pending: PathBuf,
    previous: PathBuf,
    restore: PathBuf,
    parent: PathBuf,
    audio_file_name: String,
}

impl StorePaths {
    fn new(audio: &Path) -> Result<Self, QualityStoreError> {
        let audio_file_name = audio
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(QualityStoreError::InvalidPath)?;
        let stem = audio
            .file_stem()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .ok_or(QualityStoreError::InvalidPath)?;
        let parent = audio
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let sidecar_name = format!("{stem}-quality.json");
        Ok(Self {
            audio: audio.to_path_buf(),
            sidecar: parent.join(&sidecar_name),
            pending: parent.join(format!("{sidecar_name}.pending")),
            previous: parent.join(format!("{sidecar_name}.previous")),
            restore: parent.join(format!("{sidecar_name}.restore")),
            parent: parent.to_path_buf(),
            audio_file_name: audio_file_name.to_owned(),
        })
    }

    fn temporaries(&self) -> [&Path; 3] {
        [&self.pending, &self.previous, &self.restore]
    }

    fn has_temporary(&self) -> Result<bool, QualityStoreError> {
        for path in self.temporaries() {
            if present(path)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// The mandatory sidecar location. This API never consults optional evidence settings.
pub fn quality_sidecar_path(audio: &Path) -> Result<PathBuf, QualityStoreError> {
    Ok(StorePaths::new(audio)?.sidecar)
}

/// Exact registered transaction paths, also used by retention after a process restart.
pub fn quality_temporary_paths(audio: &Path) -> Result<[PathBuf; 3], QualityStoreError> {
    let paths = StorePaths::new(audio)?;
    Ok([paths.pending, paths.previous, paths.restore])
}

/// Remove this recording's authority and registered transaction files during recording deletion.
/// The caller must first stop its writer. Do not call this for evidence opt-out.
pub fn remove_quality_files(audio: &Path) -> Result<(), QualityStoreError> {
    let paths = StorePaths::new(audio)?;
    let mut failed = false;
    for path in [
        &paths.sidecar,
        &paths.pending,
        &paths.previous,
        &paths.restore,
    ] {
        if remove_if_present(path).is_err() {
            failed = true;
        }
    }
    sync_parent(&paths.parent)?;
    if failed {
        Err(QualityStoreError::Remove)
    } else {
        Ok(())
    }
}

/// A live store cannot be reopened or promoted from a recovery snapshot. Recovery reads preserve
/// incomplete storage state; they do not retroactively assert a successful capture handoff.
pub struct QualityStore {
    paths: StorePaths,
    durable: QualitySnapshot,
    failed: bool,
    faults: Faults,
}

impl QualityStore {
    /// Durably publish the initial zero-frame record before the caller creates the WAV.
    /// The opaque ID is the existing recording/job identity, not an app or human-readable title.
    pub fn begin(
        audio: &Path,
        recording_id: &str,
        initial: &QualityRecord,
    ) -> Result<Self, QualityStoreError> {
        Self::begin_with_faults(audio, recording_id, initial, Faults::default())
    }

    fn begin_with_faults(
        audio: &Path,
        recording_id: &str,
        initial: &QualityRecord,
        mut faults: Faults,
    ) -> Result<Self, QualityStoreError> {
        validate_identity(recording_id)?;
        initial
            .validate()
            .map_err(|_| QualityStoreError::InvalidQuality)?;
        let paths = StorePaths::new(audio)?;
        if present(&paths.audio)? || present(&paths.sidecar)? || paths.has_temporary()? {
            return Err(QualityStoreError::AlreadyExists);
        }
        let durable = QualitySnapshot {
            storage_version: QUALITY_STORE_VERSION,
            recording_id: recording_id.to_owned(),
            audio_file_name: paths.audio_file_name.clone(),
            state: QualityStorageState::InProgress,
            expected_retained_frames: Some(0),
            quality: initial.clone(),
            settled_quality: None,
        };
        publish(&paths, &durable, false, &mut faults)?;
        Ok(Self {
            paths,
            durable,
            failed: false,
            faults,
        })
    }

    pub fn snapshot(&self) -> &QualitySnapshot {
        &self.durable
    }

    /// Publish loss/epoch mappings before any dependent fill or later PCM is admitted. The count
    /// describes the prefix accounted for by this snapshot; it becomes exact at finalization.
    pub fn persist_before_audio(
        &mut self,
        quality: &QualityRecord,
        expected_retained_frames: u64,
    ) -> Result<(), QualityStoreError> {
        self.require_state(QualityStorageState::InProgress)?;
        self.publish_snapshot(
            quality,
            expected_retained_frames,
            QualityStorageState::InProgress,
        )
    }

    /// Call after terminal loss/filter-tail accounting and before invoking WAV finalization.
    pub fn prepare_finalization(
        &mut self,
        quality: &QualityRecord,
        exact_retained_frames: u64,
    ) -> Result<(), QualityStoreError> {
        self.require_state(QualityStorageState::InProgress)?;
        self.publish_snapshot(
            quality,
            exact_retained_frames,
            QualityStorageState::Finalizing,
        )
    }

    /// After explicit successful WAV finalization, pass its independently verified frame count.
    /// The caller owns WAV parsing/header validation. This repeats file and parent-directory sync
    /// itself, then publishes `audio_durable` before the caller may deliver completion.
    pub fn complete_after_audio_sync(
        &mut self,
        actual_retained_frames: u64,
    ) -> Result<(), QualityStoreError> {
        self.require_state(QualityStorageState::Finalizing)?;
        if Some(actual_retained_frames) != self.durable.expected_retained_frames {
            self.failed = true;
            return Err(QualityStoreError::FrameCountMismatch);
        }
        let synced = (|| {
            File::open(&self.paths.audio)
                .map_err(|_| QualityStoreError::AudioUnavailable)?
                .sync_all()
                .map_err(|_| QualityStoreError::FileSync)?;
            sync_parent(&self.paths.parent)
        })();
        if let Err(error) = synced {
            self.failed = true;
            return Err(error);
        }
        let quality = self.durable.quality.clone();
        self.publish_snapshot(
            &quality,
            actual_retained_frames,
            QualityStorageState::AudioDurable,
        )
    }

    fn require_state(&self, state: QualityStorageState) -> Result<(), QualityStoreError> {
        if self.failed || self.durable.state != state {
            Err(QualityStoreError::InvalidTransition)
        } else {
            Ok(())
        }
    }

    fn publish_snapshot(
        &mut self,
        quality: &QualityRecord,
        frames: u64,
        state: QualityStorageState,
    ) -> Result<(), QualityStoreError> {
        quality
            .validate()
            .map_err(|_| QualityStoreError::InvalidQuality)?;
        let previous_frames = self
            .durable
            .expected_retained_frames
            .ok_or(QualityStoreError::InvalidTransition)?;
        if frames < previous_frames {
            return Err(QualityStoreError::FrameCountMismatch);
        }
        preserves_history(quality, &self.durable.quality)?;
        let mut next = self.durable.clone();
        next.quality = quality.clone();
        next.expected_retained_frames = Some(frames);
        next.state = state;
        match publish(&self.paths, &next, true, &mut self.faults) {
            Ok(()) => {
                self.durable = next;
                Ok(())
            }
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }
}

fn preserves_history(
    next: &QualityRecord,
    previous: &QualityRecord,
) -> Result<(), QualityStoreError> {
    let mut merged = next.clone();
    merged
        .merge_snapshot(previous)
        .map_err(|_| QualityStoreError::HistoryRegression)?;
    if &merged != next {
        Err(QualityStoreError::HistoryRegression)
    } else {
        Ok(())
    }
}

/// Preserve all facts while allowing only processing resolution to change. This comparison does
/// not resolve the supplied record; callers must have independently established recovery.
fn preserves_settled_history(
    next: &QualityRecord,
    previous: &QualityRecord,
) -> Result<(), QualityStoreError> {
    let mut next = next.clone();
    let mut previous = previous.clone();
    for quality in [&mut next, &mut previous] {
        quality
            .resolve_processing_after_batch(
                crate::BatchRecoveryOutcome::SameBackendWholeFileSuccess,
            )
            .map_err(|_| QualityStoreError::InvalidQuality)?;
    }
    preserves_history(&next, &previous)
}

/// Retain settled processing history in the existing mandatory authority. If no envelope exists,
/// create a history-only legacy-unknown envelope without capture facts or a frame-count assertion.
/// Existing identity, capture record, storage phase and frame binding remain immutable.
/// Call before removing a recording's last transient quality carrier. Registered transaction paths,
/// owner-only publication, rollback and retention are the same as capture publication.
pub fn persist_settled_quality(
    audio: &Path,
    recording_id: &str,
    expected_settled: Option<&QualityRecord>,
    quality: &QualityRecord,
) -> Result<(), QualityStoreError> {
    persist_settled_with_faults(
        audio,
        recording_id,
        expected_settled,
        quality,
        &mut Faults::default(),
    )
}

fn persist_settled_with_faults(
    audio: &Path,
    recording_id: &str,
    expected_settled: Option<&QualityRecord>,
    quality: &QualityRecord,
    faults: &mut Faults,
) -> Result<(), QualityStoreError> {
    validate_identity(recording_id)?;
    let paths = StorePaths::new(audio)?;
    if paths.has_temporary()? {
        return Err(QualityStoreError::InterruptedPublication);
    }
    // Acquire the existing publication owner BEFORE reading. A concurrent redo can otherwise
    // publish between our read and the exclusive link, letting a stale snapshot erase its history.
    match fs::hard_link(&paths.sidecar, &paths.previous) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if expected_settled.is_some() {
                return Err(QualityStoreError::HistoryRegression);
            }
            let snapshot = QualitySnapshot {
                storage_version: QUALITY_STORE_VERSION,
                recording_id: recording_id.to_owned(),
                audio_file_name: paths.audio_file_name.clone(),
                state: QualityStorageState::LegacyUnknown,
                expected_retained_frames: None,
                quality: QualityRecord::default(),
                settled_quality: Some(quality.clone()),
            };
            return publish(&paths, &snapshot, false, faults);
        }
        Err(_) => return Err(QualityStoreError::Write),
    }
    let prepared = (|| {
        sync_parent(&paths.parent)?;
        let mut snapshot = read_snapshot(&paths.previous, &paths, recording_id)?
            .ok_or(QualityStoreError::InvalidTransition)?;
        if snapshot.settled_quality.as_ref() != expected_settled {
            return Err(QualityStoreError::HistoryRegression);
        }
        preserves_settled_history(quality, &snapshot.quality)?;
        if let Some(previous) = &snapshot.settled_quality {
            preserves_settled_history(quality, previous)?;
        }
        snapshot.settled_quality = Some(quality.clone());
        encode(&snapshot)
    })();
    match prepared {
        Ok(bytes) => publish_owned(&paths, &bytes, true, faults),
        Err(error) => {
            remove_if_present(&paths.previous)?;
            Err(error)
        }
    }
}

/// Read recovery authority using the expected identity and a frame count from validated WAV data.
/// `None` never certifies completion. Missing sidecars preserve legacy-unknown semantics; corrupt
/// data/bindings return content-free errors. Surviving transaction files always fail closed, while
/// a valid prior snapshot can still recover known gaps if the main sidecar is unavailable.
pub fn load_quality(
    audio: &Path,
    recording_id: &str,
    actual_retained_frames: Option<u64>,
) -> Result<QualityReadout, QualityStoreError> {
    validate_identity(recording_id)?;
    let paths = StorePaths::new(audio)?;
    let interrupted = paths.has_temporary()?;
    let primary = read_snapshot(&paths.sidecar, &paths, recording_id);
    let snapshot = if interrupted {
        // Prefer the visible snapshot: it may contain additional known gaps even if publication
        // failed. Fall back to the durable backup without silently discarding capture history.
        let previous = read_snapshot(&paths.previous, &paths, recording_id);
        match (primary, previous) {
            (Ok(Some(current)), Ok(Some(prior))) => {
                preserves_history(&current.quality, &prior.quality)?;
                if let Some(prior_settled) = &prior.settled_quality {
                    preserves_settled_history(
                        current
                            .settled_quality
                            .as_ref()
                            .ok_or(QualityStoreError::HistoryRegression)?,
                        prior_settled,
                    )?;
                }
                Some(current)
            }
            (Ok(Some(current)), _) => Some(current),
            (_, Ok(Some(prior))) => Some(prior),
            (Ok(None), Ok(None)) => read_snapshot(&paths.pending, &paths, recording_id)?,
            (Err(error), _) | (_, Err(error)) => return Err(error),
        }
    } else {
        primary?
    };
    let status = if interrupted {
        QualityReadStatus::InterruptedPublication
    } else if let Some(snapshot) = &snapshot {
        match snapshot.state {
            QualityStorageState::LegacyUnknown => QualityReadStatus::LegacyUnknown,
            QualityStorageState::InProgress => QualityReadStatus::InProgress,
            QualityStorageState::Finalizing => QualityReadStatus::Finalizing,
            QualityStorageState::AudioDurable => match actual_retained_frames {
                None => QualityReadStatus::AudioUnverified,
                Some(frames) if Some(frames) != snapshot.expected_retained_frames => {
                    QualityReadStatus::FrameCountMismatch
                }
                Some(_) => QualityReadStatus::AudioDurable,
            },
        }
    } else {
        QualityReadStatus::LegacyUnknown
    };
    Ok(QualityReadout { status, snapshot })
}

fn validate_identity(identity: &str) -> Result<(), QualityStoreError> {
    // A recording ID is an opaque bounded identifier, not unbounded user text.
    if identity.is_empty() || identity.len() > 1024 || identity.chars().any(char::is_control) {
        Err(QualityStoreError::InvalidIdentity)
    } else {
        Ok(())
    }
}

fn read_snapshot(
    path: &Path,
    paths: &StorePaths,
    recording_id: &str,
) -> Result<Option<QualitySnapshot>, QualityStoreError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(QualityStoreError::Read),
    };
    if !file
        .metadata()
        .map_err(|_| QualityStoreError::Read)?
        .is_file()
    {
        return Err(QualityStoreError::Read);
    }
    let mut bytes = Vec::new();
    file.take(MAX_QUALITY_SIDECAR_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| QualityStoreError::Read)?;
    if bytes.len() > MAX_QUALITY_SIDECAR_BYTES {
        return Err(QualityStoreError::TooLarge);
    }
    let snapshot: QualitySnapshot =
        serde_json::from_slice(&bytes).map_err(|_| QualityStoreError::Corrupt)?;
    if snapshot.storage_version != QUALITY_STORE_VERSION {
        return Err(QualityStoreError::UnsupportedVersion);
    }
    if snapshot.recording_id != recording_id || snapshot.audio_file_name != paths.audio_file_name {
        return Err(QualityStoreError::BindingMismatch);
    }
    validate_snapshot(&snapshot)?;
    Ok(Some(snapshot))
}

fn validate_snapshot(snapshot: &QualitySnapshot) -> Result<(), QualityStoreError> {
    snapshot
        .quality
        .validate()
        .map_err(|_| QualityStoreError::InvalidQuality)?;
    match snapshot.state {
        QualityStorageState::LegacyUnknown => {
            if snapshot.expected_retained_frames.is_some()
                || snapshot.quality != QualityRecord::default()
                || snapshot.settled_quality.is_none()
            {
                return Err(QualityStoreError::InvalidTransition);
            }
        }
        QualityStorageState::InProgress
        | QualityStorageState::Finalizing
        | QualityStorageState::AudioDurable => {
            if snapshot.expected_retained_frames.is_none() {
                return Err(QualityStoreError::InvalidTransition);
            }
        }
    }
    if let Some(settled) = &snapshot.settled_quality {
        preserves_settled_history(settled, &snapshot.quality)?;
    }
    Ok(())
}

struct BoundedBytes(Vec<u8>);

impl Write for BoundedBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_QUALITY_SIDECAR_BYTES.saturating_sub(self.0.len()) {
            return Err(io::Error::other("quality storage size limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encode(snapshot: &QualitySnapshot) -> Result<Vec<u8>, QualityStoreError> {
    validate_snapshot(snapshot)?;
    let mut bytes = BoundedBytes(Vec::new());
    serde_json::to_writer(&mut bytes, snapshot).map_err(|error| {
        if error.is_io() {
            QualityStoreError::TooLarge
        } else {
            QualityStoreError::InvalidQuality
        }
    })?;
    Ok(bytes.0)
}

fn present(path: &Path) -> Result<bool, QualityStoreError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(QualityStoreError::Read),
    }
}

fn private_new(path: &Path) -> Result<File, QualityStoreError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(|_| QualityStoreError::Write)
}

fn sync_parent(parent: &Path) -> Result<(), QualityStoreError> {
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| QualityStoreError::DirectorySync)
}

fn remove_if_present(path: &Path) -> Result<(), QualityStoreError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(QualityStoreError::Remove),
    }
}

fn publish(
    paths: &StorePaths,
    snapshot: &QualitySnapshot,
    has_prior: bool,
    faults: &mut Faults,
) -> Result<(), QualityStoreError> {
    let encoded = encode(snapshot)?;
    if paths.has_temporary()? {
        return Err(QualityStoreError::InterruptedPublication);
    }
    // A durable prior inode remains independently reachable until publication/rollback is synced.
    // Deterministic, exclusive temporary names make interrupted work discoverable for this recording.
    if has_prior {
        fs::hard_link(&paths.sidecar, &paths.previous).map_err(|_| QualityStoreError::Write)?;
        if let Err(error) = sync_parent(&paths.parent) {
            let _ = remove_if_present(&paths.previous);
            return Err(error);
        }
    }
    let prepared = (|| {
        let mut snapshot = snapshot.clone();
        if has_prior
            && let Some(prior) = read_snapshot(&paths.previous, paths, &snapshot.recording_id)?
            && let Some(mut settled) = prior.settled_quality
        {
            // The capture writer may still hold a pre-settlement in-memory snapshot. Carry the
            // adjunct forward under ownership; only this writer may advance capture storage state.
            settled
                .merge_snapshot(&snapshot.quality)
                .map_err(|_| QualityStoreError::HistoryRegression)?;
            snapshot.settled_quality = Some(settled);
            return encode(&snapshot);
        }
        Ok(encoded)
    })();
    let bytes = match prepared {
        Ok(bytes) => bytes,
        Err(error) => {
            if has_prior {
                remove_if_present(&paths.previous)?;
            }
            return Err(error);
        }
    };
    publish_owned(paths, &bytes, has_prior, faults)
}

/// The caller owns the exclusive transaction paths and has pinned the prior inode, if present.
fn publish_owned(
    paths: &StorePaths,
    bytes: &[u8],
    has_prior: bool,
    faults: &mut Faults,
) -> Result<(), QualityStoreError> {
    let mut renamed = false;
    let mut pending_owned = false;
    let result = (|| {
        let mut file = private_new(&paths.pending)?;
        pending_owned = true;
        faults.check(FaultPoint::Write, QualityStoreError::Write)?;
        file.write_all(bytes)
            .map_err(|_| QualityStoreError::Write)?;
        faults.check(FaultPoint::FileSync, QualityStoreError::FileSync)?;
        file.sync_all().map_err(|_| QualityStoreError::FileSync)?;
        faults.check(FaultPoint::Rename, QualityStoreError::Rename)?;
        if has_prior {
            fs::rename(&paths.pending, &paths.sidecar).map_err(|_| QualityStoreError::Rename)?;
            pending_owned = false;
        } else {
            // Exclusive initial publication: a capture/history publisher that won after our
            // absence check, or even a malformed existing inode, must never be overwritten.
            fs::hard_link(&paths.pending, &paths.sidecar).map_err(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    QualityStoreError::AlreadyExists
                } else {
                    QualityStoreError::Rename
                }
            })?;
        }
        renamed = true;
        faults.check(FaultPoint::DirectorySync, QualityStoreError::DirectorySync)?;
        sync_parent(&paths.parent)
    })();
    if let Err(error) = result {
        if renamed && rollback(paths, has_prior, faults).is_err() {
            // Never remove the only recoverable prior snapshot or its completion barrier here.
            return Err(QualityStoreError::Rollback);
        }
        if pending_owned {
            let _ = remove_if_present(&paths.pending);
        }
        if has_prior {
            let _ = remove_if_present(&paths.previous);
        }
        return Err(error);
    }
    // The new authority is already directory-synced. A backup unlink need not become durable to
    // make that true: resurrection of a backup after a crash only causes a conservative incomplete
    // read. A failed unlink leaves the barrier in place and prevents this call delivering completion.
    if has_prior {
        remove_if_present(&paths.previous)?;
    }
    if pending_owned {
        remove_if_present(&paths.pending)?;
    }
    Ok(())
}

fn rollback(
    paths: &StorePaths,
    has_prior: bool,
    faults: &mut Faults,
) -> Result<(), QualityStoreError> {
    if has_prior {
        // Keep `previous` until restoration's directory sync succeeds. Consuming it with a rename
        // would lose the recovery barrier if that sync failed.
        fs::hard_link(&paths.previous, &paths.restore).map_err(|_| QualityStoreError::Rollback)?;
        faults.check(FaultPoint::RollbackRename, QualityStoreError::Rollback)?;
        fs::rename(&paths.restore, &paths.sidecar).map_err(|_| QualityStoreError::Rollback)?;
    } else {
        remove_if_present(&paths.sidecar)?;
    }
    faults.check(
        FaultPoint::RollbackDirectorySync,
        QualityStoreError::Rollback,
    )?;
    sync_parent(&paths.parent)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FaultPoint {
    Write,
    FileSync,
    Rename,
    DirectorySync,
    RollbackRename,
    RollbackDirectorySync,
}

#[derive(Default)]
struct Faults {
    #[cfg(test)]
    failures: Vec<FaultPoint>,
}

impl Faults {
    fn check(
        &mut self,
        point: FaultPoint,
        error: QualityStoreError,
    ) -> Result<(), QualityStoreError> {
        #[cfg(test)]
        if self.failures.first() == Some(&point) {
            self.failures.remove(0);
            return Err(error);
        }
        let _ = (point, error);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CaptureEpoch, CaptureEpochReason, QualityCause, QualityClass, QualityResolution,
        QualitySpan, QualityStage, QualityTrack, SourceCompleteness,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "corti-quality-store-{}-{}",
                std::process::id(),
                NEXT_TEST.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn audio(&self) -> PathBuf {
            self.0.join("synthetic.wav")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn initial() -> QualityRecord {
        let mut record = QualityRecord::observed();
        record
            .insert_epoch(CaptureEpoch {
                id: 0,
                source_start_ns: Some(0),
                source_rate_hz: 48_000,
                source_frame_base: 0,
                retained_start_frame: 0,
                retained_rate_hz: 48_000,
                mic_channels: 1,
                tap_channels: 1,
                reason: CaptureEpochReason::Start,
            })
            .unwrap();
        record
    }

    fn with_loss() -> QualityRecord {
        let mut record = initial();
        record
            .insert_span(QualitySpan {
                epoch: 0,
                start_frame: 2,
                end_frame: 4,
                track: QualityTrack::Both,
                cause: QualityCause::RingOverflow,
                class: QualityClass::SourceLoss,
                stage: QualityStage::Capture,
                resolution: QualityResolution::Outstanding,
            })
            .unwrap();
        record
    }

    fn jumped_record() -> QualityRecord {
        let mut record = with_loss();
        record
            .insert_epoch(CaptureEpoch {
                id: 1,
                source_start_ns: Some(120_000_000_000),
                source_rate_hz: 44_100,
                source_frame_base: 5_760_000,
                retained_start_frame: 8,
                retained_rate_hz: 48_000,
                mic_channels: 1,
                tap_channels: 1,
                reason: CaptureEpochReason::RateChange,
            })
            .unwrap();
        record
            .insert_span(QualitySpan {
                epoch: 1,
                start_frame: 5_760_000,
                end_frame: 5_760_002,
                track: QualityTrack::Both,
                cause: QualityCause::RingOverflow,
                class: QualityClass::SourceLoss,
                stage: QualityStage::Capture,
                resolution: QualityResolution::Outstanding,
            })
            .unwrap();
        record
            .record_omitted_spans(QualityClass::SourceLoss, QualityResolution::Outstanding, 3)
            .unwrap();
        record.record_omitted_epochs(2);
        record
    }

    // A generated, valid 16-bit mono PCM WAV. The store deliberately delegates header parsing to
    // the caller; no real recording, transcript, or private content enters these tests.
    fn synthetic_wav(path: &Path, frames: u32) {
        let data_bytes = frames.checked_mul(2).unwrap();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&48_000u32.to_le_bytes());
        bytes.extend_from_slice(&96_000u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_bytes.to_le_bytes());
        bytes.resize(44 + data_bytes as usize, 0);
        fs::write(path, bytes).unwrap();
    }

    fn assert_no_temporary(audio: &Path) {
        for path in quality_temporary_paths(audio).unwrap() {
            assert!(!path.exists());
        }
    }

    fn legacy_history() -> QualityRecord {
        let mut quality = initial();
        quality
            .insert_span(QualitySpan {
                epoch: 0,
                start_frame: 8,
                end_frame: 10,
                track: QualityTrack::Mic,
                cause: QualityCause::DecodeFailure,
                class: QualityClass::ProcessingLoss,
                stage: QualityStage::Batch,
                resolution: QualityResolution::Outstanding,
            })
            .unwrap();
        // The ordinary loader merges real observed facts with missing capture authority.
        quality.merge_snapshot(&QualityRecord::default()).unwrap();
        quality
    }

    #[test]
    fn legacy_history_create_reopen_and_compare_and_swap_preserve_unknown_capture() {
        let fixture = Fixture::new();
        let audio = fixture.audio();
        synthetic_wav(&audio, 16);
        let history = legacy_history();
        persist_settled_quality(&audio, "synthetic", None, &history).unwrap();
        let read = load_quality(&audio, "synthetic", Some(16)).unwrap();
        assert_eq!(read.status, QualityReadStatus::LegacyUnknown);
        assert!(!read.permits_completion());
        let snapshot = read.snapshot.unwrap();
        assert_eq!(snapshot.quality(), &QualityRecord::default());
        assert_eq!(snapshot.settled_quality(), Some(&history));
        let value = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(value["storage_version"], 1);
        assert_eq!(value["state"], "legacy_unknown");
        assert!(value["expected_retained_frames"].is_null());
        let mut later = history.clone();
        later
            .record_omitted_spans(
                QualityClass::ProcessingLoss,
                QualityResolution::Outstanding,
                1,
            )
            .unwrap();
        assert_eq!(
            persist_settled_quality(&audio, "synthetic", None, &later),
            Err(QualityStoreError::HistoryRegression)
        );
        persist_settled_quality(&audio, "synthetic", Some(&history), &later).unwrap();
        assert_eq!(
            persist_settled_quality(&audio, "synthetic", Some(&history), &history),
            Err(QualityStoreError::HistoryRegression)
        );
        let reopened = load_quality(&audio, "synthetic", None).unwrap();
        assert_eq!(reopened.status, QualityReadStatus::LegacyUnknown);
        assert!(!reopened.permits_completion());
        assert_eq!(reopened.snapshot.unwrap().settled_quality(), Some(&later));
        assert_no_temporary(&audio);
    }

    #[test]
    fn legacy_history_success_and_empty_results_never_promote_capture_storage() {
        for history in [legacy_history(), QualityRecord::default()] {
            let fixture = Fixture::new();
            let audio = fixture.audio();
            persist_settled_quality(&audio, "synthetic", None, &history).unwrap();
            let mut successful = history.clone();
            successful
                .resolve_processing_after_batch(
                    crate::BatchRecoveryOutcome::SameBackendWholeFileSuccess,
                )
                .unwrap();
            persist_settled_quality(&audio, "synthetic", Some(&history), &successful).unwrap();
            for frames in [None, Some(0), Some(u64::MAX)] {
                let read = load_quality(&audio, "synthetic", frames).unwrap();
                assert_eq!(read.status, QualityReadStatus::LegacyUnknown);
                assert!(!read.permits_completion());
                let snapshot = read.snapshot.unwrap();
                assert_eq!(snapshot.state(), QualityStorageState::LegacyUnknown);
                assert_eq!(snapshot.expected_retained_frames(), None);
                assert_eq!(snapshot.quality(), &QualityRecord::default());
                assert_eq!(snapshot.settled_quality(), Some(&successful));
                assert_eq!(
                    successful.source_completeness(),
                    SourceCompleteness::Unknown
                );
                // Even an internal handle cannot use the capture lifecycle to promote this form.
                let mut store = QualityStore {
                    paths: StorePaths::new(&audio).unwrap(),
                    durable: snapshot,
                    failed: false,
                    faults: Faults::default(),
                };
                assert_eq!(
                    store.persist_before_audio(&successful, 0),
                    Err(QualityStoreError::InvalidTransition)
                );
                assert_eq!(
                    store.prepare_finalization(&successful, 0),
                    Err(QualityStoreError::InvalidTransition)
                );
                assert_eq!(
                    store.complete_after_audio_sync(0),
                    Err(QualityStoreError::InvalidTransition)
                );
            }
        }
    }

    #[test]
    fn legacy_history_rejects_observed_input_instead_of_rewriting_known_facts() {
        let fixture = Fixture::new();
        let audio = fixture.audio();
        let observed = with_loss();
        assert_eq!(
            persist_settled_quality(&audio, "synthetic", None, &observed),
            Err(QualityStoreError::HistoryRegression)
        );
        assert!(!quality_sidecar_path(&audio).unwrap().exists());
        assert_no_temporary(&audio);
        let mut merged = observed.clone();
        merged.merge_snapshot(&QualityRecord::default()).unwrap();
        persist_settled_quality(&audio, "synthetic", None, &merged).unwrap();
        let stored = load_quality(&audio, "synthetic", Some(0)).unwrap();
        let saved = stored.snapshot.unwrap();
        assert_eq!(saved.settled_quality(), Some(&merged));
        assert_eq!(saved.settled_quality().unwrap().epochs, observed.epochs);
        assert_eq!(saved.settled_quality().unwrap().spans, observed.spans);
        assert_eq!(saved.quality(), &QualityRecord::default());
        let before = fs::read(quality_sidecar_path(&audio).unwrap()).unwrap();
        assert_eq!(
            persist_settled_quality(&audio, "synthetic", Some(&merged), &observed),
            Err(QualityStoreError::HistoryRegression)
        );
        assert_eq!(
            fs::read(quality_sidecar_path(&audio).unwrap()).unwrap(),
            before
        );
        assert_no_temporary(&audio);
    }

    #[test]
    fn legacy_history_state_dependent_fields_and_bindings_are_strict() {
        let fixture = Fixture::new();
        let audio = fixture.audio();
        let history = legacy_history();
        persist_settled_quality(&audio, "synthetic", None, &history).unwrap();
        let sidecar = quality_sidecar_path(&audio).unwrap();
        let valid = fs::read(&sidecar).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&valid).unwrap();
        for edit in 0..8 {
            let mut invalid = value.clone();
            match edit {
                0 => invalid["expected_retained_frames"] = serde_json::json!(0),
                1 => invalid["quality"] = serde_json::to_value(initial()).unwrap(),
                2 => invalid["settled_quality"] = serde_json::Value::Null,
                3 => invalid["state"] = serde_json::json!("audio_durable"),
                4 => {
                    invalid
                        .as_object_mut()
                        .unwrap()
                        .remove("expected_retained_frames");
                }
                5 => invalid["settled_quality"]["coverage"] = serde_json::json!("observed"),
                6 => invalid["settled_quality"]["spans"][0]["end_frame"] = serde_json::json!(0),
                _ => {
                    invalid.as_object_mut().unwrap().remove("settled_quality");
                }
            }
            let bytes = serde_json::to_vec(&invalid).unwrap();
            fs::write(&sidecar, &bytes).unwrap();
            assert!(
                load_quality(&audio, "synthetic", Some(0)).is_err(),
                "edit {edit}"
            );
            assert!(
                persist_settled_quality(&audio, "synthetic", Some(&history), &history).is_err()
            );
            assert_eq!(fs::read(&sidecar).unwrap(), bytes);
            assert_no_temporary(&audio);
        }
        fs::write(&sidecar, &valid).unwrap();
        assert_eq!(
            persist_settled_quality(&audio, "other", Some(&history), &history),
            Err(QualityStoreError::BindingMismatch)
        );
        assert_eq!(
            load_quality(&audio, "other", None),
            Err(QualityStoreError::BindingMismatch)
        );
        assert_eq!(fs::read(&sidecar).unwrap(), valid);
        let other = fixture.0.join("other.wav");
        fs::copy(&sidecar, quality_sidecar_path(&other).unwrap()).unwrap();
        assert_eq!(
            load_quality(&other, "synthetic", None),
            Err(QualityStoreError::BindingMismatch)
        );
    }

    #[test]
    fn legacy_history_initial_publication_cannot_clobber_collisions_or_interrupted_work() {
        for target in 0..4 {
            let fixture = Fixture::new();
            let audio = fixture.audio();
            let paths = StorePaths::new(&audio).unwrap();
            let path = [
                &paths.sidecar,
                &paths.pending,
                &paths.previous,
                &paths.restore,
            ][target];
            fs::write(path, b"synthetic-malformed-existing").unwrap();
            assert!(persist_settled_quality(&audio, "synthetic", None, &legacy_history()).is_err());
            assert_eq!(fs::read(path).unwrap(), b"synthetic-malformed-existing");
            if target != 0 {
                assert!(!paths.sidecar.exists());
            }
        }
        // Exercise the commit step after another publisher has won the absent-file race.
        for capture_won in [false, true] {
            let fixture = Fixture::new();
            let audio = fixture.audio();
            let paths = StorePaths::new(&audio).unwrap();
            let candidate = QualitySnapshot {
                storage_version: QUALITY_STORE_VERSION,
                recording_id: "synthetic".into(),
                audio_file_name: paths.audio_file_name.clone(),
                state: QualityStorageState::LegacyUnknown,
                expected_retained_frames: None,
                quality: QualityRecord::default(),
                settled_quality: Some(legacy_history()),
            };
            if capture_won {
                QualityStore::begin(&audio, "synthetic", &initial()).unwrap();
            } else {
                fs::write(&paths.sidecar, b"synthetic-malformed-existing").unwrap();
            }
            let before = fs::read(&paths.sidecar).unwrap();
            assert_eq!(
                publish_owned(
                    &paths,
                    &encode(&candidate).unwrap(),
                    false,
                    &mut Faults::default()
                ),
                Err(QualityStoreError::AlreadyExists)
            );
            assert_eq!(fs::read(&paths.sidecar).unwrap(), before);
            assert_no_temporary(&audio);
        }
    }

    #[test]
    fn legacy_history_concurrent_initial_and_later_publishers_preserve_the_winner() {
        for initial_publication in [true, false] {
            let fixture = Fixture::new();
            let audio = fixture.audio();
            let history = legacy_history();
            if !initial_publication {
                persist_settled_quality(&audio, "synthetic", None, &history).unwrap();
            }
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
            let workers: Vec<_> = [20, 24]
                .into_iter()
                .map(|start_frame| {
                    let audio = audio.clone();
                    let barrier = barrier.clone();
                    let baseline = history.clone();
                    let mut quality = history.clone();
                    quality
                        .insert_span(QualitySpan {
                            start_frame,
                            end_frame: start_frame + 1,
                            ..quality.spans[0]
                        })
                        .unwrap();
                    std::thread::spawn(move || {
                        barrier.wait();
                        let result = persist_settled_quality(
                            &audio,
                            "synthetic",
                            (!initial_publication).then_some(&baseline),
                            &quality,
                        );
                        (quality, result)
                    })
                })
                .collect();
            barrier.wait();
            let results: Vec<_> = workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect();
            assert_eq!(
                results.iter().filter(|(_, result)| result.is_ok()).count(),
                1
            );
            let winner = &results.iter().find(|(_, result)| result.is_ok()).unwrap().0;
            let read = load_quality(&audio, "synthetic", None).unwrap();
            assert_eq!(read.status, QualityReadStatus::LegacyUnknown);
            assert_eq!(read.snapshot.unwrap().settled_quality(), Some(winner));
            let mut merged = winner.clone();
            for (quality, _) in results.iter() {
                merged.merge_snapshot(quality).unwrap();
            }
            persist_settled_quality(&audio, "synthetic", Some(winner), &merged).unwrap();
            assert_eq!(
                load_quality(&audio, "synthetic", None)
                    .unwrap()
                    .snapshot
                    .unwrap()
                    .settled_quality(),
                Some(&merged)
            );
            assert_eq!(merged.spans.len(), 3);
            assert_no_temporary(&audio);
        }
    }

    #[test]
    fn legacy_history_publication_faults_restore_absence_or_exact_prior_bytes() {
        for initial_publication in [true, false] {
            for point in [
                FaultPoint::Write,
                FaultPoint::FileSync,
                FaultPoint::Rename,
                FaultPoint::DirectorySync,
            ] {
                let fixture = Fixture::new();
                let audio = fixture.audio();
                let history = legacy_history();
                let sidecar = quality_sidecar_path(&audio).unwrap();
                if !initial_publication {
                    persist_settled_quality(&audio, "synthetic", None, &history).unwrap();
                }
                let before = fs::read(&sidecar).ok();
                let mut next = history.clone();
                next.record_omitted_spans(
                    QualityClass::ProcessingLoss,
                    QualityResolution::Outstanding,
                    1,
                )
                .unwrap();
                let mut faults = Faults {
                    failures: vec![point],
                };
                assert!(
                    persist_settled_with_faults(
                        &audio,
                        "synthetic",
                        (!initial_publication).then_some(&history),
                        &next,
                        &mut faults
                    )
                    .is_err()
                );
                assert_eq!(fs::read(&sidecar).ok(), before);
                assert_no_temporary(&audio);
                let read = load_quality(&audio, "synthetic", Some(0)).unwrap();
                assert_eq!(read.status, QualityReadStatus::LegacyUnknown);
                assert!(!read.permits_completion());
                assert_eq!(read.snapshot.is_none(), initial_publication);
            }
        }
    }

    #[test]
    fn legacy_history_interrupted_updates_keep_private_history_and_registered_cleanup() {
        let fixture = Fixture::new();
        let audio = fixture.audio();
        let history = legacy_history();
        persist_settled_quality(&audio, "synthetic", None, &history).unwrap();
        let paths = StorePaths::new(&audio).unwrap();
        let mut next = history.clone();
        next.record_omitted_spans(
            QualityClass::ProcessingLoss,
            QualityResolution::Outstanding,
            1,
        )
        .unwrap();
        let mut faults = Faults {
            failures: vec![FaultPoint::DirectorySync, FaultPoint::RollbackRename],
        };
        assert_eq!(
            persist_settled_with_faults(&audio, "synthetic", Some(&history), &next, &mut faults),
            Err(QualityStoreError::Rollback)
        );
        let visible = load_quality(&audio, "synthetic", Some(16)).unwrap();
        assert_eq!(visible.status, QualityReadStatus::InterruptedPublication);
        assert!(!visible.permits_completion());
        assert_eq!(visible.snapshot.unwrap().settled_quality(), Some(&next));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [&paths.sidecar, &paths.previous, &paths.restore] {
                assert_eq!(
                    fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
        fs::write(&paths.sidecar, b"malformed-current").unwrap();
        let recovered = load_quality(&audio, "synthetic", None).unwrap();
        assert_eq!(recovered.status, QualityReadStatus::InterruptedPublication);
        assert_eq!(
            recovered.snapshot.unwrap().settled_quality(),
            Some(&history)
        );
        assert_eq!(
            persist_settled_quality(&audio, "synthetic", Some(&history), &next),
            Err(QualityStoreError::InterruptedPublication)
        );
        let unrelated = fixture.0.join("unrelated-quality.json");
        fs::write(&unrelated, b"keep").unwrap();
        remove_quality_files(&audio).unwrap();
        assert!(!paths.sidecar.exists());
        assert_no_temporary(&audio);
        assert_eq!(fs::read(unrelated).unwrap(), b"keep");
    }

    #[test]
    fn legacy_history_caps_reject_before_replacing_history() {
        let fixture = Fixture::new();
        let audio = fixture.audio();
        let history = legacy_history();
        persist_settled_quality(&audio, "synthetic", None, &history).unwrap();
        let paths = StorePaths::new(&audio).unwrap();
        let mut snapshot = load_quality(&audio, "synthetic", None)
            .unwrap()
            .snapshot
            .unwrap();
        let before = fs::read(&paths.sidecar).unwrap();
        snapshot.recording_id = "x".repeat(MAX_QUALITY_SIDECAR_BYTES);
        assert_eq!(
            publish(&paths, &snapshot, true, &mut Faults::default()),
            Err(QualityStoreError::TooLarge)
        );
        assert_eq!(fs::read(&paths.sidecar).unwrap(), before);
        assert_no_temporary(&audio);
        let oversized = vec![b' '; MAX_QUALITY_SIDECAR_BYTES + 1];
        fs::write(&paths.sidecar, &oversized).unwrap();
        assert_eq!(
            load_quality(&audio, "synthetic", None),
            Err(QualityStoreError::TooLarge)
        );
        assert_eq!(
            persist_settled_quality(&audio, "synthetic", Some(&history), &history),
            Err(QualityStoreError::TooLarge)
        );
        assert_eq!(fs::read(&paths.sidecar).unwrap(), oversized);
        assert_no_temporary(&audio);
    }

    #[test]
    fn capture_wire_frame_counts_remain_numbers_and_cannot_be_unknown() {
        let fixture = Fixture::new();
        let audio = fixture.audio();
        let store = QualityStore::begin(&audio, "synthetic", &initial()).unwrap();
        let mut value = serde_json::to_value(store.snapshot()).unwrap();
        assert_eq!(value["expected_retained_frames"], 0);
        for state in ["in_progress", "finalizing", "audio_durable"] {
            value["state"] = serde_json::json!(state);
            value["expected_retained_frames"] = serde_json::json!(8);
            let bytes = serde_json::to_vec(&value).unwrap();
            fs::write(&store.paths.sidecar, &bytes).unwrap();
            let old_wire = load_quality(&audio, "synthetic", Some(8)).unwrap();
            assert_eq!(old_wire.permits_completion(), state == "audio_durable");
            let parsed = old_wire.snapshot.unwrap();
            assert_eq!(parsed.expected_retained_frames(), Some(8));
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&encode(&parsed).unwrap()).unwrap(),
                value
            );
            value["expected_retained_frames"] = serde_json::Value::Null;
            fs::write(&store.paths.sidecar, serde_json::to_vec(&value).unwrap()).unwrap();
            assert_eq!(
                load_quality(&audio, "synthetic", Some(8)),
                Err(QualityStoreError::InvalidTransition)
            );
        }
    }

    #[test]
    fn concurrent_settlement_publishers_preserve_the_winner_and_retry_union() {
        let fixture = Fixture::new();
        let audio = fixture.audio();
        let capture = with_loss();
        QualityStore::begin(&audio, "synthetic", &capture).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let publishers: Vec<_> = [8, 12]
            .into_iter()
            .map(|start_frame| {
                let audio = audio.clone();
                let barrier = barrier.clone();
                let mut quality = capture.clone();
                quality
                    .insert_span(QualitySpan {
                        epoch: 0,
                        start_frame,
                        end_frame: start_frame + 2,
                        track: QualityTrack::Mic,
                        cause: QualityCause::MicTapMismatch,
                        class: QualityClass::ProcessingLoss,
                        stage: QualityStage::Live,
                        resolution: QualityResolution::Outstanding,
                    })
                    .unwrap();
                std::thread::spawn(move || {
                    barrier.wait();
                    let result = persist_settled_quality(&audio, "synthetic", None, &quality);
                    (quality, result)
                })
            })
            .collect();
        barrier.wait();
        let results: Vec<_> = publishers
            .into_iter()
            .map(|publisher| publisher.join().unwrap())
            .collect();
        assert_eq!(
            results.iter().filter(|(_, result)| result.is_ok()).count(),
            1
        );
        let winner = &results.iter().find(|(_, result)| result.is_ok()).unwrap().0;
        let stored = load_quality(&audio, "synthetic", None).unwrap();
        assert_eq!(stored.snapshot.unwrap().settled_quality(), Some(winner));
        let mut union = winner.clone();
        for (quality, _) in &results {
            union.merge_snapshot(quality).unwrap();
        }
        persist_settled_quality(&audio, "synthetic", Some(winner), &union).unwrap();
        let stored = load_quality(&audio, "synthetic", None).unwrap();
        assert_eq!(stored.snapshot.unwrap().settled_quality(), Some(&union));
        assert_eq!(
            union.spans.len(),
            3,
            "source loss and both live histories survive"
        );
        assert_no_temporary(&audio);
    }

    #[test]
    fn settled_history_uses_existing_publication_and_rejects_stale_attempts() {
        let fixture = Fixture::new();
        let audio = fixture.audio();
        let capture = with_loss();
        let mut writer = QualityStore::begin(&audio, "synthetic", &capture).unwrap();
        synthetic_wav(&audio, 16);
        writer.prepare_finalization(&capture, 16).unwrap();
        writer.complete_after_audio_sync(16).unwrap();
        let frozen = writer.snapshot().clone();
        let mut settled = capture.clone();
        let live = QualitySpan {
            epoch: 0,
            start_frame: 8,
            end_frame: 10,
            track: QualityTrack::Mic,
            cause: QualityCause::MicTapMismatch,
            class: QualityClass::ProcessingLoss,
            stage: QualityStage::Live,
            resolution: QualityResolution::Outstanding,
        };
        settled.insert_span(live).unwrap();
        persist_settled_quality(&audio, "synthetic", None, &settled).unwrap();
        let mut stale = capture.clone();
        stale
            .insert_span(QualitySpan {
                start_frame: 12,
                end_frame: 14,
                ..live
            })
            .unwrap();
        assert_eq!(
            persist_settled_quality(&audio, "synthetic", None, &stale),
            Err(QualityStoreError::HistoryRegression)
        );
        let mut merged = settled.clone();
        merged.merge_snapshot(&stale).unwrap();
        persist_settled_quality(&audio, "synthetic", Some(&settled), &merged).unwrap();
        let mut successful = merged.clone();
        successful
            .resolve_processing_after_batch(
                crate::BatchRecoveryOutcome::SameBackendWholeFileSuccess,
            )
            .unwrap();
        let before = fs::read(quality_sidecar_path(&audio).unwrap()).unwrap();
        for point in [
            FaultPoint::Write,
            FaultPoint::FileSync,
            FaultPoint::Rename,
            FaultPoint::DirectorySync,
        ] {
            let mut faults = Faults {
                failures: vec![point],
            };
            assert!(
                persist_settled_with_faults(
                    &audio,
                    "synthetic",
                    Some(&merged),
                    &successful,
                    &mut faults
                )
                .is_err()
            );
            assert_eq!(
                fs::read(quality_sidecar_path(&audio).unwrap()).unwrap(),
                before
            );
            assert_no_temporary(&audio);
        }
        persist_settled_quality(&audio, "synthetic", Some(&merged), &successful).unwrap();
        let mut later_failure = successful.clone();
        later_failure
            .insert_span(QualitySpan {
                stage: QualityStage::Batch,
                cause: QualityCause::DecodeFailure,
                ..live
            })
            .unwrap();
        persist_settled_quality(&audio, "synthetic", Some(&successful), &later_failure).unwrap();
        // A whole-file result started before this fresh failure cannot clear it, even at the same interval.
        assert_eq!(
            persist_settled_quality(&audio, "synthetic", Some(&successful), &successful),
            Err(QualityStoreError::HistoryRegression)
        );
        let mut actual = load_quality(&audio, "synthetic", Some(16))
            .unwrap()
            .snapshot
            .unwrap();
        assert_eq!(actual.settled_quality.take(), Some(later_failure));
        assert_eq!(actual, frozen);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(quality_sidecar_path(&audio).unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert_no_temporary(&audio);
    }

    #[test]
    fn settled_partial_history_survives_capture_finalization_without_promoting_storage() {
        let fixture = Fixture::new();
        let audio = fixture.audio();
        let capture = initial();
        let mut writer = QualityStore::begin(&audio, "synthetic", &capture).unwrap();
        let mut settled = capture.clone();
        settled
            .insert_span(QualitySpan {
                epoch: 0,
                start_frame: 8,
                end_frame: 10,
                track: QualityTrack::Mic,
                cause: QualityCause::MicTapMismatch,
                class: QualityClass::ProcessingLoss,
                stage: QualityStage::Live,
                resolution: QualityResolution::Outstanding,
            })
            .unwrap();
        persist_settled_quality(&audio, "synthetic", None, &settled).unwrap();
        assert_eq!(
            load_quality(&audio, "synthetic", Some(16)).unwrap().status,
            QualityReadStatus::InProgress
        );
        synthetic_wav(&audio, 16);
        writer.prepare_finalization(&capture, 16).unwrap();
        writer.complete_after_audio_sync(16).unwrap();
        let read = load_quality(&audio, "synthetic", Some(16)).unwrap();
        assert!(read.permits_completion());
        assert_eq!(read.snapshot.unwrap().settled_quality(), Some(&settled));
        remove_quality_files(&audio).unwrap();
        assert!(persist_settled_quality(&audio, "synthetic", None, &settled).is_err());
        assert!(!quality_sidecar_path(&audio).unwrap().exists());
        assert_no_temporary(&audio);
    }

    #[test]
    fn initial_authority_precedes_audio_and_binds_existing_identity() {
        let fixture = Fixture::new();
        let audio = fixture.audio();
        let store = QualityStore::begin(&audio, "synthetic-id", &initial()).unwrap();
        assert!(!audio.exists());
        assert_eq!(store.snapshot().state(), QualityStorageState::InProgress);
        assert_eq!(store.snapshot().expected_retained_frames(), Some(0));
        assert_eq!(
            quality_sidecar_path(&audio).unwrap(),
            fixture.0.join("synthetic-quality.json")
        );
        let read = load_quality(&audio, "synthetic-id", Some(0)).unwrap();
        assert_eq!(read.status, QualityReadStatus::InProgress);
        assert_eq!(read.quality(), Some(&initial()));
        assert!(!read.permits_completion());
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(quality_sidecar_path(&audio).unwrap()).unwrap())
                .unwrap();
        assert_eq!(value["storage_version"], 1);
        assert_eq!(value["recording_id"], "synthetic-id");
        assert_eq!(value["audio_file_name"], "synthetic.wav");
        assert_eq!(value["state"], "in_progress");
        assert!(
            value.get("settled_quality").is_none(),
            "old sidecars remain readable and absent optional fields stay absent"
        );
        assert_no_temporary(&audio);
    }

    #[test]
    fn begin_refuses_existing_audio_authority_or_registered_temporary() {
        for target in 0..5 {
            let fixture = Fixture::new();
            let audio = fixture.audio();
            let paths = StorePaths::new(&audio).unwrap();
            let path = [
                paths.audio,
                paths.sidecar,
                paths.pending,
                paths.previous,
                paths.restore,
            ]
            .into_iter()
            .nth(target)
            .unwrap();
            fs::write(&path, b"synthetic-existing").unwrap();
            assert!(matches!(
                QualityStore::begin(&audio, "synthetic-id", &initial()),
                Err(QualityStoreError::AlreadyExists)
            ));
            assert_eq!(fs::read(path).unwrap(), b"synthetic-existing");
        }
    }

    #[test]
    fn restart_preserves_large_unfilled_jump_overflow_and_exact_final_snapshot() {
        let fixture = Fixture::new();
        let audio = fixture.audio();
        let mut store = QualityStore::begin(&audio, "synthetic-id", &initial()).unwrap();
        let record = jumped_record();
        store.persist_before_audio(&record, 12).unwrap();
        let in_progress = load_quality(&audio, "synthetic-id", Some(12)).unwrap();
        assert_eq!(in_progress.status, QualityReadStatus::InProgress);
        assert_eq!(in_progress.quality(), Some(&record));
        store.prepare_finalization(&record, 12).unwrap();
        // A complete-looking header, including one produced by a writer's Drop, cannot promote it.
        synthetic_wav(&audio, 12);
        let finalizing = load_quality(&audio, "synthetic-id", Some(12)).unwrap();
        assert_eq!(finalizing.status, QualityReadStatus::Finalizing);
        assert!(!finalizing.permits_completion());
        assert_eq!(finalizing.quality(), Some(&record));
        store.complete_after_audio_sync(12).unwrap();
        drop(store);
        let complete = load_quality(&audio, "synthetic-id", Some(12)).unwrap();
        assert!(complete.permits_completion());
        assert_eq!(complete.quality(), Some(&record));
        let quality = complete.quality().unwrap();
        assert_eq!(
            quality.source_completeness(),
            SourceCompleteness::Incomplete
        );
        assert_eq!(quality.epochs[1].retained_start_frame, 8);
        assert_eq!(
            quality.epochs[1].source_time_ns(5_804_100).unwrap(),
            Some(121_000_000_000)
        );
        assert_eq!(quality.overflow.omitted_spans, [3, 0, 0]);
        assert_eq!(quality.overflow.omitted_epochs, 2);
        assert_no_temporary(&audio);
    }

    #[test]
    fn dropping_finalizing_store_never_promotes_complete_looking_wav() {
        let fixture = Fixture::new();
        let audio = fixture.audio();
        let mut store = QualityStore::begin(&audio, "synthetic-id", &initial()).unwrap();
        store.prepare_finalization(&with_loss(), 8).unwrap();
        synthetic_wav(&audio, 8);
        drop(store);
        let read = load_quality(&audio, "synthetic-id", Some(8)).unwrap();
        assert_eq!(read.status, QualityReadStatus::Finalizing);
        assert_eq!(read.quality(), Some(&with_loss()));
        assert!(!read.permits_completion());
    }

    #[test]
    fn phase_skips_missing_audio_and_mismatches_cannot_promote() {
        let fixture = Fixture::new();
        let audio = fixture.audio();
        let mut store = QualityStore::begin(&audio, "synthetic-id", &initial()).unwrap();
        assert_eq!(
            store.complete_after_audio_sync(0),
            Err(QualityStoreError::InvalidTransition)
        );
        store.prepare_finalization(&initial(), 8).unwrap();
        assert_eq!(
            store.persist_before_audio(&initial(), 8),
            Err(QualityStoreError::InvalidTransition)
        );
        assert_eq!(
            store.prepare_finalization(&initial(), 8),
            Err(QualityStoreError::InvalidTransition)
        );
        assert_eq!(
            store.complete_after_audio_sync(8),
            Err(QualityStoreError::AudioUnavailable)
        );
        assert_eq!(store.snapshot().state(), QualityStorageState::Finalizing);

        let fixture = Fixture::new();
        let audio = fixture.audio();
        let mut store = QualityStore::begin(&audio, "synthetic-id", &initial()).unwrap();
        store.prepare_finalization(&with_loss(), 8).unwrap();
        synthetic_wav(&audio, 7);
        assert_eq!(
            store.complete_after_audio_sync(7),
            Err(QualityStoreError::FrameCountMismatch)
        );
        assert_eq!(
            store.complete_after_audio_sync(8),
            Err(QualityStoreError::InvalidTransition)
        );
        let read = load_quality(&audio, "synthetic-id", Some(7)).unwrap();
        assert_eq!(read.status, QualityReadStatus::Finalizing);
        assert!(!read.permits_completion());
    }

    #[test]
    fn absent_unverified_mismatched_or_wrong_binding_never_certifies_completion() {
        let fixture = Fixture::new();
        let audio = fixture.audio();
        let missing = load_quality(&audio, "synthetic-id", Some(8)).unwrap();
        assert_eq!(missing.status, QualityReadStatus::LegacyUnknown);
        assert_eq!(missing.quality(), None);
        assert!(!missing.permits_completion());
        let mut store = QualityStore::begin(&audio, "synthetic-id", &initial()).unwrap();
        store.prepare_finalization(&initial(), 8).unwrap();
        synthetic_wav(&audio, 8);
        store.complete_after_audio_sync(8).unwrap();
        assert_eq!(
            load_quality(&audio, "synthetic-id", None).unwrap().status,
            QualityReadStatus::AudioUnverified
        );
        let mismatched = load_quality(&audio, "synthetic-id", Some(7)).unwrap();
        assert_eq!(mismatched.status, QualityReadStatus::FrameCountMismatch);
        assert!(!mismatched.permits_completion());
        assert_eq!(mismatched.quality(), Some(&initial()));
        assert_eq!(
            load_quality(&audio, "other-id", Some(8)),
            Err(QualityStoreError::BindingMismatch)
        );
        let other_audio = fixture.0.join("other.wav");
        fs::copy(
            quality_sidecar_path(&audio).unwrap(),
            quality_sidecar_path(&other_audio).unwrap(),
        )
        .unwrap();
        assert_eq!(
            load_quality(&other_audio, "synthetic-id", Some(8)),
            Err(QualityStoreError::BindingMismatch)
        );
    }

    #[test]
    fn malformed_and_future_envelopes_fail_closed_without_payload_errors() {
        let fixture = Fixture::new();
        let audio = fixture.audio();
        let store = QualityStore::begin(&audio, "synthetic-id", &initial()).unwrap();
        let sidecar = quality_sidecar_path(&audio).unwrap();
        for bytes in [b"{synthetic-private-sentinel".as_slice(), b"{}", b"null"] {
            fs::write(&sidecar, bytes).unwrap();
            assert_eq!(
                load_quality(&audio, "synthetic-id", Some(0)),
                Err(QualityStoreError::Corrupt)
            );
        }
        let mut future = store.snapshot().clone();
        future.storage_version += 1;
        fs::write(&sidecar, encode(&future).unwrap()).unwrap();
        assert_eq!(
            load_quality(&audio, "synthetic-id", Some(0)),
            Err(QualityStoreError::UnsupportedVersion)
        );
        let mut value = serde_json::to_value(store.snapshot()).unwrap();
        value["quality"]["schema_version"] = serde_json::json!(2);
        fs::write(&sidecar, serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(
            load_quality(&audio, "synthetic-id", Some(0)),
            Err(QualityStoreError::Corrupt)
        );
    }

    #[test]
    fn write_sync_rename_and_directory_sync_faults_preserve_prior_atomic_bytes() {
        for (point, expected) in [
            (FaultPoint::Write, QualityStoreError::Write),
            (FaultPoint::FileSync, QualityStoreError::FileSync),
            (FaultPoint::Rename, QualityStoreError::Rename),
            (FaultPoint::DirectorySync, QualityStoreError::DirectorySync),
        ] {
            let fixture = Fixture::new();
            let audio = fixture.audio();
            let mut store = QualityStore::begin(&audio, "synthetic-id", &with_loss()).unwrap();
            let sidecar = quality_sidecar_path(&audio).unwrap();
            let prior_bytes = fs::read(&sidecar).unwrap();
            let prior = store.snapshot().clone();
            store.faults.failures.push(point);
            assert_eq!(
                store.persist_before_audio(&jumped_record(), 12),
                Err(expected)
            );
            assert_eq!(fs::read(&sidecar).unwrap(), prior_bytes);
            assert_eq!(store.snapshot(), &prior);
            assert_no_temporary(&audio);
            assert!(!audio.exists());
            assert_eq!(
                store.persist_before_audio(&jumped_record(), 12),
                Err(QualityStoreError::InvalidTransition)
            );
            assert_eq!(
                load_quality(&audio, "synthetic-id", None)
                    .unwrap()
                    .quality(),
                Some(&with_loss())
            );
        }
    }

    #[test]
    fn initial_publication_faults_leave_no_authority_or_owned_temporary() {
        for point in [
            FaultPoint::Write,
            FaultPoint::FileSync,
            FaultPoint::Rename,
            FaultPoint::DirectorySync,
        ] {
            let fixture = Fixture::new();
            let audio = fixture.audio();
            let faults = Faults {
                failures: vec![point],
            };
            assert!(
                QualityStore::begin_with_faults(&audio, "synthetic-id", &initial(), faults)
                    .is_err()
            );
            assert!(!quality_sidecar_path(&audio).unwrap().exists());
            assert!(!audio.exists());
            assert_no_temporary(&audio);
        }
    }

    #[test]
    fn failed_rollback_keeps_prior_and_blocks_even_visible_audio_durable_state() {
        for point in [
            FaultPoint::RollbackRename,
            FaultPoint::RollbackDirectorySync,
        ] {
            let fixture = Fixture::new();
            let audio = fixture.audio();
            let record = jumped_record();
            let mut store = QualityStore::begin(&audio, "synthetic-id", &initial()).unwrap();
            store.prepare_finalization(&record, 12).unwrap();
            let prior = fs::read(&store.paths.sidecar).unwrap();
            synthetic_wav(&audio, 12);
            store.faults.failures = vec![FaultPoint::DirectorySync, point];
            assert_eq!(
                store.complete_after_audio_sync(12),
                Err(QualityStoreError::Rollback)
            );
            assert_eq!(fs::read(&store.paths.previous).unwrap(), prior);
            let primary = read_snapshot(&store.paths.sidecar, &store.paths, "synthetic-id")
                .unwrap()
                .unwrap();
            if point == FaultPoint::RollbackRename {
                assert_eq!(primary.state(), QualityStorageState::AudioDurable);
            }
            drop(store);
            let read = load_quality(&audio, "synthetic-id", Some(12)).unwrap();
            assert_eq!(read.status, QualityReadStatus::InterruptedPublication);
            assert_eq!(read.quality(), Some(&record));
            assert!(!read.permits_completion());
        }
    }

    #[test]
    fn interrupted_publication_recovers_prior_when_main_is_missing_or_corrupt() {
        for corrupt in [false, true] {
            let fixture = Fixture::new();
            let audio = fixture.audio();
            let mut store = QualityStore::begin(&audio, "synthetic-id", &with_loss()).unwrap();
            store.faults.failures = vec![FaultPoint::DirectorySync, FaultPoint::RollbackRename];
            assert_eq!(
                store.persist_before_audio(&jumped_record(), 12),
                Err(QualityStoreError::Rollback)
            );
            if corrupt {
                fs::write(&store.paths.sidecar, b"incomplete-synthetic-json").unwrap();
            } else {
                fs::remove_file(&store.paths.sidecar).unwrap();
            }
            let read = load_quality(&audio, "synthetic-id", Some(12)).unwrap();
            assert_eq!(read.status, QualityReadStatus::InterruptedPublication);
            assert_eq!(read.quality(), Some(&with_loss()));
            assert!(!read.permits_completion());
        }
    }

    #[test]
    fn pending_initial_snapshot_retains_known_history_without_certifying() {
        let fixture = Fixture::new();
        let audio = fixture.audio();
        let store = QualityStore::begin(&audio, "synthetic-id", &with_loss()).unwrap();
        fs::rename(&store.paths.sidecar, &store.paths.pending).unwrap();
        let read = load_quality(&audio, "synthetic-id", Some(0)).unwrap();
        assert_eq!(read.status, QualityReadStatus::InterruptedPublication);
        assert_eq!(read.quality(), Some(&with_loss()));
        assert!(!read.permits_completion());
    }

    #[test]
    fn history_loss_epoch_overflow_or_frame_regression_cannot_replace_snapshot() {
        let fixture = Fixture::new();
        let audio = fixture.audio();
        let mut store = QualityStore::begin(&audio, "synthetic-id", &initial()).unwrap();
        let record = jumped_record();
        store.persist_before_audio(&record, 12).unwrap();
        let prior = fs::read(&store.paths.sidecar).unwrap();
        for change in 0..3 {
            let mut regressed = record.clone();
            match change {
                0 => regressed.spans.clear(),
                1 => {
                    regressed.epochs.pop();
                    regressed.spans.retain(|span| span.epoch == 0);
                }
                _ => regressed.overflow = Default::default(),
            }
            assert_eq!(
                store.persist_before_audio(&regressed, 12),
                Err(QualityStoreError::HistoryRegression)
            );
            assert_eq!(fs::read(&store.paths.sidecar).unwrap(), prior);
        }
        assert_eq!(
            store.persist_before_audio(&record, 11),
            Err(QualityStoreError::FrameCountMismatch)
        );
        assert_eq!(fs::read(&store.paths.sidecar).unwrap(), prior);
    }

    #[test]
    fn read_and_serialization_caps_are_enforced_before_publication() {
        let fixture = Fixture::new();
        let audio = fixture.audio();
        let mut store = QualityStore::begin(&audio, "synthetic-id", &initial()).unwrap();
        let prior = fs::read(&store.paths.sidecar).unwrap();
        let mut oversized = store.snapshot().clone();
        oversized.recording_id = "x".repeat(MAX_QUALITY_SIDECAR_BYTES);
        assert_eq!(encode(&oversized), Err(QualityStoreError::TooLarge));
        assert_eq!(
            publish(&store.paths, &oversized, true, &mut store.faults),
            Err(QualityStoreError::TooLarge)
        );
        assert_eq!(fs::read(&store.paths.sidecar).unwrap(), prior);
        assert_no_temporary(&audio);
        let mut sink = BoundedBytes(Vec::new());
        sink.write_all(&vec![b' '; MAX_QUALITY_SIDECAR_BYTES])
            .unwrap();
        assert!(sink.write_all(b"x").is_err());
        assert_eq!(sink.0.len(), MAX_QUALITY_SIDECAR_BYTES);
        fs::write(
            &store.paths.sidecar,
            vec![b' '; MAX_QUALITY_SIDECAR_BYTES + 1],
        )
        .unwrap();
        assert_eq!(
            load_quality(&audio, "synthetic-id", Some(0)),
            Err(QualityStoreError::TooLarge)
        );
        assert!(matches!(
            QualityStore::begin(&fixture.0.join("large.wav"), &"x".repeat(1025), &initial()),
            Err(QualityStoreError::InvalidIdentity)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn sidecar_and_registered_files_are_private_even_on_rollback_failure() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let audio = fixture.audio();
        let mut store = QualityStore::begin(&audio, "synthetic-id", &initial()).unwrap();
        assert_eq!(
            fs::metadata(&store.paths.sidecar)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        store.faults.failures = vec![FaultPoint::DirectorySync, FaultPoint::RollbackRename];
        assert_eq!(
            store.persist_before_audio(&with_loss(), 8),
            Err(QualityStoreError::Rollback)
        );
        for path in [
            &store.paths.sidecar,
            &store.paths.previous,
            &store.paths.restore,
        ] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn recording_cleanup_removes_exact_registered_files_and_preserves_other_files() {
        let fixture = Fixture::new();
        let audio = fixture.audio();
        let store = QualityStore::begin(&audio, "synthetic-id", &initial()).unwrap();
        synthetic_wav(&audio, 0);
        for path in quality_temporary_paths(&audio).unwrap() {
            fs::write(path, b"synthetic-transaction").unwrap();
        }
        let unrelated = fixture.0.join("synthetic-quality.json.pending-unrelated");
        let other = fixture.0.join("other-quality.json");
        fs::write(&unrelated, b"synthetic-other").unwrap();
        fs::write(&other, b"synthetic-other").unwrap();
        drop(store);
        remove_quality_files(&audio).unwrap();
        assert!(!quality_sidecar_path(&audio).unwrap().exists());
        assert_no_temporary(&audio);
        assert!(audio.exists());
        assert!(unrelated.exists());
        assert!(other.exists());
        remove_quality_files(&audio).unwrap();
    }

    #[test]
    fn diagnostics_never_embed_identity_paths_or_json_payloads() {
        let fixture = Fixture::new();
        let audio = fixture.audio();
        let store = QualityStore::begin(&audio, "SYNTHETIC-PRIVATE-SENTINEL", &initial()).unwrap();
        let read = load_quality(&audio, "SYNTHETIC-PRIVATE-SENTINEL", None).unwrap();
        let diagnostics = format!(
            "{read:?} {:?} {}",
            store.snapshot(),
            QualityStoreError::Corrupt
        );
        assert!(!diagnostics.contains("SYNTHETIC-PRIVATE-SENTINEL"));
        assert!(!diagnostics.contains("synthetic.wav"));
        assert!(!diagnostics.contains("recording_id"));
        assert!(!diagnostics.contains("source_start_ns"));
    }
}
