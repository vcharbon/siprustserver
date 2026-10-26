//! A request the call takes part in ([`RequestRecord`], ADR-0055 §2): relayed
//! from the peer leg, this stack's own, received and held for a later answer,
//! or received and answered with a 2xx that awaits its ACK. A record has a
//! sent half (the request this side put on the dialog: its CSeq, branch, image
//! and destination) and a received half (the request this side received and
//! owes a final: the originator's for a relay, the request itself for a held
//! record). An INVITE's record carries its exchange beyond the final
//! ([`InviteExchange`]). A record lives where its transaction does — a leg's
//! dialog-creating INVITE on the leg, any other on the dialog holding it —
//! until that transaction settles, through the dialog's retirement, so a
//! restored record rebuilds each transaction from it.

use serde::{Deserialize, Serialize};
use sip_retransmit::timers::{T2, TIMER_F};

use super::invite_exchange::InviteExchange;
use super::outbound_invite::OutboundInvite;
use super::relay_owner::RelayOwner;

/// Host/port destination a request left toward.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostPort {
    pub host: String,
    pub port: u16,
}

/// Where a request record comes from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RequestOrigin {
    /// Relayed from the peer leg: this side sent it on the dialog holding the
    /// record for the originator's request its received half names.
    #[default]
    Relayed,
    /// This stack's own request, sent on the dialog holding the record.
    Own,
    /// Received on the dialog holding the record and owed a final a service
    /// gives on a later turn; nothing was sent for it.
    Held,
    /// Received and answered by this stack: a leg's dialog-creating INVITE,
    /// or an INVITE whose 2xx awaits its ACK on the dialog holding the record
    /// (RFC 3261 §13.3.1.4). Nothing was sent for it.
    Received,
}

/// The dialog usage a request belongs to (RFC 5057 §3).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DialogUsage {
    /// The INVITE usage: the session the dialog carries.
    #[default]
    Session,
    /// A subscription, by its event package and `id` (RFC 6665 §4.5): the
    /// implicit one a REFER opens (RFC 3515 §2.4.4), the one a NOTIFY reports.
    Subscription { event: String, id: Option<String> },
}

/// The [`RequestRecord::author`] of the keepalive OPTIONS: the id of the rule
/// that sends it.
pub const KEEPALIVE_AUTHOR: &str = "keepalive";

/// The transaction this side put on the wire for a record: what a client
/// transaction rebuilt from the record retransmits under the same branch
/// (RFC 3261 §17.1.2.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentRequest {
    /// The top-Via branch the request left with: its client transaction.
    pub branch: String,
    /// The request as it left, byte for byte.
    #[serde(with = "serde_bytes")]
    pub image: Vec<u8>,
    /// The wire destination it was sent to.
    pub destination: HostPort,
}

/// One request of the call still open.
///
/// The received half (`inbound_cseq` and the `source_*` lines) holds the
/// originator's own bytes as text, so a response to it echoes them unaltered
/// (RFC 3261 §8.2.6.2); an own record has none. The dialling INVITE's record
/// on a dialog it created holds no [`SentRequest`]: the leg's record of that
/// INVITE holds its one image.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestRecord {
    pub method: String,
    pub origin: RequestOrigin,
    /// The usage the request belongs to.
    pub usage: DialogUsage,
    /// The CSeq number this side sent the request with on the dialog holding
    /// the record; 0 for a received record, which sent nothing.
    pub outbound_cseq: i64,
    /// The transaction this side sent; `None` for a received record, for the
    /// dialling INVITE on a dialog it created, and for an INVITE whose
    /// transaction the record never held.
    pub sent: Option<SentRequest>,
    /// When the record's transaction ends with no final: the sent request's
    /// Timer F, or the held request's answer deadline
    /// ([`RequestRecord::HELD_ANSWER_MS`]); `None` for an INVITE.
    pub deadline_ms: Option<i64>,
    /// The CSeq number of the received request this side owes a final to; 0
    /// for an own record.
    pub inbound_cseq: i64,
    pub source_vias: Vec<String>,
    pub source_call_id: String,
    pub source_from: String,
    pub source_to: String,
    /// The originator CANCELled this relayed (re-)INVITE (RFC 3261 §9): the
    /// B2BUA CANCELled the outbound client transaction and the txn layer
    /// already answered the originator (200 + 487), so the target's eventual
    /// final response is resolved locally — never relayed. A crossing 2xx
    /// (target answered before the CANCEL landed) is ACKed and absorbed.
    pub cancelled: bool,
    /// The originator's request offered reliable provisionals — `100rel` in its
    /// own `Require` or `Supported` (RFC 3262 §3). The request this stack built
    /// toward the target may offer it where the originator did not (a declared
    /// advertisement), so the originator's licence is recorded here: a
    /// reliable provisional relays reliably only under it; without it the
    /// provisional relays unreliably and this stack acknowledges the responder
    /// itself.
    pub offered_100rel: bool,
    /// The originator's request carried a description (RFC 3264): an offer,
    /// on an INVITE or an UPDATE. An offerless INVITE has its offer made by
    /// the answerer.
    pub offered: bool,
    /// The originator's request's `Timestamp` value, which the response
    /// relayed back to it echoes (RFC 3261 §8.2.6.1); `None` where it stated
    /// none.
    pub source_timestamp: Option<String>,
    /// The worker that sent the request, by ordinal and worker identity: its
    /// client transaction carries it, and the final returns there (RFC 3261
    /// §18.2.2). The default names no worker.
    pub relayed_by: RelayOwner,
    /// The rule whose action sent this stack's own request, by its id: what a
    /// service reads to know a request, and its timeout, for its own. `None`
    /// for a request no rule's action sent (a relay, a held request, the
    /// engine's own PRACK).
    pub author: Option<String>,
    /// An INVITE's exchange beyond its final; `None` on any other method.
    pub invite: Option<InviteExchange>,
}

