//! The shared decoder for **route-shaped internal-event payloads** (built by
//! the router's `route_result_payload`) and the parity actions both async
//! route folds — `failover-create-leg` (`call-failure-result`) and
//! `release-reroute` (`call-release-result`) — must apply identically.
//! One parser + one parity-action builder so the folds cannot drift from each
//! other or from the initial `apply_route`.

use call::{CallModelState, TimerType};

use crate::rules::model::{RuleAction, RuleContext, TimerDelay};

/// Whether a decision fold has landed on a call already going away — the
/// call-scoped clause of [`call::helpers::leg_is_going_away`]. A `/calls`
/// result (route or reject, any outcome) applied to a `Terminating`/
/// `Terminated` call is moot: the caller already holds its final, so the fold
/// drives no forward progress — no new leg toward a callee whose caller is
/// gone, no second final on the a-leg's completed transaction (RFC 3261
/// §17.2.1). The termination in progress owns the teardown; a limiter INCR
/// the dispatching callout already admitted ages out of its window.
pub(crate) fn fold_lands_on_going_away_call(ctx: &RuleContext) -> bool {
    matches!(
        ctx.call.state(),
        CallModelState::Terminating | CallModelState::Terminated
    )
}

/// Parse a `call-failure-result` payload's `update_headers` object into the
/// `(name, set-or-remove)` pairs the response/leg builders consume.
pub(super) fn parse_header_updates(payload: &serde_json::Value) -> Vec<(String, Option<String>)> {
    payload
        .get("update_headers")
        .and_then(|v| v.as_object())
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().map(str::to_string))).collect())
        .unwrap_or_default()
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
    /// Limiter holds the dispatching task already admitted: `(entries, window)`.
    pub limiter_holds: Option<(Vec<(String, i64)>, i64)>,
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
    let port = crate::decision::read_stated_port(
        payload.get("destination").and_then(|d| d.get("port")),
    )?;
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
        header_updates: payload
            .get("update_headers")
            .and_then(|v| v.as_object())
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().map(str::to_string))).collect())
            .unwrap_or_default(),
        features: payload
            .get("features")
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
        // A core-reserved key is not a service slice and no service id may
        // collide with it (ADR-0016) — a decision response cannot write it.
        service_ext: payload
            .get("service_ext")
            .and_then(|v| v.as_object())
            .map(|m| {
                m.iter()
                    .filter(|(k, _)| !crate::rules::relay::is_core_reserved_ext(k))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default(),
        subscriptions: payload
            .get("subscriptions")
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
        body_override: match payload.get("update_body") {
            None => None,
            Some(serde_json::Value::Null) => Some(Vec::new()),
            Some(serde_json::Value::String(s)) => Some(s.clone().into_bytes()),
            Some(_) => None,
        },
        limiter_holds: payload
            .get("call_limiter")
            .and_then(|v| v.as_object())
            .and_then(|o| {
                let window = o.get("window")?.as_i64()?;
                let entries: Vec<(String, i64)> = o
                    .get("entries")?
                    .as_array()?
                    .iter()
                    .filter_map(|e| {
                        Some((e.get("id")?.as_str()?.to_string(), e.get("limit")?.as_i64()?))
                    })
                    .collect();
                Some((entries, window))
            }),
    })
}

/// The output-parity bookkeeping actions BOTH async route folds emit before
/// their `CreateLeg` — what the initial `apply_route` applies at route time:
/// features (incl. the GlobalDuration re-arm), service_ext merge, the
/// release-subscription registry, and the already-admitted limiter holds
/// (+ the LimiterRefresh cadence that keeps them alive).
pub(crate) fn route_fold_parity_actions(fold: &RouteFold, ctx: &RuleContext) -> Vec<RuleAction> {
    let mut actions = Vec::new();
    if let Some(f) = &fold.features {
        // Re-arm the duration cap from the reroute's features, as the initial
        // path does at route time (ScheduleTimer id-dedups).
        actions.push(RuleAction::ScheduleTimer {
            timer_type: TimerType::GlobalDuration,
            delay: TimerDelay::secs(f.platform.max_duration_sec),
            leg_id: None,
        });
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
    if let Some((entries, window)) = &fold.limiter_holds {
        actions.push(RuleAction::RecordLimiterHolds {
            entries: entries.clone(),
            window: *window,
        });
        actions.push(RuleAction::ScheduleTimer {
            timer_type: TimerType::LimiterRefresh,
            delay: TimerDelay::secs(ctx.config.limiter_refresh_sec),
            leg_id: None,
        });
    }
    actions
}
