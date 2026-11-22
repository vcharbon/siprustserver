//! The dialog gate's verdict (ADR-0041): before any rule reads a message of a
//! live call, the message is resolved to the dialog it names on the leg it
//! arrived on (RFC 3261 §12.2), by its tags read in its direction, and refused
//! when that dialog takes no such message. What passes reaches the rules with
//! the dialog — and for a response, the relay it answers — resolved
//! ([`call::helpers::Resolved`]). The shell sends what a refusal answers.
//!
//! A request, on any leg, CANCEL aside:
//!
//! | its dialog | the gate |
//! |---|---|
//! | no To-tag: none (§12.2.1.1) | 481 to a method this stack carries only in a dialog; an INVITE, an ACK or a method it relays in no dialog passes (`tagless_verdict`) |
//! | its To-tag names none, or a caller-facing one the caller was never shown | 481 |
//! | `Early`, `Confirmed` | pass |
//! | `Ending` (this side's BYE or CANCEL in flight) | see below |
//! | `Ended(Rejected)` | PRACK passes (RFC 3262 §3); others 481 |
//! | `Ended` by a BYE | 481, a BYE included (§15.1.2) |
//! | `Retired` | PRACK passes on the caller face (§3); others 481, or as `Ending` while its release BYE is in flight |
//!
//! A dialog this side is ending still exists until its BYE or its INVITE
//! takes a final (§15.1.2): what it makes of a request is the method table's
//! ([`crate::rules::method_table`]) — a BYE crossing ours and a PRACK pass to
//! their rules; an INFO, MESSAGE, NOTIFY or OPTIONS is answered 200 and goes
//! no further; any other request, an INVITE, UPDATE, REFER or SUBSCRIBE among
//! them, is answered 487 (§21.4.25). Past the 481s, the §8.2 checks come
//! before the dialog's CSeq order (§12.2.2): a request of a method this stack
//! carries in a dialog that requires an option tag this stack does not
//! understand draws 420, naming those tags in `Unsupported` (§8.2.2.3,
//! [`Verdict::BadExtension`]), whatever its CSeq. Then a request below its
//! dialog's last CSeq draws 500 (§12.2.2). A refusal changes no state (§8.2):
//! nothing reaches the rules or the peer, and the dialog, its last CSeq and
//! its session stay as they were. A request the gate passes, or answers 200
//! or 487, sets that last CSeq ([`note_in_order`]).
//! An ACK passes on a dialog a 2xx established, ended by
//! a BYE since or not (RFC 5407 §2); it is never answered (§17.1.1.3), so one
//! the gate refuses is dropped. An ACK naming an INVITE on its leg for which
//! the record holds no final sent reaches the rules only on a call another
//! owner served, as evidence of an answer the record lacks (ADR-0055 §4); the
//! caller's INVITE does whatever tag the ACK names, with no dialog resolved
//! when none names it. On any other call it proves nothing
//! ([`Verdict::UnexplainedAck`]): its final may be a stateless refusal of a
//! re-INVITE that never reached the call, and a 2xx's ladder draws the ACK
//! again (RFC 3261 §13.2.2.4). A CANCEL is matched by
//! transaction (§9.1) and passes unread, tagged or not. A response whose From-tag is no tag
//! this stack put on the leg answers nothing of this stack's and is dropped,
//! save a 2xx to an INVITE whose verified via-extension names a leg the record
//! lacks (ADR-0055 §4, §12). A 2xx to an INVITE whose round on its dialog
//! ended with no 2xx taken, rejected or past its Timer B, is owed nothing
//! (RFC 6026 §7.2): dropped unread and unanswered, and counted
//! ([`Verdict::RoundEnded`]).

use b2bua_sdk::event::CallEvent;
use call::helpers::{response_origin, Resolved, ResponseFacts, ResponseOrigin};
use call::{Call, Dialog, DialogEnd, DialogState};
use sip_message::generators::InDialogMethod;
use sip_message::header::Unsupported;
use sip_message::{Method, SipMessage, SipRequest, SipResponse};

