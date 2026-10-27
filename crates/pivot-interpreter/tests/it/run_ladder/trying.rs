//! The 100 Trying an INVITE server transaction owes only while its TU has not
//! answered (RFC 3261 §17.2.1): a final sent at once stands in for it.
//!
//! The scene: the caller's BYE reaches the callee, which re-INVITEs on the
//! same dialog before answering it. The system answers that re-INVITE with a
//! final at once and no 100, where the document scripts the 100 first.

use super::*;

/// `reinvite-after-bye.v3.json` with the final its `s14` names.
fn reinvite_answered(case_id: &str, status: u16, reason: &str) -> Case {
    let mut case = fixture("reinvite-after-bye.v3.json");
    case.document.case.id = case_id.into();
    let final_step = step_of(&mut case, "s14");
    final_step.msg.status = Some(status);
    final_step.msg.reason = Some(reason.into());
    case
}

/// The floor every rung here holds a FAILED run to: the run settled and left
/// its bundle on disk.
fn assert_settled_bundle(outcome: &Outcome, dir: &std::path::Path) {
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled: {:#?}", outcome.verdict);
    for file in ["pivot.json", "verdict.json", "recording/A.jsonl", "recording/B.jsonl"] {
        assert!(dir.join(file).is_file(), "{file} missing from the bundle");
    }
}

/// The B leg's re-INVITE and the system's final to it, as recorded.
fn reinvite_and_final(outcome: &Outcome) -> (RecordedMessage, RecordedMessage) {
    let b = &outcome.recording.legs()["B"];
    let reinvite = b
        .iter()
        .find(|m| m.dir == Dir::Out && text(m).starts_with("INVITE "))
        .unwrap_or_else(|| panic!("leg B re-INVITEd: {b:#?}"))
        .clone();
    let final_ = b
        .iter()
        .find(|m| m.dir == Dir::In && text(m).starts_with("SIP/2.0 487"))
        .unwrap_or_else(|| panic!("the system answered the re-INVITE 487: {b:#?}"))
        .clone();
    (reinvite, final_)
}

/// The final the system sent at once is the 100's stand-in: the 100 step is
/// retired as omitted, named once in the verdict with the final's delay, and
/// the final goes on to the step that names it. The call ends as scripted.
#[tokio::test(start_paused = true)]
async fn a_final_sent_at_once_stands_in_for_the_100_and_goes_on_to_its_own_step() {
    let scene = api_scene("pivot-trying-omitted").await;
    let case = reinvite_answered("trying-omitted", 487, "Request Terminated");
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_settled_bundle(&outcome, &dir);
    let (reinvite, final_) = reinvite_and_final(&outcome);
    let after_ms = (final_.at_us - reinvite.at_us) / 1000;
    assert_eq!(
        outcome.verdict.failures,
        [Failure::TryingOmitted {
            step: "s13".into(),
            leg: "B".into(),
            arrived: Arrived::Response {
                status: 487,
                reason: "Request Terminated".into(),
                cseq_method: "INVITE".into(),
                cseq: 1,
            },
            after_ms,
        }],
        "the omission is the one difference, named: {:#?}",
        outcome.verdict
    );
    assert_eq!(outcome.verdict.retired, ["s13"], "{:#?}", outcome.verdict);
    assert!(outcome.verdict.abandoned.is_none(), "{:#?}", outcome.verdict.abandoned);
    for step in ["s14", "s15", "s16", "s17"] {
        assert!(
            outcome.verdict.completed_steps.iter().any(|s| s == step),
            "{step} ran: {:?}",
            outcome.verdict.completed_steps
        );
    }
    assert_eq!(final_.step.as_deref(), Some("s14"), "the final is s14's: {final_:#?}");
    assert!(
        final_.note.as_deref().is_some_and(|note| note.contains("s13") && note.contains("17.2.1")),
        "the recording names the omission on the final: {final_:#?}"
    );
    let b = &outcome.recording.legs()["B"];
    let ack = b.iter().find(|m| m.dir == Dir::Out && text(m).starts_with("ACK ")).unwrap();
    assert_eq!(ack.step.as_deref(), Some("s15"), "the scripted ACK acknowledges it: {ack:#?}");
    assert_eq!(via_branch(&text(ack)), via_branch(&text(&reinvite)), "§17.1.1.3: same branch");
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The stand-in is still held to the status its own step names: a final of
/// another status is that step's substitution, charged where it would have
/// been had the 100 come, and the run goes on to the scripted ACK.
#[tokio::test(start_paused = true)]
async fn a_final_that_stood_in_for_the_100_is_held_to_the_status_its_step_names() {
    let scene = api_scene("pivot-trying-omitted-substituted").await;
    let case =
        reinvite_answered("trying-omitted-substituted", 481, "Call/Transaction Does Not Exist");
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_settled_bundle(&outcome, &dir);
    let failures = &outcome.verdict.failures;
    assert_eq!(failures.len(), 2, "{failures:#?}");
    assert!(
        matches!(&failures[0], Failure::TryingOmitted { step, .. } if step == "s13"),
        "{failures:#?}"
    );
    assert!(
        matches!(
            &failures[1],
            Failure::UnmatchedDatagram {
                step,
                gated_on: GatedOn::Response { status: 481, .. },
                arrived: Arrived::Response { status: 487, .. },
                ..
            } if step == "s14"
        ),
        "{failures:#?}"
    );
    assert_eq!(outcome.verdict.retired, ["s13", "s14"], "{:#?}", outcome.verdict);
    assert!(outcome.verdict.abandoned.is_none(), "{:#?}", outcome.verdict.abandoned);
    let b = &outcome.recording.legs()["B"];
    let ack = b.iter().find(|m| m.dir == Dir::Out && text(m).starts_with("ACK ")).unwrap();
    assert_eq!(ack.step.as_deref(), Some("s15"), "{ack:#?}");
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// No final at all stands in for nothing: an armed 100 that nothing answers
/// is the missing message its budget names, and the script ends there.
#[tokio::test(start_paused = true)]
async fn a_100_no_final_answers_still_times_out() {
    let scene = api_scene("pivot-trying-unanswered").await;
    let mut case = reinvite_answered("trying-unanswered", 487, "Request Terminated");
    case.document.flow.retain(
        |node| !matches!(node, pivot_schema::flow::FlowNode::Message(step) if step.id == "s12"),
    );
    step_of(&mut case, "s13").delay.from = "step:s11".parse().expect("s11 is a step id");
    step_of(&mut case, "s13").within_ms = Some(500);
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_settled_bundle(&outcome, &dir);
    assert!(
        matches!(
            outcome.verdict.failures.as_slice(),
            [Failure::ExpectTimedOut { step, .. }] if step == "s13"
        ),
        "{:#?}",
        outcome.verdict.failures
    );
    assert!(outcome.verdict.retired.is_empty(), "{:#?}", outcome.verdict);
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}
