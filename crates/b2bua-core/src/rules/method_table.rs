//! What differs by method for a non-INVITE request record (ADR-0055 §2), one
//! row per method; nothing here knows HA.
//!
//! Every non-INVITE row shares one rule: a final ends the exchange, and the
//! client side holds through Timer K below the transaction user (RFC 3261
//! §17.1.2.2), so a repeat of that final settles nothing. Beyond it:
//!
//! | method | usage | on a dialog this side is ending | a relay toward a leg that ended | own timeout |
//! |---|---|---|---|---|
//! | BYE | ends the dialog; leaves behind this stack's NOTIFY in flight | passes | — | the leg's end |
//! | PRACK | the session | passes | 200 (RFC 3262 §3) | denies that acknowledgement alone |
//! | NOTIFY | carries a subscription's state; leaves behind this stack's NOTIFY in flight | 200 | 481 | ends its subscription (RFC 6665 §4.2.2) |
//! | REFER | opens an implicit subscription (RFC 3515 §2.4.4) | 487 | 481 | ends the session |
//! | INFO, MESSAGE, OPTIONS | the session | 200 | 481 | ends the session |
//! | UPDATE, SUBSCRIBE, any other | the session | 487 (§21.4.25) | 481 | ends the session |
//!
//! A request whose leg is going away when its own timeout comes is that leg's
//! end, whatever its method. The INVITE row is the dialog's INVITE
//! transaction state.

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

/// What the timeout of this stack's own request of the method ends, its leg
/// still up (RFC 5057 §5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnTimeout {
    /// Nothing beyond the transaction: a PRACK's acknowledgement alone.
    Transaction,
    /// The usage the request belongs to: a NOTIFY's subscription.
    Usage,
    /// The session: the call ends.
    Session,
}

/// One row of the method table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MethodRow {
    pub on_ending: OnEnding,
    /// The final this stack owes the originator of a relay toward a leg that
    /// ended (RFC 3261 §8.2.6): its target's answer relays no further.
    pub owed_at_leg_end: (u16, &'static str),
    pub own_timeout: OwnTimeout,
    /// This stack's request of the method waits behind its own NOTIFY in
    /// flight on the dialog, so loss never delivers its requests out of CSeq
    /// order (RFC 3261 §12.2.2).
    pub waits_behind_own_notify: bool,
}

const DOES_NOT_EXIST: (u16, &str) = (481, "Call/Transaction Does Not Exist");

/// The row of `method`.
pub fn row(method: &Method) -> MethodRow {
    let ordinary = MethodRow {
        on_ending: OnEnding::Terminate,
        owed_at_leg_end: DOES_NOT_EXIST,
        own_timeout: OwnTimeout::Session,
        waits_behind_own_notify: false,
    };
    match method {
        Method::Bye => {
            MethodRow { on_ending: OnEnding::Pass, waits_behind_own_notify: true, ..ordinary }
        }
        Method::Prack => MethodRow {
            on_ending: OnEnding::Pass,
            owed_at_leg_end: (200, "OK"),
            own_timeout: OwnTimeout::Transaction,
            ..ordinary
        },
        Method::Notify => MethodRow {
            on_ending: OnEnding::Absorb,
            own_timeout: OwnTimeout::Usage,
            waits_behind_own_notify: true,
            ..ordinary
        },
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