use crate::config::B2buaConfig;
use crate::rules::actions::possibly_confirmed;
use crate::rules::evidence::acks_unanswered_invite;
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
    /// A request requiring option tags this stack does not understand: 420,
    /// its `Unsupported` listing exactly those (§8.2.2.3). It changes no state
    /// of its dialog (§8.2).
    BadExtension(Unsupported),
    /// An ACK naming no dialog that takes it: dropped unanswered (§17.1.1.3).
    Drop,
    /// An ACK of an INVITE the record holds no final sent for, on a call no
    /// other owner served: dropped unanswered, the dialog untouched, and counted
    /// (ADR-0055 §4).
    UnexplainedAck,
    /// A response that answers nothing of this stack's, dropped unread.
    Stray,
    /// A 2xx to an INVITE whose round on its dialog ended with no 2xx taken
    /// ([`call::helpers::invite_round_ended`]): owed nothing (RFC 6026 §7.2),
    /// dropped unread and unanswered, the dialog untouched, and counted.
    RoundEnded,
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
            Verdict::BadExtension(_) => Some(420),
            Verdict::Pass(_)
            | Verdict::Drop
            | Verdict::UnexplainedAck
            | Verdict::Stray
            | Verdict::RoundEnded => None,
        }
    }

    /// The dialog a request the gate answers was taken in order on, if any:
    /// a 200 or a 487 of its ending dialog's. A refusal is taken on none.
    pub fn in_order(&self) -> Option<&Resolved> {
        match self {
            Verdict::Absorbed(resolved) | Verdict::Terminated(resolved) => Some(resolved),
            Verdict::Pass(_)
            | Verdict::NoDialog
            | Verdict::OutOfOrder
            | Verdict::BadExtension(_)
            | Verdict::Drop
            | Verdict::UnexplainedAck
            | Verdict::Stray
            | Verdict::RoundEnded => None,
        }
    }
}

/// How the gate reads `event` arriving on `leg_id` of `call`. `copy_served`
/// states whether another owner served the call; it is read only for an ACK
/// of an INVITE the record holds no final sent for.
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
        SipMessage::Request(req) if acks_no_final_held(call, leg_id, req) && !copy_served() => {
            Verdict::UnexplainedAck
        }
        SipMessage::Request(req) => request_verdict(call, leg_id, req),
    }
}

/// How the gate reads `req` arriving on `leg_id` of `call`, whoever served it.
pub fn request_verdict(call: &Call, leg_id: &str, req: &SipRequest) -> Verdict {
    let method = req.method();
    if *method == Method::Cancel {
        return Verdict::Pass(Resolved::default());
    }
    let Some(to) = req.to().tag() else { return tagless_verdict(method) };
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
        Admission::Pass if dialled && names_losing_fork(dialog, method) => {
            return Verdict::NoDialog
        }
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
    if let Some(unsupported) = bad_extension(req) {
        return Verdict::BadExtension(unsupported);
    }
    let cseq = i64::from(req.cseq().seq());
    if *method != Method::Ack && dialog.peer_cseq.is_some_and(|last| cseq < last) {
        return Verdict::OutOfOrder;
    }
    taken(Resolved { dialog: Some(dialog.id()), record: None })
}

/// The option tags `req` requires that this stack does not understand (RFC
/// 3261 §8.2.2.3, [`sip_message::option_tags::not_understood`]), on a method
/// this stack carries in a dialog: any other method draws its rules' 405 or
/// 489 first (§8.2.1).
fn bad_extension(req: &SipRequest) -> Option<Unsupported> {
    InDialogMethod::try_from(req.method()).ok()?;
    sip_message::option_tags::not_understood(req)
}

