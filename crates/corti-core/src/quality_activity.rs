//! Fixed, content-free activity observations in the existing quality record.
//!
//! A positive observation is historical evidence, not continuous coverage. Only one complete
//! traversal can supply an extent proof; merging partial snapshots never creates one. A complete
//! retry can supply its own proof while prior uncertainty remains visible. Input IDs
//! must identify the same immutable pre-AEC signal and availability history across retries. They
//! are opaque, bounded caller bindings, not filenames, transcript content, or storage authority.

use serde::{Deserialize, Serialize};

use crate::{QualityError, QualityStage};

pub(crate) const MAX_ACTIVITY_RATE_HZ: u32 = 192_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityScope {
    Capture,
    Offline,
    Live,
}

impl ActivityScope {
    pub(crate) fn stage(self) -> QualityStage {
        match self {
            Self::Capture => QualityStage::Capture,
            Self::Offline => QualityStage::Batch,
            Self::Live => QualityStage::Live,
        }
    }
}

/// Stable identity of the actual pre-AEC input, including its layout/availability interpretation.
/// A cleaned redo, another recording, or a changed interpretation must have a different identity.
/// The retained rate is fixed for this input even when its source epochs change rates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ActivityInput {
    identity: [u8; 32],
    retained_rate_hz: u32,
}

impl ActivityInput {
    pub fn new(identity: [u8; 32], retained_rate_hz: u32) -> Result<Self, QualityError> {
        let input = Self {
            identity,
            retained_rate_hz,
        };
        input.validate()?;
        Ok(input)
    }

    pub fn retained_rate_hz(self) -> u32 {
        self.retained_rate_hz
    }

