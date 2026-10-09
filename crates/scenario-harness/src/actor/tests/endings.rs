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

#[tokio::test]
async fn branches_are_tried_in_order_and_the_overload_refusal_last() {
    let expect = Expect::answered().and(Branch::rejected(486));
    let names: Vec<_> = expect.branches().map(|b| b.name).collect();
    assert_eq!(names, ["answered", "rejected", "refused_overload"]);
    assert_eq!(branch(expect, &caller_refused(486, false)), Some("rejected"));
    assert_eq!(branch(expect, &caller_refused(503, false)), Some("refused_overload"));
    let off = expect.without_overload_refusal();
    assert!(off.accepts_final(486) && !off.accepts_final(503));
    assert_eq!(branch(off, &caller_refused(503, false)), None);
}

#[tokio::test]
#[should_panic(expected = "one run-time branch")]
async fn a_second_run_time_branch_is_a_scenario_bug() {
    let _ = Expect::one(Branch::rejected(486)).and(Branch::rejected(480));
}
