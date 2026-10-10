//! The engine's attempt writers: open an attempt for a routing answer, record
//! a route admission refused, and retire an ended attempt's timers. Each
//! keeps the routing answer of an attempt it opens on the turn's effects, for
//! the turn's `AttemptStarted` delivery (`rules::lifecycle_delivery`).

use b2bua_sdk::event::CallEvent;
use b2bua_sdk::lifecycle::{AttemptOpening, RouteAnswer};
use call::helpers::retire_timers;
use call::{AttemptCause, AttemptId, AttemptOutcome, Call, Deadline};

use crate::effects::{CriticalStateEffect, HandlerEffects};

/// The key under which a decision fold lists the routes admission refused
/// before its answer: each `{label, service_ext, no_answer_timeout_sec}`.
pub const REFUSED_ROUTES: &str = "refused_routes";

/// Open the attempt `opening` states, its legs ringing for `no_answer`. An
/// attempt still open ends first and its timers retire.
pub(crate) fn open(
    call: &mut Call,
    fx: &mut HandlerEffects,
    opening: AttemptOpening,
    no_answer: Deadline,
) -> AttemptId {
    let (id, ended) = call::helpers::open_attempt(call, opening.cause, no_answer);
    for earlier in ended {
        retire(call, fx, earlier);
    }
    fx.opened_attempts.push((id, opening.route));
    id
}

/// Record a route admission refused: an attempt opened for `cause` that ends
/// at once, `Refused`.
pub(crate) fn refuse(
    call: &mut Call,
    fx: &mut HandlerEffects,
    cause: AttemptCause,
    route: RouteAnswer,
    no_answer: Deadline,
) -> AttemptId {
    let id = open(call, fx, AttemptOpening::new(cause, route), no_answer);
    call::helpers::end_attempt(call, id, AttemptOutcome::Refused);
    id
}

/// Disarm every timer scoped to attempt `id`: off the ledger and the driver.
pub(crate) fn retire(call: &mut Call, fx: &mut HandlerEffects, id: AttemptId) {
    for t in retire_timers(call, |t| t.key.attempt_id() == Some(id)) {
        fx.critical.push(CriticalStateEffect::CancelTimer { key: t.key });
    }
}

/// Record the refused routes a decision fold lists ([`REFUSED_ROUTES`]),
/// before the fold's own answer is applied: a setup failover's under
/// `Failover`, an established call's reroute under `Reroute`.
pub(crate) fn record_refused_routes(call: &mut Call, fx: &mut HandlerEffects, event: &CallEvent) {
    let CallEvent::InternalEvent { topic, payload, .. } = event else {
        return;
    };
    let cause = match topic.as_str() {
        "call-failure-result" => AttemptCause::Failover,
        "call-release-result" => AttemptCause::Reroute,
        _ => return,
    };
    let Some(refused) = payload.get(REFUSED_ROUTES).and_then(|v| v.as_array()) else {
        return;
    };
    for route in refused {
        let answer = RouteAnswer {
            label: route.get("label").and_then(|v| v.as_str()).map(str::to_string),
            service_ext: crate::rules::defaults::parse_service_ext(route),
        };
        let no_answer =
            Deadline::of_secs(route.get("no_answer_timeout_sec").and_then(|v| v.as_i64()));
        refuse(call, fx, cause, answer, no_answer);
    }
}

/// One refused route as a fold lists it under [`REFUSED_ROUTES`].
pub(crate) fn refused_route_entry(
    label: Option<&str>,
    service_ext: &std::collections::BTreeMap<String, serde_json::Value>,
    no_answer: Deadline,
) -> serde_json::Value {
    serde_json::json!({
        "label": label,
        "service_ext": service_ext,
        "no_answer_timeout_sec": no_answer.millis().map(|ms| ms / 1000),
    })
}
