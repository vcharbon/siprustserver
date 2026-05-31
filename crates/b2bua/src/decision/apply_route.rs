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

    // ── relayFirst18xTo180 → strategy-aware Supported: 100rel + self-disable ─
    //
    // The B2BUA forwards alice's `Supported` to bob; the 18x-management policy
    // then strips `100rel` (and self-disables) depending on the strategy and
    // whether alice offered SDP. Port of `applyRoute.ts`'s Supported handling.
    if let crate::effects::OutboundBody::Request(req) = &mut effect.body {
        apply_supported_for_18x(req, a_invite, &mut call);
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

/// Forward alice's `Supported` onto the b-leg INVITE with strategy-aware
/// `100rel` handling, and self-disable the policy on the delayed-offer fallback.
/// Port of the `relayFirst18xTo180` block in `applyRoute.ts`:
///   - `drop-sdp`/`keep-sdp`: strip `100rel` (we never relay PRACK, alice was
///     not told to expect reliable provisional).
///   - `fake-prack` with alice SDP: keep `100rel` (bob goes reliable so we can
///     originate PRACK + cache his SDP).
///   - `fake-prack` with NO alice SDP (delayed offer): strip `100rel` AND
///     disable the policy (fall back to plain relay; no half-active state).
/// `promote-pem-to-200` is owned by the PEM service (Slice 4) and is left alone.
fn apply_supported_for_18x(invite: &mut SipRequest, a_invite: &SipRequest, call: &mut Call) {
    use call::features::RelayFirst18xStrategy;
    let strategy = match call::helpers::relay_first_18x_strategy(call) {
        Some(s) => s,
        None => return,
    };
    if strategy == RelayFirst18xStrategy::PromotePemTo200 {
        return; // PEM service owns this.
    }

    let alice_supported =
        sip_message::message_helpers::get_header(&a_invite.headers, "supported").map(str::to_string);
    let ct = sip_message::message_helpers::get_header(&a_invite.headers, "content-type")
        .unwrap_or("");
    let alice_has_sdp =
        !a_invite.body.is_empty() && ct.to_ascii_lowercase().contains("application/sdp");

    let keep_100rel = strategy == RelayFirst18xStrategy::FakePrack && alice_has_sdp;

    // Self-disable on the fake-prack delayed-offer fallback.
    if strategy == RelayFirst18xStrategy::FakePrack && !alice_has_sdp {
        if let Some(f) = call.features.as_mut() {
            f.relay_first_18x_to_180 = None;
        }
    }

    // Compute the Supported value to forward to bob.
    let supported_out: Option<String> = match &alice_supported {
        None => None,
        Some(v) => {
            if keep_100rel {
                Some(v.clone())
            } else {
                let kept: Vec<&str> = v
                    .split(',')
                    .map(str::trim)
                    .filter(|t| !t.eq_ignore_ascii_case("100rel") && !t.is_empty())
                    .collect();
                if kept.is_empty() {
                    None
                } else {
                    Some(kept.join(", "))
                }
            }
        }
    };

    // The b-leg INVITE has no Supported yet (build_b_leg omits it); set or drop.
    invite.headers.retain(|h| !h.name.eq_ignore_ascii_case("supported"));
    if let Some(val) = supported_out {
        invite.headers.push(sip_message::SipHeader {
            name: "Supported".to_string(),
            value: val,
        });
    }
}
