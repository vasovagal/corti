//! Optional local transcript evidence, owned by one retained recording.
//!
//! The portable ledger owns text exactly once. This layer adds only a fixed-size recording/attempt
//! receipt and the existing 0600 atomic-file boundary. Nothing here accepts a vault path, provider,
//! network client, or audio samples. Failures are fixed categories; paths and content never enter logs.
//!
//! The entire envelope is at most 16 MiB, including the portable 4 KiB truncation and 256 KiB final
//! quality reserves. The portable retained allocation budget is 32 MiB. Encoding allocates one exact
//! length buffer of at most 16 MiB alongside that ledger, never a serde Value copy. Readback holds
//! that one bounded file buffer while the portable preflight bounds parse/validation allocations to
//! 16*32 MiB + 4*16 MiB; the 16 MiB caller buffer is additional (592 MiB conservative total).
//! Caller-owned cleanup inputs and output are accounted separately at the cleanup integration seam;
//! these allocator-facing bounds do not claim allocator bookkeeping, process memory, or RSS bounds.

use std::io::Write;
use std::path::{Path, PathBuf};

use corti_core::RecordingMeta;
use corti_transcribe::evidence::{DEFAULT_OWNED_CAP, Ledger};
use corti_vagus::provenance::{GenerationMode, TranscriptModels, TranscriptProvenance};
use ring::rand::SecureRandom;
use ring::{digest, rand};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub(crate) const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
const ENVELOPE_RESERVE: usize = 4096;
pub(crate) const LEDGER_CAP: usize = MAX_FILE_BYTES - ENVELOPE_RESERVE;
const VERSION: u32 = 1;

pub(crate) mod integration;
pub(crate) mod live;

/// Content-free receipt carried by the existing checkpoint; no new queue field or retry scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Receipt {
    recording: [u8; 32],
    generation: [u8; 16],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Status {
    Disabled,
    Available { receipt: Receipt },
    Truncated { receipt: Receipt },
    Unavailable,
}

impl Status {
    fn receipt(self) -> Option<Receipt> {
        match self {
            Self::Available { receipt } | Self::Truncated { receipt } => Some(receipt),
            Self::Disabled | Self::Unavailable => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Error {
    Io,
    Invalid,
    Binding,
    Capacity,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope<'a> {
    version: u32,
    receipt: Receipt,
    #[serde(borrow)]
    identity: &'a serde_json::value::RawValue,
    #[serde(borrow)]
    ledger: &'a serde_json::value::RawValue,
}

/// Recoverable allowlisted provenance, never AppConfig. Hashes in portable stages fingerprint these
/// identity documents, not model artifacts or verified executable bytes. AWS's acoustic model stays
/// the existing explicit opaque service identity. Quality appears once in the ledger, not here.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityDocument {
    fingerprint_kind: String,
    version: String,
    backend: String,
    mode: GenerationMode,
    models: TranscriptModels,
    configuration: BTreeMap<String, serde_json::Value>,
}

fn allowed_identity_key(key: &str) -> bool {
    matches!(
        key,
        "language"
            | "speaker_attribution"
            | "asr_engine"
            | "threads"
            | "diarize_far_end"
            | "diarization_threshold"
            | "vad_threshold"
            | "vad_min_silence_seconds"
            | "aec"
            | "segment_cleanup"
            | "input"
            | "live_buffer_minutes"
    )
}

impl IdentityDocument {
    fn from_provenance(provenance: &TranscriptProvenance) -> Result<Self, Error> {
        let selected: BTreeMap<_, _> = provenance
            .configuration
            .iter()
            .filter(|(key, _)| allowed_identity_key(key))
            .map(|(key, value)| (key.as_str(), value))
            .collect();
        let mut budget = MetadataBudget(ENVELOPE_RESERVE - 768);
        serde_json::to_writer(
            &mut budget,
            &(
                &provenance.version,
                &provenance.backend,
                provenance.mode,
                &provenance.models,
                &selected,
            ),
        )
        .map_err(|_| Error::Capacity)?;
        let configuration = selected
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value.clone()))
            .collect();
        let identity = Self {
            fingerprint_kind: "sha256_identity_document_not_artifact".into(),
            version: provenance.version.clone(),
            backend: provenance.backend.clone(),
            mode: provenance.mode,
            models: provenance.models.clone(),
            configuration,
        };
        // Input provenance is already the app's bounded allowlist, but even custom filenames or
        // hand-edited language values must not exceed the envelope's fixed ownership/wire reserve.
        if serde_json::to_vec(&identity)
            .map_err(|_| Error::Invalid)?
            .len()
            > ENVELOPE_RESERVE - 512
        {
            return Err(Error::Capacity);
        }
        Ok(identity)
    }

