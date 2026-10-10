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
//! | `Ending` (this side's BYE in flight) | BYE, OPTIONS, PRACK pass; others 481 |
//! | `Ended(Rejected)` | PRACK passes (RFC 3262 §3); others 481 |
//! | `Ended` by a BYE | 481, a BYE included (§15.1.2) |
//! | `Retired` | 481, but a BYE crossing this stack's release of it |
//!
//! A live dialog of a call this stack is ending reads as `Ending`. A request
//! below its dialog's last CSeq draws 500 (§12.2.2). An ACK passes on a dialog
//! a 2xx established, ended by a BYE since or not (RFC 5407 §2); it is never
//! answered (§17.1.1.3), so one the gate refuses is dropped. A CANCEL is matched by
//! transaction (§9.1) and passes unread. A response whose From-tag is no tag
//! this stack put on the leg answers nothing of this stack's and is dropped.

use call::helpers::{response_origin, Resolved, ResponseFacts, ResponseOrigin};
use call::{Call, CallModelState, Dialog, DialogEnd, DialogState};
use sip_message::{Method, SipMessage, SipRequest, SipResponse};

use super::resolve::Resolution;
use super::responses::{build_481, build_out_of_order_500};
use super::RouterCtx;
use b2bua_sdk::event::CallEvent;

/// What the gate made of an event.
pub(super) enum Gate {
    /// The event reaches the rules, its message resolved.
    Pass(Resolved),
    /// A request refused here, with the answer sent (none for an ACK).
    Refused(Option<SipResponse>),
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
    let refusal = match request_verdict(call, leg_id, req) {
        Verdict::Pass(resolved) => return Gate::Pass(resolved),
        Verdict::Drop => return Gate::Refused(None),
        Verdict::NoDialog => build_481(req, None),
        Verdict::OutOfOrder => build_out_of_order_500(req),
    };
    // An in-call refusal is a message of the leg it resolved to: it takes the
    // call's stated headers.
    let mut refusal = refusal;
    crate::rules::stated_headers::stamp_response(call, leg_id, &mut refusal);
    let _ = ctx.txn.send_response(refusal.clone(), *src).await;
    Gate::Refused(Some(refusal))
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
    let ending = matches!(call.state, CallModelState::Terminating | CallModelState::Terminated);
    if !admits(dialog, method, ending) {
        return if *method == Method::Ack { Verdict::Drop } else { Verdict::NoDialog };
    }
    let Some(dialog) = dialog else { return Verdict::Pass(Resolved::default()) };
    let cseq = i64::from(req.cseq().seq());
    if *method != Method::Ack && dialog.peer_cseq.is_some_and(|last| cseq < last) {
        return Verdict::OutOfOrder;
    }
    Verdict::Pass(Resolved { dialog: Some(dialog.id()), relay: None })
}

/// Whether `dialog` takes a request of `method`; `call_ending`: its call is
/// being torn down, so a live dialog of it reads as ending.
pub(super) fn admits(dialog: Option<&Dialog>, method: &Method, call_ending: bool) -> bool {
    let Some(d) = dialog else { return false };
    // The ACK of a 2xx belongs to its INVITE transaction and outlives the
    // dialog's BYE (RFC 5407 §2, Appendix D).
    if *method == Method::Ack {
        return d.is_live() || d.was_established();
    }
    let survives_end = || matches!(method, Method::Bye | Method::Options | Method::Prack);
    match &d.state {
        DialogState::Early | DialogState::Confirmed if !call_ending => true,
        DialogState::Early | DialogState::Confirmed | DialogState::Ending => survives_end(),
        DialogState::Ended(DialogEnd::Rejected(_)) => *method == Method::Prack,
        DialogState::Ended(_) => false,
        DialogState::Retired(r) => *method == Method::Bye && r.release.is_some(),
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
