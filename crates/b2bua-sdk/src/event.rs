//! The unified event the router/handlers process — port of `SipRouter.ts`
//! `CallEvent`. A single discriminated type so handlers narrow on one input.

use std::net::SocketAddr;

use call::TimerType;
use sip_message::{SipHeader, SipMessage};
use sip_txn::{TimeoutKind, TransactionEvent};

/// One thing that happens to a call: an inbound SIP message, a fired timer, a
/// CANCEL the txn layer already answered, a client-transaction timeout, or a
/// re-entrant internal event (e.g. an async decision result).
#[derive(Debug, Clone)]
pub enum CallEvent {
    /// A SIP request/response that survived the transaction layer.
    Sip {
        message: Box<SipMessage>,
        src: SocketAddr,
        /// A response: whether it answered a client transaction this node's
        /// txn layer holds — so a non-2xx INVITE final it carries is already
        /// ACKed hop-by-hop (RFC 3261 §17.1.1.3). `false` for a response to a
        /// request another node sent (a call taken over from it), and always
        /// for a request.
        matched_client_txn: bool,
    },
    /// A B2BUA timer fired (keepalive, no-answer, max-duration, …).
    Timer {
        timer_type: TimerType,
        call_ref: String,
        leg_id: Option<String>,
    },
    /// A CANCEL matched a server INVITE txn; 200/487 already sent downstream.
    /// RFC 3261 §9 scopes a CANCEL to the one INVITE *transaction* it matched:
    /// `invite_cseq` is that INVITE's CSeq number, `in_dialog` whether it was an
    /// in-dialog re-INVITE (its `To` carried a tag). The rule layer uses these
    /// to end just the targeted renegotiation instead of the whole call.
    Cancelled {
        call_id: String,
        from_tag: String,
        invite_cseq: Option<u32>,
        in_dialog: bool,
        /// The CANCEL's own header lines — the canceller's `Reason` (RFC 3326
        /// §2) among them, which the CANCEL this stack sends onward restates.
        headers: Vec<SipHeader>,
    },
    /// A client transaction (b-leg INVITE, BYE, …) timed out with no final.
    Timeout {
        branch: String,
        call_ref: Option<String>,
        leg_id: Option<String>,
        method: Option<String>,
        /// The peer the timed-out request was sent to (forwarded from the txn
        /// layer). `None` when the txn never stored a destination — the consumer
        /// then skips per-peer failure attribution.
        destination: Option<SocketAddr>,
        /// Response-detection (Timer B/F, or an initial INVITE's
        /// `invite_first_response_timeout_sec`: nothing at all answered, the
        /// hop is dead) vs the configured out-of-dialog INVITE bound
        /// (`invite_txn_timeout_sec`, default 158 s: it answered, then went
        /// silent). Drives the per-peer metric's `response_timeout` vs
        /// `transaction_timeout` split and the `call_failure` consult's
        /// `timeout_kind`.
        timeout_kind: TimeoutKind,
    },
    /// Re-entrant internal event (async result folded back into the call).
    InternalEvent {
        call_ref: String,
        topic: String,
        outcome: String,
        payload: serde_json::Value,
        /// Raw, **binary-safe** entity bytes carried alongside the JSON
        /// `payload` (the generic service-authorable async-HTTP round-trip's
        /// response body — an arbitrary `Vec<u8>` that may contain non-UTF-8
        /// bytes). Kept OUT of `payload` on purpose: routing an opaque body
        /// through a `serde_json::Value` would coerce it to a UTF-8 string /
        /// force base64, so the bytes ride this field verbatim end to end.
        /// `Vec::new()` for every text-only internal event (reaper verdicts,
        /// the `/call/refer` and `/call/failure` decision results).
        body: Vec<u8>,
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
            TransactionEvent::Message { message, src, matched_client_txn } => {
                CallEvent::Sip { message, src, matched_client_txn }
            }
            TransactionEvent::Cancelled { call_id, from_tag, invite_cseq, in_dialog, headers } => {
                CallEvent::Cancelled { call_id, from_tag, invite_cseq, in_dialog, headers }
            }
            TransactionEvent::Timeout {
                branch,
                call_ref,
                leg_id,
                method,
                destination,
                kind,
            } => CallEvent::Timeout {
                branch,
                call_ref,
                leg_id,
                method,
                destination,
                timeout_kind: kind,
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
