//! The seeds a consumer hands the layer to rebuild in-flight transactions it
//! never saw the first copy of — a call materialised from a replica (ADR-0014,
//! ADR-0055 §2) — and the disposition of a datagram re-offered against them.

use std::net::SocketAddr;

use sip_message::{SipRequest, SipResponse};

/// One transaction to rebuild, typed on the record's terms
/// ([`TransactionLayer::seed`](crate::TransactionLayer::seed)). The first copy
/// of its request, and of its final, already left the wire from the node that
/// held it: an INVITE seed runs no retransmit ladder; a client one the record
/// holds no provisional for re-sends its INVITE once.
#[derive(Debug, Clone)]
pub enum TxnSeed {
    /// A client INVITE this side sent toward `dest`, still awaiting its final:
    /// keyed by the INVITE's top-Via branch and attributed from its Via `cr` /
    /// `lg` params. The INVITE bound is armed; no retransmit ladder runs.
    /// `provisional` says the record holds a provisional for it: the seed opens
    /// `Proceeding`, else `Trying`, where a CANCEL waits for one (RFC 3261
    /// §9.1), and the INVITE is re-sent once at the seed so the callee resends
    /// its latest provisional (§17.2.1).
    /// `cancel` is a CANCEL the record says this side sent for it while it had
    /// drawn no provisional: held until its first one (§9.1).
    /// `ack_ahead` is the ACK, as it left and where, of the 2xx to the INVITE
    /// the dialog sent before this one: sent once at the seed, ahead of the
    /// re-sent INVITE, so the peer never sees it while that INVITE is still
    /// unacknowledged (§14.1, RFC 6026 §7.1); a repeat is absorbed (§13.2.2.4).
    ClientInvite {
        invite: SipRequest,
        dest: SocketAddr,
        provisional: bool,
        cancel: Option<Box<SipRequest>>,
        ack_ahead: Option<(Vec<u8>, SocketAddr)>,
    },
    /// A server INVITE this side admitted and has not answered: `request`,
    /// rebuilt from the record as far as a response to it reads it — Via
    /// stack, From, To, Call-ID, CSeq. Keyed by its top-Via branch and sent-by
    /// (RFC 3261 §17.2.3). `to_tag` is the To-tag the node that held it
    /// already put on a provisional, so every response this layer sends on the
    /// transaction carries the tag the peer's early dialog holds (RFC 3261
    /// §17.2.1); `None` when none went out yet. `provisional` is the latest
    /// provisional the record holds as sent on it, as it left: sent once at
    /// the seed toward the top Via's response target — the record may be
    /// ahead of the wire, and a repeat is harmless (§13.3.1.1) — and the
    /// response a copy of the INVITE draws. It runs as an INVITE this layer
    /// admitted itself: a retransmission draws its latest provisional, a
    /// CANCEL 200 + 487 and `Cancelled`, a release its final.
    ServerInvite {
        request: SipRequest,
        to_tag: Option<String>,
        provisional: Option<Box<SipResponse>>,
        leg_id: Option<String>,
    },
    /// A server INVITE this side answered 2xx and still awaits the ACK for:
    /// `response`, the 2xx as it left, keyed by its top-Via branch and sent-by
    /// (RFC 3261 §17.2.3). It runs in RFC 6026 Accepted for Timer L from the
    /// seed, passing every ACK up, and answers each copy of the INVITE with
    /// `response` — this stack's choice over §8.7's silent absorption: the
    /// node holding the seed never sent the 2xx the copy may have missed. It
    /// holds a takeover copy for Timer L unless an ACK on its own branch
    /// detaches it: a 2xx's ACK takes a new branch. A transaction already on its key that has sent no final —
    /// the one the INVITE's retransmission opened before the record was read —
    /// takes `response` as its final and sends it once.
    ServerAccepted { response: SipResponse, leg_id: Option<String> },
    /// A server INVITE this side answered with a non-2xx final, its ACK not
    /// known to have arrived: `response`, the final as it left, keyed by its
    /// top-Via branch and sent-by (RFC 3261 §17.2.3). It runs in Completed
    /// from the seed and sends `response` toward its top Via's response target
    /// (RFC 3261 §18.2.2; `dest` where that names no IP) on Timer G until the
    /// ACK, which it absorbs, or Timer H (§17.2.1): the record may be ahead of
    /// the wire, so the first copy may never have left, and one that did drew
    /// its ACK toward the node that sent it. A copy of the INVITE draws
    /// `response` again. A transaction already on its key that has sent no
    /// final takes `response` as its final and sends it.
    ServerRejected { response: SipResponse, dest: SocketAddr, leg_id: Option<String> },
    /// A client non-INVITE this side sent toward `dest`, still awaiting its
    /// final: `request` as it left, keyed by its top-Via branch and attributed
    /// from its Via `cr` / `lg` params. It sends `request`'s image, byte for
    /// byte, under that branch once at the seed — the record may be ahead of
    /// the wire, so the first copy may never have left — then on Timer E from
    /// the seed, and gives up `bound_ms` from the seed: what is left of the
    /// Timer F the first copy started (RFC 3261 §17.1.2.2). A UAS that
    /// answered the first copy repeats its final to the retransmission
    /// (§17.2.2).
    ClientNonInvite { request: SipRequest, dest: SocketAddr, bound_ms: u64 },
    /// A server non-INVITE this side received and has not answered:
    /// `request`, rebuilt from the record as far as a response to it reads it
    /// — Via stack, From, To, Call-ID, CSeq — keyed by its top-Via branch,
    /// sent-by and method (RFC 3261 §17.2.3). A retransmission is absorbed
    /// with nothing sent; the consumer's final leaves on it.
    ServerNonInvite { request: SipRequest, leg_id: Option<String> },
    /// A server non-INVITE this side answered with a final: `response`, the
    /// final as it left, keyed by its top-Via branch, sent-by and method
    /// (RFC 3261 §17.2.3). It runs in Completed from the seed for Timer J and
    /// sends nothing on its own: a copy of the request draws `response`,
    /// toward the transaction's response target (§18.2.2; `dest` where that
    /// names no IP) (§17.2.2), so a copy that reaches this node after the
    /// final was decided elsewhere is never handed up as a new request. A
    /// transaction already on its key that has sent no final takes `response`
    /// as its final and sends it to the copy that opened it.
    ServerNonInviteAnswered { response: SipResponse, dest: SocketAddr, leg_id: Option<String> },
}

/// What [`TransactionLayer::reoffer`](crate::TransactionLayer::reoffer) made
/// of a datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reoffer {
    /// A transaction in the map took it: a response or a CANCEL ran the path
    /// its first arrival would have, a request was absorbed with nothing sent.
    Matched,
    /// No transaction took it; nothing was sent and nothing was emitted.
    Unmatched,
}
