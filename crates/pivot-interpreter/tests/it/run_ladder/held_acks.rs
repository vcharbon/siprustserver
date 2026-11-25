//! The ACK a final the leg took is owed, and the answer to a non-INVITE
//! request the leg took, sent past an earlier expect of the leg that never
//! matches (§6.7h).

use super::*;

/// A required expect on leg A of a request the system never sends, anchored on `from`.
fn never_arriving(id: &str, from: &str, ms: u64) -> pivot_schema::flow::FlowNode {
    never_arriving_on("A", id, from, ms)
}

/// A required expect on `leg` of a request the system never sends, anchored on `from`.
fn never_arriving_on(leg: &str, id: &str, from: &str, ms: u64) -> pivot_schema::flow::FlowNode {
    let step: pivot_schema::flow::Step = serde_json::from_value(serde_json::json!({
        "delay": { "compressible": true, "from": format!("step:{from}"), "ms": ms, "timer_linked": false },
        "check": "record", "id": id, "leg": leg, "msg": { "method": "INFO" }, "op": "expect"
    }))
    .expect("an INFO expect");
    pivot_schema::flow::FlowNode::Message(Box::new(step))
}

/// Insert `node` into the flow right before the step `before`.
fn insert_before(case: &mut Case, before: &str, node: pivot_schema::flow::FlowNode) {
    let at = case
        .document
        .flow
        .iter()
        .position(|n| matches!(n, pivot_schema::flow::FlowNode::Message(s) if s.id == before))
        .unwrap_or_else(|| panic!("no step {before:?}"));
    case.document.flow.insert(at, node);
}

/// The out-bound ACKs a leg recorded.
fn acks_out(outcome: &Outcome, leg: &str) -> Vec<RecordedMessage> {
    outcome.recording.legs()[leg]
        .iter()
        .filter(|m| m.dir == Dir::Out && text(m).starts_with("ACK "))
        .cloned()
        .collect()
}

/// When the step `step` took its datagram on `leg`.
fn taken_at(outcome: &Outcome, leg: &str, step: &str) -> u64 {
    outcome.recording.legs()[leg]
        .iter()
        .find(|m| m.dir == Dir::In && m.step.as_deref() == Some(step))
        .unwrap_or_else(|| panic!("{step} took nothing: {:#?}", outcome.recording.legs()[leg]))
        .at_us
}

fn timed_out(outcome: &Outcome, step: &str) -> bool {
    outcome
        .verdict
        .failures
        .iter()
        .any(|f| matches!(f, Failure::ExpectTimedOut { step: s, .. } if s == step))
}

/// RFC 3261 §13.2.2.4: the caller's ACK to the 2xx goes out at the document's
/// offset from the 2xx (10 ms to the INFO expect, then 40 ms), not when the
/// expect standing in front of it times out. The expect still fails the run,
/// and the close sends no second ACK.
#[tokio::test(start_paused = true)]
async fn the_ack_to_a_taken_2xx_goes_out_past_an_expect_that_never_matches() {
    let scene = api_scene("pivot-held-ack-2xx").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "linear-attempt-held-ack-2xx".to_string();
    insert_before(&mut case, "s8", never_arriving("s7i", "s7", 10));
    let ack = step_of(&mut case, "s8");
    ack.delay.from = pivot_schema::flow::Anchor::Step("s7i".into());
    ack.delay.ms = 40;
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert!(timed_out(&outcome, "s7i"), "the expect still fails: {:#?}", outcome.verdict.failures);
    let acks = acks_out(&outcome, "A");
    assert_eq!(acks.len(), 1, "one ACK on leg A: {acks:#?}");
    assert_eq!(acks[0].step.as_deref(), Some("s8"), "the step sends it: {:#?}", acks[0]);
    let offset_us = acks[0].at_us - taken_at(&outcome, "A", "s7");
    assert!((50_000..55_000).contains(&offset_us), "50 ms after the 2xx, got {offset_us} us");
    assert!(outcome.verdict.completed_steps.iter().any(|s| s == "s9"), "leg B took the ACK");
    scene.finish().await;
}

