//! The final a sent BYE is owed, and the final each auto ACK acknowledges.

use super::*;

/// The final a sent BYE is owed is RFC 3261 §15.1.2's, not the document's: a
/// capture whose vantage closed between the BYE and its answer scripts no
/// expect for it, and the answer still arrives. It is recorded as absorbed and
/// the run stays green.
#[tokio::test(start_paused = true)]
async fn the_final_owed_to_a_sent_bye_is_absorbed_where_no_expect_scripts_it() {
    let scene = api_scene("pivot-owed-bye-final").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "linear-attempt-owed-bye-final".to_string();
    case.document.flow.retain(
        |node| !matches!(node, pivot_schema::flow::FlowNode::Message(step) if step.id == "s13"),
    );
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;
    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 12, "every remaining step ran");
    let absorbed = outcome.recording.legs()["A"]
        .iter()
        .find(|m| text(m).starts_with("SIP/2.0 200") && text(m).contains("\r\nCSeq: 2 BYE\r\n"))
        .unwrap_or_else(|| panic!("the owed final was recorded: {:#?}", outcome.recording.legs()))
        .clone();
    assert!(absorbed.note.as_deref().is_some_and(|note| note.contains("15.1.2")), "{absorbed:#?}");
    scene.finish().await;
}

/// A document that scripts the final ITSELF keeps it, whatever status it
/// scripts. The absorption stands for a vantage that closed before the answer,
/// never for a document that disagrees with the answer: the captured platform
/// answered a crossing BYE `501 Not Implemented`, ours answers `200`, and that
/// substitution is the confrontation's to name, not the runner's to eat.
#[tokio::test(start_paused = true)]
async fn a_final_the_document_scripts_is_not_absorbed_when_its_status_differs() {
    let scene = api_scene("pivot-scripted-bye-final").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "linear-attempt-scripted-bye-final".to_string();
    for node in &mut case.document.flow {
        if let pivot_schema::flow::FlowNode::Message(step) = node {
            if step.id == "s13" {
                step.msg.status = Some(501);
                step.msg.reason = Some("Not Implemented".to_string());
            }
        }
    }
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;
    let surfaced = outcome.recording.legs()["A"]
        .iter()
        .find(|m| text(m).starts_with("SIP/2.0 200") && text(m).contains("\r\nCSeq: 2 BYE\r\n"))
        .unwrap_or_else(|| panic!("the final was recorded: {:#?}", outcome.recording.legs()))
        .clone();
    assert!(
        !surfaced.note.as_deref().is_some_and(|note| note.contains("15.1.2")),
        "the scripted final is confronted, not absorbed: {surfaced:#?}"
    );
    assert!(
        outcome.verdict.failures.iter().any(|f| matches!(
            f,
            Failure::UnmatchedDatagram { step, .. } if step == "s13"
        )),
        "{:#?}",
        outcome.verdict.failures
    );
    scene.finish().await;
}

/// A non-2xx final a step takes is ACKed whether or not the document states the
/// ACK (RFC 3261 §17.1.1.3: the client transaction ACKs every non-2xx final).
/// A document with no ACK step for it has a gap where its capture stopped, not
/// a behaviour to reproduce: the leg ACKs on the INVITE's own branch, out of no
/// step, and the run is green.
#[tokio::test(start_paused = true)]
async fn a_reject_a_step_took_is_acked_where_the_document_states_no_ack() {
    let scene = api_scene("pivot-reject-taken-unacked").await;
    let mut case = fixture("unscripted-final.v3.json");
    case.document.case.id = "reject-taken-no-ack-step".into();
    take_the_reject(&mut case);
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;
    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 5, "{:?}", outcome.verdict.completed_steps);

    let a = &outcome.recording.legs()["A"];
    let invite = a.iter().find(|m| m.dir == Dir::Out && text(m).starts_with("INVITE ")).unwrap();
    let acks: Vec<_> =
        a.iter().filter(|m| m.dir == Dir::Out && text(m).starts_with("ACK ")).collect();
    assert_eq!(acks.len(), 1, "leg A ACKed the 486 once: {a:#?}");
    assert!(acks[0].step.is_none(), "no step scripts it: {:#?}", acks[0]);
    assert!(text(acks[0]).contains("\r\nCSeq: 1 ACK\r\n"), "{}", text(acks[0]));
    assert_eq!(via_branch(&text(acks[0])), via_branch(&text(invite)), "§17.1.1.3: same branch");
    scene.finish().await;
}

