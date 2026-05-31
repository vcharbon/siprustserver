//! `RunContext` — the three-tier severity driver (ADR-0013 D5, port of
//! `src/test-harness/framework/RunContext.ts`).
//!
//! Each contract decorator consults the active [`RunContext`] to decide how
//! loudly a rule finding should surface:
//!
//!   - [`RunContext::RealRun`]          → decorators are off, rules don't fire.
//!   - [`RunContext::TestWithRecorder`] → all rules fire; `deferred-fail` for
//!     layer-scope invariants, `fatal` for hot-path contract violations,
//!     `advisory` for advisory checks.
//!   - [`RunContext::UnitTestOfLayer`]  → rules targeting `tag` get promoted
//!     to `fatal`; others stay `advisory`.
//!
//! Where the TS source reads `RunContext` from the Effect service map (with a
//! `real-run` fallback when absent), the Rust decorators take a `RunContext`
//! value by construction. There is no implicit fallback to fall into in a
//! test — the absence is a compile error, which is stricter and better.

use crate::anomaly::Severity;

/// The `tag` identity a [`RunContext::UnitTestOfLayer`] targets. In the source
/// this is an Effect `ServiceMap.Key`; here a layer's stable channel key (the
/// same `&'static str` passed to [`crate::Recorder::for_tag`]) is enough to
/// decide "is this rule's layer the one under test".
pub type LayerTag = &'static str;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunContext {
    /// Production. No rule fires.
    RealRun,
    /// The normal fake-stack test mode. Requires a `Recorder` in scope.
    TestWithRecorder,
    /// Isolating a single wrapped layer under property-based tests: rules
    /// whose layer is `tag` are promoted to `fatal`; others stay advisory.
    UnitTestOfLayer { tag: LayerTag },
}

impl RunContext {
    /// Resolve the severity for a finding raised by the layer identified by
    /// `this_tag`, given an optional per-rule `advisory` override.
    ///
    /// Mirrors the dispatch baked into `SignalingNetwork.contracts.ts`:
    ///
    /// | context | severity |
    /// |---|---|
    /// | `RealRun` | `Advisory` (never fails) |
    /// | `TestWithRecorder` | `DeferredFail` |
    /// | `UnitTestOfLayer{tag}` where `tag == this_tag` | `Fatal` |
    /// | `UnitTestOfLayer{tag}` where `tag != this_tag` | `Advisory` |
    ///
    /// A rule that sets `force_advisory` (the `severityOverride: "advisory"`
    /// of the source) collapses to `Advisory` in every non-fatal context.
    pub fn severity_for(self, this_tag: LayerTag, force_advisory: bool) -> Severity {
        match self {
            RunContext::RealRun => Severity::Advisory,
            RunContext::UnitTestOfLayer { tag } if tag == this_tag => Severity::Fatal,
            RunContext::UnitTestOfLayer { .. } => Severity::Advisory,
            RunContext::TestWithRecorder => {
                if force_advisory {
                    Severity::Advisory
                } else {
                    Severity::DeferredFail
                }
            }
        }
    }

    /// `true` when contract rules should run at all. `RealRun` short-circuits
    /// every decorator so production paths pay nothing.
    pub fn rules_enabled(self) -> bool {
        !matches!(self, RunContext::RealRun)
    }
}
