//! The dialog gate's verdict (ADR-0041): before any rule reads a message of a
//! live call, the message is resolved to the dialog it names on the leg it
//! arrived on (RFC 3261 §12.2), by its tags read in its direction, and refused
//! when that dialog takes no such message. What passes reaches the rules with
//! the dialog — and for a response, the relay it answers — resolved
//! ([`call::helpers::Resolved`]). The shell sends what a refusal answers.
//!
//! A request with a To-tag, on any leg:
//!
//! | its dialog | the gate |
//! |---|---|
//! | none, or a caller-facing one the caller was never shown | 481 |
//! | `Early`, `Confirmed` | pass |
//! | `Ending` (this side's BYE or CANCEL in flight) | see below |
//! | `Ended(Rejected)` | PRACK passes (RFC 3262 §3); others 481 |
//! | `Ended` by a BYE | 481, a BYE included (§15.1.2) |
//! | `Retired` | 481; one whose release BYE is in flight reads as `Ending` |
//!
//! A dialog this side is ending still exists until its BYE or its INVITE
//! takes a final (§15.1.2): what it makes of a request is the method table's
//! ([`crate::rules::method_table`]) — a BYE crossing ours and a PRACK pass to
//! their rules; an INFO, MESSAGE, NOTIFY or OPTIONS is answered 200 and goes
//! no further; any other request, an INVITE, UPDATE, REFER or SUBSCRIBE among
//! them, is answered 487 (§21.4.25). A request below its dialog's last CSeq
//! draws 500 (§12.2.2); one the gate passes or answers sets that last CSeq
//! ([`note_in_order`]). An ACK passes on a dialog a 2xx established, ended by
//! a BYE since or not (RFC 5407 §2); it is never answered (§17.1.1.3), so one
//! the gate refuses is dropped. An ACK on the a-leg naming the caller's INVITE
//! while the record holds no final sent for it reaches the rules only on a
//! call another owner served, as evidence of an answer the record lacks
//! whatever tag it names (ADR-0055 §4), with no dialog resolved when none
//! names it; otherwise it acknowledges nothing and is dropped (the answer's
//! ladder draws the ACK again, RFC 3261 §13.2.2.4). A CANCEL is matched by
//! transaction (§9.1) and passes unread. A response whose From-tag is no tag
//! this stack put on the leg answers nothing of this stack's and is dropped,
//! save a 2xx to an INVITE whose verified via-extension names a leg the record
//! lacks (ADR-0055 §4, §12).

use b2bua_sdk::event::CallEvent;
use call::helpers::{response_origin, Resolved, ResponseFacts, ResponseOrigin};
use call::{Call, Dialog, DialogEnd, DialogState};
use sip_message::{Method, SipMessage, SipRequest, SipResponse};

use crate::config::B2buaConfig;
use crate::rules::actions::possibly_confirmed;
use crate::rules::method_table::OnEnding;

/// The gate's reading of a message.
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    /// The message reaches the rules, naming this dialog.
    Pass(Resolved),
    /// Its dialog takes no such request: 481 (RFC 3261 §12.2.2).
    NoDialog,
    /// Below its dialog's last CSeq: 500 (§12.2.2).
    OutOfOrder,
    /// A request its ending dialog takes without carrying it further: 200.
    Absorbed(Resolved),
    /// A request its ending dialog will not act on: 487 (§21.4.25).
    Terminated(Resolved),
    /// An ACK naming no dialog that takes it: dropped unanswered (§17.1.1.3).
    Drop,
    /// A response that answers nothing of this stack's, dropped unread.
    Stray,
}

impl Verdict {
    /// The status the gate answers a refused request with; `None` for a
    /// message it passes or drops.
    pub fn status(&self) -> Option<u16> {
        match self {
            Verdict::NoDialog => Some(481),
            Verdict::OutOfOrder => Some(500),
            Verdict::Absorbed(_) => Some(200),
            Verdict::Terminated(_) => Some(487),
            Verdict::Pass(_) | Verdict::Drop | Verdict::Stray => None,
        }
    }