    fn fingerprints(&self) -> corti_transcribe::evidence::Identity {
        fn hash(value: &impl Serialize) -> Option<String> {
            let bytes = serde_json::to_vec(value).ok()?;
            let digest = digest::digest(&digest::SHA256, &bytes);
            Some(
                digest
                    .as_ref()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            )
        }
        corti_transcribe::evidence::Identity {
            configuration_hash: hash(&self.configuration),
            model_hash: hash(&self.models),
            runtime_hash: hash(&(&self.version, &self.backend, self.mode)),
        }
    }
}

struct MetadataBudget(usize);
impl Write for MetadataBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.0 {
            return Err(std::io::Error::other("evidence metadata capacity"));
        }
        self.0 -= bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// No Debug/Clone: the session contains transcript text.
pub(crate) struct Session {
    ledger: Ledger,
    receipt: Receipt,
    identity: IdentityDocument,
}

impl Session {
    /// A fresh transcription always starts a fresh generation. Never load a pre-checkpoint ledger:
    /// #93 retries ASR after that crash and atomically replaces the previous attempt instead.
    pub(crate) fn begin(id: &str, meta: &RecordingMeta) -> Result<Self, Error> {
        Self::with_provenance(
            id,
            meta,
            &TranscriptProvenance::legacy_unknown(GenerationMode::Batch),
        )
    }

    pub(crate) fn with_provenance(
        id: &str,
        meta: &RecordingMeta,
        provenance: &TranscriptProvenance,
    ) -> Result<Self, Error> {
        let mut generation = [0; 16];
        rand::SystemRandom::new()
            .fill(&mut generation)
            .map_err(|_| Error::Io)?;
        Ok(Self {
            ledger: Ledger::with_limits(LEDGER_CAP, DEFAULT_OWNED_CAP)
                .map_err(|_| Error::Capacity)?,
            receipt: Receipt {
                recording: recording_binding(id, meta),
                generation,
            },
            identity: IdentityDocument::from_provenance(provenance)?,
        })
    }

    pub(crate) fn identity(&self) -> corti_transcribe::evidence::Identity {
        self.identity.fingerprints()
    }

    pub(crate) fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub(crate) fn ledger_mut(&mut self) -> &mut Ledger {
        &mut self.ledger
    }

    /// Live windows reuse this session and atomically replace its one file. A truncation marker is
    /// success with partial evidence; neither cap exhaustion nor any I/O failure can fail filing.
    pub(crate) fn persist(&self, audio: &Path) -> Status {
        match self.store(audio) {
            Ok(()) if self.ledger.truncation().cause.is_some() => Status::Truncated {
                receipt: self.receipt,
            },
            Ok(()) => Status::Available {
                receipt: self.receipt,
            },
            Err(_) => {
                unavailable();
                Status::Unavailable
            }
        }
    }

    fn store(&self, audio: &Path) -> Result<(), Error> {
        let bytes = self.encode()?;
        crate::private_file::atomic_write_private(&path_for(audio), &bytes, "local evidence")
            .map_err(|_| Error::Io)
    }

