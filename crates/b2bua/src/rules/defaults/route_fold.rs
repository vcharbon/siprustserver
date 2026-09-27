//! The shared decoder for **route-shaped internal-event payloads** (built by
//! the router's `route_result_payload`) and the parity actions both async
//! route folds — `failover-create-leg` (`call-failure-result`) and
//! `release-reroute` (`call-release-result`) — must apply identically, and the
//! reader of the limiter state those consults' folds carry
//! ([`route_fold_limiter_state`]). One parser +
//! one parity-action builder so the folds cannot drift from each other or from
//! the initial `apply_route`.

use call::{CallModelState, TimerType};

use crate::event::CallEvent;
use crate::rules::model::{RuleAction, RuleContext, TimerDelay};
use b2bua_sdk::header_update::payload_lines;

/// Whether a decision fold has landed on a call already going away — the
/// call-scoped clause of [`call::helpers::leg_is_going_away`]. A `/calls`
/// result (route or reject, any outcome) applied to a `Terminating`/
/// `Terminated` call is moot: the caller already holds its final, so the fold
/// drives no forward progress — no new leg toward a callee whose caller is
/// gone, no second final on the a-leg's completed transaction (RFC 3261
/// §17.2.1). The termination in progress owns the teardown; the limiter state
/// a route fold carries still becomes the call's ([`route_fold_limiter_state`]).
pub(crate) fn fold_lands_on_going_away_call(ctx: &RuleContext) -> bool {
    matches!(ctx.call.state(), CallModelState::Terminating | CallModelState::Terminated)
}

/// Parse a `call-failure-result` payload's `update_headers` object into the
/// `(name, line-or-removal)` pairs the response/leg builders consume — one pair
/// per stated line, in the stated order.
pub(crate) fn parse_header_updates(payload: &serde_json::Value) -> Vec<(String, Option<String>)> {
    payload_lines(payload.get("update_headers"))
}

/// The decoded fields of a route-shaped payload.
pub(crate) struct RouteFold {
    pub destination: (String, u16),
    pub new_ruri: Option<String>,
    pub new_from: Option<String>,
    pub new_to: Option<String>,
    pub no_answer: Option<i64>,
    pub callback_context: Option<String>,
    pub header_updates: Vec<(String, Option<String>)>,
    pub features: Option<call::features::FeatureActivations>,
    pub service_ext: call::ExtMap,
    /// `None` = field absent from the payload (an old emitter) → leave the
    /// call's registry untouched; `Some` (possibly empty) = the route owns it.
    pub subscriptions: Option<Vec<call::ReleaseEventKind>>,
    /// `update_body` wire shape: absent = keep A's INVITE body, null = drop
    /// (`Some(vec![])`), string = substitute.
    pub body_override: Option<Vec<u8>>,
    /// The call's admission state after the dispatching task replaced its
    /// set on the limiter; `None` on a payload that carries none.
    pub limiter: Option<call::CallLimiterState>,
}

/// Parse a route-shaped payload. `None` only when the mandatory
/// `destination.host` is missing (a malformed fold).
pub(crate) fn parse_route_fold(payload: &serde_json::Value) -> Option<RouteFold> {
    let host = payload
        .get("destination")
        .and_then(|d| d.get("host"))
        .and_then(|v| v.as_str())?
        .to_string();
    // Absent ⇒ RFC 3261 §19.1.2's 5060; stated-but-no-port ⇒ no fold, so the
    // reroute never dials 5060 on a host the decision did not name. The
    // payload is this stack's own round-trip of a typed `Option<u16>`
    // (`callouts::route_payload`), so the refusal is unreachable by
    // construction — it is the reader's contract, not a live branch.
    let port =
        crate::decision::read_stated_port(payload.get("destination").and_then(|d| d.get("port")))?;
    Some(RouteFold {
        destination: (host, port),
        new_ruri: payload.get("new_ruri").and_then(|v| v.as_str()).map(str::to_string),
        new_from: payload.get("new_from").and_then(|v| v.as_str()).map(str::to_string),
        new_to: payload.get("new_to").and_then(|v| v.as_str()).map(str::to_string),
        no_answer: payload.get("no_answer_timeout_sec").and_then(|v| v.as_i64()),
        callback_context: payload
            .get("callback_context")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        header_updates: parse_header_updates(payload),
        features: payload.get("features").and_then(|v| serde_json::from_value(v.clone()).ok()),
        service_ext: parse_service_ext(payload),
        subscriptions: payload
            .get("subscriptions")
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
        body_override: match payload.get("update_body") {
            None => None,
            Some(serde_json::Value::Null) => Some(Vec::new()),
            Some(serde_json::Value::String(s)) => Some(s.clone().into_bytes()),
            Some(_) => None,
        },
        limiter: admitted_state(payload),
    })
}

