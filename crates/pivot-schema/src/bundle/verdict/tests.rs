use super::*;

#[test]
fn the_first_step_naming_failure_becomes_the_run_s_failed_step() {
    let mut verdict = RunVerdict::ok("c", "upstream-fake");
    assert!(verdict.passed());
    verdict.fail(Failure::SettleTimedOut { budget_ms: 32000, open: vec!["B".into()] });
    assert!(!verdict.passed());
    assert_eq!(verdict.failed_step, None);
    verdict.fail(Failure::ExpectTimedOut {
        step: "s7".into(),
        leg: "A".into(),
        gated_on: "486".into(),
        within_ms: 32000,
    });
    assert_eq!(verdict.failed_step.as_deref(), Some("s7"));
    verdict.fail(Failure::SendFailed { step: "s9".into(), leg: "A".into(), detail: "x".into() });
    assert_eq!(verdict.failed_step.as_deref(), Some("s7"), "the FIRST one stands");
    assert_eq!(verdict.failures.len(), 3);
}

#[test]
fn a_verdict_round_trips_through_its_bundle_form() {
    let mut verdict = RunVerdict::ok("authored-cancel-race", "upstream-fake");
    verdict.branches.insert("a1".into(), "cancelled".into());
    verdict.completed_steps.push("s1".into());
    verdict.informative.push(Informative {
        class: CheckClass::CdrVocabulary,
        finding: Failure::CdrMismatch {
            expected: "events matches InviteReceived".into(),
            observed: "invite_received".into(),
        },
    });
    let text = serde_json::to_string(&verdict).unwrap();
    assert_eq!(serde_json::from_str::<RunVerdict>(&text).unwrap(), verdict);
    // The status is a statement about gating checks; an informative finding
    // is not one of them.
    assert!(verdict.passed());
}

/// What the lane states about how it ran the case survives the bundle
/// form, and a verdict stating nothing writes no field.
#[test]
fn the_lanes_facts_are_written_only_when_stated() {
    let mut verdict = RunVerdict::ok("c", "upstream-fake");
    assert!(!serde_json::to_string(&verdict).unwrap().contains("lane_facts"));
    verdict.lane_facts.insert("service".into(), "scripted".into());
    let text = serde_json::to_string(&verdict).unwrap();
    assert!(text.contains(r#""lane_facts":{"service":"scripted"}"#), "{text}");
    assert_eq!(serde_json::from_str::<RunVerdict>(&text).unwrap(), verdict);
}

fn violation(emitter: &str) -> crate::violation::RfcViolation {
    crate::violation::RfcViolation {
        rule: RfcRule::No200AfterCancel,
        step: "s11".into(),
        emitter: emitter.into(),
        relays: None,
    }
}

#[test]
fn a_scripted_peer_s_violation_is_listed_and_gates_nothing() {
    let mut verdict = RunVerdict::ok("bc-rc-cancel-xing", "upstream-fake");
    verdict.note_violation(&violation("uas1"));
    assert_eq!(verdict.rfc_violations.len(), 1);
    assert!(!verdict.rfc_violations[0].gating);
    assert!(verdict.passed(), "{:#?}", verdict.failures);
    // And it survives the bundle, because the listing is the point.
    let text = serde_json::to_string(&verdict).unwrap();
    assert!(text.contains("no-200-after-cancel"), "{text}");
}

#[test]
fn a_violation_the_system_under_test_emits_gates() {
    let mut verdict = RunVerdict::ok("bc-rc-cancel-xing", "upstream-fake");
    verdict.note_violation(&violation("sut"));
    assert!(verdict.rfc_violations[0].gating);
    assert!(!verdict.passed());
    assert!(matches!(verdict.failures.first(), Some(Failure::RfcViolationUnverified { .. })));
}

/// A `sut` entry carrying on a scripted party's is listed with what it
/// relays and gates nothing: the claim is the party's, stated beside it.
#[test]
fn a_relayed_violation_the_system_under_test_carries_on_gates_nothing() {
    let mut verdict = RunVerdict::ok("relayed", "upstream-fake");
    verdict.note_violation(&crate::violation::RfcViolation {
        relays: Some("s4".into()),
        ..violation("sut")
    });
    assert!(!verdict.rfc_violations[0].gating);
    assert_eq!(verdict.rfc_violations[0].relays.as_deref(), Some("s4"));
    assert!(verdict.passed(), "{:#?}", verdict.failures);
    let text = serde_json::to_string(&verdict).unwrap();
    assert!(text.contains(r#""relays":"s4""#), "{text}");
}
