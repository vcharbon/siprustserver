//! The seeds a consumer hands the layer to rebuild in-flight transactions it
//! never saw the first copy of — a call materialised from a replica (ADR-0014,
//! ADR-0055 §2) — and the disposition of a datagram re-offered against them.

use std::net::SocketAddr;

use sip_message::{SipRequest, SipResponse};

/// One transaction to rebuild, typed on the record's terms
/// ([`TransactionLayer::seed`](crate::TransactionLayer::seed)). The first copy
/// of its request, and of its final, already left the wire from the node that
/// held it: an INVITE seed runs no retransmit ladder.
#[derive(Debug, Clone)]
pub enum TxnSeed {
    /// A client INVITE this side sent toward `dest`, still awaiting its final:
    /// keyed by the INVITE's top-Via branch and attributed from its Via `cr` /
    /// `lg` params. The INVITE bound is armed; no retransmit ladder runs.
    /// `provisional` says the record holds a provisional for it: the seed opens
    /// `Proceeding`, else `Trying`, where a CANCEL waits for one (RFC 3261 §9.1)
    /// and, held, has the INVITE re-sent once so the callee resends its latest
    /// provisional (§17.2.1).
    /// `cancel` is a CANCEL the record says this side sent for it while it had
    /// drawn no provisional: held until its first one (§9.1).
    ClientInvite {
        invite: SipRequest,
        dest: SocketAddr,
        provisional: bool,
        cancel: Option<Box<SipRequest>>,
    },
    /// A server INVITE this side admitted and has not answered: `request`,
    /// rebuilt from the record as far as a response to it reads it — Via
    /// stack, From, To, Call-ID, CSeq. Keyed by its top-Via branch and sent-by
    /// (RFC 3261 §17.2.3). `to_tag` is the To-tag the node that held it
    /// already put on a provisional, so every response this layer sends on the
    /// transaction carries the tag the peer's early dialog holds (RFC 3261
    /// §17.2.1); `None` when none went out yet. It runs as an INVITE this
    /// layer admitted itself: a retransmission draws its latest provisional, a
    /// CANCEL 200 + 487 and `Cancelled`, a release its final.
    ServerInvite { request: SipRequest, to_tag: Option<String>, leg_id: Option<String> },
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
    /// A client non-INVITE this side sent toward `dest`, still awaiting its
    /// final: `request` as it left, keyed by its top-Via branch and attributed
    /// from its Via `cr` / `lg` params. It retransmits `request` under that
    /// branch on Timer E from the seed and gives up on Timer F from the seed
    /// (RFC 3261 §17.1.2.2): a UAS that answered the first copy repeats its
    /// final to the retransmission (§17.2.2).
    ClientNonInvite { request: SipRequest, dest: SocketAddr },
    /// A server non-INVITE this side received and has not answered:
    /// `request`, rebuilt from the record as far as a response to it reads it
    /// — Via stack, From, To, Call-ID, CSeq — keyed by its top-Via branch,
    /// sent-by and method (RFC 3261 §17.2.3). A retransmission is absorbed
    /// with nothing sent; the consumer's final leaves on it.
    ServerNonInvite { request: SipRequest, leg_id: Option<String> },
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
