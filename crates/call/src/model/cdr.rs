//! CDR event records appended to [`Call::cdr_events`](crate::model::Call) over
//! the call's life. Append helper: [`crate::helpers::add_cdr_event`].

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CdrEventType {
    InviteReceived,
    InviteSent,
    Provisional,
    Answer,
    Bye,
    Cancel,
    Timeout,
    Reject,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CdrEvent {
    #[serde(rename = "type")]
    pub event_type: CdrEventType,
    pub timestamp: i64,
    pub leg_id: String,
    pub status_code: Option<i64>,
    pub reason: Option<String>,
    /// The count of decisions applied to the call when the event was
    /// written (`Call::decision_ordinal` at the append); `0` before the
    /// first decision.
    #[serde(default)]
    pub decision_ordinal: u32,
}
