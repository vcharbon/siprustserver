//! `apply_route` — translate a "route" decision into call state + the outbound
//! b-leg INVITE. Port of `decision/apply/applyRoute.ts` (the load-bearing path:
//! attach features, seed service ext, run the limiter, create the b-leg).

use b2bua_sdk::lifecycle::{AttemptOpening, RouteAnswer};
use call::helpers::{add_originated_b_leg, mark_decision};
use call::{AttemptCause, Call, Deadline, DecisionKind, TerminationCause, TimerKey, TimerType};
use sip_clock::Clock;
use sip_message::SipRequest;
use sip_txn::IdGen;

use crate::config::B2buaConfig;
use crate::decision::{header_lines, no_answer_deadline, CallDecisionEngine};
use crate::destination_allowlist::{classify_admission, AdmissionVerdict};
use crate::effects::{HandlerEffects, HandlerResult};
use crate::limiter::{AdmitOutcome, LimiterEntry, LimiterWorker};
use crate::metrics::AdmitSite;
use crate::rules::capabilities;
use crate::rules::relay;
use crate::timer_arm::arm;

use super::schemas::{BodyUpdate, RouteDecision};

/// Bound on chained limiter-reject failovers, so a misconfigured loop
/// (`/call/failure` keeps returning a limited destination) can't recurse
/// forever. Shared with the router's async-failover fold, which runs the same
/// admit → re-consult chain for a failover route.
pub(crate) const MAX_LIMITER_FAILOVER: u32 = 5;

