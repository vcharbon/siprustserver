//! `apply_route` — translate a "route" decision into call state + the outbound
//! b-leg INVITE. Port of `decision/apply/applyRoute.ts` (the load-bearing path:
//! attach features, seed service ext, run the limiter, create the b-leg).

use call::helpers::{add_cdr_event, mark_decision, set_call_ext};
use call::{Call, CallLimiterState, CdrEvent, CdrEventType, DecisionKind, TimerEntry, TimerType};
use sip_clock::Clock;
use sip_message::SipRequest;
use sip_txn::IdGen;

use crate::config::B2buaConfig;
use crate::decision::{CallDecisionEngine, CallFailureRequest, CallTreatment, FailureInfo};
use crate::effects::{CriticalStateEffect, HandlerEffects, HandlerResult};
use crate::limiter::{AdmitOutcome, CallLimiter, LimiterEntry};
use crate::rules::capabilities;
use crate::rules::relay;
use crate::target_admission::{classify_admission, AdmissionVerdict};

use super::schemas::{BodyUpdate, RouteDecision};

/// Bound on chained limiter-reject failovers, so a misconfigured loop
/// (`/call/failure` keeps returning a limited destination) can't recurse
/// forever. Shared with the router's async-failover fold, which runs the same
/// admit → re-consult chain for a failover route.
pub(crate) const MAX_LIMITER_FAILOVER: u32 = 5;

