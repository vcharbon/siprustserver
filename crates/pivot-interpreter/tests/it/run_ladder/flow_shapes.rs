//! Flow shapes: the linear call, `alt`, socket-scoped claims, a loopback endpoint, two interleaved
//! calls, and the red path of a wrong final.

use super::*;

#[tokio::test(start_paused = true)]
async fn rung_one_a_linear_answered_call_runs_green_and_writes_its_bundle() {
    let scene = api_scene("pivot-linear-attempt").await;
    let (outcome, dir) = replay(&scene, "linear-attempt.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 13, "every step ran");

    // s7's inline checks include one the CAPTURED platform's egress carries and
    // this system does not. It is classified, this lane is not the document's
    // origin lane, so it is evaluated, recorded, and the step still completes
    // (§9.1) — a downgrade is not a skip, and it is not a pass either.
    assert_eq!(outcome.verdict.informative.len(), 1, "{:#?}", outcome.verdict.informative);
    let note = &outcome.verdict.informative[0];
    assert_eq!(note.class, CheckClass::OriginPlatformHeader);
    assert!(
        matches!(&note.finding, Failure::CheckFailed { site, field, .. }
            if site == "step \"s7\"" && field == "header(P-Charging-Vector)"),
        "{note:#?}"
    );
    scene.finish().await;
}

#[tokio::test(start_paused = true)]
async fn rung_two_an_alt_commits_to_the_branch_that_arrived_and_discards_the_other() {
    let scene = api_scene("pivot-alt-answered").await;
    let (outcome, dir) = replay(&scene, "alt-answered.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(
        outcome.verdict.branches.get("a1").map(String::as_str),
        Some("answered"),
        "the run states which branch it took"
    );
    assert!(
        !outcome.verdict.completed_steps.contains(&"s12".to_string()),
        "the branch that did not run contributes no step"
    );
    scene.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_claim_is_scoped_to_the_socket_the_invite_arrived_on() {
    // TWO calls, dialled at two different sockets, each callee leg claiming by
    // `ruri-pos` on its own endpoint. `ep-a-second` sorts FIRST, so offering an
    // INVITE to every endpoint instead of to the one it arrived on hands call
    // one to leg D and fails the run.
    let scene = api_scene("pivot-two-endpoint-claim").await;
    let second = scene.h.agent("second", &format!("127.0.0.1:{IDLE_PORT}")).await;
    let extra = BTreeMap::from([("uas2".to_string(), second)]);
    let (outcome, dir) = replay_on(&scene, "two-endpoint-claim.v3.json", extra).await;
    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 22, "both calls ran end to end");
    // Every leg heard something: the second socket is real traffic, not a
    // declaration.
    let legs = outcome.recording.legs();
    for leg in ["A", "B", "C", "D"] {
        assert!(!legs[leg].is_empty(), "leg {leg} heard nothing: {legs:#?}");
    }
    // And the claim landed on the socket that received it.
    let on_d = legs["D"].iter().any(|m| m.step.as_deref() == Some("s10"));
    assert!(on_d, "the second call's INVITE claimed leg D: {:#?}", legs["D"]);
    scene.finish().await;
}

/// The red path: the wire is RFC-complete on both sides, and the DOCUMENT is
/// what is wrong. The run must fail by name, at the right step, and still leave
/// its bundle behind.
///
/// The 200 that arrives is a FINAL on the very transaction `s5` waits for, so
/// the 486 it gates on can never come and the run CANNOT GO ON (§11.2). Polarity
/// has nothing to do with it: this document declares nothing, and its script
/// still ends, its call is still closed generically, and it still settles.
#[tokio::test(start_paused = true)]
async fn a_run_whose_document_asserts_the_wrong_final_fails_by_name_and_keeps_its_bundle() {
    let scene = api_scene("pivot-wrong-final-status").await;
    let (outcome, dir) = replay(&scene, "wrong-final-status.v3.json").await;

    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert_eq!(outcome.verdict.failed_step.as_deref(), Some("s5"));
    let unmatched = outcome
        .verdict
        .failures
        .iter()
        .find(|f| matches!(f, Failure::UnmatchedDatagram { .. }))
        .unwrap_or_else(|| {
            panic!("no unmatched-datagram failure: {:#?}", outcome.verdict.failures)
        });
    let Failure::UnmatchedDatagram { step, leg, gated_on, arrived, .. } = unmatched else {
        unreachable!("matched above")
    };
    assert_eq!((step.as_str(), leg.as_str()), ("s5", "A"));
    // A 486 to an INVITE is a final a compliant callee CAN send, so the gate is
    // rejecting a PLAUSIBLE assertion, not an impossible one.
    assert!(matches!(gated_on, GatedOn::Response { status: 486, .. }), "{gated_on}");
    assert!(matches!(arrived, Arrived::Response { status: 200, .. }), "{arrived}");
    // The document's own error is the WHOLE account: the close ended the call it
    // could no longer script, so nothing is piled on top of the real finding.
    assert_eq!(outcome.verdict.failures.len(), 1, "{:#?}", outcome.verdict.failures);
    assert!(outcome.timing.settled_at_ms.is_some(), "a run whose call was closed settles");

    // A POSITIVE run that cannot go on abandons and closes exactly like any
    // other: leg A acknowledges the 2xx it took and BYEs the dialog it opened,
    // leg B answers the teardown that reaches the side which ANSWERED.
    let abandoned = outcome.verdict.abandoned.as_ref().expect("the script was abandoned");
    assert_eq!((abandoned.leg.as_deref(), abandoned.step.as_deref()), (Some("A"), Some("s5")));
    let acts: Vec<(&str, CloseOwed)> =
        abandoned.closed.iter().map(|act| (act.leg.as_str(), act.owed)).collect();
    assert_eq!(
        acts,
        [("A", CloseOwed::Ack), ("A", CloseOwed::Bye), ("B", CloseOwed::Answer)],
        "{:#?}",
        abandoned.closed
    );
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();

    // The evidence survives the failure.
    for file in ["pivot.json", "run-config.json", "verdict.json", "timing.json"] {
        assert!(dir.join(file).is_file(), "{file} missing from a failed run's bundle");
    }
    let written: pivot_interpreter::RunVerdict =
        serde_json::from_str(&std::fs::read_to_string(dir.join("verdict.json")).unwrap()).unwrap();
    assert_eq!(written.failed_step.as_deref(), Some("s5"));
    assert!(dir.join("recording/A.jsonl").is_file());
    assert!(!std::fs::read_to_string(dir.join("recording/A.jsonl")).unwrap().is_empty());

    scene.finish().await;
}

/// A LOOPBACK endpoint: one socket hosts both the caller and the callee, which
/// is the binding every captured document uses. The callee's `background`
/// policy has to be found on the ENDPOINT, since only one of the two actors
/// sharing the socket is pumped.
#[tokio::test(start_paused = true)]
async fn rung_two_a_loopback_endpoint_hosts_both_roles_on_one_socket() {
    // The document dials the b-leg back at the socket the a-leg came from, and
    // the scene honours it — same decision as every other rung.
    let scene = api_scene("pivot-loopback-attempt").await;
    let extra = BTreeMap::from([("uas1".to_string(), scene.alice.clone())]);
    let (outcome, dir) = replay_on(&scene, "loopback-attempt.v3.json", extra).await;
    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 13, "every step ran");
    // Both legs rode ONE socket, and each kept its own ladder.
    let legs = outcome.recording.legs();
    assert!(!legs["A"].is_empty() && !legs["B"].is_empty(), "{legs:#?}");
    // The `background` counters are not vacuous: real OPTIONS crossed the wire
    // and were answered outside the flow.
    let answered = legs
        .values()
        .flatten()
        .filter(|m| m.note.as_deref().is_some_and(|n| n.starts_with("background")))
        .count();
    assert!(answered >= 2, "the system's keepalives were answered: {legs:#?}");
    scene.finish().await;
}

