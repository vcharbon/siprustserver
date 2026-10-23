//! The initial INVITE's consult (ADR-0052 §7): the trace intake, the decision
//! round trip with the clock readings around it, and the route applied with
//! its admit, each step between two awaits a fold of
//! [`crate::initial_invite`]. A setup its caller CANCELed before its turn ran
//! asks the decision engine nothing ([`cancelled_setup`]).

use call::{Call, Direction};
use sip_clock::Clock;
use sip_message::SipRequest;

use crate::config::B2buaConfig;
use crate::decision::apply_route::apply_route;
use crate::decision::{CallDecisionEngine, NewCallResponse};
use crate::effects::{HandlerEffects, HandlerResult};
use crate::initial_invite::{build_request, setup_event, treatment, Initial};
use crate::limiter::LimiterWorker;
use crate::rules::{seed_services, ActionExecutor, ServiceDef};
use crate::trace;
use crate::turn_scope::TurnScope;

/// The initial-INVITE turn of a setup its caller CANCELed before the turn ran:
/// the call opens its trace (ADR-0026), and nothing else happens — no
/// decision request, no limiter admit, no wire effect. The `Cancelled` turn
/// queued behind it ends the call with its Cancel CDR.
pub fn cancelled_setup(
    mut call: Call,
    a_invite: &SipRequest,
    invite_wire: &[u8],
    now_ms: i64,
) -> HandlerResult {
    trace::intake::activate(&mut call, a_invite, invite_wire, now_ms);
    HandlerResult { call, effects: HandlerEffects::new() }
}

/// Run the initial-INVITE decision + route/reject. `call` must already carry the
/// a-leg (from [`crate::initial_invite::build_initial_call`]), and `a_invite` is
/// its snapshot rebuilt; `invite_wire` is the datagram it arrived as, which only
/// the router holds (the call carries a header snapshot, not bytes). `now_ms` is
/// the turn's timestamp — every CDR entry and timer deadline this handler mints
/// reads it, so it is taken once; `clock` measures the decision round trip,
/// whose two ends are by definition not the same instant.
#[allow(clippy::too_many_arguments)]
pub async fn handle_initial_invite(
    mut call: Call,
    a_invite: &SipRequest,
    decision: &dyn CallDecisionEngine,
    limiter: &LimiterWorker,
    config: &B2buaConfig,
    turn: &mut TurnScope,
    services: &[ServiceDef],
    invite_wire: &[u8],
    clock: &Clock,
    now_ms: i64,
) -> HandlerResult {
    let req = build_request(a_invite);

    // ── Trace intake (ADR-0026) ────────────────────────────────────────────
    // The admission chain runs here, once, before the decision round trip, so a
    // sampled call's story starts at its own INVITE. The consult's answer is
    // the other door (an engine force-enable, on this route or a later failover
    // one), and it backfills what it missed.
    trace::intake::activate(&mut call, a_invite, invite_wire, now_ms);
    let mut request_json = trace::intake::request_body(&call, &req);

    let sent_at_ms = clock.now_ms();
    let response = decision.new_call(req).await;
    let received_at_ms = clock.now_ms();
    if let Ok(NewCallResponse::Route(route)) = &response {
        // The route that turned the trace on arrived after its own request was
        // sent, so rebuild that request: it is derived from the a-leg INVITE, so
        // the rebuilt body IS the body that went out.
        if route.trace && trace::intake::force_enable(&mut call, invite_wire, now_ms) {
            request_json = Some(trace::intake::json_body(&build_request(a_invite)));
        }
    }
    trace::intake::record_new_call(&call, request_json, &response, sent_at_ms, received_at_ms);

    match treatment(call, response, a_invite, config, &mut turn.ids, now_ms) {
        Initial::Route { call, route } => {
            // The route built the Call; now run each service's `init` (ADR-0016
            // X8), folding any seed (cursor + data + initial actions) through the
            // normal executor.
            // A dormant service (`init` → `None`) and the empty-service list
            // both leave the result untouched.
            let result = apply_route(
                *call,
                *route,
                a_invite,
                invite_wire,
                decision,
                limiter,
                config,
                &mut turn.ids,
                clock,
                now_ms,
                0,
            )
            .await;
            let mut exec = ActionExecutor { config, turn, now_ms };
            let setup_event = setup_event(&result.call, a_invite);
            seed_services(result, services, &mut exec, &setup_event, "a", Direction::FromA)
        }
        Initial::Final(result) => *result,
    }
}