impl RequestRecord {
    /// How long a sent non-INVITE request awaits its final: Timer F, 64·T1
    /// (RFC 3261 §17.1.2.2).
    #[expect(clippy::cast_possible_wrap, reason = "Timer F is the constant 32 s")]
    pub const TIMER_F_MS: i64 = TIMER_F as i64;

    /// How long a held non-INVITE request waits for the service's final
    /// before the platform answers it: the originator's Timer F less T2, the
    /// interval its last retransmissions are paced at, so the final reaches
    /// it before its transaction gives up (RFC 4320 §4.2).
    #[expect(clippy::cast_possible_wrap, reason = "Timer F and T2 are constants")]
    pub const HELD_ANSWER_MS: i64 = (TIMER_F - T2) as i64;

    /// A relay of `method` sent at `outbound_cseq` for the originator's
    /// request at `inbound_cseq`; nothing else is known of it yet.
    pub fn relayed(method: &str, outbound_cseq: i64, inbound_cseq: i64) -> Self {
        RequestRecord {
            method: method.into(),
            origin: RequestOrigin::Relayed,
            usage: DialogUsage::Session,
            outbound_cseq,
            sent: None,
            deadline_ms: None,
            inbound_cseq,
            source_vias: Vec::new(),
            source_call_id: String::new(),
            source_from: String::new(),
            source_to: String::new(),
            cancelled: false,
            offered_100rel: false,
            offered: false,
            source_timestamp: None,
            relayed_by: RelayOwner::default(),
            author: None,
            invite: (method == "INVITE").then(InviteExchange::default),
        }
    }

    /// This stack's own `method` sent as `sent` at `outbound_cseq`, awaiting
    /// its final until `deadline_ms`.
    pub fn own(
        method: &str,
        outbound_cseq: i64,
        sent: SentRequest,
        deadline_ms: i64,
        usage: DialogUsage,
    ) -> Self {
        RequestRecord {
            origin: RequestOrigin::Own,
            usage,
            sent: Some(sent),
            deadline_ms: Some(deadline_ms),
            ..RequestRecord::relayed(method, outbound_cseq, 0)
        }
    }

    /// The dialling INVITE of a leg, numbered `cseq` and sent as `sent`
    /// (`None` where its image is not kept), with its dialling facts.
    pub fn dialled(cseq: i64, sent: Option<SentRequest>, dialled: OutboundInvite) -> Self {
        RequestRecord {
            origin: RequestOrigin::Own,
            sent,
            invite: Some(InviteExchange { dialled: Some(dialled), ..InviteExchange::default() }),
            ..RequestRecord::relayed("INVITE", cseq, 0)
        }
    }

    /// The dialling INVITE numbered `cseq` on a dialog it created: its image
    /// and dialling facts are the leg's record's.
    pub fn dialled_on_dialog(cseq: i64) -> Self {
        RequestRecord { origin: RequestOrigin::Own, ..RequestRecord::relayed("INVITE", cseq, 0) }
    }

    /// An INVITE this stack received numbered `cseq` and answers itself.
    pub fn received_invite(cseq: i64) -> Self {
        RequestRecord {
            origin: RequestOrigin::Received,
            ..RequestRecord::relayed("INVITE", 0, cseq)
        }
    }

    /// Whether this side sent the request: a relayed or own record.
    pub fn is_sent(&self) -> bool {
        matches!(self.origin, RequestOrigin::Relayed | RequestOrigin::Own)
    }

    /// Whether the record is a relay.
    pub fn is_relay(&self) -> bool {
        self.origin == RequestOrigin::Relayed
    }

    /// Whether the record is this stack's own request.
    pub fn is_own(&self) -> bool {
        self.origin == RequestOrigin::Own
    }

    /// Whether the record is a received request held for a later answer.
    pub fn is_held(&self) -> bool {
        self.origin == RequestOrigin::Held
    }

    /// The branch the sent request left with; `None` where the record holds
    /// no sent transaction.
    pub fn branch(&self) -> Option<&str> {
        self.sent.as_ref().map(|s| s.branch.as_str())
    }

    /// Whether the record is an INVITE's.
    pub fn is_invite(&self) -> bool {
        self.invite.is_some()
    }

    /// Whether a final this side receives still settles the record: a sent
    /// non-INVITE until its final; a relayed INVITE until a 2xx is taken for
    /// it. An INVITE this stack sent on its own account settles through its
    /// dialog's INVITE exchange, not as an answered record.
    pub fn awaits_final(&self) -> bool {
        match &self.invite {
            None => self.is_sent(),
            Some(exchange) => self.is_relay() && !exchange.taken(),
        }
    }

    /// Whether the record's sent half is the request a final numbered `cseq`
    /// for `method`, on the client transaction `branch`, answers: a sent
    /// non-INVITE transaction by its branch (RFC 3261 §17.1.3), so a final to
    /// another request reusing the number answers nothing here; an INVITE's
    /// by its number.
    pub fn answered_by(&self, method: &str, cseq: i64, branch: Option<&str>) -> bool {
        let transaction = match (&self.sent, self.is_invite()) {
            (Some(sent), false) => branch == Some(sent.branch.as_str()),
            _ => true,
        };
        self.awaits_final()
            && transaction
            && self.outbound_cseq == cseq
            && self.method.eq_ignore_ascii_case(method)
    }
}
