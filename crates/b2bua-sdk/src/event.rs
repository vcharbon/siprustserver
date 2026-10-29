//! The unified event the router/handlers process — port of `SipRouter.ts`
//! `CallEvent`. A single discriminated type so handlers narrow on one input.

use std::net::SocketAddr;

use call::TimerKey;
use call::TxnTimeoutKind;
use sip_message::{Bytes, SipHeader, SipMessage};

/// One thing that happens to a call: an inbound SIP message, a fired timer, a
/// CANCEL the txn layer already answered, a client-transaction timeout, or a
/// re-entrant internal event (e.g. an async decision result).
#[derive(Debug, Clone)]
pub enum CallEvent {
    /// A SIP request/response that survived the transaction layer. A
    /// request's `message` carries the top Via the server transport stamped
    /// on receipt (RFC 3261 §18.2.1); `datagram` is the message as it arrived,
    /// byte for byte, what a trace records (ADR-0026).
    Sip { message: Box<SipMessage>, src: SocketAddr, datagram: Bytes },
    /// A B2BUA timer fired (keepalive, no-answer, max-duration, …): its key
    /// and the generation it was armed under. The router delivers it only
    /// while the call's ledger holds that key under that generation.
    Timer {
        key: TimerKey,
        generation: u64,
        call_ref: String,
        /// The incarnation of the call that scheduled it ([`Self::incarnation`]).
        incarnation: Option<String>,
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
        /// The To-tag the layer's 200 and 487 carried.
        to_tag: Option<String>,
        /// The instant the CANCEL reached the call's worker, its 200 and 487
        /// already sent: what the call records them at, however long the
        /// event waits behind the call's running turn.
        at_ms: i64,
    },
    /// A client transaction (b-leg INVITE, BYE, …) timed out with no final.
    Timeout {
        branch: String,
        call_ref: Option<String>,
        leg_id: Option<String>,
        method: Option<String>,
        /// The CSeq number of the request that drew no final.
        cseq: u32,
        /// The peer the timed-out request was sent to (forwarded from the txn
        /// layer). `None` when the txn never stored a destination — the consumer
        /// then skips per-peer failure attribution.
        destination: Option<SocketAddr>,
        /// Response-detection (Timer B/F, or an initial INVITE's
        /// first-response deadline `invite_first_response_timeout_sec`:
        /// nothing at all answered, the hop is dead) vs the configured out-of-dialog INVITE bound
        /// (`invite_txn_timeout_sec`, default 158 s: it answered, then went
        /// silent). Drives the per-peer metric's `response_timeout` vs
        /// `transaction_timeout` split and the `call_failure` consult's
        /// `timeout_kind`.
        timeout_kind: TxnTimeoutKind,
        /// The incarnation of the call that sent the request
        /// ([`Self::incarnation`]), from the mark on its Via.
        incarnation: Option<String>,
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
        /// The incarnation of the call whose turn sent the work this event
        /// answers ([`Self::incarnation`]).
        incarnation: Option<String>,
    },
    /// A lifecycle event of the call the turn caused, as the engine delivers
    /// it to the active services after the turn's claim. Never dispatched.
    Lifecycle(crate::lifecycle::LifecycleEvent),
}

/// The [`CallEvent::InternalEvent`] topic of the transaction layer's notice
/// that an initial INVITE's first-response deadline passed with a `100 Trying`
/// as its only response (ADR-0044): the next hop is alive, nothing past it has
/// answered. Its payload names the leg (`leg_id`) and the branch (`branch`).
pub const HOP_ALIVE_TOPIC: &str = "hop-alive";

impl CallEvent {
    /// `message`, received from `src` as the datagram it is.
    pub fn sip(message: SipMessage, src: SocketAddr) -> Self {
        let datagram = message.image().clone();
        CallEvent::Sip { message: Box::new(message), src, datagram }
    }

    /// The incarnation of the call this event belongs to
    /// (`call::Call::incarnation`): a timer fire, a transaction timeout and an
    /// internal event carry the one of the call they were armed, sent or
    /// answered for. `None` for an event of no particular call (an inbound
    /// message reads its incarnation off the wire) or one stamped with none,
    /// which belongs to whichever call is live on its `call_ref`.
    pub fn incarnation(&self) -> Option<&str> {
        match self {
            CallEvent::Timer { incarnation, .. }
            | CallEvent::Timeout { incarnation, .. }
            | CallEvent::InternalEvent { incarnation, .. } => incarnation.as_deref(),
            CallEvent::Sip { .. } | CallEvent::Cancelled { .. } | CallEvent::Lifecycle(_) => None,
        }
    }

    /// `self` stamped with `incarnation` where it carries an incarnation and
    /// none is stated yet: an event a call's turn re-enters is that call's.
    pub fn stamped(mut self, incarnation: &str) -> Self {
        if let CallEvent::Timer { incarnation: slot, .. }
        | CallEvent::Timeout { incarnation: slot, .. }
        | CallEvent::InternalEvent { incarnation: slot, .. } = &mut self
        {
            slot.get_or_insert_with(|| incarnation.to_string());
        }
        self
    }

    /// An asynchronous trigger: a timer fire, a transaction timeout, or an
    /// internal-event fold — a source of progress on the call's own clock,
    /// not a peer's message. On a call already going away the rule executor
    /// absorbs these unless the rule is a teardown rule
    /// (`RuleDefinition::teardown`).
    pub fn is_asynchronous_trigger(&self) -> bool {
        matches!(
            self,
            CallEvent::Timer { .. } | CallEvent::Timeout { .. } | CallEvent::InternalEvent { .. }
        )
    }

    /// Short discriminator for logs / reports.
    pub fn kind(&self) -> &'static str {
        match self {
            CallEvent::Sip { .. } => "sip",
            CallEvent::Timer { .. } => "timer",
            CallEvent::Cancelled { .. } => "cancelled",
            CallEvent::Timeout { .. } => "timeout",
            CallEvent::InternalEvent { .. } => "internal-event",
            CallEvent::Lifecycle(_) => "lifecycle",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_re_entered_event_takes_its_turns_incarnation_unless_it_states_one() {
        let fold = |incarnation: Option<&str>| CallEvent::InternalEvent {
            call_ref: "w0|cid|tag".into(),
            topic: "t".into(),
            outcome: "o".into(),
            payload: serde_json::Value::Null,
            body: Vec::new(),
            incarnation: incarnation.map(str::to_string),
        };
        assert_eq!(fold(None).stamped("w0|cid|tag#k").incarnation(), Some("w0|cid|tag#k"));
        assert_eq!(
            fold(Some("w0|cid|tag#j")).stamped("w0|cid|tag#k").incarnation(),
            Some("w0|cid|tag#j")
        );
    }
}
