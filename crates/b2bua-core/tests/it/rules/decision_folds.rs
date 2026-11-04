//! A decision fold as the rules read it: the ordinal a rule reads, and a
//! fold on a call already going away dropped whole while a live call's applies.

use super::fixtures::*;
use super::*;

/// A rule reads the decision ordinal current at its turn: `0` on a call no
/// decision touched, then the count of applied decisions.
#[test]
fn rule_call_reads_the_current_decision_ordinal() {
    let mut call = test_call();
    assert_eq!(RuleCall::new(&call).decision_ordinal(), 0, "no decision yet");
    call::helpers::mark_decision(
        &mut call,
        1_000,
        call::DecisionKind::Route,
        Some("a".to_string()),
        Some("first".to_string()),
    );
    assert_eq!(RuleCall::new(&call).decision_ordinal(), 1, "one decision applied");
}

/// The ids the executor admits for the fold on `call`.
fn fold_candidates(
    call: &call::Call,
    topic: &str,
    outcome: &str,
    payload: serde_json::Value,
) -> Vec<&'static str> {
    let event = fold_event(call, topic, outcome, payload);
    let config = B2buaConfig::default();
    let ctx = ctx_for(call, &event, &config);
    pick_ranked(&default_rules(), call, &ctx).iter().map(|r| r.id).collect()
}

/// The named decision-fold rule's own output for the fold on `call` — the
/// handler reached directly, whatever selection would admit.
fn fold_result(
    call: &call::Call,
    rule_id: &str,
    topic: &str,
    outcome: &str,
    payload: serde_json::Value,
) -> Vec<RuleAction> {
    let event = fold_event(call, topic, outcome, payload);
    let config = B2buaConfig::default();
    let ctx = ctx_for(call, &event, &config);
    let rules = default_rules();
    let rule = rules.iter().find(|r| r.id == rule_id).unwrap_or_else(|| panic!("{rule_id} exists"));
    (rule.handle)(&ctx).unwrap_or_else(|| panic!("{rule_id} handles its fold")).actions
}

#[test]
fn decision_folds_on_a_terminating_call_are_dropped_whole() {
    // A `/call/new` decision result landing on a call already going away drives
    // no forward progress, whatever it decides: no failover/reroute leg toward
    // a caller-less callee, no second final on the a-leg's completed
    // transaction (RFC 3261 §17.2.1), no re-termination. Two layers pin it:
    // the executor's going-away gate keeps every fold rule from being a
    // candidate at all, and each handler still drops the fold whole when
    // reached directly (defense in depth).
    let mut call = test_call();
    call.state = CallModelState::Terminating;
    call.callback_context = Some("cb".into());
    call::helpers::add_b_leg(&mut call, b_leg_pending());
    for (rule_id, topic, outcome, payload) in [
        ("failover-create-leg", "call-failure-result", "failover", route_fold_payload()),
        (
            "failover-reject",
            "call-failure-result",
            "reject",
            serde_json::json!({ "code": 484, "reason": "Address Incomplete" }),
        ),
        (
            "failover-redirect",
            "call-failure-result",
            "redirect",
            serde_json::json!({
                "code": 302,
                "contacts": [ { "uri": "sip:carol@10.0.0.3", "q": null } ],
            }),
        ),
        (
            "failover-terminate",
            "call-failure-result",
            "terminate",
            serde_json::json!({ "status": 486, "reason": "Busy Here" }),
        ),
        ("release-result-release", "call-release-result", "release", serde_json::json!({})),
        ("release-result-reroute", "call-release-result", "reroute", route_fold_payload()),
    ] {
        let candidates = fold_candidates(&call, topic, outcome, payload.clone());
        assert!(
            candidates.is_empty(),
            "the gate admits no fold rule on a terminating call, got {candidates:?}",
        );
        let actions = fold_result(&call, rule_id, topic, outcome, payload);
        assert!(
            actions.is_empty(),
            "{rule_id} on a terminating call is dropped whole, got {actions:?}",
        );
    }
}

#[test]
fn decision_folds_on_a_live_call_still_apply() {
    // The going-away guard is narrow: the same folds on an Active call keep
    // their normal bodies.
    let mut call = test_call();
    call.callback_context = Some("cb".into());
    call::helpers::add_b_leg(&mut call, b_leg_pending());

    let actions = fold_result(
        &call,
        "failover-create-leg",
        "call-failure-result",
        "failover",
        route_fold_payload(),
    );
    assert!(
        actions.iter().any(|a| matches!(a, RuleAction::CreateLeg { .. })),
        "live failover fold creates the leg, got {actions:?}",
    );

    let actions = fold_result(
        &call,
        "failover-reject",
        "call-failure-result",
        "reject",
        serde_json::json!({ "code": 484, "reason": "Address Incomplete" }),
    );
    assert!(
        actions.iter().any(|a| matches!(a, RuleAction::RespondToALeg { status: 484, .. })),
        "live reject fold answers the caller, got {actions:?}",
    );

    // The reroute fold's live shape (answered + subscribed) is pinned by the
    // release-reroute suite; here the release outcome suffices as the control.
    let actions = fold_result(
        &call,
        "release-result-release",
        "call-release-result",
        "release",
        serde_json::json!({}),
    );
    assert!(
        actions.iter().any(|a| matches!(a, RuleAction::BeginTermination { .. })),
        "live release fold tears down, got {actions:?}",
    );
}
