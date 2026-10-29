use scenario_harness::actor::FailedAt;
use scenario_harness::StepError;

use super::*;

fn key(from: &str, to: &str, label: &str, nth: u32) -> ArrowKey {
    ArrowKey { from: from.into(), to: to.into(), label: label.into(), nth }
}

fn stat(offset: f64, duplicates: u64, resent: u64) -> ArrowStat {
    ArrowStat { count: 1, duplicates, resent, offset_ms_sum: offset }
}

/// One call: alice's INVITE, bob's leg, and the 180 then 486 alice received;
/// `charges` per lane.
fn call(charges: &[(&str, &str)]) -> CallMessages {
    CallMessages {
        caller: "alice".into(),
        arrows: vec![
            (key("alice", "SUT", "INVITE", 1), stat(0.0, 0, 1)),
            (key("SUT", "bob", "INVITE", 1), stat(5.0, 0, 0)),
            (key("SUT", "alice", "180 (INVITE)", 1), stat(20.0, 1, 0)),
            (key("SUT", "alice", "486 (INVITE)", 1), stat(40.0, 0, 0)),
        ],
        other: ArrowStat::default(),
        charges: charges
            .iter()
            .map(|(lane, label)| (lane.to_string(), Some(key("SUT", lane, label, 1))))
            .collect(),
    }
}

fn step(who: &'static str, barrier: &'static str, cause: StepError) -> CallOutcome {
    CallOutcome::Step { at: FailedAt { who, goal: None, barrier }, cause }
}

fn timeout_of(who: &'static str) -> CallOutcome {
    step(who, "answered", StepError::timeout("answered", None, "a 200"))
}

const OK: CallOutcome = CallOutcome::Ok { branch: "answered" };

#[test]
#[ignore = "slow lane: loadgen"]
fn a_failure_is_charged_to_the_failing_agents_last_message_in() {
    let mut agg = LadderAgg::new(true);
    let both = [("alice", "486 (INVITE)"), ("bob", "INVITE")];
    agg.fold(call(&both), &OK);
    let refused = step(
        "alice",
        "answered",
        StepError::WrongStatus {
            who: "alice".into(),
            expected: 200,
            got: 486,
            reason: "Busy Here".into(),
        },
    );
    agg.fold(call(&both), &refused);
    agg.fold(call(&[("alice", "180 (INVITE)"), ("bob", "INVITE")]), &timeout_of("bob"));
    agg.fold(
        call(&[("alice", "180 (INVITE)")]),
        &step("settle", "settle", StepError::timeout("settle", None, "settled")),
    );
    agg.fold(call(&[("alice", "180 (INVITE)")]), &timeout_of("bob"));
    agg.fold(call(&both), &CallOutcome::RfcAuditFail(Vec::new()));
    let l = agg.project("basic_call", "alice");
    assert_eq!(l.lanes, vec!["alice", "SUT", "bob"]);
    let labels: Vec<_> = l.arrows.iter().map(|a| format!("{}>{}", a.to, a.label)).collect();
    let by_offset = ["SUT>INVITE", "bob>INVITE", "alice>180 (INVITE)", "alice>486 (INVITE)"];
    assert_eq!(labels, by_offset, "by mean offset");
    let errors = |to: &str, label: &str| {
        l.arrows.iter().find(|a| a.to == to && a.label == label).unwrap().errors.clone()
    };
    let cc = |class: &str| vec![ClassCount { class: class.into(), count: 1 }];
    assert_eq!(errors("alice", "486 (INVITE)"), cc("status_486"), "the wrong status itself");
    assert_eq!(errors("bob", "INVITE"), cc("timeout"), "bob's failure on bob's lane");
    assert_eq!(errors("alice", "180 (INVITE)"), cc("timeout"), "a barrier's on the caller's");
    assert_eq!(l.before_reply, cc("timeout"), "bob had received nothing");
    assert_eq!(l.after_call, cc("rfc_audit_fail"), "a post-call verdict has no arrow");
    let invite = &l.arrows[0];
    assert_eq!((invite.count, invite.resent, l.arrows[2].retransmissions), (6, 6, 6));
    assert_eq!((agg.received, agg.resent, l.calls, l.engine, l.truncated), (6, 6, 6, true, false));
}

#[test]
#[ignore = "slow lane: loadgen"]
fn a_ladder_shows_its_first_arrows_and_folds_the_rest() {
    let mut agg = LadderAgg::new(false);
    let arrows = (0..SHOWN_ARROWS as u32 + 6)
        .map(|n| (key("alice", "SUT", "OPTIONS", n + 1), stat(f64::from(n), 0, 0)))
        .collect();
    let last = key("alice", "SUT", "OPTIONS", SHOWN_ARROWS as u32 + 6);
    let charges = vec![("alice".to_string(), Some(last))];
    let messages =
        CallMessages { caller: "alice".into(), arrows, other: ArrowStat::default(), charges };
    agg.fold(messages, &timeout_of("alice"));
    let l = agg.project("long", "alice");
    assert_eq!(l.arrows.len(), SHOWN_ARROWS);
    let other = l.other.expect("an other row");
    assert_eq!(other.count, 6);
    assert_eq!(other.errors, vec![ClassCount { class: "timeout".into(), count: 1 }]);
    assert!(!l.truncated, "every arrow kept, only the display is cut");
}

#[test]
#[ignore = "slow lane: loadgen"]
fn past_its_key_cap_a_ladder_folds_new_arrows_into_other_and_says_so() {
    let mut agg = LadderAgg::new(false);
    let arrows = |from: u32, to: u32| -> Vec<(ArrowKey, ArrowStat)> {
        (from..to).map(|n| (key("alice", "SUT", "OPTIONS", n), stat(0.0, 0, 0))).collect()
    };
    let call = |arrows| CallMessages { caller: "alice".into(), arrows, ..CallMessages::default() };
    agg.fold(call(arrows(0, MAX_KEYS as u32)), &OK);
    assert!(!agg.project("s", "alice").truncated, "exactly at the cap");
    agg.fold(call(arrows(0, MAX_KEYS as u32 + 3)), &OK);
    let l = agg.project("s", "alice");
    assert!(l.truncated);
    let shown: u64 = l.arrows.iter().map(|a| a.count).sum();
    assert_eq!(shown + l.other.map_or(0, |o| o.count), 2 * MAX_KEYS as u64 + 3, "none lost");
    let mut capped = LadderAgg::new(false);
    let mut one = call(Vec::new());
    one.other = stat(0.0, 0, 0);
    capped.fold(one, &OK);
    assert!(capped.project("s", "alice").truncated, "a call past its own cap truncates too");
}

#[test]
#[ignore = "slow lane: loadgen"]
fn a_late_reanswer_counts_on_its_arrow_folded_before_or_after() {
    let mut agg = LadderAgg::new(false);
    let bob_invite = key("SUT", "bob", "INVITE", 1);
    agg.late(&bob_invite);
    agg.fold(call(&[]), &OK);
    agg.late(&bob_invite);
    agg.late(&key("SUT", "alice", "486 (INVITE)", 1));
    let l = agg.project("basic_call", "alice");
    let late = |label: &str, to: &str| {
        l.arrows.iter().find(|a| a.label == label && a.to == to).map(|a| a.late_answered)
    };
    assert_eq!(late("INVITE", "bob"), Some(2), "a repair before its call folded counts too");
    assert_eq!(late("486 (INVITE)", "alice"), Some(1));
    assert_eq!(late("INVITE", "SUT"), Some(0));
    assert_eq!(l.arrows.iter().find(|a| a.to == "bob").map(|a| a.count), Some(1), "one call");
}
