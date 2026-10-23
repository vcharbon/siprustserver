//! The verdict's statement of an abandoned script and the generic close that
//! ended its call (§11.2).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What the generic close put on the wire for one leg (§11.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CloseOwed {
    /// A final response to a request the leg took and never answered.
    Answer,
    /// The ACK a final it took is owed (RFC 3261 §13.2.2.4, §17.1.1.3).
    Ack,
    /// The BYE that ends a dialog the leg opened (RFC 3261 §15).
    Bye,
    /// The CANCEL for an INVITE it sent that has no final (RFC 3261 §9.1).
    Cancel,
}

/// One act of the generic close: what a scripted leg emitted to end what it
/// still held once its script had ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct CloseAct {
    pub leg: String,
    /// The obligation it discharged.
    pub owed: CloseOwed,
    /// The start line of the message it put on the wire. The datagram itself is
    /// in the recording, on this leg, noted as the close's.
    pub sent: String,
}

/// A script a run that COULD NOT GO ON ended, and the generic close that
/// terminated its call instead (§11.2). Polarity-free: a positive run that
/// cannot go on is abandoned and closed exactly like a negative one, and its
/// verdict still fails.
///
/// The abandonment is stated rather than hidden: `completed_steps` stays the
/// truth about what ran, this is the truth about what did not, and the acts are
/// what ended the call in the script's place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Abandoned {
    /// The leg the aborting delta landed on, where the abort named one. A run
    /// stopped by something legless — a stall, a closed transport, a flow with
    /// nothing left to wake — abandons with no leg.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg: Option<String>,
    /// The flow step it named, where it named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    /// The flow nodes the script never ran.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<String>,
    /// What the close emitted, in the order it emitted it. Empty where every
    /// scripted leg already held nothing open.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub closed: Vec<CloseAct>,
}
