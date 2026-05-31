//! `apply_route` — translate a "route" decision into call state + the outbound
//! b-leg INVITE. Port of `decision/apply/applyRoute.ts` (the load-bearing path:
//! attach features, seed service ext, run the limiter, create the b-leg).

use call::helpers::set_call_ext;
use call::{Call, CallLimiterState, CdrEvent, CdrEventType, TimerEntry, TimerType};
use sip_message::SipRequest;
use sip_txn::IdGen;

use crate::config::B2buaConfig;
use crate::effects::{CriticalStateEffect, HandlerEffects, HandlerResult};
use crate::limiter::CallLimiter;
use crate::rules::relay;

use super::schemas::{BodyUpdate, RouteDecision};

/// Apply a route decision to `call` (which already carries the a-leg), creating
/// the first b-leg + its outbound INVITE.
pub async fn apply_route(
    mut call: Call,
    route: RouteDecision,
    a_invite: &SipRequest,
    limiter: &dyn CallLimiter,
    config: &B2buaConfig,
    id_gen: &IdGen,
    now_ms: i64,
) -> HandlerResult {
    let mut fx = HandlerEffects::new();

    call.features = Some(route.features.clone());
    call.callback_context = route.callback_context.clone();

    // Seed per-service ext slices (service-layer activation gate).
    for (service_id, value) in route.service_ext {
        call = set_call_ext(call, &service_id, Some(value));
    }

    // Admission control (no-op limiter this slice; the seam is faithful).
    for entry in &route.call_limiter {
        let admitted = limiter.check_and_increment(&entry.id, entry.limit).await;
        call.limiter_entries.push(CallLimiterState {
            limiter_id: entry.id.clone(),
            limit: entry.limit,
            origin_window: now_ms / 1000,
            increment_succeeded: Some(admitted),
        });
    }

    let leg_id = "b-1";
    let dest = (route.destination.host.clone(), route.destination.port());
    let no_answer = route
        .no_answer_timeout_sec
        .or(route.features.no_answer_timeout_sec);
    let (mut leg, mut effect) = relay::build_b_leg(
        &call.call_ref,
        leg_id,
        a_invite,
        dest,
        route.new_ruri.as_deref(),
        no_answer,
        config,
        id_gen,
    );

    // Body substitution on the b-leg INVITE (route.update_body).
    if let crate::effects::OutboundBody::Request(req) = &mut effect.body {
        match &route.update_body {
            BodyUpdate::Keep => {}
            BodyUpdate::Drop => {
                req.body.clear();
                if let Some(d) = leg.dialogs.first_mut() {
                    d.ext.cached_sdp = None;
                }
            }
            BodyUpdate::Replace(s) => req.body = s.clone().into_bytes(),
        }
    }

    call.b_legs.push(leg);
    call.cdr_events.push(CdrEvent {
        event_type: CdrEventType::InviteSent,
        timestamp: now_ms,
        leg_id: leg_id.to_string(),
        status_code: None,
        reason: None,
    });
    fx.outbound.push(effect);

    // No-answer ring timer (cancelled by confirm-dialog).
    if let Some(secs) = no_answer {
        let entry = TimerEntry {
            id: format!("NoAnswer:{leg_id}"),
            timer_type: TimerType::NoAnswer,
            fire_at: now_ms + secs * 1000,
            leg_id: Some(leg_id.to_string()),
        };
        call.timers.push(entry.clone());
        fx.critical.push(CriticalStateEffect::ScheduleTimer(entry));
    }

    HandlerResult { call, effects: fx }
}
