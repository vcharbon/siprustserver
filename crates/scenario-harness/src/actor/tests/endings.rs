//! The accepted endings read the caller's final observed state: which branch
//! each recorded outcome selects, and what the caller may end on cleanly.

use crate::actor::*;

fn caller_refused(code: u16, early_media: bool) -> ObservedState {
    let obs = ObservedState::new();
    let now = tokio::time::Instant::now();
    if early_media {
        let fact = ResponseFact {
            status: 183,
            reason: "Session Progress".into(),
            cseq_method: "INVITE".into(),
            body_len: 120,
            body_is_sdp: true,
            early_tag: None,
            typed: None,
        };
        obs.record(Observation::LegResponse { leg: "alice", fact }, now);
        obs.record(Observation::LegEarly { leg: "alice" }, now);
    }
    obs.record(Observation::LegFinal { leg: "alice", status: code, reason: "Decline".into() }, now);
    obs.record(Observation::LegTerminated { leg: "alice" }, now);
    obs
}

fn branch(expect: Expect, obs: &ObservedState) -> Option<&'static str> {
    verdict(expect, CallVerdict::Ok, obs, "alice").branch()
}

#[tokio::test]
async fn early_media_then_the_final_is_its_own_branch() {
    let expect = Expect::one(Branch::early_media_rejected(603)).without_overload_refusal();
    assert_eq!(branch(expect, &caller_refused(603, true)), Some("early_media_rejected"));
    assert_eq!(branch(expect, &caller_refused(603, false)), None, "no early media");
    assert_eq!(branch(expect, &caller_refused(486, true)), None, "another final");
}

/// The run's declared overload refusal joins a non-emergency call's set, last.
#[tokio::test]
async fn branches_are_tried_in_order_and_the_run_s_overload_refusal_last() {
    let h = crate::Harness::new("endings-overload");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let mut env =
        crate::realcall::CallEnv::for_functional(&alice, &bob, None, bob.addr(), "X", "t");
    let declared = Expect::answered().and(Branch::rejected(486));
    let names = |e: Expect| e.branches().map(|b| b.name).collect::<Vec<_>>();

    assert_eq!(names(declared.in_context(&env)), ["answered", "rejected"], "none declared");
    env.overload_reject = Some(503);
    let expect = declared.in_context(&env);
    assert_eq!(names(expect), ["answered", "rejected", "refused_overload"]);
    assert_eq!(branch(expect, &caller_refused(486, false)), Some("rejected"));
    assert_eq!(branch(expect, &caller_refused(503, false)), Some("refused_overload"));
    assert!(!expect.accepts_final(480));
    let off = declared.without_overload_refusal().in_context(&env);
    assert!(off.accepts_final(486) && !off.accepts_final(503));
    assert_eq!(branch(off, &caller_refused(503, false)), None);
    env.emergency = true;
    assert_eq!(names(declared.in_context(&env)), ["answered", "rejected"], "emergency");
    h.finish().await;
}

/// A refusal under a predicate ends the call on its final like any refusal,
/// and holds only when the final state shows the predicate: here, a callee
/// whose dialog never confirmed, which a confirmed-then-ended one is not.
#[tokio::test]
async fn a_refusal_under_a_predicate_holds_only_with_it() {
    fn callee_never_answered(s: &StateInner) -> bool {
        !s.leg("bob").was_confirmed()
    }
    const UNANSWERED: Branch =
        Branch::new("unanswered", Ending::RefusedWhen(480, callee_never_answered));
    let expect = Expect::one(UNANSWERED).without_overload_refusal();
    assert!(expect.accepts_final(480) && !expect.accepts_final(486));
    let rang = caller_refused(480, false);
    rang.record(Observation::LegEarly { leg: "bob" }, tokio::time::Instant::now());
    rang.record(Observation::LegTerminated { leg: "bob" }, tokio::time::Instant::now());
    assert_eq!(branch(expect, &rang), Some("unanswered"));
    let answered = caller_refused(480, false);
    answered.record(Observation::LegTerminated { leg: "bob" }, tokio::time::Instant::now());
    answered.record(Observation::LegConfirmed { leg: "bob" }, tokio::time::Instant::now());
    assert_eq!(branch(expect, &answered), None, "the callee had answered");
}

#[tokio::test]
#[should_panic(expected = "one run-time branch")]
async fn a_second_run_time_branch_is_a_scenario_bug() {
    let _ = Expect::one(Branch::rejected(486)).and(Branch::rejected(480));
}