/// Apply a route decision to `call` (which already carries the a-leg), creating
/// the first b-leg + its outbound INVITE. `depth` tracks chained limiter-reject
/// failovers (start at 0); `invite_wire` is the a-leg INVITE's datagram, which a
/// late trace activation backfills from. `now_ms` is the turn's timestamp (every
/// CDR entry and timer deadline below reads it); `clock` measures the failover
/// round trip.
#[allow(clippy::too_many_arguments)]
pub async fn apply_route(
    mut call: Call,
    route: RouteDecision,
    a_invite: &SipRequest,
    invite_wire: &[u8],
    decision: &dyn CallDecisionEngine,
    limiter: &dyn CallLimiter,
    config: &B2buaConfig,
    id_gen: &IdGen,
    clock: &Clock,
    now_ms: i64,
    depth: u32,
) -> HandlerResult {
    // RFC 3261 §16.3: a request whose hop budget is spent may not be passed on,
    // and a leg this element originates IS passing it on. The decision was still
    // consulted — an INVITE at 0 is a liveness probe the backend is meant to
    // answer — so this only ever fires where the backend routed one anyway, at
    // depth 0, which is what keeps a routing loop through this element finite.
    if sip_message::hops::hops_exhausted(a_invite) {
        return crate::initial_invite::reject_call(
            call,
            a_invite,
            483,
            Some("Too Many Hops".into()),
            None,
            &[],
            id_gen,
            now_ms,
        );
    }

    let mut fx = HandlerEffects::new();

    // The engine force-enable (ADR-0026 §3), honored on EVERY route that reaches
    // here — the initial one and every `/call/failure` failover route — so "trace
    // this call from here on" works mid-call. Idempotent for a sampled call; a
    // newly activated one is backfilled with the INVITE it arrived on.
    if route.trace {
        crate::trace::intake::force_enable(&mut call, invite_wire, now_ms);
    }

    // The withhold latch: a failover route that does not restate the withheld
    // option tags cannot restore one — union the standing list into this
    // route's declaration before it replaces the features (SetFeatures parity).
    let mut features = route.features.clone();
    features.latch_withheld_option_tags(call.features.as_ref());
    call.features = Some(features);
    call.callback_context = route.callback_context.clone();
    // Release-event subscription registry: recorded like
    // `features`, so it replicates and survives takeover. The latest applied
    // route owns the set (a limiter-reject failover recursion re-enters here
    // and overwrites with ITS route's subscriptions — decision-response
    // parity, same as `features`).
    call.subscriptions = route.subscriptions.clone();

    // Seed per-service ext slices (service-layer activation gate). A
    // core-reserved key is not a service slice and no service id may collide
    // with it (ADR-0016) — a decision response cannot write it.
    for (service_id, value) in route.service_ext {
        if crate::rules::relay::is_core_reserved_ext(&service_id) {
            continue;
        }
        call = set_call_ext(call, &service_id, Some(value));
    }

    // ── Target admission: reject non-IP non-allow-listed destinations early ──
    // Catches the case where call-control returns a bogus host (e.g. `kindlab`
    // from a misconfigured fixture, or a `.svc.cluster.local` name the K8s runner
    // constructs that has no live pod). Without this the host would flow to the
    // send path and block on `getaddrinfo`/`EAI_AGAIN`; admission is the cheap
    // early filter — emit `503` and terminate BEFORE any b-leg state / limiter
    // INCR is allocated (port of `applyRoute.ts`'s admission block). `reject_call`
    // is the Rust analogue of `buildAdmissionRejectResult` (503 + To-tag +
    // terminate effects + Reject CDR). Done before the limiter loop so a rejected
    // target never acquires a hold.
    if classify_admission(&route.destination.host, &config.worker_allowed_target_suffixes)
        == AdmissionVerdict::Reject
    {
        return crate::initial_invite::reject_call(
            call,
            a_invite,
            503,
            Some("Service Unavailable".into()),
            None,
            &[],
            id_gen,
            now_ms,
        );
    }

    // Admission control: one BATCHED + TRANSACTIONAL admit for every limiter
    // entry — all increment, or none. The b2bua owns the fail-open policy.
    if !route.call_limiter.is_empty() {
        let entries: Vec<LimiterEntry> = route
            .call_limiter
            .iter()
            .map(|e| LimiterEntry { id: e.id.clone(), limit: e.limit })
            .collect();
        let outcome = limiter.admit(&entries).await;
        if crate::trace::sampled(&call) {
            crate::trace::emit::limiter(
                &call,
                now_ms,
                "admit",
                &format!("{entries:?} -> {outcome:?}"),
            );
        }
        match outcome {
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
                return Box::pin(limiter_reject_failover(
                    call,
                    limiter_id,
                    a_invite,
                    invite_wire,
                    decision,
                    limiter,
                    config,
                    id_gen,
                    clock,
                    now_ms,
                    depth,
                ))
                .await;
            }
        }
    }

    // The route is admitted: it is what the call is handled under from here
    // on, and the leg it dials, or the service that dials for it, is stamped
    // under this mark. A route the hop budget, the target admission or the
    // limiter refused was never applied and is no mark; a limiter failover's
    // route answers no failed leg.
    let kind = if depth == 0 { DecisionKind::Route } else { DecisionKind::FailoverRoute };
    let leg_id = (depth == 0).then(|| "a".to_string());
    call = mark_decision(call, now_ms, kind, leg_id, route.label.clone());

    // Announcement / deferred-routing services (ADR-0016 slice 8): when the
    // decision attaches a `service_ext` slice that defers routing (it set
    // `call.ext[<id>].defer_routing == true`), the normal destination leg is NOT
    // created here — the service's `init` owns leg creation (e.g. an unadopted
    // media leg toward an MRF, dialing the real destination later). The
    // GlobalDuration backstop below still arms for the call under its anchor.
    if defers_routing(&call) {
        if route.features.platform.arms_cap_at_creation(config.setup_timeout_sec) {
            arm_global_duration(
                &mut call,
                &mut fx,
                route.features.platform.max_duration_sec,
                now_ms,
            );
        }
        arm_setup_timeout(&mut call, &mut fx, config.setup_timeout_sec, now_ms);
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
    // A route-supplied ring deadline above `bound − margin` is held under the
    // configured transaction bound so the CANCEL→487 exchange still completes
    // inside the live b-leg client transaction (`relay::clamp_no_answer`).
    let no_answer = route
        .no_answer_timeout_sec
        .or(route.features.no_answer_timeout_sec)
        .map(|secs| relay::clamp_no_answer(config, &call.call_ref, secs));
    // Additive header rewrites (PAI, PANI, any X-*). Structural From/To/R-URI go
    // through the typed fields below, never this map (ADR-0017 X2).
    let header_updates: Vec<(String, Option<String>)> = route
        .update_headers
        .as_ref()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    // Whether the INVITE this route mints carries an offer: the relayed
    // a-leg body under `Keep`, none under `Drop`, the substitute under
    // `Replace`. The strategy's withhold and the `fake-prack` delayed-offer
    // fallback both read it.
    let offers_sdp = match &route.update_body {
        BodyUpdate::Keep => relay::carries_sdp(a_invite),
        BodyUpdate::Drop => false,
        BodyUpdate::Replace(body) => !body.is_empty(),
    };
    // A decision field that does not read has no destination behind it: refuse
    // the route rather than originate toward a fabricated address (055). The
    // caller gets a final (ADR-0022's guarantee holds) and the CDR names the
    // field; the malformed text itself stays off the wire.
    let (mut leg, mut effect) = match relay::build_b_leg(
        &call.call_ref,
        leg_id,
        call.emergency == Some(true),
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
        &capabilities::relaying_for_leg(&call, leg_id, a_invite.headers()),
        call.features.as_ref().and_then(|f| f.charging_vector.as_ref()),
        &capabilities::withheld_option_tags(&call, None, offers_sdp),
        &capabilities::offered_option_tags(&call, None),
        None,
    ) {
        Ok(built) => built,
        Err(err) => {
            tracing::warn!(
                call_ref = %call.call_ref,
                detail = %err.detail(),
                "routing decision refused"
            );
            return crate::initial_invite::reject_call(
                call,
                a_invite,
                500,
                Some(err.to_string()),
                None,
                &[],
                id_gen,
                now_ms,
            );
        }
    };

    // After the mint read the armed strategy's withhold: the fallback the
    // INVITE was minted under is the one the call now runs.
    disable_fake_prack_on_a_delayed_offer(&mut call, offers_sdp);

    // Body substitution on the b-leg INVITE (route.update_body) — one
    // thaw/freeze, so the INVITE the wire sees and the image it carries stay
    // the same message.
    if let crate::effects::OutboundBody::Request(req) = &mut effect.body {
        let mut draft = req.thaw();
        match &route.update_body {
            BodyUpdate::Keep => {}
            BodyUpdate::Drop => {
                draft = draft.without_body();
                if let Some(d) = leg.dialogs.first_mut() {
                    d.ext.cached_sdp = None;
                }
            }
            BodyUpdate::Replace(s) => {
                draft = draft.with_body(s.clone().into_bytes().into());
                effect.provenance = crate::effects::Provenance::Authored;
            }
        }
        if let Ok(edited) = draft.freeze() {
            *req = edited;
        }
    }

    call.b_legs.push(leg);
    call = add_cdr_event(
        call,
        CdrEvent {
            event_type: CdrEventType::InviteSent,
            timestamp: now_ms,
            leg_id: leg_id.to_string(),
            status_code: None,
            reason: None,
            decision_ordinal: 0,
        },
    );
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
    //
    // Under the `Answer` anchor (`MaxDurationAnchor`) the cap bounds the
    // established call only, so it is armed at the answer alone whenever the
    // `SetupTimeout` deadline bounds the setup; with that deadline disabled the
    // creation-time backstop arms as above.
    if route.features.platform.arms_cap_at_creation(config.setup_timeout_sec) {
        arm_global_duration(&mut call, &mut fx, route.features.platform.max_duration_sec, now_ms);
    }
    arm_setup_timeout(&mut call, &mut fx, config.setup_timeout_sec, now_ms);

    HandlerResult { call, effects: fx }
}

