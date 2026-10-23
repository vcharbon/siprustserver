//! The limiter refused a route on the call's own turn: the `/call/failure`
//! consult it raises, awaited, and the treatment that answers it applied
//! ([`failover_fold`]) — a failover route applied as the route was, or the
//! call's final.

use call::Call;
use sip_clock::Clock;
use sip_ids::TurnIds;
use sip_message::SipRequest;

use crate::config::B2buaConfig;
use crate::decision::failover_fold::{self, Consult, Failover};
use crate::decision::{CallDecisionEngine, CallTreatment};
use crate::effects::HandlerResult;
use crate::limiter::LimiterWorker;

/// The limiter refused this route: consult `/call/failure` for a failover
/// treatment and apply it, or answer the call's final
/// ([`failover_fold::consult_or_busy`]).
#[allow(clippy::too_many_arguments)]
pub(super) async fn limiter_reject_failover(
    call: Call,
    limiter_id: String,
    a_invite: &SipRequest,
    invite_wire: &[u8],
    decision: &dyn CallDecisionEngine,
    limiter: &LimiterWorker,
    config: &B2buaConfig,
    ids: &mut TurnIds,
    clock: &Clock,
    now_ms: i64,
    depth: u32,
) -> HandlerResult {
    let consult =
        failover_fold::consult_or_busy(call, &limiter_id, a_invite, config, ids, now_ms, depth);
    let (mut call, req) = match consult {
        Consult::Final(result) => return *result,
        Consult::Raise { call, request } => (*call, *request),
    };
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
            let rebuilt = crate::trace::intake::json_body(&failover_fold::limiter_failure_request(
                &call,
                &limiter_id,
            ));
            if crate::trace::intake::force_enable(&mut call, invite_wire, now_ms) {
                request_json = Some(rebuilt);
            }
        }
    }
    record_failure_round_trip(&call, request_json, &response, sent_at_ms, received_at_ms);
    match failover_fold::treatment(call, response, a_invite, config, ids, now_ms) {
        Failover::Route { call, route } => {
            Box::pin(super::apply_route::apply_route(
                *call,
                *route,
                a_invite,
                invite_wire,
                decision,
                limiter,
                config,
                ids,
                clock,
                now_ms,
                depth + 1,
            ))
            .await
        }
        Failover::Final(result) => *result,
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
