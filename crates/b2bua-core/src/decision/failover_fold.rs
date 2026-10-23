//! The limiter refused a route on the call's own turn: the folds around the
//! `/call/failure` consult it raises (ADR-0052 §7) — whether there is one to
//! raise, the request, and the treatment that answers it: a failover route,
//! to apply as the route was, or the call's final.

use call::helpers::mark_decision;
use call::{Call, DecisionKind, TerminationCause};
use sip_ids::TurnIds;
use sip_message::SipRequest;

use crate::config::B2buaConfig;
use crate::decision::{
    CallDecisionError, CallFailureRequest, CallTreatment, FailureInfo, RouteDecision,
    MAX_LIMITER_FAILOVER,
};
use crate::effects::HandlerResult;

/// Whether the refused route's call fails over.
pub enum Consult {
    /// It does: `request` is the consult to raise for `call`.
    Raise { call: Box<Call>, request: Box<CallFailureRequest> },
    /// It does not: the call's final.
    Final(Box<HandlerResult>),
}

/// The limiter refused this route: consult `/call/failure` for a failover
/// treatment, or answer `486 Busy Here` when the call has no callback context
/// to fail over with — or has already burned [`MAX_LIMITER_FAILOVER`] hops,
/// so a plan that keeps returning limited destinations terminates instead of
/// looping.
pub fn consult_or_busy(
    call: Call,
    limiter_id: &str,
    a_invite: &SipRequest,
    config: &B2buaConfig,
    ids: &mut TurnIds,
    now_ms: i64,
    depth: u32,
) -> Consult {
    if call.callback_context.is_none() || depth >= MAX_LIMITER_FAILOVER {
        return Consult::Final(Box::new(crate::initial_invite::reject_call(
            call,
            a_invite,
            486,
            Some("Busy Here".into()),
            None,
            &[],
            &config.minted_final_advertisement,
            ids,
            now_ms,
            TerminationCause::Admission,
        )));
    }
    let request = Box::new(limiter_failure_request(&call, limiter_id));
    Consult::Raise { call: Box::new(call), request }
}

/// The `/call/failure` consult a limiter reject raises: origin `call_limiter`,
/// the refusing limiter, and the call's context snapshot. Pure — the trace path
/// rebuilds it after a late activation, and the rebuild must equal what was sent.
pub fn limiter_failure_request(call: &Call, limiter_id: &str) -> CallFailureRequest {
    CallFailureRequest {
        callback_context: call.callback_context.clone(),
        failure: FailureInfo {
            origin: "call_limiter".to_string(),
            status_code: None,
            limiter_id: Some(limiter_id.to_string()),
            failed_leg_id: None,
            timeout_kind: None,
            sip_headers: Vec::new(),
        },
        snapshot: crate::decision::CallSnapshot::of(call),
    }
}

/// What the failover consult's answer leaves.
pub enum Failover {
    /// A failover route, applied as the route was.
    Route { call: Box<Call>, route: Box<RouteDecision> },
    /// The call's final.
    Final(Box<HandlerResult>),
}

/// The treatment `response` states for the limiter-refused `call`.
pub fn treatment(
    call: Call,
    response: Result<CallTreatment, CallDecisionError>,
    a_invite: &SipRequest,
    config: &B2buaConfig,
    ids: &mut TurnIds,
    now_ms: i64,
) -> Failover {
    let advertisement = &config.minted_final_advertisement;
    let result = match response {
        Ok(CallTreatment::Route(route)) => {
            return Failover::Route { call: Box::new(call), route: Box::new(route) }
        }
        // A limiter refusal names no failed leg: these marks carry none.
        Ok(CallTreatment::Reject(rj)) => super::apply_reject::apply_reject(
            call,
            rj,
            DecisionKind::FailoverReject,
            None,
            a_invite,
            advertisement,
            ids,
            now_ms,
        ),
        Ok(CallTreatment::Redirect(rd)) => super::apply_reject::apply_redirect(
            call,
            rd,
            DecisionKind::FailoverRedirect,
            None,
            a_invite,
            advertisement,
            ids,
            now_ms,
        ),
        // Relay with no captured failure (a limiter reject is pre-leg) → 480
        // fallback (ADR-0017 X5).
        Ok(CallTreatment::Relay { label }) => {
            let call = mark_decision(call, now_ms, DecisionKind::FailoverTerminate, None, label);
            crate::initial_invite::reject_call(
                call,
                a_invite,
                480,
                Some("Temporarily Unavailable".into()),
                None,
                &[],
                advertisement,
                ids,
                now_ms,
                TerminationCause::DecisionReject,
            )
        }
        // The engine's own refusal: the final it stated; no answer: a bare
        // 503 (ADR-0043). Either is the stack's own, no decision and no mark.
        Err(CallDecisionError::Refused { code, reason, update_headers }) => {
            super::apply_reject::stated_refusal(
                call,
                code,
                reason,
                update_headers.as_ref(),
                a_invite,
                advertisement,
                ids,
                now_ms,
            )
        }
        Err(CallDecisionError::Unavailable(_)) => crate::initial_invite::reject_call(
            call,
            a_invite,
            503,
            None,
            None,
            &[],
            advertisement,
            ids,
            now_ms,
            TerminationCause::Admission,
        ),
    };
    Failover::Final(Box::new(result))
}
