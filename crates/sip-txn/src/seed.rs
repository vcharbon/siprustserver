//! The seeds a consumer hands the layer to rebuild in-flight INVITE
//! transactions it never saw the first copy of — a call materialised from a
//! replica (ADR-0014) — and the disposition of a datagram re-offered against
//! them.

use std::net::SocketAddr;

use sip_message::SipRequest;

/// One INVITE transaction to rebuild, typed on the record's terms
/// ([`TransactionLayer::seed`](crate::TransactionLayer::seed)). A seed goes
/// straight to `Proceeding`: the first copy already left the wire from the
/// node that held it, so nothing is re-sent.
#[derive(Debug, Clone)]
pub enum TxnSeed {
    /// A client INVITE this side sent toward `dest`, still awaiting its final:
    /// keyed by the INVITE's top-Via branch and attributed from its Via `cr` /
    /// `lg` params. The INVITE bound is armed; no retransmit ladder runs.
    ClientInvite { invite: SipRequest, dest: SocketAddr },
    /// A server INVITE this side admitted and has not answered: keyed by
    /// `branch`, matched by a CANCEL on `call_id` + `from_tag`. `to_tag` is the
    /// To-tag the node that held it already put on a provisional, so every
    /// response this layer sends on the transaction carries the tag the peer's
    /// early dialog holds (RFC 3261 §17.2.1); `None` when none went out yet.
    /// With `original_request` present the layer answers a CANCEL 200 + 487 and
    /// emits `Cancelled` as for any INVITE it admitted itself; without it the
    /// seed absorbs the INVITE's retransmissions (each drawing a 100 Trying),
    /// and a CANCEL naming it is handed up as a `Message` for the consumer to
    /// answer 481 (§9.2).
    ServerInvite {
        branch: String,
        call_id: String,
        from_tag: String,
        to_tag: Option<String>,
        leg_id: Option<String>,
        original_request: Option<SipRequest>,
    },
}

/// What [`TransactionLayer::reoffer`](crate::TransactionLayer::reoffer) made
/// of a datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reoffer {
    /// A transaction in the map took it: the layer did what the first arrival
    /// would have done and emitted whatever that emits.
    Matched,
    /// No transaction took it; nothing was sent and nothing was emitted.
    Unmatched,
}