/// What the gate makes of a request with no To-tag, which names no dialog
/// (RFC 3261 §12.2.1.1). A method this stack carries only within a dialog
/// draws 481 (§12.2.2; an INFO rides only an invite dialog usage, RFC 6086
/// §4.2.1). An INVITE (the initial one's retransmission, or a merged or looped
/// copy), an ACK, and a method this stack relays in no dialog, which the rules
/// answer by its method (§8.2.1), pass with no dialog resolved. A tagless
/// OPTIONS never reaches the gate: the router answers it as a keepalive
/// before any call lookup.
fn tagless_verdict(method: &Method) -> Verdict {
    match InDialogMethod::try_from(method) {
        Ok(InDialogMethod::Invite) | Err(()) => Verdict::Pass(Resolved::default()),
        Ok(_) => Verdict::NoDialog,
    }
}

/// Whether `method` is a PRACK on a retired, unreleased dialog of a leg this
/// stack dialled: a losing fork, shown no provisional of this stack's, whose
/// tag it shares with its siblings, so the PRACK names nothing (481).
fn names_losing_fork(dialog: Option<&Dialog>, method: &Method) -> bool {
    *method == Method::Prack && dialog.is_some_and(|d| d.is_retired() && !d.release_in_flight())
}

/// Note the request the gate passed or answered in order on the dialog it
/// names: its CSeq becomes that dialog's remote sequence number (RFC 3261
/// §12.2.2). An ACK numbers nothing (§13.2.2.4).
pub fn note_in_order(call: &mut Call, leg_id: &str, resolved: &Resolved, event: &CallEvent) {
    let CallEvent::Sip { message, .. } = event else { return };
    let (SipMessage::Request(req), Some(dialog)) = (message.as_ref(), &resolved.dialog) else {
        return;
    };
    if *req.method() == Method::Ack {
        return;
    }
    call::helpers::advance_peer_cseq(call, leg_id, dialog, i64::from(req.cseq().seq()));
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
        DialogState::Ended(DialogEnd::Rejected(_)) | DialogState::Retired
            if *method == Method::Prack =>
        {
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
    acks_unanswered_invite((&call.a_leg, call.a_leg_invite.cseq), leg_id, ack, || None)
}

/// Whether `ack` names an INVITE on `leg_id` for which the record holds no final
/// sent, on the dialog it names (ADR-0055 §4's trigger 3).
fn acks_no_final_held(call: &Call, leg_id: &str, ack: &SipRequest) -> bool {
    acks_unanswered_invite((&call.a_leg, call.a_leg_invite.cseq), leg_id, ack, || {
        let leg = call::helpers::find_leg(call, leg_id)?;
        let dialog = leg.dialogs.of_request(ack.to().tag(), ack.from().tag())?;
        let id = dialog.id();
        let relayed_to = call::helpers::relay_peer_dialog(call, leg_id, Some(&id));
        Some((dialog, relayed_to.map(|(_, d)| d)))
    })
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
        ResponseOrigin::Dialog(resolved) if answers_ended_round(call, leg_id, &resolved, resp) => {
            Verdict::RoundEnded
        }
        ResponseOrigin::Dialog(resolved) => Verdict::Pass(resolved),
        ResponseOrigin::Creates | ResponseOrigin::Request => Verdict::Pass(Resolved::default()),
        ResponseOrigin::Stray => Verdict::Stray,
    }
}

/// Whether `resp`, resolved to a dialog of `leg_id`, is a 2xx to an INVITE
/// whose round there ended with no 2xx taken.
fn answers_ended_round(call: &Call, leg_id: &str, resolved: &Resolved, resp: &SipResponse) -> bool {
    let is_invite_2xx =
        (200..300).contains(&resp.status()) && resp.cseq().method() == Method::Invite;
    let Some(id) = resolved.dialog.as_ref().filter(|_| is_invite_2xx) else { return false };
    let dialog = call::helpers::find_leg(call, leg_id)
        .and_then(|leg| leg.dialogs.by_tags(&id.our_tag, &id.peer_tag));
    dialog.is_some_and(|d| call::helpers::invite_round_ended(d, i64::from(resp.cseq().seq())))
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
