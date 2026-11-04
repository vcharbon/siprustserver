//! Upward events the transaction layer emits to its consumer (the proxy's or
//! B2BUA's router), and the client-transaction handles `send_request` returns.

use std::net::SocketAddr;

use bytes::Bytes;
use call::TxnTimeoutKind;
use sip_message::{Method, SipHeader, SipMessage};

/// A deduplicated/processed event for the upstream router. The peer address is
/// a `SocketAddr` (sip-net's everywhere-`SocketAddr` convention, ADR-0005).
#[derive(Debug, Clone)]
pub enum TransactionEvent {
    /// A SIP message that survived dedup/absorption and should reach the app.
    Message {
        message: Box<SipMessage>,
        src: SocketAddr,
        /// A response: whether it matched a client transaction at dispatch — one
        /// this node sent and this layer answers for, ACKing a non-2xx INVITE
        /// final itself (RFC 3261 §17.1.1.3). A CANCEL response never matches
        /// (it shares its INVITE's branch and is never handed to that
        /// transaction); always `false` for a request.
        matched_client_txn: bool,
        /// The datagram the message arrived as, byte for byte. A request's
        /// `message` carries the top Via the server transport stamped on
        /// receipt (RFC 3261 §18.2.1); this is what the peer sent.
        datagram: Bytes,
    },
    /// A CANCEL matched a server INVITE txn; this layer sent the 200/487, or
    /// holds them for release (`TransactionConfig::cancel_bound_ms`). RFC 3261
    /// §9 scopes a CANCEL to the one INVITE *transaction* it matched, so the
    /// event carries enough for the consumer to tell an initial-INVITE CANCEL
    /// (tear the call down) from one targeting an in-dialog re-INVITE (end
    /// that renegotiation only, keep the call): `invite_cseq` is the matched
    /// INVITE's CSeq number and `in_dialog` is whether its `To` already
    /// carried a tag (an established dialog — a re-INVITE). Both derive from
    /// the matched txn's stored original request; `(None, false)` only on the
    /// degenerate no-stored-request path, which consumers treat as the
    /// initial-INVITE (teardown) case.
    Cancelled {
        call_id: String,
        from_tag: String,
        invite_cseq: Option<u32>,
        in_dialog: bool,
        /// The CANCEL's own header lines. RFC 3326 §2 scopes `Reason` to CANCEL
        /// and BYE, so the request the layer already answered is the only place
        /// the canceller's cause is ever stated.
        headers: Vec<SipHeader>,
        /// The To-tag the layer answered under — on the 200 to the CANCEL and
        /// the 487 to the INVITE alike: the tag the INVITE's UAS transaction
        /// had bound, or pinned for it here. `None` only when none could be.
        to_tag: Option<String>,
    },
    /// A client transaction's Timer B/F (or the configured out-of-dialog
    /// INVITE bound, default 158 s) fired with no final response — the
    /// transaction timed out.
    Timeout {
        branch: String,
        call_ref: Option<String>,
        leg_id: Option<String>,
        /// The `ci` mark of the call incarnation that sent the request, from
        /// its Via beside `cr` / `lg`.
        incarnation_mark: Option<String>,
        /// SIP method of the timed-out transaction (INVITE / BYE / …).
        method: Option<String>,
        /// The CSeq number of the request that drew no final: with `method`,
        /// which request of its dialog it was.
        cseq: u32,
        /// The destination the timed-out request was sent to, forwarded from the
        /// `Transaction`'s stored `destination`. `None` only when the txn never
        /// recorded one (legacy/server paths) — consumers then skip per-peer
        /// attribution rather than fabricate an address.
        destination: Option<SocketAddr>,
        /// Which timeout fired: the short response-detection timer (Timer B/F,
        /// or an initial INVITE's first-response deadline,
        /// `TransactionConfig::invite_first_response_timeout_ms`) vs
        /// the configured out-of-dialog INVITE bound
        /// (`TransactionConfig::invite_initial_timeout_ms`, default 158 s). Lets
        /// consumers split `response_timeout` from `transaction_timeout`.
        kind: TxnTimeoutKind,
    },
    /// An initial INVITE's first-response deadline
    /// (`TransactionConfig::invite_first_response_timeout_ms`, ADR-0044) passed
    /// with a `100 Trying` as the only response: the next hop is alive and
    /// nothing past it has answered. Told once per transaction, never on a
    /// branch a 1xx above 100 answered (the TU received that one).
    HopAlive {
        branch: String,
        call_ref: String,
        leg_id: String,
        /// The `ci` mark of the call incarnation that sent the INVITE.
        incarnation_mark: Option<String>,
    },
    /// **The last transaction for a *watched* call reached a terminal state** —
    /// every transaction attributed to `call_ref` has left the map (final response
    /// and ACK→Timer H for an INVITE, Timer J for a non-INVITE, or Timer B/F on
    /// failure). Emitted only for call_refs the consumer registered via
    /// [`watch_self_release`](crate::TransactionLayer::watch_self_release); the
    /// B2BUA's acting-backup self-release (ADR-0014) keys on it to shed its live
    /// takeover copy without polling.
    CallQuiesced { call_ref: String },
}