/// The `(topic, outcome)` of the folds whose dispatching task may have changed
/// the call's set on the limiter: the two route-shaped folds (a failover
/// route, a release reroute), and the resolutions a refused route ends in (the
/// failure chain's reject, redirect or terminate after a refusal, a release
/// whose reroute was refused). Only these carry a limiter state.
const ROUTE_FOLDS: [(&str, &str); 6] = [
    ("call-failure-result", "failover"),
    ("call-failure-result", "reject"),
    ("call-failure-result", "redirect"),
    ("call-failure-result", "terminate"),
    ("call-release-result", "reroute"),
    ("call-release-result", "release"),
];

/// The call's admission state a route fold carries: its dispatching task
/// replaced the call's set on the limiter, or a refusal dropped it, before the
/// fold was posted, so the call the fold names owns that outcome from then
/// on — stated on its record, or released when no call is left to state it
/// on. `None` for any other event, or a fold whose task left the set as it
/// was.
pub(crate) fn route_fold_limiter_state(event: &CallEvent) -> Option<call::CallLimiterState> {
    let CallEvent::InternalEvent { topic, outcome, payload, .. } = event else {
        return None;
    };
    if !ROUTE_FOLDS.iter().any(|(t, o)| t == topic && o == outcome) {
        return None;
    }
    admitted_state(payload)
}

/// The [`RuleAction::SetLimiterState`] a non-route resolution states when its
/// fold carries a limiter state (a refused route dropped the call's set).
pub(crate) fn fold_limiter_state_action(event: &CallEvent) -> Option<RuleAction> {
    route_fold_limiter_state(event).map(set_limiter_state)
}

/// The [`RuleAction::SetLimiterState`] stating `limiter` on the call.
pub(crate) fn set_limiter_state(limiter: call::CallLimiterState) -> RuleAction {
    RuleAction::SetLimiterState {
        key: limiter.key,
        counted: limiter.counted,
        release_owed: limiter.release_owed,
        ids: limiter.ids,
    }
}

/// A route payload's `call_limiter` object: `None` when absent or malformed.
fn admitted_state(payload: &serde_json::Value) -> Option<call::CallLimiterState> {
    serde_json::from_value(payload.get("call_limiter")?.clone()).ok()
}

/// The decision's `label` on a fold payload, read by the router's fold mark
/// (`decision_log::fold_decided`): absent (or not a string) = none.
pub(crate) fn parse_label(payload: &serde_json::Value) -> Option<String> {
    payload.get("label").and_then(|v| v.as_str()).map(str::to_string)
}

/// The decision's `service_ext` on a fold payload — a reject's, a redirect's or
/// a release's, merged as a route's are; a core-reserved key is not a service
/// slice (ADR-0016).
pub(crate) fn parse_service_ext(payload: &serde_json::Value) -> call::ExtMap {
    payload
        .get("service_ext")
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .filter(|(k, _)| !crate::rules::relay::is_core_reserved_ext(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// The output-parity bookkeeping actions BOTH async route folds emit before
/// their `CreateLeg` — what the initial `apply_route` applies at route time:
/// features (incl. the GlobalDuration re-arm), service_ext merge, the
/// release-subscription registry, and the limiter state (+ the LimiterRefresh
/// cadence that keeps a counted call's lease alive). The applied route owns
/// the call's set: the dispatching task replaced it on the limiter, and the
/// fold states whether the call is counted.
pub(crate) fn route_fold_parity_actions(fold: &RouteFold, ctx: &RuleContext) -> Vec<RuleAction> {
    let mut actions = Vec::new();
    if let Some(f) = &fold.features {
        // Re-arm the duration cap from the reroute's features, as the initial
        // path does at route time (ScheduleTimer id-dedups) — under the same
        // anchor: with the cap anchored at the answer, the answer this fold leads
        // to arms it (`MaxDurationAnchor`).
        if f.platform.arms_cap_at_creation(ctx.config.setup_timeout_sec) {
            actions.push(RuleAction::ScheduleTimer {
                timer_type: TimerType::GlobalDuration,
                delay: TimerDelay::secs(f.platform.max_duration_sec),
                leg_id: None,
            });
        }
        actions.push(RuleAction::SetFeatures { features: f.clone() });
    }
    if !fold.service_ext.is_empty() {
        actions.push(RuleAction::MergeCallExt { ext: fold.service_ext.clone() });
    }
    if let Some(events) = &fold.subscriptions {
        // The latest applied route OWNS the registry (empty clears), exactly
        // like `apply_route` on the initial path.
        actions.push(RuleAction::SetSubscriptions { events: events.clone() });
    }
    if let Some(limiter) = &fold.limiter {
        actions.push(set_limiter_state(limiter.clone()));
    }
    if fold.limiter.as_ref().is_some_and(|l| l.counted) {
        actions.push(RuleAction::ScheduleTimer {
            timer_type: TimerType::LimiterRefresh,
            delay: TimerDelay::secs(ctx.config.limiter_refresh_sec),
            leg_id: None,
        });
    }
    actions
}
