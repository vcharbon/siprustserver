//! `RecordedAnomaly` + `Severity` — the cross-layer finding ledger.
//!
//! In the TS source `RecordedAnomaly` is a wide discriminated union with one
//! arm per layer (`signalingAudit`, `queueLeak`, `codecParity`, …). A closed
//! Rust enum would force every layer to edit this crate to add an arm, which
//! defeats "shared foundation imported by most layers". So the Rust shape is a
//! **flat struct** with a `&'static str` `kind` discriminant the layer owns,
//! plus the fields every variant shared (`check`, `detail`, `severity`,
//! `bind_key`). A layer encodes any extra fields it needs (queue depth,
//! in-flight count) into `detail`.

use crate::scenario::LaneKey;

/// Three-tier severity (ADR-0013 D5).
///
///   - [`Severity::Fatal`]        — the decorator fails the call *now*
///     (`unit-test-of-layer` hot path).
///   - [`Severity::DeferredFail`] — recorded; the layer-close finalizer turns
///     a non-empty deferred set into a failure (`test-with-recorder`).
///   - [`Severity::Advisory`]     — recorded silently, never fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Severity {
    Fatal,
    DeferredFail,
    Advisory,
}

impl Severity {
    /// Whether a finding at this severity should fail the surrounding test.
    /// `Advisory` never does; `Fatal`/`DeferredFail` do (at different times).
    pub fn fails(self) -> bool {
        !matches!(self, Severity::Advisory)
    }
}

/// One finding on the shared ledger. `kind` is the layer-owned discriminant
/// (e.g. `"signalingAudit"`, `"queueLeak"`); `check` is the rule/invariant id
/// (e.g. `"rfc.viaBranch"`, `"A1_inFlightImbalance"`); `detail` is the
/// human-readable explanation. `bind_key` ties the finding to a peer lane when
/// one applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedAnomaly {
    pub kind: &'static str,
    pub check: String,
    pub detail: String,
    pub bind_key: Option<LaneKey>,
    pub severity: Severity,
    /// Capture-order tiebreaker from the shared [`crate::EventSequencer`].
    pub seq: u64,
    /// Report-only timestamp (see [`crate::time`]).
    pub at_ms: u64,
}

impl RecordedAnomaly {
    /// Construct a finding. Callers usually go through a layer-specific
    /// helper that fills `kind`/`check`; this is the low-level constructor.
    pub fn new(
        kind: &'static str,
        check: impl Into<String>,
        detail: impl Into<String>,
        severity: Severity,
        bind_key: Option<LaneKey>,
        seq: u64,
        at_ms: u64,
    ) -> Self {
        Self {
            kind,
            check: check.into(),
            detail: detail.into(),
            bind_key,
            severity,
            seq,
            at_ms,
        }
    }
}