/// The block-settling rule on its OTHER path (§6.8): the member that completes
/// the block is a tolerated absence the BUDGET retired, not a message.
///
/// §6.5's budget-release is a completion like any other, so the alt it closes
/// settles with it and the send anchored on that alt still fires. A release
/// path that stamped the step but not the block would strand that send with no
/// deadline and end the run `flow-incomplete`.
#[tokio::test(start_paused = true)]
async fn an_alt_its_released_absence_completes_still_settles_for_what_waits_on_it() {
    let scene = api_scene("pivot-alt-released-settles").await;
    let mut case = fixture("alt-answered.v3.json");
    case.document.case.id = "alt-released-settles".into();
    // The committed branch now ENDS on a tolerated absence: a 481 to the BYE the
    // callee already answered 200, which only its budget can retire.
    let mut absent = case
        .document
        .flow
        .iter()
        .flat_map(pivot_schema::flow::FlowNode::steps)
        .find(|step| step.id == "s11")
        .expect("the answered branch ends on the 200 to the BYE")
        .clone();
    absent.id = "s98".into();
    absent.msg.status = Some(481);
    absent.msg.reason = None;
    absent.optional = true;
    absent.within_ms = Some(250);
    absent.delay.from = "step:s11".parse().expect("s11 is a step id");
    absent.delay.ms = 0;
    let pivot_schema::flow::FlowNode::Alt(alt) =
        case.document.flow.iter_mut().find(|node| node.id() == "a1").expect("the fixture's alt")
    else {
        panic!("a1 is the alt")
    };
    alt.branches[0].steps.push(absent);
    // And a step OUTSIDE the alt waits on the block itself.
    let mut after = case
        .document
        .flow
        .iter()
        .flat_map(pivot_schema::flow::FlowNode::steps)
        .find(|step| step.id == "s11")
        .expect("the 200 to the BYE")
        .clone();
    after.id = "s99".into();
    after.msg.status = Some(481);
    after.msg.reason = None;
    after.optional = true;
    after.within_ms = Some(250);
    after.delay.from = "step:a1".parse().expect("a1 is the alt's own id");
    after.delay.ms = 0;
    case.document.flow.push(pivot_schema::flow::FlowNode::Message(Box::new(after)));
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_bundle_is_complete(&outcome, &dir);
    for released in ["s98", "s99"] {
        assert!(
            outcome.verdict.released_optional.iter().any(|step| step == released),
            "{released} was released by its budget: {:#?}",
            outcome.verdict.released_optional
        );
    }
    scene.finish().await;
}

/// `calls[] > 1`: two calls in one document, ordered against each other by the
/// one cross-call device the format has.
#[tokio::test(start_paused = true)]
async fn rung_three_two_calls_interleave_through_after() {
    let scene = api_scene("pivot-two-calls").await;
    let (outcome, dir) = replay(&scene, "two-calls.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 18, "every step of both calls ran");

    // `after` is an ordering, and the run honoured it: the second call's INVITE
    // went out only once the first call's ACK had landed, and the first call's
    // BYE only once the second had been refused.
    let order = |id: &str| {
        outcome
            .verdict
            .completed_steps
            .iter()
            .position(|s| s == id)
            .unwrap_or_else(|| panic!("{id} never completed"))
    };
    assert!(order("s7") < order("s8"), "the second call waited on the first call's ACK");
    assert!(order("s14") < order("s15"), "the first call's teardown waited on the second");
    // Four legs, four ladders.
    let legs = outcome.recording.legs();
    for leg in ["A", "B", "C", "D"] {
        assert!(legs.contains_key(leg) && !legs[leg].is_empty(), "leg {leg} is empty: {legs:#?}");
    }
    scene.finish().await;
}
