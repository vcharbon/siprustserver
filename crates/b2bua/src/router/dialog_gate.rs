//! The dialog gate's shell (ADR-0041): the verdict is the core's
//! ([`b2bua_core::dialog_gate`]); here a refused request is answered — 481,
//! 500, 200 (an OPTIONS stating what this stack supports, §11.2) or 487,
//! each under the call's stated headers — and whether another owner served
//! the call is read off this node's topology for the ACK the verdict asks it
//! of.

use call::helpers::Resolved;
use call::Call;
use sip_message::{Method, SipMessage, SipResponse};

use super::resolve::Resolution;
use super::responses::{
    build_200, build_200_advertising, build_481, build_487, build_out_of_order_500,
};
use super::RouterCtx;
use b2bua_core::dialog_gate::{verdict, Verdict};
use b2bua_sdk::event::CallEvent;

pub(super) use b2bua_core::dialog_gate::note_in_order;

/// What the gate made of an event.
pub(super) enum Gate {
    /// The event reaches the rules, its message resolved.
    Pass(Resolved),
    /// A request answered here, with its answer (none for an ACK) for the
    /// caller to send through the call's outbox, and the dialog it was taken
    /// in order on, if any.
    Answered { answer: Option<Box<SipResponse>>, in_order: Option<Resolved> },
    /// A response that answers nothing of this stack's, dropped unread.
    Dropped,
}

/// Resolve `event` on `call` (its source leg `res.source_leg_id`), answering a
/// refused request.
pub(super) fn admit(ctx: &RouterCtx, call: &Call, event: &CallEvent, res: &Resolution) -> Gate {
    let leg_id = res.source_leg_id.as_str();
    let served = || copy_served(ctx, call);
    let refused = match verdict(call, leg_id, event, &ctx.config, served) {
        Verdict::Pass(resolved) => return Gate::Pass(resolved),
        Verdict::Stray => return Gate::Dropped,
        Verdict::Drop => return Gate::Answered { answer: None, in_order: None },
        refused => refused,
    };
    let in_order = refused.in_order().cloned();
    let CallEvent::Sip { message, .. } = event else { return Gate::Dropped };
    let SipMessage::Request(req) = message.as_ref() else { return Gate::Dropped };
    let answer = match refused {
        // An OPTIONS 200 states what this stack supports (§11.2).
        Verdict::Absorbed(_) if *req.method() == Method::Options => {
            build_200_advertising(req, &ctx.config.node_capabilities)
        }
        Verdict::Absorbed(_) => build_200(req),
        Verdict::Terminated(_) => build_487(req),
        Verdict::OutOfOrder => build_out_of_order_500(req),
        Verdict::NoDialog | Verdict::Pass(_) | Verdict::Drop | Verdict::Stray => {
            build_481(req, None)
        }
    };
    // A request whose lines no response can echo is answered with none.
    let Ok(mut answer) = answer else { return Gate::Answered { answer: None, in_order } };
    // An in-call answer is a message of the leg it resolved to: it takes the
    // call's stated headers.
    for skipped in crate::rules::stated_headers::stamp_response(call, leg_id, &mut answer) {
        let detail = format!("stated header: {skipped}");
        crate::model_invariants::stated_text_skipped(&ctx.metrics, call, &detail);
    }
    Gate::Answered { answer: Some(Box::new(answer)), in_order }
}

/// Whether a copy served `call` ([`crate::store::CallTopology::copy_served`],
/// read on this node). Only then is an ACK of the caller's unanswered INVITE
/// evidence (ADR-0055 §4); otherwise the copy that answered it is live and its
/// write is on its way.
fn copy_served(ctx: &RouterCtx, call: &Call) -> bool {
    let this = &ctx.relay_owner.ordinal;
    ctx.state.topology(&call.call_ref).is_some_and(|t| t.copy_served(this))
}