    pub(crate) fn validate(self) -> Result<(), QualityError> {
        if self.identity == [0; 32]
            || self.retained_rate_hz == 0
            || self.retained_rate_hz > MAX_ACTIVITY_RATE_HZ
        {
            Err(QualityError::InvalidActivity)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FarReference {
    Unknown,
    Present,
    Intermittent,
    Absent,
}

/// Evidence for a single full traversal [0, end_frame) of its containing scope's exact input/rate.
/// Private construction prevents callers from promoting a union of partial runs to completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct ActivityExtent {
    pub(crate) end_frame: u64,
    pub(crate) all_zero: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityEvidence {
    pub(crate) input: Option<ActivityInput>,
    pub(crate) observed_tap: bool,
    pub(crate) nonzero_tap: bool,
    pub(crate) sustained_zero_tap: bool,
    pub(crate) absent_tap_layout: bool,
    pub(crate) suspected: bool,
    pub(crate) uncertain: bool,
    pub(crate) binding_conflict: bool,
    pub(crate) proof_conflict: bool,
    /// Conservative lower bound for this scope, max-merged across retries, never a unique-run total.
    #[serde(default)]
    pub(crate) omitted_runs: u64,
    pub(crate) complete: Option<ActivityExtent>,
}

impl ActivityEvidence {
    pub fn input(&self) -> Option<ActivityInput> {
        self.input
    }

    /// Input authority for a new traversal. Historical uncertainty alone does not invalidate the
    /// binding, but a conflicting identity/rate/extent or contradictory complete proof does.
    pub fn unambiguous_input(&self) -> Option<ActivityInput> {
        (!self.binding_conflict && !self.proof_conflict)
            .then_some(self.input)
            .flatten()
    }

    pub fn suspected(&self) -> bool {
        self.suspected
    }

    pub fn omitted_runs(&self) -> u64 {
        self.omitted_runs
    }

    /// Historical uncertainty is sticky even after a later complete traversal.
    pub fn uncertain(&self) -> bool {
        self.uncertain || self.binding_conflict || self.proof_conflict
    }

    /// Independently complete traversal, despite any older observation uncertainty. Conflicting
    /// input/rate/end bindings or a complete-zero/nonzero contradiction invalidate this projection.
    pub fn complete_end_frame(&self) -> Option<u64> {
        (!self.binding_conflict && !self.proof_conflict)
            .then_some(self.complete)
            .flatten()
            .map(|extent| extent.end_frame)
    }

    pub fn far_reference(&self) -> FarReference {
        if self.binding_conflict || self.proof_conflict {
            FarReference::Unknown
        } else if self.nonzero_tap {
            if self.sustained_zero_tap || self.absent_tap_layout {
                FarReference::Intermittent
            } else {
                FarReference::Present
            }
        } else if self.complete.is_some() {
            FarReference::Absent
        } else {
            FarReference::Unknown
        }
    }

    pub(crate) fn validate(&self) -> Result<(), QualityError> {
        let invalid = || Err(QualityError::InvalidActivity);
        let Some(input) = self.input else {
            return if *self == Self::default() {
                Ok(())
            } else {
                invalid()
            };
        };
        input.validate()?;
        if (self.nonzero_tap || self.sustained_zero_tap) && !self.observed_tap
            || self.suspected && !self.sustained_zero_tap
        {
            return invalid();
        }
        if let Some(extent) = self.complete
            && (extent.end_frame == 0
                || (!self.observed_tap && !self.absent_tap_layout)
                || (extent.all_zero == self.nonzero_tap && !self.proof_conflict))
        {
            return invalid();
        }
        Ok(())
    }

    pub(crate) fn merge(&mut self, other: Self) {
        match (self.input, other.input) {
            (None, input) => self.input = input,
            (Some(a), Some(b)) if a != b => {
                self.input = Some(a.min(b));
                self.binding_conflict = true;
            }
            _ => {}
        }
        match (self.complete, other.complete) {
            (None, proof) => self.complete = proof,
            (Some(a), Some(b)) if a != b => {
                self.complete = Some(a.min(b));
                self.proof_conflict = true;
            }
            _ => {}
        }
        self.observed_tap |= other.observed_tap;
        self.nonzero_tap |= other.nonzero_tap;
        self.sustained_zero_tap |= other.sustained_zero_tap;
        self.absent_tap_layout |= other.absent_tap_layout;
        self.suspected |= other.suspected;
        self.uncertain |= other.uncertain;
        self.binding_conflict |= other.binding_conflict;
        self.omitted_runs = self.omitted_runs.max(other.omitted_runs);
        self.proof_conflict |= other.proof_conflict
            || self
                .complete
                .is_some_and(|proof| proof.all_zero && self.nonzero_tap);
    }
}

/// Three fixed slots. There is no per-attempt vector, audio, level history, or second quality ledger.
/// Scoped omitted-run counts are conservative maxima over comparable retries. Conflicting inputs
/// retain the conflict and lower bounds, not an exact unique-run total or per-input identity list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualityActivity {
    #[serde(default)]
    pub(crate) omission_counter_saturated: bool,
    #[serde(default)]
    capture: ActivityEvidence,
    #[serde(default)]
    offline: ActivityEvidence,
    #[serde(default)]
    live: ActivityEvidence,
}

impl QualityActivity {
    /// Degraded activity omissions saturated without changing source-loss accounting authority.
    pub fn omissions_saturated(&self) -> bool {
        self.omission_counter_saturated
    }

    pub fn get(&self, scope: ActivityScope) -> &ActivityEvidence {
        match scope {
            ActivityScope::Capture => &self.capture,
            ActivityScope::Offline => &self.offline,
            ActivityScope::Live => &self.live,
        }
    }

    pub(crate) fn observe(&mut self, scope: ActivityScope, evidence: ActivityEvidence) {
        match scope {
            ActivityScope::Capture => &mut self.capture,
            ActivityScope::Offline => &mut self.offline,
            ActivityScope::Live => &mut self.live,
        }
        .merge(evidence);
    }

    pub(crate) fn validate(&self) -> Result<(), QualityError> {
        self.capture.validate()?;
        self.offline.validate()?;
        self.live.validate()
    }

    pub(crate) fn merge(&mut self, other: Self) {
        self.omission_counter_saturated |= other.omission_counter_saturated;
        self.capture.merge(other.capture);
        self.offline.merge(other.offline);
        self.live.merge(other.live);
    }
}