/// The same reject with the ACK the document DOES state: the step owns it, and
/// the leg sends no second one of its own.
#[tokio::test(start_paused = true)]
async fn a_reject_whose_ack_the_document_states_is_acked_by_that_step_alone() {
    let scene = api_scene("pivot-reject-taken-acked").await;
    let mut case = fixture("unscripted-final.v3.json");
    case.document.case.id = "reject-taken-ack-step".into();
    take_the_reject(&mut case);
    let ack: pivot_schema::flow::Step = serde_json::from_value(serde_json::json!({
        "auto": true,
        "delay": { "compressible": true, "from": "step:s5", "ms": 0, "timer_linked": false },
        "id": "s6", "leg": "A", "msg": { "cseq": 1, "method": "ACK" }, "op": "send"
    }))
    .expect("an ACK step");
    case.document.flow.push(pivot_schema::flow::FlowNode::Message(Box::new(ack)));
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;
    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 6, "{:?}", outcome.verdict.completed_steps);

    let a = &outcome.recording.legs()["A"];
    let acks: Vec<_> =
        a.iter().filter(|m| m.dir == Dir::Out && text(m).starts_with("ACK ")).collect();
    assert_eq!(acks.len(), 1, "one ACK, the document's: {a:#?}");
    assert_eq!(acks[0].step.as_deref(), Some("s6"), "{:#?}", acks[0]);
    scene.finish().await;
}

