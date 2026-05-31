//! CDR (call detail record) writing — port of `CdrWriter.ts`. One record is
//! written per call at termination, carrying the accumulated `Call.cdr_events`.
//! The in-memory writer ([`InMemoryCdrWriter`]) lets tests assert exactly one
//! CDR per call; [`BufferedCdrWriter`] is the production drop-on-overload buffer.

mod buffered;
mod memory;

pub use buffered::BufferedCdrWriter;
pub use memory::InMemoryCdrWriter;

use async_trait::async_trait;
use serde::Serialize;

use call::{Call, CdrEvent, LegDisposition, LegState};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CdrLeg {
    pub call_id: String,
    pub from_tag: String,
    pub state: LegState,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CdrBLeg {
    pub leg_id: String,
    pub call_id: String,
    pub state: LegState,
    pub disposition: LegDisposition,
}

/// One completed call's record.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CdrRecord {
    pub call_ref: String,
    pub created_at: i64,
    pub terminated_at: i64,
    pub a_leg: CdrLeg,
    pub b_legs: Vec<CdrBLeg>,
    pub events: Vec<CdrEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub billing_context: Option<String>,
}

/// Build a [`CdrRecord`] from a terminated call.
pub fn build_record(call: &Call, terminated_at: i64) -> CdrRecord {
    CdrRecord {
        call_ref: call.call_ref.clone(),
        created_at: call.created_at,
        terminated_at,
        a_leg: CdrLeg {
            call_id: call.a_leg.call_id.clone(),
            from_tag: call.a_leg.from_tag.clone(),
            state: call.a_leg.state,
        },
        b_legs: call
            .b_legs
            .iter()
            .map(|l| CdrBLeg {
                leg_id: l.leg_id.clone(),
                call_id: l.call_id.clone(),
                state: l.state,
                disposition: l.disposition,
            })
            .collect(),
        events: call.cdr_events.clone(),
        billing_context: call.billing_context.clone(),
    }
}

/// The CDR sink. `write` is non-blocking-fast; `read_all` is the test accessor.
#[async_trait]
pub trait CdrWriter: Send + Sync {
    async fn write(&self, call: &Call, terminated_at: i64);
    async fn read_all(&self) -> Vec<CdrRecord>;
}
