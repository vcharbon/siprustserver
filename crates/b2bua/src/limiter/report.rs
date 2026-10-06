//! The admit reports that reach a call as internal events, and the service
//! action's result event.
//!
//! An admit sent off the call's turn — a route fold's (a failover route, a
//! release reroute, or the refusal a non-route resolution follows) and a
//! service's replacement of the call's admission set
//! (`RuleAction::ReplaceAdmissionSet`) — comes back to the call as an internal
//! event carrying its [`AdmitReport`]. The router applies the report to the
//! call before any rule reads the event ([`admit_report_of`]), whichever rule
//! then claims it; a report reaching no call releases its key when owed. The
//! service's result event ([`LimiterAdmitResult`]) is then the service's to
//! read: its `correlation_id`, its outcome, and the call's limiter state
//! already stating what the limiter holds.

use call::{AdmitReport, Call};

use crate::effects::{HandlerEffects, SoftBoundedEffect};
use b2bua_sdk::event::CallEvent;

/// The `(topic, outcome)` of the route folds whose dispatching task may have
/// sent an admit: the two route-shaped folds (a failover route, a release
/// reroute), and the resolutions a refused route ends in (the failure chain's
/// reject, redirect or terminate after a refusal, a release whose reroute was
/// refused). Only these carry a report among the route folds.
const ROUTE_FOLDS: [(&str, &str); 6] = [
    ("call-failure-result", "failover"),
    ("call-failure-result", "reject"),
    ("call-failure-result", "redirect"),
    ("call-failure-result", "terminate"),
    ("call-release-result", "reroute"),
    ("call-release-result", "release"),
];

/// The route folds among [`ROUTE_FOLDS`] that run the route their admit
/// asked for, whatever its outcome: the call runs on the route's set.
const RUNS_ROUTE: [(&str, &str); 2] =
    [("call-failure-result", "failover"), ("call-release-result", "reroute")];

/// A service's replacement of the call's admission set, answered: what the
/// service's rule reads under [`TOPIC`](Self::TOPIC).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LimiterAdmitResult {
    /// The call the admit was sent for.
    pub call_ref: String,
    /// The service-minted id the action carried, echoed.
    pub correlation_id: String,
    /// The admit and its outcome.
    pub report: AdmitReport,
}

impl LimiterAdmitResult {
    /// The internal-event topic a service's admit result rides to its call;
    /// the event's outcome is the admit's ([`call::AdmitOutcome::label`]).
    pub(crate) const TOPIC: &'static str = b2bua_sdk::fold_payload::LIMITER_ADMIT_RESULT;

    /// The payload key every event carrying an admit report states it under.
    pub(crate) const REPORT: &'static str = b2bua_sdk::fold_payload::LIMITER_ADMIT_REPORT;

    /// The internal event that carries this result to its call.
    pub(crate) fn into_event(self) -> CallEvent {
        CallEvent::InternalEvent {
            call_ref: self.call_ref,
            topic: Self::TOPIC.to_string(),
            outcome: self.report.outcome.label().to_string(),
            incarnation: Some(self.report.key.clone()),
            payload: serde_json::json!({
                "correlation_id": self.correlation_id,
                Self::REPORT: self.report,
            }),
            body: Vec::new(),
        }
    }
}

/// The admit report `event` carries: a route fold's (`None` when its task
/// sent no admit) or a service's admit result's. `None` for any other event.
pub(crate) fn admit_report_of(event: &CallEvent) -> Option<AdmitReport> {
    let CallEvent::InternalEvent { topic, outcome, payload, .. } = event else {
        return None;
    };
    let carries = topic == LimiterAdmitResult::TOPIC
        || ROUTE_FOLDS.iter().any(|(t, o)| t == topic && o == outcome);
    if !carries {
        return None;
    }
    b2bua_sdk::fold_payload::admit_report(payload)
}