impl TransactionEvent {
    /// The response `resp` received from `src`, its datagram its own image.
    pub(crate) fn response(
        resp: sip_message::SipResponse,
        src: SocketAddr,
        matched_client_txn: bool,
    ) -> Self {
        let datagram = resp.image().clone();
        TransactionEvent::Message {
            message: Box::new(SipMessage::Response(resp)),
            src,
            matched_client_txn,
            datagram,
        }
    }
}

/// The class of an event the full output queue refused: the `reason` label of
/// both the drop counter (an ordinary event, lost) and the deferral counter (a
/// critical event, delivered later), so backpressure reads per message class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventQueueClass {
    RequestInvite,
    RequestOther,
    Response,
    Cancelled,
    Timeout,
    CallQuiesced,
}

impl EventQueueClass {
    pub(crate) fn of(event: &TransactionEvent) -> Self {
        match event {
            TransactionEvent::Cancelled { .. } => Self::Cancelled,
            TransactionEvent::Timeout { .. } | TransactionEvent::HopAlive { .. } => Self::Timeout,
            TransactionEvent::CallQuiesced { .. } => Self::CallQuiesced,
            TransactionEvent::Message { message, .. } => match message.as_ref() {
                SipMessage::Response(_) => Self::Response,
                SipMessage::Request(r) if r.method() == Method::Invite => Self::RequestInvite,
                SipMessage::Request(_) => Self::RequestOther,
            },
        }
    }

    pub const ALL: [EventQueueClass; 6] = [
        Self::RequestInvite,
        Self::RequestOther,
        Self::Response,
        Self::Cancelled,
        Self::Timeout,
        Self::CallQuiesced,
    ];

    /// Stable metric-label for this reason (`reason="..."`).
    pub const fn label(self) -> &'static str {
        match self {
            Self::RequestInvite => "request_invite",
            Self::RequestOther => "request_other",
            Self::Response => "response",
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::CallQuiesced => "call_quiesced",
        }
    }

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::RequestInvite => 0,
            Self::RequestOther => 1,
            Self::Response => 2,
            Self::Cancelled => 3,
            Self::Timeout => 4,
            Self::CallQuiesced => 5,
        }
    }
}

use sip_message::SipRequest;

/// Handle to an outgoing client transaction, returned by `send_request`. Later
/// messages sourced from the same transaction (CANCEL, ACK-for-2xx) reuse the
/// branch / original request RFC 3261 mandates (§9.1, §13.2.2.4) — send those via
/// `send_raw` (or `send_request`, which routes a CANCEL/ACK raw): they share this
/// transaction's branch and must NOT open a second, colliding client transaction.
#[derive(Debug, Clone)]
pub enum ClientTransactionHandle {
    Invite { branch: String, original_invite: SipRequest, destination: SocketAddr },
    NonInvite { branch: String, original_request: SipRequest, destination: SocketAddr },
}

impl ClientTransactionHandle {
    pub fn branch(&self) -> &str {
        match self {
            Self::Invite { branch, .. } | Self::NonInvite { branch, .. } => branch,
        }
    }
    pub fn destination(&self) -> SocketAddr {
        match self {
            Self::Invite { destination, .. } | Self::NonInvite { destination, .. } => *destination,
        }
    }
}