    /// The dialog a request the gate answers was taken in order on, if any.
    pub fn in_order(&self) -> Option<&Resolved> {
        match self {
            Verdict::Absorbed(resolved) | Verdict::Terminated(resolved) => Some(resolved),
            _ => None,
        }
    }
}

/// How the gate reads `event` arriving on `leg_id` of `call`. `copy_served`
/// states whether another owner served the call; it is read only for an ACK
/// of the caller's unanswered INVITE.
pub fn verdict(
    call: &Call,
    leg_id: &str,
    event: &CallEvent,
    config: &B2buaConfig,
    copy_served: impl FnOnce() -> bool,
) -> Verdict {
    let CallEvent::Sip { message, .. } = event else { return Verdict::Pass(Resolved::default()) };
    match message.as_ref() {
        SipMessage::Response(resp) => response_verdict(call, leg_id, resp, config),
        SipMessage::Request(req) if acks_unanswered_caller(call, leg_id, req) && !copy_served() => {
            Verdict::Drop
        }
        SipMessage::Request(req) => request_verdict(call, leg_id, req),
    }
}

/// How the gate reads `req` arriving on `leg_id` of `call`, whoever served it.
pub fn request_verdict(call: &Call, leg_id: &str, req: &SipRequest) -> Verdict {
    let method = req.method();
    let (Some(to), false) = (req.to().tag(), *method == Method::Cancel) else {
        return Verdict::Pass(Resolved::default());
    };
    // A leg the record lacks (another owner dialled it) holds no dialog here.
    let Some(leg) = call::helpers::find_leg(call, leg_id) else {
        return if *method == Method::Ack { Verdict::Drop } else { Verdict::NoDialog };
    };
    // A caller-facing dialog names nothing until the caller was shown its tag:
    // in a provisional, or in the 2xx that established it.
    let dialled = leg.outbound_invite().is_some();
    let dialog = leg
        .dialogs
        .of_request(Some(to), req.from().tag())
        .filter(|d| dialled || d.shown || d.was_established());
    let taken: fn(Resolved) -> Verdict = match admission(dialog, method) {
        Admission::Pass => Verdict::Pass,
        Admission::Absorb => Verdict::Absorbed,
        Admission::Terminate => Verdict::Terminated,
        Admission::Refuse
            if *method == Method::Ack
                && (acks_unanswered_caller(call, leg_id, req) || acks_held(call, leg_id, req)) =>
        {
            return Verdict::Pass(Resolved::default())
        }
        Admission::Refuse if *method == Method::Ack => return Verdict::Drop,
        Admission::Refuse => return Verdict::NoDialog,
    };
    let Some(dialog) = dialog else { return Verdict::NoDialog };
    let cseq = i64::from(req.cseq().seq());
    if *method != Method::Ack && dialog.peer_cseq.is_some_and(|last| cseq < last) {
        return Verdict::OutOfOrder;
    }
    taken(Resolved { dialog: Some(dialog.id()), record: None })
}

/// Note the request the gate passed or answered in order on the dialog it
/// names: its CSeq becomes that dialog's remote sequence number (RFC 3261
/// §12.2.2). An ACK numbers nothing (§13.2.2.4).
pub fn note_in_order(call: Call, leg_id: &str, resolved: &Resolved, event: &CallEvent) -> Call {
    let CallEvent::Sip { message, .. } = event else { return call };
    let (SipMessage::Request(req), Some(dialog)) = (message.as_ref(), &resolved.dialog) else {
        return call;
    };
    if *req.method() == Method::Ack {
        return call;
    }
    call::helpers::advance_peer_cseq(call, leg_id, dialog, i64::from(req.cseq().seq()))
}

