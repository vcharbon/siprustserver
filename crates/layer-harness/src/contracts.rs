//! The reusable vocabulary of the four-wrapper pattern (the
//! `effect-layer-test` SKILL, ADR-0013).
//!
//! The wrappers themselves are per-service **decorator structs** (they live in
//! each layer crate's `contracts` module, because the wrapped trait differs).
//! What every layer shares — and what this module standardizes — is:
//!
//!   - the **rule shapes** each wrapper evaluates ([`PropertyRule`],
//!     [`ParanoidCheck`], [`AuditRule`]);
//!   - the **failure shapes** they raise ([`PropertyViolation`],
//!     [`ParanoidViolation`]);
//!   - the **composition order** they must compose in ([`COMPOSITION_ORDER`]).
//!
//! Severity routing for findings is on [`crate::RunContext`]; the cross-call
//! finding ledger is [`crate::RecordedAnomaly`]. Parity (blue-vs-green) has no
//! rule trait here — it is a direct deep-equal of two impls' outputs, written
//! inline where two deterministic impls exist.

use std::fmt;

use crate::recorder::Stamped;

/// The fixed order the wrappers compose in (outermost → innermost), copied
/// verbatim from `withCanonicalContracts`. A decorator stack must read
/// `PropertyTest(ParanoidInputs(ScopedAudit(impl)))` so preconditions are
/// checked before contract properties and recording sits closest to the impl.
pub const COMPOSITION_ORDER: &str = "propertyTest(paranoidInputs(scopedAudit(impl)))";

/// A CONTRACT property asserted on every call: given the call's `input` and
/// `output`, return `Ok` or a failure reason. Drives a `propertyTest`
/// decorator. (`O` is often `Result<_, _>` so a property can inspect failures.)
pub trait PropertyRule<I, O>: Send + Sync {
    /// Stable id, e.g. `"P1_roundTrips"`.
    fn id(&self) -> &'static str;
    fn check(&self, input: &I, output: &O) -> Result<(), String>;
}

/// A caller-side precondition on an input — what the caller must respect.
/// Drives a `paranoidInputs` decorator. A failure is a *programmer error*
/// (defect), surfaced via [`ParanoidViolation`], not a recoverable `Err`.
pub trait ParanoidCheck<I>: Send + Sync {
    /// Stable id, e.g. `"PA3_send_validDest"`.
    fn id(&self) -> &'static str;
    fn check(&self, input: &I) -> Result<(), String>;
}

/// A cross-call invariant evaluated over the recorded event log at scope
/// close. Drives a `scopedAudit` decorator. Returns zero or more violation
/// detail strings. `force_advisory` pins the rule to the advisory tier
/// regardless of `RunContext` (the source's `severityOverride: "advisory"`).
pub trait AuditRule<E>: Send + Sync {
    fn name(&self) -> &'static str;
    fn force_advisory(&self) -> bool {
        false
    }
    fn check(&self, events: &[Stamped<E>]) -> Vec<String>;
}

/// A failed CONTRACT property. For a service whose methods return `Result`, a
/// decorator surfaces this in the error channel; for an infallible method it
/// is a defect. Carries the property id for caller-side dispatch (mirrors the
/// source's `_tag` field).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyViolation {
    pub property_id: &'static str,
    pub detail: String,
}

impl fmt::Display for PropertyViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.property_id, self.detail)
    }
}

impl std::error::Error for PropertyViolation {}

/// A violated caller-side precondition — a programmer error. Decorators raise
/// it by panicking (the Rust analogue of the source's `Effect.die`); tests
/// exercising the precondition surface catch it with `#[should_panic]` /
/// `catch_unwind`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParanoidViolation {
    pub check: &'static str,
    pub detail: String,
}

impl ParanoidViolation {
    pub fn new(check: &'static str, detail: impl Into<String>) -> Self {
        Self {
            check,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for ParanoidViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "paranoid precondition {}: {}", self.check, self.detail)
    }
}

impl std::error::Error for ParanoidViolation {}