/// The router's step before any rule reads `event`: the admit report it
/// carries is applied to `call` when it names the call's key. A service's
/// admit result applies as an admit ([`call::CallLimiterState::apply_admit`]);
/// a route fold applies as a fold ([`call::CallLimiterState::apply_fold`]),
/// running its route when it is one of `RUNS_ROUTE`. One naming another key
/// (an earlier call under the same `call_ref`) leaves the call alone and
/// queues that key's release on `fx` when owed. The report applied, if any.
pub(crate) fn apply_to_call(
    call: &mut Call,
    event: &CallEvent,
    fx: &mut HandlerEffects,
) -> Option<AdmitReport> {
    let report = admit_report_of(event)?;
    if report.key != call.limiter.key() {
        if let Some(key) = report.owed_release() {
            fx.soft.push(SoftBoundedEffect::ReleaseLimiter { key: key.to_string() });
        }
        return None;
    }
    match event {
        CallEvent::InternalEvent { topic, outcome, .. } if topic != LimiterAdmitResult::TOPIC => {
            let runs_route = RUNS_ROUTE.iter().any(|(t, o)| t == topic && o == outcome);
            call.limiter.apply_fold(&report, runs_route);
        }
        _ => call.limiter.apply_admit(&report),
    }
    Some(report)
}

#[cfg(test)]
mod tests {
    use call::{AdmitOutcome, CallLimiterState, LimiterEntry};

    use super::*;

    fn report() -> AdmitReport {
        AdmitReport {
            key: "c#k".into(),
            change: 3,
            entries: vec![LimiterEntry { id: "x".into(), limit: 2 }],
            outcome: AdmitOutcome::Admitted,
        }
    }

    #[test]
    fn a_service_admit_result_round_trips_its_report() {
        let event = LimiterAdmitResult {
            call_ref: "c".into(),
            correlation_id: "svc:1".into(),
            report: report(),
        }
        .into_event();
        let CallEvent::InternalEvent { topic, outcome, payload, .. } = &event else {
            unreachable!()
        };
        assert_eq!((topic.as_str(), outcome.as_str()), ("limiter-admit-result", "admitted"));
        assert_eq!(payload["correlation_id"], "svc:1");
        assert_eq!(admit_report_of(&event), Some(report()));
    }

    #[test]
    fn only_the_events_that_carry_an_admit_yield_a_report() {
        let event = |topic: &str, outcome: &str| CallEvent::InternalEvent {
            call_ref: "c".into(),
            topic: topic.into(),
            outcome: outcome.into(),
            payload: serde_json::json!({ "limiter_admit": report() }),
            body: Vec::new(),
            incarnation: None,
        };
        assert!(admit_report_of(&event("call-failure-result", "failover")).is_some());
        assert!(admit_report_of(&event("call-release-result", "release")).is_some());
        assert!(admit_report_of(&event("service-http-result", "ok")).is_none());
        assert!(admit_report_of(&event("call-refer-result", "allow")).is_none());
    }

    fn call_holding_x() -> Call {
        use crate::router::test_support::{invite, src};
        let config = crate::config::B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
        let invite = invite("w0", "w1", "admit");
        let ids = sip_txn::IdGen::seeded(1);
        let mut call = crate::initial_invite::build_initial_call(&invite, src(), &config, &ids, 0);
        call.limiter = CallLimiterState::admitted(
            "c#k".into(),
            1,
            vec![LimiterEntry { id: "x".into(), limit: 2 }],
        );
        call
    }

    fn fold(report: AdmitReport) -> CallEvent {
        CallEvent::InternalEvent {
            call_ref: "c".into(),
            topic: "call-failure-result".into(),
            outcome: "failover".into(),
            payload: serde_json::json!({ "limiter_admit": report }),
            body: Vec::new(),
            incarnation: None,
        }
    }

    #[test]
    fn a_report_of_the_call_s_key_is_applied_before_the_rules() {
        let mut call = call_holding_x();
        let mut fx = HandlerEffects::new();
        assert_eq!(apply_to_call(&mut call, &fold(report()), &mut fx), Some(report()));
        assert_eq!(call.limiter.held_set().change, 3);
        assert_eq!(call.limiter.number_admit().0, 4, "the next admit is numbered above it");
        assert!(fx.soft.is_empty(), "nothing to release");
    }