/// Apply a route decision to `call` (which already carries the a-leg), creating
/// the first b-leg + its outbound INVITE under the attempt the route opens (a
/// route admission refuses is an attempt that ends `Refused`; a route whose
/// dial a service defers opens none here). `depth` tracks chained limiter-reject
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
    limiter: &LimiterWorker,
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
            &config.minted_final_advertisement,
            id_gen,
            now_ms,
            TerminationCause::Admission,
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

    // The call-lifetime latch: a failover route that does not restate the
    // withheld option tags cannot restore one — merged before its features
    // replace the standing ones (SetFeatures parity).
    let mut features = route.features.clone();
    features.latch_call_lifetime(call.features.as_ref());
    call.features = Some(features);
    call.callback_context = route.callback_context.clone();
    // Release-event subscription registry: recorded like
    // `features`, so it replicates and survives takeover. The latest applied
    // route owns the set (a limiter-reject failover recursion re-enters here
    // and overwrites with ITS route's subscriptions — decision-response
    // parity, same as `features`).
    call.subscriptions = route.subscriptions.clone();

    // The routing answer the attempt this route opens dials, as its services
    // read it.
    let answer = RouteAnswer { label: route.label.clone(), service_ext: route.service_ext.clone() };
    let cause = if depth == 0 { AttemptCause::Route } else { AttemptCause::Failover };
    let ring = no_answer_deadline(route.no_answer_ms, Some(&route.features));

    // Seed per-service ext slices (service-layer activation gate).
    call = super::apply_reject::seed_service_ext(call, route.service_ext);

    // ── Destination allow-list: reject non-IP non-allow-listed destinations early ──
    // Catches the case where call-control returns a bogus host (e.g. `kindlab`
    // from a misconfigured fixture, or a `.svc.cluster.local` name the K8s runner
    // constructs that has no live pod). Without this the host would flow to the
    // send path and block on `getaddrinfo`/`EAI_AGAIN`; admission is the cheap
    // early filter — emit `503` and terminate BEFORE any b-leg state or limiter
    // hold is allocated (port of `applyRoute.ts`'s admission block). `reject_call`
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
            &config.minted_final_advertisement,
            id_gen,
            now_ms,
            TerminationCause::Admission,
        );
    }

    if let Some(limiter_id) = admit_route(&mut call, &route.call_limiter, limiter, now_ms).await {
        // The refused route is an attempt that dialled nothing.
        crate::attempts::refuse(&mut call, &mut fx, cause, answer, ring);
        let mut result = Box::pin(super::limiter_failover::limiter_reject_failover(
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
        fx.extend(std::mem::take(&mut result.effects));
        result.effects = fx;
        return result;
    }

    // The route is admitted: it is what the call is handled under from here
    // on, and the leg it dials, or the service that dials for it, is stamped
    // under this mark. A route the hop budget, the destination allow-list or the
    // limiter refused was never applied and is no mark; a limiter failover's
    // route answers no failed leg.
    let kind = if depth == 0 { DecisionKind::Route } else { DecisionKind::FailoverRoute };
    let leg_id = (depth == 0).then(|| "a".to_string());
    call = mark_decision(call, now_ms, kind, leg_id, route.label.clone());

    // Announcement / deferred-routing services (ADR-0016): when the decision
    // attaches a `service_ext` slice that defers routing (it set
    // `call.ext[<id>].defer_routing == true`), the normal destination leg is NOT
    // created here — the service's `init` owns leg creation (e.g. an unadopted
    // media leg toward an MRF, dialing the real destination later). The
    // GlobalDuration backstop below still arms for the call under its anchor.
    if defers_routing(&call) {
        if route.features.platform.arms_cap_at_creation(config.setup_timeout_ms) {
            arm_global_duration(
                &mut call,
                &mut fx,
                route.features.platform.max_duration_ms,
                now_ms,
            );
        }
        arm_setup_timeout(&mut call, &mut fx, config.setup_timeout_ms, now_ms);
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
    let leg_id = call::helpers::next_b_leg_id(&call);
    let leg_id = leg_id.as_str();
    let dest = (route.destination.host.clone(), route.destination.port());
    // The decision's header statements (sets of one or more lines, removals).
    // Structural From/To/R-URI go through the typed fields below, never this
    // map (ADR-0017 X2).
    let header_updates: Vec<(String, Option<String>)> =
        route.update_headers.as_ref().map(header_lines).unwrap_or_default();
    // Whether the INVITE this route mints carries an offer: the relayed
    // a-leg description under `Keep` and `AttachParts`, none under `Drop`, the
    // substitute under `Replace`. The strategy's withhold and the `fake-prack`
    // delayed-offer fallback both read it.
    let offers_sdp = match &route.update_body {
        BodyUpdate::Keep | BodyUpdate::AttachParts(_) => relay::carries_sdp(a_invite),
        BodyUpdate::Drop => false,
        BodyUpdate::Replace(body) => !body.is_empty(),
    };
    // Attached parts are composed at the mint, beside the a-leg's description.
    let attached = match &route.update_body {
        BodyUpdate::AttachParts(parts) => Some(
            b2bua_sdk::model::Body::from_leg(a_invite.body().to_vec(), call.a_leg.leg_id.clone())
                .with_parts(parts.clone()),
        ),
        _ => None,
    };
    // A decision field that does not read has no destination behind it: refuse
    // the route rather than originate toward a fabricated address. The
    // caller gets a final (ADR-0022's guarantee holds) and the CDR names the
    // field; the malformed text itself stays off the wire.
    let (mut leg, mut effect) = match relay::build_b_leg(
        relay::CallMarks::of(&call),
        leg_id,
        a_invite,
        dest,
        route.new_ruri.as_deref(),
        route.new_from.as_deref(),
        route.new_to.as_deref(),
        config,
        id_gen,
        attached.as_ref(),
        &header_updates,
        &capabilities::relaying_for_leg(&call, leg_id, a_invite.headers()),
        crate::rules::charging::minting_arm(&call, leg_id, None),
        &capabilities::withheld_option_tags(&call, None, offers_sdp),
        &capabilities::offered_option_tags(&call, None),
        None,
        call.a_leg.invite_final_sent.is_none(),
        now_ms,
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
                &config.minted_final_advertisement,
                id_gen,
                now_ms,
                TerminationCause::Admission,
            );
        }
    };

    // The route's adds yield to the INVITE as minted.
    let adds = route.update_headers.as_ref().map(b2bua_sdk::header_update::header_adds);
    crate::rules::stated_headers::add_to_minted(&mut leg, &mut effect, &adds.unwrap_or_default());

    // After the mint read the armed strategy's withhold: the fallback the
    // INVITE was minted under is the one the call now runs.
    disable_fake_prack_on_a_delayed_offer(&mut call, offers_sdp);

    // Body substitution on the b-leg INVITE (route.update_body) — one
    // thaw/freeze, so the INVITE the wire sees and the image it carries stay
    // the same message.
    if let crate::effects::OutboundBody::Request(req) = &mut effect.body {
        let mut draft = req.thaw();
        match &route.update_body {
            BodyUpdate::Keep | BodyUpdate::AttachParts(_) => {}
            BodyUpdate::Drop => {
                draft = draft.without_body();
            }
            BodyUpdate::Replace(s) => {
                draft = draft.with_body(s.clone().into_bytes().into());
                effect.provenance = crate::effects::Provenance::Authored;
            }
        }
        if let Ok(edited) = draft.freeze() {
            *req = edited;
        }
        // The client-transaction handle holds the INVITE as it leaves; the
        // leg holds no dialog before a response to it.
        if let Some(handle) = leg.outbound_invite.as_mut().and_then(|i| i.txn.as_mut()) {
            handle.original_invite = req.image().to_vec();
        }
    }

    leg.sdp_session = relay::opened(
        &effect,
        match &route.update_body {
            BodyUpdate::Keep | BodyUpdate::AttachParts(_) => relay::Author::Leg(&call.a_leg.leg_id),
            BodyUpdate::Drop | BodyUpdate::Replace(_) => relay::Author::Stack,
        },
    );
    call = add_originated_b_leg(call, leg, now_ms);
    fx.outbound.push(effect);
    // The leg rings for the attempt's deadline from its first 18x
    // (`ring_deadline`).
    let attempt =
        crate::attempts::open(&mut call, &mut fx, AttemptOpening::new(cause, answer).ringing(ring));
    call::helpers::join_attempt(&mut call, attempt, leg_id);

    // Global-duration backstop, armed at *call creation* (not just at answer).
    //
    // A call enters `Active` the moment its a-leg is built (initial_invite), and
    // `confirm-dialog` is what arms GlobalDuration + Keepalive — so a call whose
    // b-leg INVITE never reaches a final response (lost 200, a UAS that drops the
    // INVITE under load, ring-forever) sits `Active` with NEITHER. Its only other
    // reaper is the NoAnswer ring timer, but that is armed *only* when the route
    // supplies `no_answer_ms` (the scripted endurance adapter supplies
    // `None`) and the leg rang. The result: ~0.4% of calls reach `Active` with an empty
    // `call.timers`, so no timer ever fires and they leak forever — surviving past
    // even the 1h GlobalDuration cap because it was never armed (observed:
    // ~1095 ESTABLISHED calls flat for >4h on a never-killed worker).
    //
    // Arming GlobalDuration here gives every call the absolute duration cap as a
    // backstop. `confirm-dialog` (and the promote/18x confirm paths) re-arm the
    // same `GlobalDuration` key at answer with the same `max_duration_ms`, so
    // an answered call is unaffected (the re-arm replaces the instance); a
    // stuck-in-setup call is now reaped at the cap by the existing
    // `max-duration` rule.
    //
    // Under the `Answer` anchor (`MaxDurationAnchor`) the cap bounds the
    // established call only, so it is armed at the answer alone whenever the
    // `SetupTimeout` deadline bounds the setup; with that deadline disabled the
    // creation-time backstop arms as above.
    if route.features.platform.arms_cap_at_creation(config.setup_timeout_ms) {
        arm_global_duration(&mut call, &mut fx, route.features.platform.max_duration_ms, now_ms);
    }
    arm_setup_timeout(&mut call, &mut fx, config.setup_timeout_ms, now_ms);

    HandlerResult { call, effects: fx }
}