    fn encode(&self) -> Result<Vec<u8>, Error> {
        // The tiny prefix owns only fixed-size numeric fields. It fits the 512-byte reserve even
        // when every byte serializes as a three-digit decimal number.
        let receipt = serde_json::to_vec(&self.receipt).map_err(|_| Error::Invalid)?;
        let start = b"{\"version\":1,\"receipt\":";
        let identity = serde_json::to_vec(&self.identity).map_err(|_| Error::Invalid)?;
        let identity_key = b",\"identity\":";
        let middle = b",\"ledger\":";
        let overhead =
            start.len() + receipt.len() + identity_key.len() + identity.len() + middle.len() + 1;
        if overhead > ENVELOPE_RESERVE {
            return Err(Error::Capacity);
        }
        let length = self.ledger.serialized_len().map_err(|_| Error::Invalid)? + overhead;
        if length > MAX_FILE_BYTES {
            return Err(Error::Capacity);
        }
        let mut bytes = Vec::with_capacity(length);
        bytes.extend_from_slice(start);
        bytes.extend_from_slice(&receipt);
        bytes.extend_from_slice(identity_key);
        bytes.extend_from_slice(&identity);
        bytes.extend_from_slice(middle);
        self.ledger
            .write_json(&mut bytes)
            .map_err(|_| Error::Invalid)?;
        bytes.write_all(b"}").map_err(|_| Error::Io)?;
        if bytes.len() != length || bytes.capacity() > MAX_FILE_BYTES {
            return Err(Error::Capacity);
        }
        Ok(bytes)
    }

    /// Filing-only replay verifies the exact recording/attempt receipt and never rewrites the file.
    /// Missing, corrupt, opted-out, or replaced evidence cannot make a valid checkpoint unusable.
    pub(crate) fn load(
        id: &str,
        meta: &RecordingMeta,
        audio: &Path,
        expected: Status,
    ) -> Result<Self, Error> {
        let expected = expected.receipt().ok_or(Error::Binding)?;
        if expected.recording != recording_binding(id, meta) {
            return Err(Error::Binding);
        }
        let bytes =
            crate::private_file::read_private(&path_for(audio), "local evidence", MAX_FILE_BYTES)
                .map_err(|_| Error::Io)?
                .ok_or(Error::Io)?;
        let envelope: Envelope<'_> = serde_json::from_slice(&bytes).map_err(|_| Error::Invalid)?;
        if envelope.version != VERSION || envelope.receipt != expected {
            return Err(Error::Binding);
        }
        if envelope.identity.get().len() > ENVELOPE_RESERVE - 512 {
            return Err(Error::Capacity);
        }
        let identity: IdentityDocument =
            serde_json::from_str(envelope.identity.get()).map_err(|_| Error::Invalid)?;
        if identity.fingerprint_kind != "sha256_identity_document_not_artifact"
            || identity
                .configuration
                .keys()
                .any(|key| !allowed_identity_key(key))
        {
            return Err(Error::Invalid);
        }
        let ledger = Ledger::read_json_with_limits(
            envelope.ledger.get().as_bytes(),
            LEDGER_CAP,
            DEFAULT_OWNED_CAP,
        )
        .map_err(|_| Error::Invalid)?;
        let fingerprints = identity.fingerprints();
        if ledger.groups().iter().any(|group| {
            group.identity.configuration_hash != fingerprints.configuration_hash
                || group.identity.model_hash != fingerprints.model_hash
                || group.identity.runtime_hash != fingerprints.runtime_hash
        }) {
            return Err(Error::Binding);
        }
        Ok(Self {
            ledger,
            receipt: expected,
            identity,
        })
    }
}

fn recording_binding(id: &str, meta: &RecordingMeta) -> [u8; 32] {
    let mut hash = digest::Context::new(&digest::SHA256);
    hash.update(b"corti-local-evidence-recording-v1\0");
    hash.update(&(id.len() as u64).to_le_bytes());
    hash.update(id.as_bytes());
    hash.update(&meta.started_at.timestamp().to_le_bytes());
    hash.update(&meta.started_at.timestamp_subsec_nanos().to_le_bytes());
    hash.finish()
        .as_ref()
        .try_into()
        .expect("SHA256 has 32 bytes")
}

