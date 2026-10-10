//! The limiter refused a route on the call's own turn: the `/call/failure`
//! consult it raises, and the treatment that answers it — a failover route
//! applied as the route was, or the call's final.

use call::helpers::mark_decision;
use call::{Call, DecisionKind, TerminationCause};
use sip_clock::Clock;
use sip_message::SipRequest;
use sip_txn::IdGen;

use crate::config::B2buaConfig;
use crate::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallTreatment, FailureInfo,
};
use crate::effects::HandlerResult;
use crate::limiter::LimiterWorker;

use super::apply_route::MAX_LIMITER_FAILOVER;

/// The limiter refused this route: consult `/call/failure` for a failover
/// treatment and apply it, or answer `486 Busy Here` when the call has no
/// callback context to fail over with — or has already burned
/// [`MAX_LIMITER_FAILOVER`] hops, so a plan that keeps returning limited
/// destinations terminates instead of looping.
#[allow(clippy::too_many_arguments)]
pub(super) async fn limiter_reject_failover(
    mut call: Call,
    limiter_id: String,
    a_invite: &SipRequest,
    invite_wire: &[u8],
    decision: &dyn CallDecisionEngine,
    limiter: &LimiterWorker,
    config: &B2buaConfig,
    id_gen: &IdGen,
    clock: &Clock,
    now_ms: i64,
    depth: u32,
) -> HandlerResult {
    if call.callback_context.is_none() || depth >= MAX_LIMITER_FAILOVER {
        return crate::initial_invite::reject_call(
            call,
            a_invite,
            486,
            Some("Busy Here".into()),
            None,
            &[],
            &config.minted_final_advertisement,
            id_gen,
            now_ms,
            TerminationCause::Admission,
        );
    }
    let req = limiter_failure_request(&call, &limiter_id);
    let mut request_json = crate::trace::intake::request_body(&call, &req);
    let sent_at_ms = clock.now_ms();
    let response = decision.call_failure(req).await;
    let received_at_ms = clock.now_ms();
    // A failover route carrying `trace: true` opens the trace right here, so
    // activate before recording: the consult that turned tracing on is the first
    // thing its child span carries. The rebuild is derived from the same call,
    // so it IS the body that went out.
    if let Ok(CallTreatment::Route(route2)) = &response {
        if route2.trace && !crate::trace::sampled(&call) {
            let rebuilt =
                crate::trace::intake::json_body(&limiter_failure_request(&call, &limiter_id));
            if crate::trace::intake::force_enable(&mut call, invite_wire, now_ms) {
                request_json = Some(rebuilt);
            }
        }
    }
    record_failure_round_trip(&call, request_json, &response, sent_at_ms, received_at_ms);
    match response {
        Ok(CallTreatment::Route(route2)) => {
            Box::pin(super::apply_route::apply_route(
                call,
                route2,
                a_invite,
                invite_wire,
                decision,
                limiter,
                config,
                id_gen,
                clock,
                now_ms,
                depth + 1,
            ))
            .await
        }
        // A limiter refusal names no failed leg: these marks carry none.
        Ok(CallTreatment::Reject(rj)) => super::apply_reject::apply_reject(
            call,
            rj,
            DecisionKind::FailoverReject,
            None,
            a_invite,
            &config.minted_final_advertisement,
            id_gen,
            now_ms,
        ),
        Ok(CallTreatment::Redirect(rd)) => super::apply_reject::apply_redirect(
            call,
            rd,
            DecisionKind::FailoverRedirect,
            None,
            a_invite,
            &config.minted_final_advertisement,
            id_gen,
            now_ms,
        ),
        // Relay with no captured failure (a limiter reject is pre-leg) → 480
        // fallback (ADR-0017 X5); a backend error → the final the engine
        // stated, else 486 Busy Here: the stack's own, no decision behind it
        // and no mark.
        Ok(CallTreatment::Relay { label }) => {
            let call = mark_decision(call, now_ms, DecisionKind::FailoverTerminate, None, label);
            crate::initial_invite::reject_call(
                call,
                a_invite,
                480,
                Some("Temporarily Unavailable".into()),
                None,
                &[],
                &config.minted_final_advertisement,
                id_gen,
                now_ms,
                TerminationCause::DecisionReject,
            )
        }
        Err(CallDecisionError::Refused { code, reason, update_headers }) => {
            crate::initial_invite::reject_call(
                call,
                a_invite,
                code,
                reason,
                update_headers.as_ref(),
                &[],
                &config.minted_final_advertisement,
                id_gen,
                now_ms,
                TerminationCause::Admission,
            )
        }
        Err(CallDecisionError::Unavailable(_)) => crate::initial_invite::reject_call(
            call,
            a_invite,
            486,
            Some("Busy Here".into()),
            None,
            &[],
            &config.minted_final_advertisement,
            id_gen,
            now_ms,
            TerminationCause::Admission,
        ),
    }
}

/// The `/call/failure` consult a limiter reject raises: origin `call_limiter`,
/// the refusing limiter, and the call's context snapshot. Pure — the trace path
/// rebuilds it after a late activation, and the rebuild must equal what was sent.
fn limiter_failure_request(call: &Call, limiter_id: &str) -> CallFailureRequest {
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

/// Record the limiter-reject `/call/failure` consult as a child span on a traced
/// call (ADR-0026), with the times the request left and the response landed.
/// `request_json` is `None` for an unsampled call, which is also when this
/// returns immediately — nothing was serialized for it.
fn record_failure_round_trip(
    call: &Call,
    request_json: Option<Vec<u8>>,
    response: &Result<CallTreatment, crate::decision::CallDecisionError>,
    sent_at_ms: i64,
    received_at_ms: i64,
) {
    let Some(request) = request_json else {
        return;
    };
    let (outcome, body) = match response {
        Ok(treatment) => (
            match treatment {
                CallTreatment::Route(_) => "route",
                CallTreatment::Redirect(_) => "redirect",
                CallTreatment::Reject(_) => "reject",
                CallTreatment::Relay { .. } => "relay",
            },
            crate::trace::intake::json_body(treatment),
        ),
        Err(err) => crate::trace::intake::error_outcome(err),
    };
    crate::trace::emit::round_trip(
        call,
        "/call/failure",
        sent_at_ms,
        &request,
        received_at_ms,
        outcome,
        &body,
    );
}
