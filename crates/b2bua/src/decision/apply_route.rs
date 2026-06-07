//! `apply_route` — translate a "route" decision into call state + the outbound
//! b-leg INVITE. Port of `decision/apply/applyRoute.ts` (the load-bearing path:
//! attach features, seed service ext, run the limiter, create the b-leg).

use call::helpers::set_call_ext;
use call::{Call, CallLimiterState, CdrEvent, CdrEventType, TimerEntry, TimerType};
use sip_message::SipRequest;
use sip_txn::IdGen;

use crate::config::B2buaConfig;
use crate::decision::{CallDecisionEngine, CallFailureRequest, CallTreatment, FailureInfo};
use crate::effects::{CriticalStateEffect, HandlerEffects, HandlerResult};
use crate::limiter::{AdmitOutcome, CallLimiter, LimiterEntry};
use crate::rules::relay;

use super::schemas::{BodyUpdate, RouteDecision};

/// Bound on chained limiter-reject failovers, so a misconfigured loop
/// (`/call/failure` keeps returning a limited destination) can't recurse forever.
const MAX_LIMITER_FAILOVER: u32 = 5;

/// Apply a route decision to `call` (which already carries the a-leg), creating
/// the first b-leg + its outbound INVITE. `depth` tracks chained limiter-reject
/// failovers (start at 0).
#[allow(clippy::too_many_arguments)]
pub async fn apply_route(
    mut call: Call,
    route: RouteDecision,
    a_invite: &SipRequest,
    decision: &dyn CallDecisionEngine,
    limiter: &dyn CallLimiter,
    config: &B2buaConfig,
    id_gen: &IdGen,
    now_ms: i64,
    depth: u32,
) -> HandlerResult {
    let mut fx = HandlerEffects::new();

    call.features = Some(route.features.clone());
    call.callback_context = route.callback_context.clone();

    // Seed per-service ext slices (service-layer activation gate).
    for (service_id, value) in route.service_ext {
        call = set_call_ext(call, &service_id, Some(value));
    }

    // Admission control: one BATCHED + TRANSACTIONAL admit for every limiter
    // entry — all increment, or none. The b2bua owns the fail-open policy.
    if !route.call_limiter.is_empty() {
        let entries: Vec<LimiterEntry> = route
            .call_limiter
            .iter()
            .map(|e| LimiterEntry {
                id: e.id.clone(),
                limit: e.limit,
            })
            .collect();
        match limiter.admit(&entries).await {
            AdmitOutcome::Admitted { window } => {
                for e in &route.call_limiter {
                    call.limiter_entries.push(CallLimiterState {
                        limiter_id: e.id.clone(),
                        limit: e.limit,
                        origin_window: window,
                        increment_succeeded: Some(true),
                    });
                }
                // Arm the refresh timer so a long call migrates its holds to the
                // current window before they age out of the summed lookback.
                let entry = TimerEntry {
                    id: format!("{:?}", TimerType::LimiterRefresh),
                    timer_type: TimerType::LimiterRefresh,
                    fire_at: now_ms + config.limiter_refresh_sec * 1000,
                    leg_id: None,
                };
                call.timers.push(entry.clone());
                fx.critical.push(CriticalStateEffect::ScheduleTimer(entry));
            }
            // Fail open: admit, record NO holds (nothing released or refreshed).
            AdmitOutcome::Unavailable => {}
            AdmitOutcome::Rejected { limiter_id } => {
                // Failover via /call/failure when a callback context is set
                // (bounded), else answer 486 Busy Here and terminate.
                if call.callback_context.is_some() && depth < MAX_LIMITER_FAILOVER {
                    let req = CallFailureRequest {
                        callback_context: call.callback_context.clone(),
                        failure: FailureInfo {
                            origin: "call_limiter".to_string(),
                            status_code: None,
                            limiter_id: Some(limiter_id),
                        },
                    };
                    match decision.call_failure(req).await {
                        Ok(CallTreatment::Route(route2)) => {
                            return Box::pin(apply_route(
                                call, route2, a_invite, decision, limiter, config, id_gen, now_ms,
                                depth + 1,
                            ))
                            .await;
                        }
                        Ok(CallTreatment::Reject(rj)) => {
                            return crate::initial_invite::reject_call(
                                call, a_invite, rj.reject_code, rj.reject_reason,
                                rj.update_headers.as_ref(), &[], id_gen, now_ms,
                            );
                        }
                        Ok(CallTreatment::Redirect(rd)) => {
                            return crate::initial_invite::reject_call(
                                call, a_invite, rd.code, rd.reason,
                                rd.update_headers.as_ref(), &rd.contacts, id_gen, now_ms,
                            );
                        }
                        // Relay with no captured failure (a limiter reject is
                        // pre-leg) → 480 fallback (ADR-0017 X5); backend error →
                        // 486 Busy Here (today's behaviour).
                        Ok(CallTreatment::Relay) => {
                            return crate::initial_invite::reject_call(
                                call, a_invite, 480, Some("Temporarily Unavailable".into()),
                                None, &[], id_gen, now_ms,
                            );
                        }
                        Err(_) => {
                            return crate::initial_invite::reject_call(
                                call, a_invite, 486, Some("Busy Here".into()), None, &[],
                                id_gen, now_ms,
                            );
                        }
                    }
                } else {
                    return crate::initial_invite::reject_call(
                        call, a_invite, 486, Some("Busy Here".into()), None, &[], id_gen, now_ms,
                    );
                }
            }
        }
    }

    // Announcement / deferred-routing services (ADR-0016 slice 8): when the
    // decision attaches a `service_ext` slice that defers routing (it set
    // `call.ext[<id>].defer_routing == true`), the normal destination leg is NOT
    // created here — the service's `init` owns leg creation (e.g. an unadopted
    // media leg toward an MRF, dialing the real destination later). The
    // GlobalDuration backstop below still arms for the call.
    if defers_routing(&call) {
        arm_global_duration(&mut call, &mut fx, route.features.platform.max_duration_sec, now_ms);
        return HandlerResult { call, effects: fx };
    }

    // NOTE: a DNS-name b-leg callee (e.g. a headless-StatefulSet `sipp-uas` pod
    // FQDN the UAC injects via `X-Api-Call.destination`) is NOT resolved here. The
    // B2BUA only routes the b-leg to the LB (via the `b2b_outbound_proxy` Route);
    // the LB resolves the Request-URI name to the pod IP and forwards it (kept off
    // the B2BUA so resolution/next-hop selection lives in one place — the proxy).
    // A per-pod name is single-A, so it resolves consistently across retransmits
    // and the call never splits across pods. The R-URI therefore carries the name
    // (set by the decision engine) straight through.
    let leg_id = "b-1";
    let dest = (route.destination.host.clone(), route.destination.port());
    let no_answer = route
        .no_answer_timeout_sec
        .or(route.features.no_answer_timeout_sec);
    // Additive header rewrites (PAI, PANI, any X-*). Structural From/To/R-URI go
    // through the typed fields below, never this map (ADR-0017 X2).
    let header_updates: Vec<(String, Option<String>)> = route
        .update_headers
        .as_ref()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    let (mut leg, mut effect) = relay::build_b_leg(
        &call.call_ref,
        leg_id,
        a_invite,
        dest,
        route.new_ruri.as_deref(),
        route.new_from.as_deref(),
        route.new_to.as_deref(),
        no_answer,
        config,
        id_gen,
        None,
        &header_updates,
        None,
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

    // Global-duration backstop, armed at *call creation* (not just at answer).
    //
    // A call enters `Active` the moment its a-leg is built (initial_invite), and
    // `confirm-dialog` is what arms GlobalDuration + Keepalive — so a call whose
    // b-leg INVITE never reaches a final response (lost 200, a UAS that drops the
    // INVITE under load, ring-forever) sits `Active` with NEITHER. Its only other
    // reaper is the NoAnswer ring timer, but that is armed *only* when the route
    // supplies `no_answer_timeout_sec` (the scripted endurance adapter supplies
    // `None`). The result: ~0.4% of calls reach `Active` with an empty
    // `call.timers`, so no timer ever fires and they leak forever — surviving past
    // even the 1h GlobalDuration cap because it was never armed (observed:
    // ~1095 ESTABLISHED calls flat for >4h on a never-killed worker).
    //
    // Arming GlobalDuration here gives every call the absolute duration cap as a
    // backstop. `confirm-dialog` (and the promote/18x confirm paths) re-arm the
    // same `GlobalDuration` id at answer with the same `max_duration_sec`, so an
    // answered call is unaffected (the re-arm supersedes via the driver's epoch
    // bump and `replace_timer_by_id`'s id-dedup); a stuck-in-setup call is now
    // reaped at the cap by the existing `max-duration` rule.
    arm_global_duration(&mut call, &mut fx, route.features.platform.max_duration_sec, now_ms);

    HandlerResult { call, effects: fx }
}

/// Arm the GlobalDuration absolute-cap backstop on the call (idempotent by id).
/// Factored out so the deferred-routing path arms it too.
fn arm_global_duration(call: &mut Call, fx: &mut HandlerEffects, max_duration_sec: i64, now_ms: i64) {
    let global = TimerEntry {
        id: format!("{:?}", TimerType::GlobalDuration),
        timer_type: TimerType::GlobalDuration,
        fire_at: now_ms + max_duration_sec * 1000,
        leg_id: None,
    };
    call.timers = call::helpers::replace_timer_by_id(std::mem::take(&mut call.timers), global.clone());
    fx.critical.push(CriticalStateEffect::ScheduleTimer(global));
}

/// Whether any service-ext slice asks the framework to defer normal destination
/// routing (its `defer_routing` flag is `true`), so the owning service's `init`
/// creates the legs instead (ADR-0016 slice 8). Generic — no service is named here.
fn defers_routing(call: &Call) -> bool {
    call.ext.as_ref().is_some_and(|ext| {
        ext.values()
            .any(|v| v.get("defer_routing").and_then(|d| d.as_bool()) == Some(true))
    })
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