/// What a dialog makes of a request.
#[derive(Debug, PartialEq, Eq)]
pub enum Admission {
    /// The request reaches the rules.
    Pass,
    /// The dialog takes it and nothing carries it further: 200.
    Absorb,
    /// The dialog is ending and acts on it no more: 487.
    Terminate,
    /// No dialog takes it: 481, or an ACK dropped.
    Refuse,
}

/// What `dialog` makes of a request of `method`.
pub fn admission(dialog: Option<&Dialog>, method: &Method) -> Admission {
    let Some(d) = dialog else { return Admission::Refuse };
    // The ACK of a 2xx belongs to its INVITE transaction and outlives the
    // dialog's BYE (RFC 5407 §2, Appendix D).
    if *method == Method::Ack {
        return if d.is_live() || d.was_established() {
            Admission::Pass
        } else {
            Admission::Refuse
        };
    }
    let ending = match &d.state {
        DialogState::Early | DialogState::Confirmed => return Admission::Pass,
        DialogState::Ended(DialogEnd::Rejected(_)) if *method == Method::Prack => {
            return Admission::Pass
        }
        DialogState::Ending(_) => true,
        DialogState::Retired => d.release_in_flight(),
        DialogState::Ended(_) => false,
    };
    if !ending {
        return Admission::Refuse;
    }
    match crate::rules::method_table::row(method).on_ending {
        OnEnding::Pass => Admission::Pass,
        OnEnding::Absorb => Admission::Absorb,
        OnEnding::Terminate => Admission::Terminate,
    }
}

/// Whether `ack`, an ACK on `leg_id` naming a dialog by its To-tag, names
/// the caller's INVITE while the record holds no final sent for it.
fn acks_unanswered_caller(call: &Call, leg_id: &str, ack: &SipRequest) -> bool {
    *ack.method() == Method::Ack
        && ack.to().tag().is_some()
        && leg_id == call.a_leg.leg_id
        && ack.cseq().seq() == call.a_leg_invite.cseq
        && call.a_leg.invite_final_sent().is_none()
}

/// Whether `ack`, an ACK of the caller's INVITE, names a caller dialog held
/// possibly confirmed (ADR-0055 §4): her ACK ends its hold.
fn acks_held(call: &Call, leg_id: &str, ack: &SipRequest) -> bool {
    leg_id == call.a_leg.leg_id
        && ack.cseq().seq() == call.a_leg_invite.cseq
        && ack.to().tag().is_some_and(|tag| possibly_confirmed::holds(call, tag))
}

/// How the gate reads the response `resp` arriving on `leg_id`.
fn response_verdict(
    call: &Call,
    leg_id: &str,
    resp: &SipResponse,
    config: &B2buaConfig,
) -> Verdict {
    if call::helpers::find_leg(call, leg_id).is_none() && names_unknown_leg(resp, config) {
        return Verdict::Pass(Resolved::default());
    }
    let cseq = resp.cseq();
    let facts = ResponseFacts {
        to: resp.to().tag(),
        from: resp.from().tag(),
        cseq: i64::from(cseq.seq()),
        method: cseq.method().as_str(),
        status: resp.status(),
        branch: resp.top_via().branch(),
    };
    match response_origin(call, leg_id, facts) {
        ResponseOrigin::Dialog(resolved) => Verdict::Pass(resolved),
        ResponseOrigin::Creates | ResponseOrigin::Request => Verdict::Pass(Resolved::default()),
        ResponseOrigin::Stray => Verdict::Stray,
    }
}

/// Whether `resp` is a 2xx to an INVITE whose verified via-extension names
/// the leg it arrived on.
fn names_unknown_leg(resp: &SipResponse, config: &B2buaConfig) -> bool {
    resp.cseq().method() == Method::Invite
        && (200..300).contains(&resp.status())
        && crate::stack_identity::read_via_extension(
            resp.top_via(),
            "INVITE",
            config.cluster_key.as_ref(),
        )
        .is_some()
}

#[cfg(test)]
#[path = "dialog_gate_tests.rs"]
mod tests;
