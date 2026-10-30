//! Local-only, versioned transcription evidence. No persistence, logging, networking, or audio API.
//! Content-bearing types deliberately implement neither Debug nor Clone.
//! Existing TranscriptSegment stays unchanged; Markdown is a display, not a timing/recovery source.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};

use crate::segment::{CleanupReason, CleanupStage, EchoVerdict};
use corti_core::{QualityRecord, QualityTrack, TranscriptSegment};
use serde::{Deserialize, Serialize};

#[path = "evidence/preflight.rs"]
mod preflight;

pub const VERSION: u32 = 1;
pub const DEFAULT_CAP: usize = 16 * 1024 * 1024;
pub const METADATA_RESERVE: usize = 4096;
// Inside the 16 MiB cap, not an additional unbounded snapshot. The shared quality model contains
// bounded numeric/enum fields only. This matches the existing quality-sidecar bound conservatively.
pub const QUALITY_RESERVE: usize = 256 * 1024;
/// Separate retained-owned-capacity budget. No promise about caller allocations or allocator RSS.
pub const DEFAULT_OWNED_CAP: usize = 2 * DEFAULT_CAP;
pub const QUALITY_OWNED_RESERVE: usize = 64 * 1024;

// Per-operation borrowed row index only; never serialized or retained. Oversized/custom ledgers or
// failed allocation use the original scan. One index is dropped before admitting another group.
const ROW_LOOKUP_BYTES: usize = 4 * 1024 * 1024;
struct RowLookup<'a> {
    groups: &'a [Group],
    rows: Option<Vec<&'a Row>>,
}
impl<'a> RowLookup<'a> {
    fn new(groups: &'a [Group]) -> Self {
        Self::with_budget(groups, ROW_LOOKUP_BYTES)
    }
    fn with_budget(groups: &'a [Group], budget: usize) -> Self {
        Self::with_reservation(groups, budget, |rows, count| rows.try_reserve_exact(count))
    }
    fn with_reservation(
        groups: &'a [Group],
        budget: usize,
        reserve: impl FnOnce(&mut Vec<&'a Row>, usize) -> Result<(), std::collections::TryReserveError>,
    ) -> Self {
        let rows = (|| {
            let count = groups
                .iter()
                .try_fold(0usize, |count, group| count.checked_add(group.rows.len()))?;
            if count.checked_mul(std::mem::size_of::<&Row>())? > budget {
                return None;
            }
            let mut rows = Vec::new();
            reserve(&mut rows, count).ok()?;
            if vector_bytes(&rows) > budget {
                return None;
            }
            rows.extend(groups.iter().flat_map(|group| &group.rows));
            rows.sort_unstable_by_key(|row| row.id);
            Some(rows)
        })();
        Self { groups, rows }
    }
    fn get(&self, id: RowId) -> Option<&'a Row> {
        match &self.rows {
            Some(rows) => rows
                .binary_search_by_key(&id, |row| row.id)
                .ok()
                .map(|index| rows[index]),
            None => self.groups.iter().find_map(|group| group.row(id)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Capacity,
    InvalidVersion,
    InvalidIdentity,
    InvalidTiming,
    InvalidLink,
    InvalidEdit,
    InvalidQuality,
    InvalidDocument,
    Io,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct InvocationId(pub u64);

/// Unique owner, never inferred from speaker, timestamps, text, or a cleanup-local index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RowId {
    pub invocation: InvocationId,
    pub slot: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    // None is an explicit unavailable identity, never a fabricated fingerprint of an unknown model.
    // Values are existing SHA-256 hex or opaque HMAC-SHA-256 base64url identities; this API hashes nothing.
    pub configuration_hash: Option<String>,
    pub model_hash: Option<String>,
    pub runtime_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AecOrigin {
    None,
    LiveShortLookahead,
    Capture,
    OfflineFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalSource {
    LiveStream,
    RetainedProcessedWav,
    OfflineInput,
    ProviderText,
    /// Caller-supplied context with no retained upstream identity; never claimed to be fresh ASR.
    UnattributedContext,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignalIdentity {
    pub source: SignalSource,
    pub aec: AecOrigin,
    pub source_rate_hz: Option<u32>,
    pub lookahead_frames: Option<u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    pub id: RowId,
    // None explicitly means unavailable source-track attribution, e.g. provider input.
    pub track: Option<QualityTrack>,
    // #161 EarlyEcho uses a caller-assigned region ordinal, not a cleanup-local row position.
    pub source_region_index: Option<u64>,
    pub segment: TranscriptSegment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StageKind {
    TrueAsr,
    FallbackAsr,
    /// The actual external context supplied to cleanup when no retained row links are available.
    /// This is a context snapshot, never a newly claimed ASR invocation.
    ContextSnapshot,
    /// Ordered caller references released from retained early/window stages for actual cleanup.
    CleanupAssembly,
    /// Actual pause-based descendants of retained ASR/Echo input, with explicit UTF-8 lineage.
    Segmentation,
    /// Actual speaker-attributed descendants, never a new ASR invocation.
    Diarization,
    ProviderInput,
    FinalProse,
    Cleanup {
        #[serde(with = "CleanupStageWire")]
        pass: CleanupStage,
    },
}

// Serialization adapters use the actual existing decision enums; no parallel policy is invented.
#[derive(Serialize, Deserialize)]
#[serde(remote = "CleanupStage", rename_all = "snake_case")]
enum CleanupStageWire {
    Sort,
    Echo,
    Merge,
    Backchannel,
    Fillers,
    Lexicon,
    EarlyEcho,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "EchoVerdict", rename_all = "snake_case")]
enum EchoVerdictWire {
    Keep,
    KeepCriticalDifference,
    SuspectedDuplicate,
    DropAcoustic,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "CleanupReason", rename_all = "snake_case")]
enum CleanupReasonWire {
    TimeOrder,
    Echo(#[serde(with = "EchoVerdictWire")] EchoVerdict),
    SameSpeakerContinuation,
    OverlappingBackchannel,
    FillersOrStutters,
    TextRule,
}

pub use crate::segment::{ByteSpan, EditKind, TextEdit};

/// Raw f32 metric bit patterns preserve silence infinities without JSON converting them to null.
/// These are content-free aggregate measurements, not PCM.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EchoEvidence {
    pub mic_db_bits: u32,
    pub echo_estimate_db_bits: u32,
    pub double_talk_fraction_bits: u32,
    pub blocks: u64,
}

/// Preserve #161's content-free source description; these times are never a row-identity lookup key.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EchoSourceDescription {
    pub me: bool,
    pub start: f64,
    pub end: f64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    #[serde(with = "CleanupReasonWire")]
    pub reason: CleanupReason,
    // Preserve #161's indices: stage-local except EarlyEcho's source-region ordinal.
    pub input_indices: Vec<u64>,
    pub originals: Vec<RowId>,
    pub replacements: Vec<RowId>,
    pub edits: Vec<TextEdit>,
    pub evidence: Option<EchoEvidence>,
    pub source: Option<EchoSourceDescription>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stage {
    pub id: InvocationId,
    pub kind: StageKind,
    // The previous invocation in this group, or a retained invocation for a later linked group.
    // A fresh ASR invocation has no text-stage input. Never a text/time join.
    pub input: Option<InvocationId>,
    /// Ordered assembly used only by ProviderInput, instead of `input`. Each row must still be
    /// present in its named parent's output. This cannot resurrect a discarded historical row.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assembled_input: Vec<ParentRow>,
    /// Actual carry/context rows considered by this invocation, linked by owner and parent output.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<ParentRow>,
    /// Caller protection on an ASR snapshot; later stages carry protection through actual edits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub protected_output: Vec<RowId>,
    /// Ordered, disjoint source ranges for segmentation/diarization output. No inferred joins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub derivations: Vec<Derivation>,
    pub output: Vec<RowId>,
    pub decisions: Vec<Decision>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParentRow {
    pub parent: InvocationId,
    pub row: RowId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Derivation {
    pub original: RowId,
    pub source: ByteSpan,
    pub replacement: RowId,
}

/// One atomic retained unit: a source/transform invocation plus its linked decisions.
/// Later provider/prose groups may refer to a previously retained stage without copying its rows.
/// Rows are owned once; unchanged stage output points at the same immutable row.
///
/// ```compile_fail
/// fn requires_debug<T: std::fmt::Debug>() {}
/// requires_debug::<corti_transcribe::evidence::Group>();
/// ```
/// ```compile_fail
/// fn requires_clone<T: Clone>() {}
/// requires_clone::<corti_transcribe::evidence::Group>();
/// ```
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    pub window: u64,
    pub identity: Identity,
    pub signal: SignalIdentity,
    pub rows: Vec<Row>,
    pub stages: Vec<Stage>,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Truncation {
    pub omitted_groups: u64,
    pub omitted_stages: u64,
    pub omitted_rows: u64,
    pub first_omitted_invocation: Option<InvocationId>,
    pub last_omitted_invocation: Option<InvocationId>,
    pub counter_saturated: bool,
    pub cause: Option<TruncationCause>,
    #[serde(default)]
    pub omitted_decisions: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TruncationCause {
    SerializedCap,
    OwnedMemoryCap,
    UpstreamDecisions,
    TraceUnavailable,
    TransientCap,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    schema_version: u32,
    groups: Vec<Group>,
    last_invocation: InvocationId,
    truncation: Truncation,
    final_quality: Option<QualityRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Append {
    Retained,
    Omitted,
}

pub struct Ledger {
    document: Document,
    cap: usize,
    group_wire_bytes: usize,
    owned_cap: usize,
    group_heap_bytes: usize,
}

/// Count serde's actual escaped UTF-8 output, without allocating a serialization-sized scratch copy.
struct CountWriter {
    count: usize,
    limit: usize,
}

impl Write for CountWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.count) {
            return Err(io::Error::other("evidence capacity"));
        }
        self.count += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn wire_size(value: &impl Serialize, limit: usize) -> Result<usize, Error> {
    let mut counter = CountWriter { count: 0, limit };
    serde_json::to_writer(&mut counter, value).map_err(|_| Error::Capacity)?;
    Ok(counter.count)
}

impl Ledger {
    pub fn new(cap: usize) -> Result<Self, Error> {
        Self::with_limits(cap, cap.checked_mul(2).ok_or(Error::Capacity)?)
    }

    pub fn with_limits(cap: usize, owned_cap: usize) -> Result<Self, Error> {
        if cap > isize::MAX as usize
            || owned_cap > isize::MAX as usize
            || cap < QUALITY_RESERVE + METADATA_RESERVE
            || owned_cap < std::mem::size_of::<Self>() + QUALITY_OWNED_RESERVE
        {
            return Err(Error::Capacity);
        }
        let document = Document {
            schema_version: VERSION,
            groups: Vec::new(),
            last_invocation: InvocationId(0),
            truncation: Truncation::default(),
            final_quality: None,
        };
        wire_size(&document, METADATA_RESERVE)?;
        Ok(Self {
            document,
            cap,
            group_wire_bytes: 0,
            owned_cap,
            group_heap_bytes: 0,
        })
    }

    pub fn groups(&self) -> &[Group] {
        &self.document.groups
    }

    pub fn truncation(&self) -> &Truncation {
        &self.document.truncation
    }

    pub fn final_quality(&self) -> Option<&QualityRecord> {
        self.document.final_quality.as_ref()
    }

    /// Ledger value plus actual retained String/Vec capacities, excluding allocator bookkeeping.
    pub fn owned_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + vector_bytes(&self.document.groups)
            + self.group_heap_bytes
            + self
                .document
                .final_quality
                .as_ref()
                .map_or(0, quality_heap_bytes)
    }

    pub fn owned_limit(&self) -> usize {
        self.owned_cap
    }

    pub fn next_invocation(&self) -> Result<InvocationId, Error> {
        self.document
            .last_invocation
            .0
            .checked_add(1)
            .map(InvocationId)
            .ok_or(Error::InvalidLink)
    }

    /// Moves a bounded group into the ledger. Never clones its text and never partially retains it.
    /// First cap exhaustion seals content; later groups only advance the truncation summary.
    pub fn append(&mut self, group: Group) -> Result<Append, Error> {
        let last = group.check_ids(self.next_invocation()?)?;
        if let Some(cause) = self.document.truncation.cause {
            self.omit(&group, last, cause);
            return Ok(Append::Omitted);
        }
        let separator = usize::from(!self.document.groups.is_empty());
        let available = self.cap - QUALITY_RESERVE - METADATA_RESERVE - self.group_wire_bytes;
        let bytes = wire_size(&group, available.saturating_sub(separator));
        if bytes.is_err() || available < separator {
            self.omit(&group, last, TruncationCause::SerializedCap);
            return Ok(Append::Omitted);
        }
        let heap = group.heap_bytes();
        let mut replacement = Vec::new();
        let projected_capacity = if self.document.groups.len() == self.document.groups.capacity() {
            replacement
                .try_reserve_exact(self.document.groups.len() + 1)
                .map_err(|_| Error::Capacity)?;
            vector_bytes(&replacement)
        } else {
            vector_bytes(&self.document.groups)
        };
        let projected = std::mem::size_of::<Self>()
            .saturating_add(projected_capacity)
            .saturating_add(self.group_heap_bytes)
            .saturating_add(heap)
            .saturating_add(QUALITY_OWNED_RESERVE);
        if projected > self.owned_cap {
            self.omit(&group, last, TruncationCause::OwnedMemoryCap);
            return Ok(Append::Omitted);
        }
        group.validate(&self.document.groups)?;
        if replacement.capacity() != 0 {
            replacement.extend(std::mem::take(&mut self.document.groups));
            self.document.groups = replacement;
        }
        self.group_heap_bytes += heap;
        self.group_wire_bytes += bytes? + separator;
        self.document.last_invocation = last;
        self.document.groups.push(group);
        Ok(Append::Retained)
    }

    fn omit(&mut self, group: &Group, last: InvocationId, cause: TruncationCause) {
        let truncation = &mut self.document.truncation;
        truncation.cause.get_or_insert(cause);
        for (counter, amount) in [
            (&mut truncation.omitted_groups, 1),
            (&mut truncation.omitted_stages, group.stages.len() as u64),
            (&mut truncation.omitted_rows, group.rows.len() as u64),
            (
                &mut truncation.omitted_decisions,
                group
                    .stages
                    .iter()
                    .map(|stage| (stage.decisions.len() + stage.derivations.len()) as u64)
                    .sum(),
            ),
        ] {
            match counter.checked_add(amount) {
                Some(value) => *counter = value,
                None => {
                    *counter = u64::MAX;
                    truncation.counter_saturated = true;
                }
            }
        }
        truncation
            .first_omitted_invocation
            .get_or_insert(group.stages[0].id);
        truncation.last_omitted_invocation = Some(last);
        self.document.last_invocation = last;
    }

    /// Final quality is the same shared type, retained even after content truncation.
    /// Invalid/oversized input is rejected without silently replacing the previous authoritative record.
    pub fn set_final_quality(&mut self, quality: QualityRecord) -> Result<(), Error> {
        if quality_heap_bytes(&quality) > QUALITY_OWNED_RESERVE {
            return Err(Error::InvalidQuality);
        }
        quality.validate().map_err(|_| Error::InvalidQuality)?;
        wire_size(&quality, QUALITY_RESERVE).map_err(|_| Error::InvalidQuality)?;
        self.document.final_quality = Some(quality);
        Ok(())
    }

    /// Exact preimage of a retained stage. The caller gets references, not another text ledger.
    /// Omitted or unknown invocations are unavailable, never reconstructed from final prose.
    pub fn reconstruct_input(&self, invocation: InvocationId) -> Result<Vec<&Row>, Error> {
        for group in &self.document.groups {
            if let Some(stage) = group.stages.iter().find(|stage| stage.id == invocation) {
                if !stage.assembled_input.is_empty() {
                    return stage
                        .assembled_input
                        .iter()
                        .map(|input| {
                            self.document
                                .groups
                                .iter()
                                .find_map(|group| group.row(input.row))
                                .ok_or(Error::InvalidLink)
                        })
                        .collect();
                }
                let source = match stage.input {
                    Some(input) => self
                        .document
                        .groups
                        .iter()
                        .flat_map(|g| &g.stages)
                        .find(|stage| stage.id == input)
                        .ok_or(Error::InvalidLink)?,
                    None => stage,
                };
                return source
                    .output
                    .iter()
                    .map(|id| {
                        self.document
                            .groups
                            .iter()
                            .find_map(|g| g.row(*id))
                            .ok_or(Error::InvalidLink)
                    })
                    .collect();
            }
        }
        Err(Error::Unavailable)
    }

    pub fn serialized_len(&self) -> Result<usize, Error> {
        wire_size(&self.document, self.cap)
    }

    /// Production persistence can stream directly to its bounded atomic-file writer; no full JSON clone.
    pub fn write_json(&self, writer: impl Write) -> Result<(), Error> {
        self.serialized_len()?;
        serde_json::to_writer(writer, &self.document).map_err(|_| Error::Io)
    }

    pub fn read_json(bytes: &[u8], cap: usize) -> Result<Self, Error> {
        Self::read_json_with_limits(bytes, cap, cap.checked_mul(2).ok_or(Error::Capacity)?)
    }

    pub fn read_json_with_limits(
        bytes: &[u8],
        cap: usize,
        owned_cap: usize,
    ) -> Result<Self, Error> {
        if bytes.len() > cap {
            return Err(Error::Capacity);
        }
        let mut ledger = Self::with_limits(cap, owned_cap)?;
        preflight::check(bytes, owned_cap)?;
        let mut document: Document =
            serde_json::from_slice(bytes).map_err(|_| Error::InvalidDocument)?;
        if document.schema_version != VERSION {
            return Err(Error::InvalidVersion);
        }
        // JSON carries lengths, not allocation capacities. Drop parse-time spare capacity before
        // admission. Boxed-slice/string conversion produces length-sized retained allocations;
        // each is no larger than the corresponding owner that originally wrote these bytes.
        // Parsing/reallocation scratch is not covered by the retained-owned-capacity limit.
        compact_vec(&mut document.groups);
        for group in &mut document.groups {
            group.compact_owned();
        }
        if let Some(quality) = &mut document.final_quality {
            compact_vec(&mut quality.epochs);
            compact_vec(&mut quality.spans);
        }
        ledger.group_heap_bytes = document.groups.iter().fold(0usize, |total, group| {
            total.saturating_add(group.heap_bytes())
        });
        if std::mem::size_of::<Self>()
            .saturating_add(vector_bytes(&document.groups))
            .saturating_add(ledger.group_heap_bytes)
            .saturating_add(QUALITY_OWNED_RESERVE)
            > owned_cap
        {
            return Err(Error::Capacity);
        }
        // Validate the canonical retained representation in place. Re-appending would regrow
        // vectors and reintroduce allocation-dependent admission. Never omit a readback group.
        for (index, group) in document.groups.iter().enumerate() {
            let last = group.check_ids(ledger.next_invocation()?)?;
            let separator = usize::from(index != 0);
            let available = cap - QUALITY_RESERVE - METADATA_RESERVE - ledger.group_wire_bytes;
            let available = available.checked_sub(separator).ok_or(Error::Capacity)?;
            ledger.group_wire_bytes += wire_size(group, available)? + separator;
            group.validate(&document.groups[..index])?;
            ledger.document.last_invocation = last;
        }
        let t = &document.truncation;
        if t.omitted_groups == 0 {
            if *t != Truncation::default()
                || document.last_invocation != ledger.document.last_invocation
            {
                return Err(Error::InvalidDocument);
            }
        } else if t.first_omitted_invocation != Some(ledger.next_invocation()?)
            || t.cause.is_none()
            || t.last_omitted_invocation != Some(document.last_invocation)
            || t.omitted_stages < t.omitted_groups
            || document.last_invocation.0 < ledger.next_invocation()?.0
            || t.omitted_stages != document.last_invocation.0 - ledger.document.last_invocation.0
        {
            return Err(Error::InvalidDocument);
        }
        ledger.document.last_invocation = document.last_invocation;
        ledger.document.truncation = document.truncation;
        ledger.document.groups = document.groups;
        if let Some(quality) = document.final_quality {
            ledger.set_final_quality(quality)?;
        }
        ledger.serialized_len()?;
        Ok(ledger)
    }
}

impl Group {
    fn compact_owned(&mut self) {
        compact_vec(&mut self.rows);
        compact_vec(&mut self.stages);
        for hash in [
            &mut self.identity.configuration_hash,
            &mut self.identity.model_hash,
            &mut self.identity.runtime_hash,
        ]
        .into_iter()
        .flatten()
        {
            compact_string(hash);
        }
        for row in &mut self.rows {
            compact_string(&mut row.segment.text);
            if let corti_core::Speaker::Other(label) = &mut row.segment.speaker {
                compact_string(label);
            }
        }
        for stage in &mut self.stages {
            compact_vec(&mut stage.assembled_input);
            compact_vec(&mut stage.context);
            compact_vec(&mut stage.protected_output);
            compact_vec(&mut stage.derivations);
            compact_vec(&mut stage.output);
            compact_vec(&mut stage.decisions);
            for decision in &mut stage.decisions {
                compact_vec(&mut decision.input_indices);
                compact_vec(&mut decision.originals);
                compact_vec(&mut decision.replacements);
                compact_vec(&mut decision.edits);
                for edit in &mut decision.edits {
                    if let EditKind::Lexicon { rule_id } = &mut edit.action {
                        compact_string(rule_id);
                    }
                }
            }
        }
    }

    fn heap_bytes(&self) -> usize {
        let mut bytes = vector_bytes(&self.rows).saturating_add(vector_bytes(&self.stages));
        for hash in [
            &self.identity.configuration_hash,
            &self.identity.model_hash,
            &self.identity.runtime_hash,
        ]
        .into_iter()
        .flatten()
        {
            bytes = bytes.saturating_add(hash.capacity());
        }
        for row in &self.rows {
            bytes = bytes.saturating_add(row.segment.text.capacity());
            if let corti_core::Speaker::Other(label) = &row.segment.speaker {
                bytes = bytes.saturating_add(label.capacity());
            }
        }
        for stage in &self.stages {
            bytes = bytes
                .saturating_add(vector_bytes(&stage.assembled_input))
                .saturating_add(vector_bytes(&stage.context))
                .saturating_add(vector_bytes(&stage.protected_output))
                .saturating_add(vector_bytes(&stage.derivations))
                .saturating_add(vector_bytes(&stage.output))
                .saturating_add(vector_bytes(&stage.decisions));
            for d in &stage.decisions {
                bytes = bytes
                    .saturating_add(vector_bytes(&d.input_indices))
                    .saturating_add(vector_bytes(&d.originals))
                    .saturating_add(vector_bytes(&d.replacements))
                    .saturating_add(vector_bytes(&d.edits));
                for edit in &d.edits {
                    if let EditKind::Lexicon { rule_id } = &edit.action {
                        bytes = bytes.saturating_add(rule_id.capacity());
                    }
                }
            }
        }
        bytes
    }

    fn check_ids(&self, first: InvocationId) -> Result<InvocationId, Error> {
        if self.stages.is_empty() {
            return Err(Error::InvalidLink);
        }
        for (index, stage) in self.stages.iter().enumerate() {
            if Some(stage.id.0) != first.0.checked_add(index as u64) {
                return Err(Error::InvalidLink);
            }
        }
        Ok(self.stages.last().ok_or(Error::InvalidLink)?.id)
    }

    fn row(&self, id: RowId) -> Option<&Row> {
        self.rows.iter().find(|row| row.id == id)
    }

    fn validate(&self, retained: &[Group]) -> Result<(), Error> {
        for hash in [
            &self.identity.configuration_hash,
            &self.identity.model_hash,
            &self.identity.runtime_hash,
        ]
        .into_iter()
        .flatten()
        {
            let sha256_hex = hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit());
            let opaque_hmac = hash.len() == 43
                && hash
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
            if !sha256_hex && !opaque_hmac {
                return Err(Error::InvalidIdentity);
            }
        }
        if self.signal.source_rate_hz == Some(0)
            || (self.signal.aec == AecOrigin::LiveShortLookahead
                && (self.signal.source != SignalSource::LiveStream
                    || self.signal.source_rate_hz.is_none()
                    || self.signal.lookahead_frames.is_none()))
            || (self.signal.source == SignalSource::ProviderText
                && self.signal.aec != AecOrigin::None)
        {
            return Err(Error::InvalidIdentity);
        }
        let stages: BTreeSet<_> = self.stages.iter().map(|stage| stage.id).collect();
        let mut rows = BTreeMap::new();
        for row in &self.rows {
            if !stages.contains(&row.id.invocation) || rows.insert(row.id, row).is_some() {
                return Err(Error::InvalidLink);
            }
            if !row.segment.start.is_finite()
                || !row.segment.end.is_finite()
                || row.segment.start < 0.0
                || row.segment.end < row.segment.start
            {
                return Err(Error::InvalidTiming);
            }
        }
        let retained_rows = RowLookup::new(retained);
        let lookup = |id: &RowId| rows.get(id).copied().or_else(|| retained_rows.get(*id));
        let mut used = BTreeSet::new();
        for (index, stage) in self.stages.iter().enumerate() {
            let mut protected = BTreeSet::new();
            for row in &stage.protected_output {
                if !stage.output.contains(row) || !protected.insert(*row) {
                    return Err(Error::InvalidLink);
                }
            }
            for context in &stage.context {
                let parent = retained
                    .iter()
                    .flat_map(|g| &g.stages)
                    .find(|parent| parent.id == context.parent)
                    .ok_or(Error::InvalidLink)?;
                if !parent.output.contains(&context.row) {
                    return Err(Error::InvalidLink);
                }
            }
            let (input, has_input): (std::borrow::Cow<'_, [RowId]>, bool) = if !stage
                .assembled_input
                .is_empty()
            {
                if index != 0
                    || stage.input.is_some()
                    || !matches!(
                        stage.kind,
                        StageKind::ProviderInput | StageKind::CleanupAssembly
                    )
                {
                    return Err(Error::InvalidLink);
                }
                let mut ids = Vec::new();
                let mut unique = BTreeSet::new();
                for source in &stage.assembled_input {
                    let parent = retained
                        .iter()
                        .flat_map(|g| &g.stages)
                        .find(|p| p.id == source.parent)
                        .ok_or(Error::InvalidLink)?;
                    if !parent.output.contains(&source.row) || !unique.insert(source.row) {
                        return Err(Error::InvalidLink);
                    }
                    if stage.kind == StageKind::CleanupAssembly
                        && parent.protected_output.contains(&source.row)
                        && !protected.contains(&source.row)
                    {
                        return Err(Error::InvalidLink);
                    }
                    ids.push(source.row);
                }
                (std::borrow::Cow::Owned(ids), true)
            } else {
                let previous = if index == 0 {
                    match stage.input {
                        Some(id) => Some(
                            retained
                                .iter()
                                .flat_map(|g| &g.stages)
                                .find(|p| p.id == id)
                                .ok_or(Error::InvalidLink)?,
                        ),
                        None => None,
                    }
                } else {
                    let previous = &self.stages[index - 1];
                    if stage.input != Some(previous.id) {
                        return Err(Error::InvalidLink);
                    }
                    Some(previous)
                };
                if stage.kind == StageKind::CleanupAssembly
                    && previous.is_some_and(|parent| {
                        parent
                            .protected_output
                            .iter()
                            .any(|row| !protected.contains(row))
                    })
                {
                    return Err(Error::InvalidLink);
                }
                (
                    std::borrow::Cow::Borrowed(previous.map_or(&[][..], |p| p.output.as_slice())),
                    previous.is_some(),
                )
            };
            // An assembly only orders retained owners. Its contract must also hold for direct
            // append and readback, independently of the higher-level recording adapter.
            if stage.kind == StageKind::CleanupAssembly
                && (stage.output != input.as_ref()
                    || rows.keys().any(|row| row.invocation == stage.id))
            {
                return Err(Error::InvalidLink);
            }
            if !has_input
                && matches!(
                    stage.kind,
                    StageKind::Cleanup { .. }
                        | StageKind::FinalProse
                        | StageKind::Segmentation
                        | StageKind::Diarization
                )
            {
                return Err(Error::InvalidLink);
            }
            if has_input
                && matches!(
                    stage.kind,
                    StageKind::TrueAsr | StageKind::FallbackAsr | StageKind::ContextSnapshot
                )
            {
                return Err(Error::InvalidLink);
            }
            if has_input
                && matches!(
                    stage.kind,
                    StageKind::ProviderInput | StageKind::CleanupAssembly
                )
                && stage.output != input.as_ref()
            {
                return Err(Error::InvalidLink);
            }
            let mut outputs = BTreeSet::new();
            for id in &stage.output {
                if lookup(id).is_none() || id.invocation > stage.id || !outputs.insert(*id) {
                    return Err(Error::InvalidLink);
                }
                if id.invocation != stage.id && !input.contains(id) {
                    return Err(Error::InvalidLink);
                }
                if rows.contains_key(id) {
                    used.insert(*id);
                }
            }
            if matches!(stage.kind, StageKind::Segmentation | StageKind::Diarization) {
                let parent = retained
                    .iter()
                    .flat_map(|g| &g.stages)
                    .chain(self.stages[..index].iter())
                    .find(|candidate| Some(candidate.id) == stage.input)
                    .ok_or(Error::InvalidLink)?;
                lineage::validate(stage, &input, &parent.protected_output, &lookup)?;
            } else if !stage.derivations.is_empty() {
                return Err(Error::InvalidLink);
            }
            if let StageKind::Cleanup { pass } = stage.kind {
                validate_cleanup(stage, pass, &input, &lookup)?;
                let parent = retained
                    .iter()
                    .flat_map(|g| &g.stages)
                    .chain(self.stages[..index].iter())
                    .find(|candidate| Some(candidate.id) == stage.input)
                    .ok_or(Error::InvalidLink)?;
                let expected: Vec<_> = stage
                    .output
                    .iter()
                    .filter(|row| {
                        parent.protected_output.contains(row)
                            || stage.decisions.iter().any(|decision| {
                                decision.replacements.contains(row)
                                    && (decision
                                        .originals
                                        .iter()
                                        .any(|id| parent.protected_output.contains(id))
                                        || decision.reason
                                            == CleanupReason::Echo(
                                                EchoVerdict::KeepCriticalDifference,
                                            ))
                            })
                    })
                    .copied()
                    .collect();
                if stage.protected_output != expected {
                    return Err(Error::InvalidLink);
                }
            } else if !stage.decisions.is_empty() {
                return Err(Error::InvalidLink);
            }
        }
        if used.len() != rows.len() {
            return Err(Error::InvalidLink);
        }
        Ok(())
    }
}

fn compact_vec<T>(items: &mut Vec<T>) {
    *items = std::mem::take(items).into_boxed_slice().into_vec();
}

fn compact_string(text: &mut String) {
    *text = std::mem::take(text).into_boxed_str().into_string();
}

fn vector_bytes<T>(items: &Vec<T>) -> usize {
    items.capacity().saturating_mul(std::mem::size_of::<T>())
}

fn quality_heap_bytes(quality: &QualityRecord) -> usize {
    vector_bytes(&quality.epochs).saturating_add(vector_bytes(&quality.spans))
}

fn same_metadata(a: &Row, b: &Row) -> bool {
    a.track == b.track
        && a.source_region_index == b.source_region_index
        && a.segment.speaker == b.segment.speaker
        && a.segment.start.to_bits() == b.segment.start.to_bits()
        && a.segment.end.to_bits() == b.segment.end.to_bits()
}

fn validate_cleanup<'a>(
    stage: &Stage,
    pass: CleanupStage,
    input: &[RowId],
    lookup: &impl Fn(&RowId) -> Option<&'a Row>,
) -> Result<(), Error> {
    let mut explained = BTreeSet::new();
    for d in &stage.decisions {
        let reason_matches = matches!(
            (pass, d.reason),
            (CleanupStage::Sort, CleanupReason::TimeOrder)
                | (
                    CleanupStage::Echo | CleanupStage::EarlyEcho,
                    CleanupReason::Echo(_)
                )
                | (CleanupStage::Merge, CleanupReason::SameSpeakerContinuation)
                | (
                    CleanupStage::Backchannel,
                    CleanupReason::OverlappingBackchannel
                )
                | (CleanupStage::Fillers, CleanupReason::FillersOrStutters)
                | (CleanupStage::Lexicon, CleanupReason::TextRule)
        );
        if !reason_matches
            || d.originals.is_empty()
            || d.originals.len() != d.input_indices.len()
            || d.replacements.iter().any(|id| !stage.output.contains(id))
        {
            return Err(Error::InvalidLink);
        }
        for (position, original) in d.input_indices.iter().zip(&d.originals) {
            if !explained.insert(*original) {
                return Err(Error::InvalidLink);
            }
            if pass == CleanupStage::EarlyEcho {
                if !input.contains(original)
                    || lookup(original)
                        .ok_or(Error::InvalidLink)?
                        .source_region_index
                        != Some(*position)
                {
                    return Err(Error::InvalidLink);
                }
            } else if input.get(usize::try_from(*position).map_err(|_| Error::InvalidLink)?)
                != Some(original)
            {
                return Err(Error::InvalidLink);
            }
        }
        if let Some(source) = &d.source
            && (!source.start.is_finite()
                || !source.end.is_finite()
                || source.start < 0.0
                || source.end < source.start)
        {
            return Err(Error::InvalidTiming);
        }
        if !matches!(pass, CleanupStage::Echo | CleanupStage::EarlyEcho)
            && (d.evidence.is_some() || d.source.is_some())
        {
            return Err(Error::InvalidLink);
        }
        match pass {
            CleanupStage::Sort => {
                if d.originals.len() != 1 || d.replacements != d.originals || !d.edits.is_empty() {
                    return Err(Error::InvalidLink);
                }
            }
            CleanupStage::Echo | CleanupStage::EarlyEcho => {
                if d.originals.len() != 1 || !d.edits.is_empty() {
                    return Err(Error::InvalidLink);
                }
                if d.reason == CleanupReason::Echo(EchoVerdict::DropAcoustic) {
                    if !d.replacements.is_empty()
                        || d.evidence.as_ref().is_none_or(|e| e.blocks == 0)
                    {
                        return Err(Error::InvalidLink);
                    }
                } else if d.replacements != d.originals {
                    return Err(Error::InvalidLink);
                }
            }
            CleanupStage::Backchannel => {
                if d.originals.len() != 1 || !d.replacements.is_empty() || !d.edits.is_empty() {
                    return Err(Error::InvalidLink);
                }
            }
            CleanupStage::Merge => validate_merge(stage.id, d, lookup)?,
            CleanupStage::Fillers | CleanupStage::Lexicon => {
                if d.originals.len() != 1 || d.replacements.len() > 1 {
                    return Err(Error::InvalidEdit);
                }
                let before = lookup(&d.originals[0]).ok_or(Error::InvalidLink)?;
                let after = match d.replacements.first() {
                    Some(id) => {
                        let row = lookup(id).ok_or(Error::InvalidLink)?;
                        if id.invocation != stage.id || !same_metadata(before, row) {
                            return Err(Error::InvalidLink);
                        }
                        row.segment.text.as_str()
                    }
                    None => "",
                };
                for edit in &d.edits {
                    let allowed = match pass {
                        CleanupStage::Lexicon => matches!(edit.action, EditKind::Lexicon { .. }),
                        CleanupStage::Fillers => matches!(
                            edit.action,
                            EditKind::Filler
                                | EditKind::Stutter
                                | EditKind::Case
                                | EditKind::Whitespace
                        ),
                        _ => false,
                    };
                    if !allowed {
                        return Err(Error::InvalidEdit);
                    }
                }
                validate_edits(&before.segment.text, after, &d.edits)?;
            }
        }
    }
    if pass == CleanupStage::Sort {
        let input_set: BTreeSet<_> = input.iter().collect();
        let output_set: BTreeSet<_> = stage.output.iter().collect();
        if input.len() != stage.output.len() || input_set != output_set {
            return Err(Error::InvalidLink);
        }
        for (position, id) in input.iter().enumerate() {
            if stage.output.get(position) != Some(id) && !explained.contains(id) {
                return Err(Error::InvalidLink);
            }
        }
        for pair in stage.output.windows(2) {
            let first = lookup(&pair[0]).ok_or(Error::InvalidLink)?;
            let next = lookup(&pair[1]).ok_or(Error::InvalidLink)?;
            if first.segment.start.total_cmp(&next.segment.start).is_gt() {
                return Err(Error::InvalidLink);
            }
            if first.segment.start.to_bits() == next.segment.start.to_bits()
                && input.iter().position(|id| *id == pair[0])
                    > input.iter().position(|id| *id == pair[1])
            {
                return Err(Error::InvalidLink);
            }
        }
    } else {
        // Every structural or textual change must be exactly the result of the linked decisions.
        // Unchanged input rows retain identity and order; each merge consumes one contiguous range.
        let (mut read, mut written) = (0, 0);
        while read < input.len() {
            if let Some(d) = stage
                .decisions
                .iter()
                .find(|d| d.originals.first() == Some(&input[read]))
            {
                if input.get(read..read + d.originals.len()) != Some(d.originals.as_slice())
                    || stage.output.get(written..written + d.replacements.len())
                        != Some(d.replacements.as_slice())
                {
                    return Err(Error::InvalidLink);
                }
                read += d.originals.len();
                written += d.replacements.len();
            } else {
                if stage.output.get(written) != Some(&input[read]) {
                    return Err(Error::InvalidLink);
                }
                read += 1;
                written += 1;
            }
        }
        if written != stage.output.len() {
            return Err(Error::InvalidLink);
        }
    }
    Ok(())
}

fn validate_merge<'a>(
    owner: InvocationId,
    d: &Decision,
    lookup: &impl Fn(&RowId) -> Option<&'a Row>,
) -> Result<(), Error> {
    if d.originals.len() < 2
        || d.replacements.len() != 1
        || !d.edits.is_empty()
        || d.replacements[0].invocation != owner
    {
        return Err(Error::InvalidLink);
    }
    let first = lookup(&d.originals[0]).ok_or(Error::InvalidLink)?;
    let last = lookup(d.originals.last().ok_or(Error::InvalidLink)?).ok_or(Error::InvalidLink)?;
    let out = lookup(&d.replacements[0]).ok_or(Error::InvalidLink)?;
    if out.segment.speaker != first.segment.speaker
        || out.track != first.track
        || out.source_region_index.is_some()
        || out.segment.start.to_bits() != first.segment.start.to_bits()
        || out.segment.end.to_bits() != last.segment.end.to_bits()
    {
        return Err(Error::InvalidLink);
    }
    let mut position = 0;
    for (index, id) in d.originals.iter().enumerate() {
        let source = lookup(id).ok_or(Error::InvalidLink)?;
        if source.segment.speaker != first.segment.speaker || source.track != first.track {
            return Err(Error::InvalidLink);
        }
        let mut text = source.segment.text.as_str();
        if index > 0 {
            text = text.trim_start();
        }
        if index + 1 < d.originals.len() {
            text = text.trim_end();
        }
        // An empty intermediate fragment's joining space is removed by the next trim_end.
        if index > 0 && index + 1 < d.originals.len() && text.is_empty() {
            continue;
        }
        if index > 0 {
            if out.segment.text.as_bytes().get(position) != Some(&b' ') {
                return Err(Error::InvalidEdit);
            }
            position += 1;
        }
        if out.segment.text.get(position..position + text.len()) != Some(text) {
            return Err(Error::InvalidEdit);
        }
        position += text.len();
    }
    if position != out.segment.text.len() {
        return Err(Error::InvalidEdit);
    }
    Ok(())
}

fn validate_edits(before: &str, after: &str, edits: &[TextEdit]) -> Result<(), Error> {
    let (mut left, mut right) = (0, 0);
    for edit in edits {
        let (bs, be, os, oe) = (
            edit.before.start as usize,
            edit.before.end as usize,
            edit.after.start as usize,
            edit.after.end as usize,
        );
        if bs < left
            || os < right
            || bs > be
            || os > oe
            || !before.is_char_boundary(bs)
            || !before.is_char_boundary(be)
            || !after.is_char_boundary(os)
            || !after.is_char_boundary(oe)
            || before.get(left..bs) != after.get(right..os)
        {
            return Err(Error::InvalidEdit);
        }
        if let EditKind::Lexicon { rule_id } = &edit.action
            && (rule_id.is_empty() || rule_id.len() > 128)
        {
            return Err(Error::InvalidEdit);
        }
        let before_edit = &before[bs..be];
        let after_edit = &after[os..oe];
        if matches!(edit.action, EditKind::Case)
            && !is_first_scalar_case_mapping(before_edit, after_edit)
        {
            return Err(Error::InvalidEdit);
        }
        if matches!(edit.action, EditKind::Whitespace)
            && (!before_edit.chars().all(char::is_whitespace)
                || !after_edit.chars().all(char::is_whitespace))
        {
            return Err(Error::InvalidEdit);
        }
        left = be;
        right = oe;
    }
    if before.get(left..) != after.get(right..) {
        return Err(Error::InvalidEdit);
    }
    Ok(())
}

// The existing filler pass applies upper_first/lower_first, including Unicode expansions.
// A trace may span just that scalar or include an unchanged suffix. It cannot substitute an
// arbitrary case-fold-equivalent string, recase the suffix, or cross invalid UTF-8 boundaries.
fn is_first_scalar_case_mapping(before: &str, after: &str) -> bool {
    let mut chars = before.chars();
    chars.next().is_some_and(|first| {
        first.to_uppercase().chain(chars.clone()).eq(after.chars())
            || first.to_lowercase().chain(chars).eq(after.chars())
    })
}

#[cfg(test)]
#[path = "evidence/tests.rs"]
mod tests;

#[path = "evidence/lineage.rs"]
mod lineage;
#[path = "evidence/record.rs"]
mod record;
pub use lineage::DerivedSegment;
pub use record::{
    CleanupInput, InputOrigin, MAX_CAPTURE_BYTES, MAX_CAPTURE_ROWS, RecordedCleanup,
    RecordedWindow, RecordingIdentity,
};
