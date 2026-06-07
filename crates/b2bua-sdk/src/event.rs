//! The unified event the router/handlers process — port of `SipRouter.ts`
//! `CallEvent`. A single discriminated type so handlers narrow on one input.

use std::net::SocketAddr;

use call::TimerType;
use sip_message::SipMessage;
use sip_txn::TransactionEvent;

/// One thing that happens to a call: an inbound SIP message, a fired timer, a
/// CANCEL the txn layer already answered, a client-transaction timeout, or a
/// re-entrant internal event (e.g. an async decision result).
#[derive(Debug, Clone)]
pub enum CallEvent {
    /// A SIP request/response that survived the transaction layer.
    Sip {
        message: Box<SipMessage>,
        src: SocketAddr,
    },
    /// A B2BUA timer fired (keepalive, no-answer, max-duration, …).
    Timer {
        timer_type: TimerType,
        call_ref: String,
        leg_id: Option<String>,
    },
    /// A CANCEL matched a server INVITE txn; 200/487 already sent downstream.
    Cancelled { call_id: String, from_tag: String },
    /// A client transaction (b-leg INVITE, BYE, …) timed out with no final.
    Timeout {
        branch: String,
        call_ref: Option<String>,
        leg_id: Option<String>,
        method: Option<String>,
    },
    /// Re-entrant internal event (async result folded back into the call).
    InternalEvent {
        call_ref: String,
        topic: String,
        outcome: String,
        payload: serde_json::Value,
    },
    /// The transaction layer reports the last transaction for a *watched* call has
    /// cleared (ADR-0014). The router uses it to self-release an acting-backup
    /// takeover copy whose served transaction(s) are done.
    CallQuiesced { call_ref: String },
}

impl CallEvent {
    /// Map a transaction-layer event into a `CallEvent`. Pure.
    pub fn from_txn(event: TransactionEvent) -> Self {
        match event {
            TransactionEvent::Message { message, src } => CallEvent::Sip { message, src },
            TransactionEvent::Cancelled { call_id, from_tag } => {
                CallEvent::Cancelled { call_id, from_tag }
            }
            TransactionEvent::Timeout {
                branch,
                call_ref,
                leg_id,
                method,
            } => CallEvent::Timeout {
                branch,
                call_ref,
                leg_id,
                method,
            },
            TransactionEvent::CallQuiesced { call_ref } => CallEvent::CallQuiesced { call_ref },
        }
    }

    /// Short discriminator for logs / reports.
    pub fn kind(&self) -> &'static str {
        match self {
            CallEvent::Sip { .. } => "sip",
            CallEvent::Timer { .. } => "timer",
            CallEvent::Cancelled { .. } => "cancelled",
            CallEvent::Timeout { .. } => "timeout",
            CallEvent::InternalEvent { .. } => "internal-event",
            CallEvent::CallQuiesced { .. } => "call-quiesced",
        }
    }
}