/// The limiter refused this route: consult `/call/failure` for a failover
/// treatment and apply it, or answer `486 Busy Here` when the call has no
/// callback context to fail over with — or has already burned
/// [`MAX_LIMITER_FAILOVER`] hops, so a plan that keeps returning limited
/// destinations terminates instead of looping.
#[allow(clippy::too_many_arguments)]
async fn limiter_reject_failover(
    mut call: Call,
    limiter_id: String,
    a_invite: &SipRequest,
    invite_wire: &[u8],
    decision: &dyn CallDecisionEngine,
    limiter: &dyn CallLimiter,
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
            id_gen,
            now_ms,
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
            Box::pin(apply_route(
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
            id_gen,
            now_ms,
        ),
        Ok(CallTreatment::Redirect(rd)) => {
            let call = mark_decision(call, now_ms, DecisionKind::FailoverRedirect, None, rd.label);
            crate::initial_invite::reject_call(
                call,
                a_invite,
                rd.code,
                rd.reason,
                rd.update_headers.as_ref(),
                &rd.contacts,
                id_gen,
                now_ms,
            )
        }
        // Relay with no captured failure (a limiter reject is pre-leg) → 480
        // fallback (ADR-0017 X5); a backend error → 486 Busy Here, the
        // stack's own final, no decision behind it and no mark.
        Ok(CallTreatment::Relay { label }) => {
            let call = mark_decision(call, now_ms, DecisionKind::FailoverTerminate, None, label);
            crate::initial_invite::reject_call(
                call,
                a_invite,
                480,
                Some("Temporarily Unavailable".into()),
                None,
                &[],
                id_gen,
                now_ms,
            )
        }
        Err(_) => crate::initial_invite::reject_call(
            call,
            a_invite,
            486,
            Some("Busy Here".into()),
            None,
            &[],
            id_gen,
            now_ms,
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
        Err(err) => ("error", err.to_string().into_bytes()),
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

/// Arm the GlobalDuration absolute-cap backstop on the call (idempotent by id).
/// Factored out so the deferred-routing path arms it too.
fn arm_global_duration(
    call: &mut Call,
    fx: &mut HandlerEffects,
    max_duration_sec: i64,
    now_ms: i64,
) {
    let global = TimerEntry {
        id: format!("{:?}", TimerType::GlobalDuration),
        timer_type: TimerType::GlobalDuration,
        fire_at: now_ms + max_duration_sec * 1000,
        leg_id: None,
    };
    call.timers =
        call::helpers::replace_timer_by_id(std::mem::take(&mut call.timers), global.clone());
    fx.critical.push(CriticalStateEffect::ScheduleTimer(global));
}

/// Arm the call-level a-leg setup deadline (`SetupTimeout`) — armed once at
/// route time, **arm-if-absent** so a reroute (the limiter-failover recursion
/// re-enters `apply_route`) never extends the caller's total wait. Cancelled
/// at answer (`confirm-dialog` and the promote/18x confirm paths); fired by
/// the CORE `setup-timeout` rule (408 to A, CANCEL pending b-legs). Lives in
/// `call.timers`, so a reclaimed mid-setup call still carries its deadline —
/// the configured sip-txn initial-INVITE bound dies with a crashed node and
/// left such calls holding their limiter slots for the full GlobalDuration
/// (endurance 2026-06-12). `setup_timeout_sec <= 0` disables.
fn arm_setup_timeout(
    call: &mut Call,
    fx: &mut HandlerEffects,
    setup_timeout_sec: i64,
    now_ms: i64,
) {
    if setup_timeout_sec <= 0 {
        return;
    }
    let id = format!("{:?}", TimerType::SetupTimeout);
    if call.timers.iter().any(|t| t.id == id) {
        return;
    }
    let entry = TimerEntry {
        id,
        timer_type: TimerType::SetupTimeout,
        fire_at: now_ms + setup_timeout_sec * 1000,
        leg_id: None,
    };
    call.timers.push(entry.clone());
    fx.critical.push(CriticalStateEffect::ScheduleTimer(entry));
}

/// Whether any service-ext slice asks the framework to defer normal destination
/// routing (its `defer_routing` flag is `true`), so the owning service's `init`
/// creates the legs instead (ADR-0016 slice 8). Generic — no service is named here.
fn defers_routing(call: &Call) -> bool {
    call.ext.as_ref().is_some_and(|ext| {
        ext.values().any(|v| v.get("defer_routing").and_then(|d| d.as_bool()) == Some(true))
    })
}

/// The `fake-prack` delayed-offer fallback: an INVITE with no offer leaves
/// the stack nothing to acknowledge a reliable provisional's answer with, so
/// the strategy disables itself and the call falls back to plain relay — no
/// half-active state. The INVITE's `Supported` is the mint's
/// (`capabilities::withheld_option_tags` keeps `100rel` off it); nothing is
/// rewritten here. Every other strategy stands. The initial route alone
/// falls back: a leg minted later without an offer is kept unreliable by the
/// same withhold, and the mask stays up — the caller was shown one early
/// dialog and the 2xx owes it that To-tag (`relay_first_18x`).
fn disable_fake_prack_on_a_delayed_offer(call: &mut Call, offers_sdp: bool) {
    use call::features::RelayFirst18xStrategy;
    if offers_sdp
        || call::helpers::relay_first_18x_strategy(call) != Some(RelayFirst18xStrategy::FakePrack)
    {
        return;
    }
    if let Some(f) = call.features.as_mut() {
        f.relay_first_18x_to_180 = None;
    }
}
