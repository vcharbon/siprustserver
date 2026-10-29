//! A scenario's message ladder: every arrow its calls exchanged with the SUT,
//! merged over the calls, with the retransmissions and re-sends of each and
//! the failures detected at it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One scenario's ladder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Ladder {
    pub scenario: String,
    /// Calls folded in.
    pub calls: u64,
    /// Some call of the scenario ran the retransmit engine (re-sends are
    /// counted only then).
    pub engine: bool,
    /// The lanes, in display order: the caller, the SUT, the callee roles.
    pub lanes: Vec<String>,
    /// The arrows, by mean offset from call start (bounded).
    pub arrows: Vec<LadderArrow>,
    /// Every arrow past the bound, merged (`from`, `to` and `label` empty).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<LadderArrow>,
    /// Failures judged after the call, on no arrow (`rfc_audit_fail`,
    /// `check_fail`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after_call: Vec<ClassCount>,
    /// Failures decided before any message came in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub before_reply: Vec<ClassCount>,
}

/// One arrow of a ladder, merged over the calls.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LadderArrow {
    pub from: String,
    pub to: String,
    /// `INVITE`, or the status and its CSeq method: `180 (INVITE)`.
    pub label: String,
    /// Which distinct occurrence of `(from, to, label)` in its call (from 1).
    pub nth: u32,
    /// Calls that carried it.
    pub count: u64,
    /// Copies received after the first.
    pub retransmissions: u64,
    /// Copies sent after the first.
    pub resent: u64,
    /// Mean offset of its first copy from call start (ms).
    pub mean_offset_ms: f64,
    /// Failed calls whose failure was detected here: the last message in
    /// before the outcome.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ClassCount>,
}

/// A result class and how many calls ended in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClassCount {
    pub class: String,
    pub count: u64,
}
