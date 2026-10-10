//! What a received message names on the leg it arrived on ([`Resolved`]), as
//! the engine resolves it before any rule reads the message (RFC 3261 §12.2):
//! the dialog its tags name, read by its direction, and for a response the
//! relay it answers (§8.1.3.3). A response that names nothing this stack sent
//! is told apart ([`ResponseOrigin`]) so it reaches no rule.

use crate::model::{Call, Dialog, DialogId, Leg, PendingRequest};

use super::dialog::{relayed_pending, PendingHolder};
use super::leg::{find_leg, leg_own_tag};

/// The dialog the turn's message names, and the relay a response answers.
/// Empty for any other event, and for a message naming no dialog (a request
/// without a To-tag, a CANCEL, a response creating a dialog).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Resolved {
    /// The dialog of the source leg the message names.
    pub dialog: Option<DialogId>,
    /// The relayed request a response answers and where it is kept.
    pub relay: Option<(PendingHolder, PendingRequest)>,
}

/// The resolution of an event that names no dialog.
pub static UNRESOLVED: Resolved = Resolved { dialog: None, relay: None };

/// What a received response answers.
#[derive(Clone, Debug, PartialEq)]
pub enum ResponseOrigin {
    /// A response in the dialog its tags name (a tagless one: the leg's
    /// confirmed dialog), with the relay it answers when it answers one.
    Dialog(Resolved),
    /// A 1xx or 2xx to the INVITE that dialled the leg, under a To-tag no
    /// dialog of the leg has yet: it creates one (RFC 3261 §12.1.2).
    Creates,
    /// A response to a request this stack sent on the leg that names no
    /// dialog: a non-2xx final of the dialling INVITE, a response with no
    /// To-tag, a CANCEL's.
    Request,
    /// Its From-tag is no tag this stack put on the leg: it answers no request
    /// of this stack's (§8.1.3.3).
    Stray,
}

/// The tags, CSeq and status of a received response.
#[derive(Clone, Copy, Debug)]
pub struct ResponseFacts<'a> {
    pub to: Option<&'a str>,
    pub from: Option<&'a str>,
    pub cseq: i64,
    /// The CSeq method: the method of the request it answers.
    pub method: &'a str,
    pub status: u16,
}

/// What the response `facts` describes answers on `leg_id`.
pub fn response_origin(call: &Call, leg_id: &str, facts: ResponseFacts<'_>) -> ResponseOrigin {
    let Some(leg) = find_leg(call, leg_id) else { return ResponseOrigin::Stray };
    let named = match facts.to {
        Some(_) => leg.dialogs.of_response(facts.to, facts.from),
        None => leg.dialogs.confirmed().filter(|d| Some(d.our_tag.as_str()) == facts.from),
    };
    if let Some(d) = named {
        let relay = relayed_pending(call, leg_id, facts.to, facts.from, facts.cseq)
            .filter(|(_, p)| p.method.eq_ignore_ascii_case(facts.method));
        return ResponseOrigin::Dialog(Resolved { dialog: Some(d.id()), relay });
    }
    let Some(from) = facts.from.filter(|f| holds_our_tag(leg, f)) else {
        return ResponseOrigin::Stray;
    };
    let dialling =
        leg.outbound_invite.as_ref().is_some_and(|i| i.our_tag == from && i.cseq == facts.cseq);
    let invite = facts.method.eq_ignore_ascii_case("INVITE");
    if dialling && invite && facts.to.is_some() && (101..300).contains(&facts.status) {
        return ResponseOrigin::Creates;
    }
    ResponseOrigin::Request
}

/// Whether `tag` is one this stack put on `leg`: the tag of its INVITE or of
/// the dialog it answers the leg's INVITE under, or its own tag in any of the
/// leg's dialogs.
fn holds_our_tag(leg: &Leg, tag: &str) -> bool {
    leg_own_tag(leg) == Some(tag) || leg.dialogs.iter().any(|d: &Dialog| d.our_tag == tag)
}