/// RFC 3261 §17.1.1.3: a non-2xx final is ACKed by its client transaction at
/// once, so the scripted ACK to the 486 goes out when the 486 arrives, not at
/// its dwell behind the expect in front of it, nor when that expect times out.
#[tokio::test(start_paused = true)]
async fn the_ack_to_a_taken_reject_goes_out_at_once_past_an_expect_that_never_matches() {
    let scene = api_scene("pivot-held-ack-reject").await;
    let mut case = fixture("unscripted-final.v3.json");
    case.document.case.id = "reject-held-ack".into();
    let taken = step_of(&mut case, "s5");
    taken.msg.status = Some(486);
    taken.msg.reason = Some("Busy Here".into());
    case.document.flow.push(never_arriving("s6", "s5", 10));
    let ack: pivot_schema::flow::Step = serde_json::from_value(serde_json::json!({
        "delay": { "compressible": true, "from": "step:s6", "ms": 300, "timer_linked": false },
        "auto": true,
        "id": "s7", "leg": "A", "msg": { "cseq": 1, "method": "ACK" }, "op": "send"
    }))
    .expect("an ACK step");
    case.document.flow.push(pivot_schema::flow::FlowNode::Message(Box::new(ack)));
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert!(timed_out(&outcome, "s6"), "the expect still fails: {:#?}", outcome.verdict.failures);
    let acks = acks_out(&outcome, "A");
    assert_eq!(acks.len(), 1, "one ACK on leg A: {acks:#?}");
    assert_eq!(acks[0].step.as_deref(), Some("s7"), "the step sends it: {:#?}", acks[0]);
    let offset_us = acks[0].at_us - taken_at(&outcome, "A", "s5");
    assert!(offset_us < 5_000, "at once after the 486, got {offset_us} us");
    scene.finish().await;
}

/// The ACK waits for its final: where the 2xx never arrives, nothing sends the
/// scripted ACK before the expect in front of it times out.
#[tokio::test(start_paused = true)]
async fn an_ack_whose_final_never_arrived_stays_behind_the_expect_in_front() {
    let scene = api_scene("pivot-held-ack-no-final").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "linear-attempt-held-ack-no-final".to_string();
    case.document.flow.retain(|node| {
        !matches!(node, pivot_schema::flow::FlowNode::Message(s) if s.id == "s6" || s.id == "s7")
    });
    insert_before(&mut case, "s8", never_arriving("s7i", "s5", 10));
    let ack = step_of(&mut case, "s8");
    ack.delay.from = pivot_schema::flow::Anchor::Step("s5".into());
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert!(timed_out(&outcome, "s7i"), "{:#?}", outcome.verdict.failures);
    assert!(
        acks_out(&outcome, "A").iter().all(|m| m.step.as_deref() != Some("s8")),
        "no 2xx was taken, so the step never ran: {:#?}",
        outcome.recording.legs()["A"]
    );
    scene.finish().await;
}

/// Leg B awaiting, before the BYE, an INFO the document draws from A's ACK
/// (s8), the way a relayed request stands: the system never sends it.
fn await_an_info_relayed_to_b(case: &mut Case) {
    insert_before(case, "s11", never_arriving_on("B", "s9i", "s8", 0));
}

/// When leg `leg` sent the datagram of step `step`.
fn sent_at(outcome: &Outcome, leg: &str, step: &str) -> Option<u64> {
    outcome.recording.legs()[leg]
        .iter()
        .find(|m| m.dir == Dir::Out && m.step.as_deref() == Some(step))
        .map(|m| m.at_us)
}

/// RFC 3261 §17.2.2: the callee's 200 to the BYE it took goes out at its
/// dwell from the BYE, not when the INFO expect standing in front of it times
/// out. The expect still fails the run, and the caller takes the 200.
#[tokio::test(start_paused = true)]
async fn the_answer_to_a_taken_bye_goes_out_past_an_expect_that_never_matches() {
    let scene = api_scene("pivot-held-bye-answer").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "linear-attempt-held-bye-answer".to_string();
    await_an_info_relayed_to_b(&mut case);
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert!(timed_out(&outcome, "s9i"), "the expect still fails: {:#?}", outcome.verdict.failures);
    let answered = sent_at(&outcome, "B", "s12")
        .unwrap_or_else(|| panic!("leg B answered the BYE: {:#?}", outcome.recording.legs()["B"]));
    let offset_us = answered - taken_at(&outcome, "B", "s11");
    assert!(offset_us < 5_000, "at the 200's dwell from the BYE, got {offset_us} us");
    assert!(outcome.verdict.completed_steps.iter().any(|s| s == "s13"), "leg A took the 200");
    scene.finish().await;
}

/// The answer waits for its request: where the BYE never arrives, nothing
/// answers it before the expect in front times out.
#[tokio::test(start_paused = true)]
async fn an_answer_to_a_request_never_taken_stays_behind_the_expect_in_front() {
    let scene = api_scene("pivot-held-bye-answer-no-bye").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "linear-attempt-held-bye-answer-no-bye".to_string();
    case.document.flow.retain(|node| {
        !matches!(node, pivot_schema::flow::FlowNode::Message(s) if s.id == "s10" || s.id == "s13")
    });
    step_of(&mut case, "s11").delay.from = pivot_schema::flow::Anchor::Step("s9".into());
    await_an_info_relayed_to_b(&mut case);
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert!(timed_out(&outcome, "s9i"), "{:#?}", outcome.verdict.failures);
    assert_eq!(
        sent_at(&outcome, "B", "s12"),
        None,
        "no BYE was taken: {:#?}",
        outcome.recording.legs()["B"]
    );
    scene.finish().await;
}