    /// A service's result leaves the set a service moved the call onto; a
    /// failover fold moves the call onto its route's set, which it holds.
    #[test]
    fn a_failover_fold_moves_the_call_onto_its_route() {
        let mut call = call_holding_x();
        call.limiter.replace_set(vec![LimiterEntry { id: "y".into(), limit: 2 }], true);
        let mut fx = HandlerEffects::new();
        let result = LimiterAdmitResult {
            call_ref: "c".into(),
            correlation_id: "svc:1".into(),
            report: AdmitReport { outcome: AdmitOutcome::Unavailable, ..report() },
        };
        apply_to_call(&mut call, &result.into_event(), &mut fx);
        assert!(call.limiter.runs_on().is_some() && call.limiter.runs_uncounted());
        apply_to_call(&mut call, &fold(report()), &mut fx);
        assert_eq!(call.limiter.runs_on(), Some(&report().entries[..]));
        assert!(!call.limiter.runs_uncounted(), "the route's set is held");
    }

    /// A release consult reserved a change; a service then moved the call
    /// onto `[t]` under the next one, admitted. The reroute fold lands with
    /// its admit of `[r]` superseded, and the route runs anyway: the call runs
    /// on `[r]`, which the limiter does not hold. A resolution after a refusal
    /// (a release) runs the call on its target.
    #[test]
    fn a_route_fold_moves_the_call_onto_its_route_in_any_report_order() {
        let entry = |id: &str| LimiterEntry { id: id.into(), limit: 2 };
        let mut call = call_holding_x();
        let consult = call.limiter.owe_consult(1);
        let call::Replacement::Send { change: moved_at, .. } =
            call.limiter.replace_set(vec![entry("t")], true)
        else {
            panic!("the move sends an admit");
        };
        call.limiter.apply_admit(&AdmitReport {
            key: "c#k".into(),
            change: moved_at,
            entries: vec![entry("t")],
            outcome: AdmitOutcome::Admitted,
        });
        let superseded = AdmitReport {
            key: "c#k".into(),
            change: consult,
            entries: vec![entry("r")],
            outcome: AdmitOutcome::Superseded {
                held: call::LimiterHeld { change: moved_at, entries: vec![entry("t")] },
            },
        };
        let reroute = |outcome: &str| CallEvent::InternalEvent {
            call_ref: "c".into(),
            topic: "call-release-result".into(),
            outcome: outcome.into(),
            payload: serde_json::json!({ "limiter_admit": superseded.clone() }),
            body: Vec::new(),
            incarnation: None,
        };
        let mut fx = HandlerEffects::new();
        let mut moved = call.clone();
        apply_to_call(&mut moved, &reroute("reroute"), &mut fx);
        assert_eq!(moved.limiter.runs_on(), Some(&[entry("r")][..]), "the call runs on the route");
        assert!(moved.limiter.runs_uncounted(), "held is [t]: r is not counted");
        apply_to_call(&mut call, &reroute("release"), &mut fx);
        assert_eq!(call.limiter.runs_on(), None, "a release runs no route");
    }

    #[test]
    fn a_report_of_another_key_releases_that_key_when_owed_and_leaves_the_call() {
        let mut call = call_holding_x();
        let before = call.limiter.clone();
        let mut fx = HandlerEffects::new();
        let earlier = AdmitReport { key: "c#earlier".into(), ..report() };
        assert_eq!(apply_to_call(&mut call, &fold(earlier.clone()), &mut fx), None);
        assert_eq!(call.limiter, before, "the resident call is untouched");
        assert!(matches!(
            &fx.soft[..],
            [SoftBoundedEffect::ReleaseLimiter { key }] if key == "c#earlier"
        ));
        let mut fx = HandlerEffects::new();
        let unsent = AdmitReport { outcome: AdmitOutcome::NotSent, ..earlier };
        apply_to_call(&mut call, &fold(unsent), &mut fx);
        assert!(fx.soft.is_empty(), "an unsent admit owes nothing");
    }
}