/// Admission control: one admit of the route's whole limiter set, as
/// `stated`, keyed by the call and numbered by its next change — all or
/// none, nothing sent for a route stating none. The initial route holds
/// nothing yet, so a refusal releases nothing. The b2bua owns the fail-open
/// policy (`CallLimiterState::apply_admit`): a sent admit owes the call's
/// release whatever its answer; only an admitted set is counted, and the
/// turn's refresh invariant arms its refresh (`limiter::call::arm_refresh`).
/// The worker answers within the admit bound. The refusing limiter's id when
/// a cap refused the route.
async fn admit_route(
    call: &mut Call,
    stated: &[LimiterEntry],
    limiter: &LimiterWorker,
    now_ms: i64,
) -> Option<String> {
    if stated.is_empty() {
        return None;
    }
    let entries = stated.to_vec();
    let (change, held) = call.limiter.number_admit();
    let key = call.limiter.key().to_string();
    let report = limiter.admit(AdmitSite::Initial, &key, change, &held, entries, false).await;
    if crate::trace::sampled(call) {
        crate::trace::emit::limiter(
            call,
            now_ms,
            "admit",
            &format!("{:?} -> {:?}", report.entries, report.outcome),
        );
    }
    call.limiter.apply_admit(&report);
    match report.outcome {
        // Counted: the turn's refresh arming keeps its lease.
        AdmitOutcome::Admitted => None,
        // Fail open: the call runs uncounted (no refresh). A supersession or
        // a release fence needs another admit or a release of the key, minted
        // this turn: both unreachable (the worker counts a fence under
        // `site="initial"`).
        AdmitOutcome::Unavailable
        | AdmitOutcome::NotSent
        | AdmitOutcome::Superseded { .. }
        | AdmitOutcome::Released => None,
        AdmitOutcome::Rejected { limiter_id, .. } => Some(limiter_id),
    }
}

