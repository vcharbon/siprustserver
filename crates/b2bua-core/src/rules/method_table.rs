//! What differs by method for a request record (ADR-0055 §2), one row per
//! method; nothing here knows HA.
//!
//! Every non-INVITE row shares one rule: a final ends the exchange, and the
//! client side holds through Timer K below the transaction user (RFC 3261
//! §17.1.2.2), so a repeat of that final settles nothing. An INVITE's 2xx
//! leaves its record open for the ACK it owes (§13.2.2.4). Beyond it:
//!
//! | method | usage | offer/answer (RFC 3264) | on a dialog this side is ending | a relay toward a leg that ended |
//! |---|---|---|---|---|
//! | INVITE | the session; its 2xx owes an ACK; reliable provisionals under `RSeq` (RFC 3262 §3); 491 on glare (§14.2) | opens an exchange | — | — |
//! | UPDATE | the session; 491 on glare (RFC 3311 §5.2) | opens an exchange | 487 (§21.4.25) | 481 |
//! | PRACK | the session | may carry the answer (RFC 3262 §5) | passes | 200 (RFC 3262 §3) |
//! | BYE | ends the dialog; leaves behind the ACK of this stack's 2xx to the dialog's creating INVITE (§15) and behind this stack's NOTIFY in flight | — | passes | — |
//! | NOTIFY | carries a subscription's state; leaves behind this stack's NOTIFY in flight | — | 200 | 481 |
//! | REFER | opens an implicit subscription (RFC 3515 §2.4.4) | — | 487 | 481 |
//! | INFO, MESSAGE, OPTIONS | the session | — | 200 | 481 |
//! | SUBSCRIBE, any other | the session | — | 487 (§21.4.25) | 481 |
//!
//! What the timeout of this stack's own request ends is the rules' to decide
//! (`defaults::request_timeouts`). Offer/answer is a fact of the session the
//! dialog carries, not of a record: an offer opens in one transaction and its
//! answer may ride another (a 2xx and its ACK, a reliable provisional and its
//! PRACK).

use call::DialogUsage;
use sip_message::header::Event;
use sip_message::{Method, SipRequest};

/// What a dialog this side is ending (its BYE or CANCEL in flight, RFC 3261
/// §15.1.2) makes of a request of the method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnEnding {
    /// It reaches the rules: a BYE crossing ours, a PRACK.
    Pass,
    /// The dialog takes it and nothing carries it further: 200.
    Absorb,
    /// The dialog acts on it no more: 487 (§21.4.25).
    Terminate,
}

/// What a request of the method does to the session's offer/answer exchange
/// (RFC 3264 §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exchange {
    /// It opens one: an INVITE (an offerless one leaves the offer to its 2xx,
    /// RFC 3261 §13.2.1), an UPDATE carrying a description (RFC 3311 §5.1).
    Opens,
    /// It may carry the answer to a reliable provisional's offer (RFC 3262 §5).
    MayAnswer,
    /// It takes no part.
    None,
}

/// One row of the method table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MethodRow {
    pub exchange: Exchange,
    /// A newcomer of the method meets glare over an exchange or an INVITE
    /// transaction still open on its dialog and is refused, 491 or 500 by
    /// what is open (RFC 3261 §14.2, RFC 3311 §5.2).
    pub glare: bool,
    /// A 2xx to it owes an ACK, end to end (RFC 3261 §13.2.2.4).
    pub acked_2xx: bool,
    pub on_ending: OnEnding,
    /// The final this stack owes the originator of a relay toward a leg that
    /// ended (RFC 3261 §8.2.6): its target's answer relays no further.
    pub owed_at_leg_end: (u16, &'static str),
    /// This stack's request of the method waits behind its own NOTIFY in
    /// flight on the dialog, so loss never delivers its requests out of CSeq
    /// order (RFC 3261 §12.2.2).
    pub waits_behind_own_notify: bool,
    /// This stack's request of the method waits for the ACK of its 2xx to
    /// the dialog's creating INVITE, or that 2xx's give-up (RFC 3261 §15).
    pub waits_behind_unacked_answer: bool,
}

const DOES_NOT_EXIST: (u16, &str) = (481, "Call/Transaction Does Not Exist");

/// The row of `method`.
pub fn row(method: &Method) -> MethodRow {
    let ordinary = MethodRow {
        exchange: Exchange::None,
        glare: false,
        acked_2xx: false,
        on_ending: OnEnding::Terminate,
        owed_at_leg_end: DOES_NOT_EXIST,
        waits_behind_own_notify: false,
        waits_behind_unacked_answer: false,
    };
    match method {
        Method::Invite => {
            MethodRow { exchange: Exchange::Opens, glare: true, acked_2xx: true, ..ordinary }
        }
        Method::Update => MethodRow { exchange: Exchange::Opens, glare: true, ..ordinary },
        Method::Bye => MethodRow {
            on_ending: OnEnding::Pass,
            waits_behind_own_notify: true,
            waits_behind_unacked_answer: true,
            ..ordinary
        },
        Method::Prack => MethodRow {
            exchange: Exchange::MayAnswer,
            on_ending: OnEnding::Pass,
            owed_at_leg_end: (200, "OK"),
            ..ordinary
        },
        Method::Notify => {
            MethodRow { on_ending: OnEnding::Absorb, waits_behind_own_notify: true, ..ordinary }
        }
        Method::Info | Method::Message | Method::Options => {
            MethodRow { on_ending: OnEnding::Absorb, ..ordinary }
        }
        _ => ordinary,
    }
}

/// The row of the method named `method` on the wire.
pub fn row_of(method: &str) -> MethodRow {
    row(&Method::from_wire(method))
}

/// The usage `req` belongs to (RFC 5057 §3): a REFER opens the implicit
/// subscription its CSeq names (RFC 3515 §2.4.4), a NOTIFY reports the one
/// its `Event` names (RFC 6665 §4.5); anything else rides the session.
pub fn usage_of(req: &SipRequest) -> DialogUsage {
    match req.method() {
        Method::Refer => DialogUsage::Subscription {
            event: "refer".into(),
            id: Some(req.cseq().seq().to_string()),
        },
        Method::Notify => match req.header::<Event>().and_then(Result::ok) {
            Some(event) => DialogUsage::Subscription {
                event: event.token().to_ascii_lowercase(),
                id: event.param("id").and_then(|v| v.as_str()).map(str::to_string),
            },
            None => DialogUsage::Session,
        },
        _ => DialogUsage::Session,
    }
}
