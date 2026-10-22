//! The dialog gate (ADR-0041): before any rule reads a message of a live call,
//! the message is resolved to the dialog it names on the leg it arrived on
//! (RFC 3261 §12.2), by its tags read in its direction, and refused here when
//! that dialog takes no such message. What passes reaches the rules with the
//! dialog — and for a response, the relay it answers — resolved
//! ([`call::helpers::Resolved`]).
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
//! takes a final (§15.1.2): a BYE crossing ours and a PRACK pass to their
//! rules; an INFO, MESSAGE, NOTIFY or OPTIONS is answered 200 here and goes
//! no further; any other request, an INVITE, UPDATE, REFER or SUBSCRIBE among
//! them, is answered 487 (§21.4.25). A request below its dialog's last CSeq
//! draws 500 (§12.2.2); one the gate passes or answers sets that last CSeq
//! ([`note_in_order`]). An ACK passes on a dialog
//! a 2xx established, ended by a BYE since or not (RFC 5407 §2); it is never
//! answered (§17.1.1.3), so one the gate refuses is dropped. A CANCEL is matched by
//! transaction (§9.1) and passes unread. A response whose From-tag is no tag
//! this stack put on the leg answers nothing of this stack's and is dropped.

use call::helpers::{response_origin, Resolved, ResponseFacts, ResponseOrigin};
use call::{Call, Dialog, DialogEnd, DialogState};
use sip_message::{Method, SipMessage, SipRequest, SipResponse};

use super::resolve::Resolution;
use super::responses::{
    build_200, build_200_advertising, build_481, build_487, build_out_of_order_500,
};
use super::RouterCtx;
use b2bua_sdk::event::CallEvent;

/// What the gate made of an event.
pub(super) enum Gate {
    /// The event reaches the rules, its message resolved.
    Pass(Resolved),
    /// A request answered here, with the answer sent (none for an ACK), and
    /// the dialog it was taken in order on, if any.
    Answered { answer: Option<Box<SipResponse>>, in_order: Option<Resolved> },
    /// A response that answers nothing of this stack's, dropped unread.
    Dropped,
}

/// Resolve `event` on `call` (its source leg `res.source_leg_id`), answering a
/// refused request on the wire.
pub(super) async fn admit(
    ctx: &RouterCtx,
    call: &Call,
    event: &CallEvent,
    res: &Resolution,
) -> Gate {
    let CallEvent::Sip { message, src, .. } = event else { return Gate::Pass(Resolved::default()) };
    let leg_id = res.source_leg_id.as_str();
    let req = match message.as_ref() {
        SipMessage::Response(resp) => return admit_response(call, leg_id, resp),
        SipMessage::Request(req) => req,
    };
    let (answer, in_order) = match request_verdict(call, leg_id, req) {
        Verdict::Pass(resolved) => return Gate::Pass(resolved),
        Verdict::Drop => return Gate::Answered { answer: None, in_order: None },
        Verdict::NoDialog => (build_481(req, None), None),
        Verdict::OutOfOrder => (build_out_of_order_500(req), None),
        // An OPTIONS 200 states what this stack supports (§11.2).
        Verdict::Absorbed(resolved) if *req.method() == Method::Options => {
            (build_200_advertising(req, &ctx.config.node_capabilities), Some(resolved))
        }
        Verdict::Absorbed(resolved) => (build_200(req), Some(resolved)),
        Verdict::Terminated(resolved) => (build_487(req), Some(resolved)),
    };
    // A request whose lines no response can echo is answered with none.
    let Ok(mut answer) = answer else { return Gate::Answered { answer: None, in_order } };
    // An in-call answer is a message of the leg it resolved to: it takes the
    // call's stated headers.
    for skipped in crate::rules::stated_headers::stamp_response(call, leg_id, &mut answer) {
        let detail = format!("stated header: {skipped}");
        crate::model_invariants::stated_text_skipped(&ctx.metrics, call, &detail);
    }
    let _ = ctx.txn.send_response(answer.clone(), *src).await;
    Gate::Answered { answer: Some(Box::new(answer)), in_order }
}

/// The gate's reading of a request.
#[derive(Debug, PartialEq)]
pub(super) enum Verdict {
    /// The request reaches the rules, naming this dialog.
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
}

/// How the gate reads `req` arriving on `leg_id` of `call`.
pub(super) fn request_verdict(call: &Call, leg_id: &str, req: &SipRequest) -> Verdict {
    let method = req.method();
    let (Some(to), false) = (req.to().tag(), *method == Method::Cancel) else {
        return Verdict::Pass(Resolved::default());
    };
    let Some(leg) = call::helpers::find_leg(call, leg_id) else {
        return Verdict::Pass(Resolved::default());
    };
    // A caller-facing dialog names nothing until the caller was shown its tag:
    // in a provisional, or in the 2xx that established it.
    let dialled = leg.outbound_invite.is_some();
    let dialog = leg
        .dialogs
        .of_request(Some(to), req.from().tag())
        .filter(|d| dialled || d.shown || d.was_established());
    let taken: fn(Resolved) -> Verdict = match admission(dialog, method) {
        Admission::Pass => Verdict::Pass,
        Admission::Absorb => Verdict::Absorbed,
        Admission::Terminate => Verdict::Terminated,
        Admission::Refuse if *method == Method::Ack => return Verdict::Drop,
        Admission::Refuse => return Verdict::NoDialog,
    };
    let Some(dialog) = dialog else { return Verdict::NoDialog };
    let cseq = i64::from(req.cseq().seq());
    if *method != Method::Ack && dialog.peer_cseq.is_some_and(|last| cseq < last) {
        return Verdict::OutOfOrder;
    }
    taken(Resolved { dialog: Some(dialog.id()), relay: None })
}

/// Note the request the gate passed in order on the dialog it names: its
/// CSeq becomes that dialog's remote sequence number (RFC 3261 §12.2.2). An
/// ACK numbers nothing (§13.2.2.4).
pub(super) fn note_in_order(
    call: Call,
    leg_id: &str,
    resolved: &Resolved,
    event: &CallEvent,
) -> Call {
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
pub(super) enum Admission {
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
pub(super) fn admission(dialog: Option<&Dialog>, method: &Method) -> Admission {
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
        DialogState::Retired(r) => r.release_in_flight(),
        DialogState::Ended(_) => false,
    };
    match method {
        _ if !ending => Admission::Refuse,
        Method::Bye | Method::Prack => Admission::Pass,
        Method::Info | Method::Message | Method::Notify | Method::Options => Admission::Absorb,
        _ => Admission::Terminate,
    }
}

/// Resolve the response `resp` arriving on `leg_id`.
fn admit_response(call: &Call, leg_id: &str, resp: &SipResponse) -> Gate {
    let cseq = resp.cseq();
    let facts = ResponseFacts {
        to: resp.to().tag(),
        from: resp.from().tag(),
        cseq: i64::from(cseq.seq()),
        method: cseq.method().as_str(),
        status: resp.status(),
    };
    match response_origin(call, leg_id, facts) {
        ResponseOrigin::Dialog(resolved) => Gate::Pass(resolved),
        ResponseOrigin::Creates | ResponseOrigin::Request => Gate::Pass(Resolved::default()),
        ResponseOrigin::Stray => Gate::Dropped,
    }
}
