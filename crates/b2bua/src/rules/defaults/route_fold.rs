//! The shared decoder for **route-shaped internal-event payloads** (built by
//! the router's `route_result_payload`) and the parity actions both async
//! route folds — `failover-create-leg` (`call-failure-result`) and
//! `release-reroute` (`call-release-result`) — must apply identically. One
//! parser + one parity-action builder so the folds cannot drift from each
//! other or from the initial `apply_route`. The admit a fold carries is the
//! router's to apply before any rule reads it (`crate::limiter::report`).

use call::{CallModelState, TimerType};

use b2bua_sdk::header_update::{payload_adds, payload_lines};
use b2bua_sdk::model::{Body, RuleAction, RuleContext, TimerDelay};

/// Whether a decision fold has landed on a call already going away — the
/// call-scoped clause of [`call::helpers::leg_is_going_away`]. A `/call/new`
/// result (route or reject, any outcome) applied to a `Terminating`/
/// `Terminated` call is moot: the caller already holds its final, so the fold
/// drives no forward progress — no new leg toward a callee whose caller is
/// gone, no second final on the a-leg's completed transaction (RFC 3261
/// §17.2.1). The termination in progress owns the teardown; the admit a route
/// fold carries was applied to the call before the rules read it.
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
    /// The adds of the payload's `update_headers`, yielding to the INVITE as
    /// minted.
    pub header_adds: Vec<(String, Vec<String>)>,
    pub features: Option<call::features::FeatureActivations>,
    pub service_ext: call::ExtMap,
    /// `Some` (possibly empty) = the route owns the call's registry; the
    /// emitter (`callouts::route_result_payload`) always writes the list.
    /// `None` = the value does not read as a list of events, a malformed fold
    /// that leaves the registry untouched rather than clearing it.
    pub subscriptions: Option<Vec<call::ReleaseEventKind>>,
    /// `update_body` wire shape: absent = keep A's INVITE body, null = drop
    /// (`Some(vec![])`), string = substitute.
    pub body_override: Option<Vec<u8>>,
    /// `attach_parts`: A's session description sent beside these parts
    /// (`BodyUpdate::AttachParts`); absent = none attached.
    pub attach_parts: Option<Vec<sip_message::MultipartPart>>,
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
        header_adds: payload_adds(payload.get("update_headers")),
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
        attach_parts: payload
            .get("attach_parts")
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
    })
}

impl RouteFold {
    /// The body the fold's leg is minted with: A's INVITE body beside the
    /// attached parts, the substitute, or `None` to relay A's body.
    pub(crate) fn leg_body(&self, ctx: &RuleContext) -> Option<Body> {
        match (&self.attach_parts, &self.body_override) {
            (Some(parts), _) => Some(
                Body::from_leg(
                    ctx.call.a_leg_invite().body.clone(),
                    ctx.call.a_leg().leg_id.clone(),
                )
                .with_parts(parts.clone()),
            ),
            (None, Some(bytes)) => Some(Body::own(bytes.clone(), None)),
            (None, None) => None,
        }
    }
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
/// features (incl. the GlobalDuration re-arm), service_ext merge and the
/// release-subscription registry. The limiter state is the router's, applied
/// from the fold's admit before the rules read it; the refresh cadence of a
/// counted call follows from it (`limiter::call::arm_refresh`).
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
    actions
}