/// A reject of another status than the caller's expect names ends the
/// transaction that expect waits on (RFC 3261 §17.1.3): the expect is retired,
/// the run goes on, and the reject is still the client transaction's to ACK
/// (§17.1.1.3) where no step behind the retired one states the ACK.
#[tokio::test(start_paused = true)]
async fn a_reject_that_retires_an_expect_is_acked_where_the_document_states_no_ack() {
    let scene = api_scene("pivot-reject-retires-unacked").await;
    let mut case = fixture("unscripted-final.v3.json");
    case.document.case.id = "reject-retires-no-ack-step".into();
    let expected = step_of(&mut case, "s5");
    expected.msg.status = Some(481);
    expected.msg.reason = None;
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_eq!(outcome.verdict.retired, ["s5"], "{:#?}", outcome.verdict);
    assert!(outcome.verdict.abandoned.is_none(), "{:#?}", outcome.verdict.abandoned);
    let a = &outcome.recording.legs()["A"];
    let invite = a.iter().find(|m| m.dir == Dir::Out && text(m).starts_with("INVITE ")).unwrap();
    let acks: Vec<_> =
        a.iter().filter(|m| m.dir == Dir::Out && text(m).starts_with("ACK ")).collect();
    assert_eq!(acks.len(), 1, "leg A ACKed the 486 once: {a:#?}");
    assert!(acks[0].step.is_none(), "no step scripts it: {:#?}", acks[0]);
    assert_eq!(via_branch(&text(acks[0])), via_branch(&text(invite)), "§17.1.1.3: same branch");
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// `unscripted-final.v3.json` with the caller's last step taking the 486 the
/// callee sends, and nothing behind it.
fn take_the_reject(case: &mut Case) {
    let taken = step_of(case, "s5");
    taken.msg.status = Some(486);
    taken.msg.reason = Some("Busy Here".into());
}

/// An auto ACK finds the final it acknowledges in LEG STATE, never in the
/// step's `cseq`.
///
/// The fixture is the shape the corpus is full of: leg B answers the call as a
/// UAS, then originates an in-dialog re-INVITE as a UAC, and every auto step
/// carries the CAPTURED CSeq — the authoring platform's numbers (458007/458009
/// on A, 385579/385581 on B), which no leg of this run can mint. The stack
/// numbers its own CSeqs 1 and 2, so a document number resolves to no
/// transaction; the ACK is still owed and still composable, because the leg
/// knows which INVITE it has outstanding.
#[tokio::test(start_paused = true)]
async fn an_auto_ack_acks_the_leg_s_own_invite_whatever_cseq_the_capture_carried() {
    let scene = api_scene("pivot-reinvite-captured-cseq").await;
    let (outcome, dir) = replay(&scene, "reinvite-captured-cseq.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(
        outcome.verdict.completed_steps.len(),
        20,
        "every step ran: {:#?}",
        outcome.verdict.failures
    );

    // The one ACK leg B puts on the wire is the stack's own number for the one
    // INVITE that leg opened — emitted by s15, the step whose `cseq` names a
    // transaction that exists nowhere in this run.
    let legs = outcome.recording.legs();
    let acks: Vec<_> = legs["B"]
        .iter()
        .filter(|m| text(m).starts_with("ACK ") && m.step.as_deref() == Some("s15"))
        .collect();
    assert_eq!(acks.len(), 1, "the re-INVITE's ACK left leg B: {:#?}", legs["B"]);
    assert!(text(acks[0]).contains("CSeq: 1 ACK"), "{}", text(acks[0]));

    // And no captured token reached the wire at all, in either direction: §6.3
    // says `cseq` is never replayed, and this run has four chances to break it.
    for token in ["458007", "458009", "385579", "385581"] {
        for (leg, messages) in legs.iter() {
            assert!(
                messages.iter().all(|m| !text(m).contains(token)),
                "leg {leg} replayed the captured token {token}: {messages:#?}"
            );
        }
    }
    scene.finish().await;
}

/// A leg holding TWO un-ACKed INVITE transactions ACKs each of them, and each
/// ACK names its OWN.
///
/// RFC 3261 §14.1 leaves one INVITE outstanding per dialog, so leg state alone
/// names the transaction an auto ACK owes — until a capture records a peer that
/// broke §14.1. This fixture is that peer: leg B originates a re-INVITE, takes
/// its 200, then pipelines a SECOND re-INVITE before ACKing the first, draws the
/// §14.2 491 the crossing earns, ACKs that, and only then takes the deferred
/// 2xx ACK. Resolving either ACK by "the INVITE this leg sent last" hands both
/// to the newer transaction and strands the first 2xx un-ACKed for the life of
/// the dialog — the SUT then ladders it out and 491s everything that follows.
#[tokio::test(start_paused = true)]
async fn a_pipelined_re_invite_does_not_take_the_earlier_transaction_s_ack() {
    let scene = api_scene("pivot-reinvite-pipelined-unacked").await;
    // The §14.1 breach is the fixture's whole subject — the scripted callee is
    // the captured peer, and reproducing its non-compliance is what the case is
    // for. The SUT's own conduct stays gated.
    scene.h.allow_violation(
        "no-re-invite-while-invite-in-progress",
        "the scripted callee pipelines the second re-INVITE on purpose: it is the shape under test",
    );
    let (outcome, dir) = replay(&scene, "reinvite-pipelined-unacked.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);

    // Leg B minted CSeq 1 for its first re-INVITE and 2 for the crossing one.
    // s18 ACKs the 491 (CSeq 2, riding that INVITE's own branch, §17.1.1.3) and
    // s19 ACKs the deferred 200 (CSeq 1, a new transaction on the dialog).
    let legs = outcome.recording.legs();
    let ack_of = |step: &str| -> String {
        legs["B"]
            .iter()
            .filter(|m| text(m).starts_with("ACK ") && m.step.as_deref() == Some(step))
            .map(|m| text(m).clone())
            .next()
            .unwrap_or_else(|| panic!("{step} put no ACK on leg B: {:#?}", legs["B"]))
    };
    let negative = ack_of("s18");
    let deferred = ack_of("s19");
    assert!(negative.contains("CSeq: 2 ACK"), "s18 ACKs the 491's own transaction: {negative}");
    assert!(
        deferred.contains("CSeq: 1 ACK"),
        "s19 ACKs the transaction it is owed to, not the one s18 discharged: {deferred}"
    );

    // And the 2xx really was acknowledged: an ACK the SUT never matched leaves
    // its server transaction in RFC 6026 Accepted, laddering the 200 out.
    let repeats = legs["B"]
        .iter()
        .filter(|m| m.dir == Dir::In)
        .filter(|m| text(m).starts_with("SIP/2.0 200 OK") && text(m).contains("CSeq: 1 INVITE"))
        .count();
    assert_eq!(repeats, 1, "the answered re-INVITE was not laddered: {:#?}", legs["B"]);

    scene.finish().await;
}

/// A leg that ACKs ONE 2xx TWICE composes both, and the discharge the pipelined
/// case needs does not deny the second step its final.
///
/// RFC 3261 §13.2.2.4 has the UAC core generate one ACK per 2xx it RECEIVES, so
/// a second ACK on a fresh branch under one 2xx is the captured peer's own
/// non-compliance — and a `send` step MODELS a peer, so the document states it.
/// Resolving the final by "an INVITE not yet ACKed" alone strands that step: the
/// first ACK discharged the only transaction, and the leg then owns a 2xx it is
/// refused the right to acknowledge.
#[tokio::test(start_paused = true)]
async fn a_second_ack_for_one_2xx_still_finds_the_final_the_first_discharged() {
    let scene = api_scene("pivot-double-ack-one-2xx").await;
    let (outcome, dir) = replay(&scene, "double-ack-one-2xx.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(
        outcome.verdict.completed_steps.len(),
        14,
        "every step ran: {:#?}",
        outcome.verdict.failures
    );

    // Both ACKs left leg A under the one 2xx's CSeq, on DISTINCT branches: a
    // second transaction on the dialog, not a retransmission of the first.
    let legs = outcome.recording.legs();
    let acks: Vec<_> =
        legs["A"].iter().filter(|m| m.dir == Dir::Out && text(m).starts_with("ACK ")).collect();
    assert_eq!(acks.len(), 2, "both ACK steps reached the wire: {:#?}", legs["A"]);
    let branch = |raw: &str| {
        raw.lines()
            .find_map(|line| line.split_once("branch="))
            .map(|(_, b)| b.trim().to_string())
            .expect("every ACK carries a Via branch")
    };
    assert_ne!(
        branch(&text(acks[0])),
        branch(&text(acks[1])),
        "the re-ACK opens its own transaction: {:#?}",
        acks
    );
    for ack in &acks {
        assert!(text(ack).contains("CSeq: 1 ACK"), "both name the answered INVITE: {}", text(ack));
    }

    scene.finish().await;
}