pub(crate) fn unavailable() {
    tracing::warn!(target: "corti::evidence", status = "unavailable", "local evidence unavailable");
}

/// Sibling of retained audio; never an exported note or a path supplied by a provider.
pub(crate) fn path_for(audio: &Path) -> PathBuf {
    audio.with_extension("evidence.json")
}

pub(crate) fn temporary_paths(audio: &Path) -> std::io::Result<Vec<PathBuf>> {
    let path = path_for(audio);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let Some(name) = path.file_name() else {
        return Ok(Vec::new());
    };
    let prefix = format!("{}.tmp-", name.to_string_lossy());
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
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

/// Live capture can publish evidence before a first note registers its queue row. The capture cache
/// is already its durable owner; inspect only its flat, production-named evidence artifacts so the
/// next opt-out sweep also covers that interval and its crash leftovers. Queue-owned foreign paths
/// continue through `remove`. Never read text, follow links, or visit subdirectories here.
pub(crate) fn remove_capture_evidence() -> Result<usize, Error> {
    let directory = corti_capture::recordings_dir().map_err(|_| Error::Io)?;
    remove_capture_evidence_in(&directory)
}

fn remove_capture_evidence_in(directory: &Path) -> Result<usize, Error> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(_) => return Err(Error::Io),
    };
    let mut removed = 0;
    let mut failed = false;
    for entry in entries {
        let Ok(entry) = entry else {
            failed = true;
            continue;
        };
        if !entry
            .file_name()
            .to_str()
            .is_some_and(is_capture_evidence_name)
        {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => removed += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => failed = true,
        }
    }
    if failed { Err(Error::Io) } else { Ok(removed) }
}

fn is_capture_evidence_name(name: &str) -> bool {
    let canonical = if let Some((canonical, temporary)) = name.rsplit_once(".tmp-") {
        let Some((pid, sequence)) = temporary.split_once('-') else {
            return false;
        };
        // Match private_file::unique_temporary_path exactly, without accepting arbitrary siblings.
        if !pid
            .parse::<u32>()
            .is_ok_and(|value| value.to_string() == pid)
            || !sequence
                .parse::<u64>()
                .is_ok_and(|value| value.to_string() == sequence)
        {
            return false;
        }
        canonical
    } else {
        name
    };
    let Some(stem) = canonical.strip_suffix(".evidence.json") else {
        return false;
    };
    // corti_capture::recording_stem: YYYYMMDD-HHMMSS-<lowercase ASCII app slug>.
    let bytes = stem.as_bytes();
    bytes.len() >= 17
        && bytes[..8].iter().all(u8::is_ascii_digit)
        && bytes[8] == b'-'
        && bytes[9..15].iter().all(u8::is_ascii_digit)
        && bytes[15] == b'-'
        && bytes[16].is_ascii_alphanumeric()
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes[16..]
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

/// Opt-out removes only optional evidence and its registered temporary files. Capture quality is
/// mandatory and deliberately absent here, including for active recordings and AWS-owned rows.
pub(crate) fn remove(audio: &Path) -> Result<usize, Error> {
    // Directory search permission may permit unlinking the known file even when read permission
    // prevents enumerating siblings. Attempt the known deletion even when the temp scan fails.
    let mut paths = vec![path_for(audio)];
    let mut removed = 0;
    let mut failed = false;
    match temporary_paths(audio) {
        Ok(temporary) => paths.extend(temporary),
        Err(_) => failed = true,
    }
    for path in paths {
        match std::fs::remove_file(path) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => failed = true,
        }
    }
    if failed { Err(Error::Io) } else { Ok(removed) }
}

#[cfg(test)]
mod tests;
