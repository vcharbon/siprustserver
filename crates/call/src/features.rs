//! Canonical `FeatureActivations` — port of
//! `src/decision/schemas/features.ts` (the closed union from
//! SplitServiceLogic.md §D5).
//!
//! Embedded in the `call` crate for now: `Call.features` carries it, so the
//! data model needs the type to round-trip. The decision/rules layer will own
//! the canonical version once it is ported — until a cross-layer cycle forces a
//! shared crate, keeping it here respects ADR-0002 ("no premature shared types
//! crate").
//!
//! Source semantics preserved: `platform` is mandatory; every feature arm is
//! optional, and **absence means "explicitly disabled," not "default enabled"**
//! (the policy guard keys on presence). Optional → `Option<T>`.

use serde::{Deserialize, Serialize};

/// Platform-mandatory keepalive activation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeepaliveActivation {
    /// Seconds between OPTIONS pokes (or whatever keepalive mechanism is used).
    pub interval_sec: i64,
    /// Tear down the leg after this many unanswered keepalives.
    pub max_missed: i64,
}

/// Platform-mandatory cap + keepalive.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformActivations {
    /// Overall call ceiling (seconds). Adapter supplies; platform caps it.
    pub max_duration_sec: i64,
    pub keepalive: KeepaliveActivation,
}

/// Optional REFER feature arm.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferFeature {
    /// Caps REFER chain depth across attended transfers. `None` → unlimited.
    pub max_chain_depth: Option<i64>,
}

/// `relayFirst18xTo180` strategy — single-variant (mutually exclusive).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RelayFirst18xStrategy {
    DropSdp,
    KeepSdp,
    FakePrack,
    PromotePemTo200,
}

/// Optional `relayFirst18xTo180` feature arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayFirst18xTo180Feature {
    pub strategy: RelayFirst18xStrategy,
}

/// One entry in the optional `callLimiters` feature arm.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallLimiterFeatureEntry {
    pub id: String,
    pub limit: i64,
}

/// Closed feature-activation union: mandatory `platform` + optional arms.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureActivations {
    pub platform: PlatformActivations,
    pub refer: Option<ReferFeature>,
    pub relay_first_18x_to_180: Option<RelayFirst18xTo180Feature>,
    pub no_answer_timeout_sec: Option<i64>,
    pub call_limiters: Option<Vec<CallLimiterFeatureEntry>>,
}
