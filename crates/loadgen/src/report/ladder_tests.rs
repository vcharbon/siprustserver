use super::*;
use scenario_harness::StepError;

fn key(from: &str, to: &str, label: &str, nth: u32) -> ArrowKey {
    ArrowKey { from: from.into(), to: to.into(), label: label.into(), nth }
}

fn stat(offset: f64, duplicates: u64, resent: u64) -> ArrowStat {
    ArrowStat { count: 1, duplicates, resent, offset_ms_sum: offset }
}

fn call(charge: Option<ArrowKey>) -> CallMessages {
    CallMessages {
        arrows: vec![
            (key("alice", "SUT", "INVITE", 1), stat(0.0, 0, 1)),
            (key("SUT", "alice", "180 (INVITE)", 1), stat(20.0, 1, 0)),
            (key("SUT", "alice", "486 (INVITE)", 1), stat(40.0, 0, 0)),
        ],
        charge,
        engine: true,
    }
}

fn timeout() -> ResultClass {
    ResultClass::from(&crate::class::CallOutcome::Step {
        at: scenario_harness::actor::FailedAt::at_barrier("answered"),
        cause: StepError::timeout("answered", None, "a 200"),
    })
}

#[test]
#[ignore = "slow lane: loadgen"]
fn a_failure_is_charged_where_it_was_detected_or_after_the_call() {
    let mut agg = LadderAgg::default();
    agg.fold(call(None), &ResultClass::Ok);
    agg.fold(call(Some(key("SUT", "alice", "486 (INVITE)", 1))), &ResultClass::WrongStatus(486));
    agg.fold(call(Some(key("SUT", "alice", "180 (INVITE)", 1))), &timeout());
    agg.fold(call(None), &timeout());
    agg.fold(call(Some(key("SUT", "alice", "180 (INVITE)", 1))), &ResultClass::RfcAuditFail);
    let l = agg.project("basic_call", "alice");
    assert_eq!(l.lanes, vec!["alice", "SUT"]);
    let labels: Vec<_> = l.arrows.iter().map(|a| a.label.as_str()).collect();
    assert_eq!(labels, vec!["INVITE", "180 (INVITE)", "486 (INVITE)"], "by mean offset");
    let errors = |label: &str| l.arrows.iter().find(|a| a.label == label).unwrap().errors.clone();
    let cc = |class: &str| vec![ClassCount { class: class.into(), count: 1 }];
    assert_eq!(errors("486 (INVITE)"), cc("status_486"), "a wrong status on itself");
    assert_eq!(errors("180 (INVITE)"), cc("timeout"), "a timeout on the last message in");
    assert_eq!(l.before_reply, cc("timeout"), "nothing came in");
    assert_eq!(l.after_call, cc("rfc_audit_fail"), "a post-call verdict has no arrow");
    let invite = &l.arrows[0];
    assert_eq!((invite.count, invite.resent, l.arrows[1].retransmissions), (5, 5, 5));
    assert_eq!((agg.received, agg.resent, l.calls, l.engine), (5, 5, 5, true));
}

#[test]
#[ignore = "slow lane: loadgen"]
fn a_ladder_shows_its_first_arrows_and_folds_the_rest() {
    let mut agg = LadderAgg::default();
    let arrows = (0..SHOWN_ARROWS as u32 + 6)
        .map(|n| (key("alice", "SUT", "OPTIONS", n + 1), stat(f64::from(n), 0, 0)))
        .collect();
    let last = key("alice", "SUT", "OPTIONS", SHOWN_ARROWS as u32 + 6);
    let messages = CallMessages { arrows, charge: Some(last), engine: false };
    agg.fold(messages, &timeout());
    let l = agg.project("long", "alice");
    assert_eq!(l.arrows.len(), SHOWN_ARROWS);
    let other = l.other.expect("an other row");
    assert_eq!(other.count, 6);
    assert_eq!(other.errors, vec![ClassCount { class: "timeout".into(), count: 1 }]);
}