/// Arm the GlobalDuration absolute-cap backstop on the call, replacing the
/// instance it held. Factored out so the deferred-routing path arms it too.
fn arm_global_duration(
    call: &mut Call,
    fx: &mut HandlerEffects,
    max_duration_ms: i64,
    now_ms: i64,
) {
    let fire_at = now_ms + max_duration_ms;
    arm(call, fx, TimerKey::call(TimerType::GlobalDuration), fire_at);
}

/// Arm the call-level a-leg setup deadline (`SetupTimeout`), once: the
/// initial route's turn is the one turn that runs this, whichever route of
/// its limiter-failover chain lands, so no later attempt extends the caller's
/// total wait. Cancelled at answer (`confirm-dialog` and the promote/18x
/// confirm paths); fired by the CORE `setup-timeout` rule (408 to A, CANCEL
/// pending b-legs). Lives in `call.timers`, so a reclaimed mid-setup call
/// still carries its deadline — the configured sip-txn initial-INVITE bound
/// dies with a crashed node and would leave such calls holding their limiter
/// slots for the full GlobalDuration. A non-positive `setup_timeout_ms` is
/// off.
fn arm_setup_timeout(call: &mut Call, fx: &mut HandlerEffects, setup_timeout_ms: i64, now_ms: i64) {
    if let Some(ms) = Deadline::of_millis(Some(setup_timeout_ms)).millis() {
        arm(call, fx, TimerKey::call(TimerType::SetupTimeout), now_ms + ms);
    }
}

/// Whether any service-ext slice asks the framework to defer normal destination
/// routing (its `defer_routing` flag is `true`), so the owning service's `init`
/// creates the legs instead (ADR-0016). Generic — no service is named here.
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
