//! The pivot v3 document (`PCAP2TEST_PIVOT_V3.md` §2): the top level and the
//! budgets stated once.
//!
//! Design rule, and the one that decides every field: **smart compiler, dumb
//! interpreter.** A corner case is compiled into explicit fields by the
//! generator, never inferred at replay time. There is deliberately no control
//! flow in the document — no loops, no conditionals, no jumps; the declared
//! alternatives of an `alt` are the only branching — and `deny_unknown_fields`
//! on every object, so a misspelling fails loudly rather than silently changing
//! a replay's meaning.
//!
//! **Emptiness is uniform**: every optional collection is OMITTED when empty.
//! Never `[]`, never `{}`, never `null`. No field's emptiness carries meaning.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::call::Call;
use crate::case::Case;
use crate::deviation::Deviation;
use crate::flow::FlowNode;
use crate::identity::Identity;
use crate::must_fail::MustFail;
use crate::placement::{Actor, Endpoint, Leg};
use crate::postcondition::Postconditions;
use crate::violation::RfcViolation;

/// The `pivot_version` this crate models.
pub const PIVOT_VERSION: u32 = 3;

/// One replayable case: a captured call reproduced, or an authored test.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PivotV3 {
    /// Always `3`. Exported as a `const`, so a mirror validating
    /// against the schema refuses an earlier version rather than accepting a
    /// document this crate would then reject.
    #[schemars(extend("const" = PIVOT_VERSION))]
    pub pivot_version: u32,
    /// Identity, provenance, replayability, informative annotations.
    pub case: Case,
    /// Every number and domain the document names, declared once. Attempts,
    /// actors and `${num:…}` accessors reference an entry by name; the driver
    /// binds each name to a real number per lane.
    pub identities: Vec<Identity>,
    /// The calls the document plays. A captured case is one call; a
    /// concurrency test is several whose flows interleave.
    pub calls: Vec<Call>,
    /// Sockets the lane must bind.
    pub endpoints: Vec<Endpoint>,
    /// Simulated elements on those sockets.
    pub actors: Vec<Actor>,
    /// Symbolic dialogs.
    pub legs: Vec<Leg>,
    /// The choreography: message steps, injected events and declared
    /// alternatives, in order.
    pub flow: Vec<FlowNode>,
    /// Peer non-compliance the replay must reproduce.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deviations: Vec<Deviation>,
    /// RFC rules a message this flow already carries breaks. Behavioural, so
    /// the replay reproduces one by running the flow unchanged; `deviations`
    /// covers the ones that change an emission.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rfc_violations: Vec<RfcViolation>,
    /// The failures this run MUST produce. A document that declares any is a
    /// NEGATIVE case: it replays a source whose non-compliance this platform
    /// does not share, so the run cannot pass by behaving well, and the
    /// document says in advance exactly how it fails.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub must_fail: Vec<MustFail>,
    /// What must hold once the flow has run and everything has settled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postconditions: Option<Postconditions>,
    /// RESERVED (§12): the deployment-extensible media vocabulary. Nothing
    /// reads it yet; it exists so the vocabulary lands additively rather than
    /// as a version bump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<BTreeMap<String, Value>>,
    /// Budgets stated once.
    pub timing: Timing,
}

impl PivotV3 {
    /// Parse a pivot document from its text.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// Serialize canonically (§2.1): keys sorted, two-space indent, one
    /// trailing newline.
    pub fn to_canonical_json(&self) -> String {
        crate::canonical::format(self).expect("a pivot document serializes")
    }

    /// Whether the document declares the version this crate models.
    pub fn version_matches(&self) -> bool {
        self.pivot_version == PIVOT_VERSION
    }

    /// Every message step in the document, `alt` branches and `unordered`
    /// groups included, in document order. The one traversal lint, formatters
    /// and interpreters share, so a construct added to [`FlowNode`] cannot be
    /// missed by one of them.
    pub fn steps(&self) -> Vec<&crate::flow::Step> {
        self.flow.iter().flat_map(FlowNode::steps).collect()
    }

    /// The wall time this document's timeline declares, in milliseconds: what a
    /// real clock spends REPRODUCING it, before the run's own overhead.
    ///
    /// The capture's own span where one was measured, otherwise the sum of every
    /// step's declared dwell — an upper bound, since each dwell lies on at most
    /// one chain of anchors.
    pub fn declared_span_ms(&self) -> u64 {
        self.timing
            .capture_span_ms
            .unwrap_or_else(|| self.steps().iter().map(|step| step.delay.ms).sum())
    }
}

/// Budgets stated once for the whole case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Timing {
    /// Default assertion timeout for every `expect`; a step's `within_ms`
    /// overrides it per step.
    pub expect_budget_ms: u64,
    /// The ceiling on the settle phase (§10): how long the runner waits for
    /// every scripted dialog to reach a terminal state and for the system's
    /// own call count to fall to zero. Failing to settle inside it is test
    /// failure — there is no soft mode.
    pub settle_budget_ms: u64,
    /// The CASE's span — the last flow step's `observed.at_us` in
    /// milliseconds. Captured documents only: an authored test has no capture
    /// to span.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_span_ms: Option<u64>,
}
