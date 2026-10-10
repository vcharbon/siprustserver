//! Negative cases (§11.2): a declared divergence, its absence, and where it lands against the
//! anchor.

use super::*;

/// **Rung seven, the negative case** (§11.2): a document that declares the
/// failure its run MUST produce, and passes only by producing exactly that one.
///
/// The source's peer never ACKed the callee's 200 — the capture holds no such
/// datagram, so the document holds no step for it — and this platform answers a
/// dialog-creating 2xx LOCALLY (RFC 3261 §13.2.2.4). So the run puts an ACK on
/// leg B where nothing expects it, and the document says so in advance. Nothing
/// is softened for it: the gate refuses the datagram, the failure is raised with
/// its site, and the recording holds it verbatim.
///
/// That delta stops NOTHING (§11.2): an ACK arriving where a BYE is expected
/// leaves the BYE as reachable as it was, so the failure is recorded and the run
/// GOES ON. The scripted teardown runs to completion, the call ends the way the
/// document says it ends, and no generic close is needed. The VERDICT is what
/// inverts.
#[tokio::test(start_paused = true)]
async fn rung_seven_a_negative_case_passes_by_failing_exactly_as_it_declared() {
    let scene = api_scene("pivot-unacked-2xx").await;
    let (outcome, dir) = replay(&scene, "unacked-2xx-negative.v3.json").await;

    assert_eq!(
        outcome.verdict.status,
        VerdictStatus::OkNegative,
        "failures: {:#?}\nrecording: {:#?}",
        outcome.verdict.failures,
        outcome.recording.legs()
    );
    assert!(outcome.verdict.failures.is_empty(), "{:#?}", outcome.verdict.failures);
    assert_eq!(outcome.verdict.failed_step, None);
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");

    // The declaration, against what the run produced for it.
    assert_eq!(outcome.verdict.must_fail.len(), 1, "{:#?}", outcome.verdict.must_fail);
    let note = &outcome.verdict.must_fail[0];
    assert_eq!(note.failure, DeclaredFailure::UnexpectedAck);
    assert_eq!(note.step, "s6", "the anchor is the 2xx the divergence turns on");
    assert_eq!(note.derived_from, RfcRule::NoAckToDialogCreating2xx);
    let observed = note.observed.as_ref().expect("the run produced the declared failure");
    let (leg, arrived) = arrival_of(observed);
    assert_eq!(leg, "B", "the anchor's own leg");
    assert!(matches!(arrived, Arrived::Request { method, .. } if method == "ACK"), "{arrived}");

    // The delta did not prevent the next message from being composed, so the
    // script ran to its end: every step, the scripted teardown included, and
    // nothing abandoned.
    assert_eq!(outcome.verdict.completed_steps.len(), 12, "{:?}", outcome.verdict.completed_steps);
    assert!(outcome.verdict.abandoned.is_none(), "{:#?}", outcome.verdict.abandoned);
    for step in ["s10", "s11", "s12"] {
        assert!(
            outcome.verdict.completed_steps.iter().any(|ran| ran == step),
            "the teardown behind the delta still ran: {:?}",
            outcome.verdict.completed_steps
        );
    }

    // The recording is a faithful account either way: the ACK is in leg B's
    // ladder, unclaimed by any step, exactly where it landed — and the BYE
    // BEHIND it is claimed by `s10`, which the ACK never disarmed.
    let legs = outcome.recording.legs();
    let ack = legs["B"]
        .iter()
        .find(|m| text(m).starts_with("ACK "))
        .unwrap_or_else(|| panic!("no ACK on leg B: {:#?}", legs["B"]));
    assert_eq!(ack.step, None, "no step claimed it — that is what made it a failure");
    let bye = legs["B"]
        .iter()
        .find(|m| text(m).starts_with("BYE "))
        .unwrap_or_else(|| panic!("no BYE on leg B: {:#?}", legs["B"]));
    assert_eq!(bye.step.as_deref(), Some("s10"), "the expect the ACK was refused against");

    assert!(dir.join("verdict.json").is_file());
    let written: pivot_interpreter::RunVerdict =
        serde_json::from_str(&std::fs::read_to_string(dir.join("verdict.json")).unwrap()).unwrap();
    assert_eq!(written.status, VerdictStatus::OkNegative);
    assert_eq!(written.must_fail, outcome.verdict.must_fail);
    assert_eq!(written.abandoned, None);

    // The postcondition the document states was evaluated: one CDR, billed for
    // a call that ended. Presence is what a negative case asserts; what is IN
    // the record is deliberately not this case's subject.
    assert_eq!(scene.b2bua.cdr_records().len(), 1);
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The first red proof, and the reason negative cases are worth generating: the
/// SAME document with the declaration removed is a plain failing run. The
/// detection the negative case rides on is alive, and a case that could go green
/// by behaving well would prove nothing.
#[tokio::test(start_paused = true)]
async fn the_same_document_without_its_declaration_fails_on_the_very_same_ack() {
    let scene = api_scene("pivot-unacked-2xx-undeclared").await;
    let mut case = fixture("unacked-2xx-negative.v3.json");
    case.document.must_fail.clear();
    // Its own id, so this variant's bundle sits beside the negative case's
    // rather than overwriting the very run it is the counter-proof to.
    case.document.case.id = "unacked-2xx-undeclared".into();
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert!(outcome.verdict.must_fail.is_empty(), "nothing was declared");
    let refused = outcome
        .verdict
        .failures
        .iter()
        .find(|f| {
            matches!(f, Failure::UnexpectedDatagram { arrived, .. }
                | Failure::UnmatchedDatagram { arrived, .. }
                if matches!(arrived, Arrived::Request { method, .. } if method == "ACK"))
        })
        .unwrap_or_else(|| panic!("no ACK failure: {:#?}", outcome.verdict.failures));
    assert_eq!(arrival_of(refused).0, "B");
    // Polarity changes nothing about the RUN, only about the verdict: the ACK
    // stopped nothing here either, so the script ran to its end, tore its own
    // call down and needed no generic close.
    assert!(outcome.verdict.abandoned.is_none());
    assert_eq!(outcome.verdict.completed_steps.len(), 12, "{:?}", outcome.verdict.completed_steps);

    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The second red proof, the other direction: reality stops failing and the
/// negative case is what turns red. The ordinary linear document — whose flow
/// DOES hold the platform's ACK — carrying the same declaration runs green on
/// the wire and fails on the declaration nothing produced.
#[tokio::test(start_paused = true)]
async fn a_declaration_whose_divergence_never_happened_fails_the_run() {
    let scene = api_scene("pivot-unacked-2xx-repaired").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "unacked-2xx-repaired".into();
    case.document.must_fail.push(MustFail {
        failure: DeclaredFailure::UnexpectedAck,
        step: "s6".into(),
        derived_from: RfcRule::NoAckToDialogCreating2xx,
    });
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    // Every step ran and the wire was clean — and the run still fails, because
    // a negative case that does not fail as declared has not passed.
    assert_eq!(outcome.verdict.completed_steps.len(), 13, "{:?}", outcome.verdict.completed_steps);
    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert_eq!(outcome.verdict.failed_step.as_deref(), Some("s6"));
    assert!(outcome.verdict.must_fail[0].observed.is_none());
    let Some(Failure::DeclaredFailureNotProduced { declared, detail, .. }) =
        outcome.verdict.failures.first()
    else {
        panic!("{:#?}", outcome.verdict.failures)
    };
    assert_eq!(*declared, DeclaredFailure::UnexpectedAck);
    assert!(detail.contains("no unclaimed ACK"), "{detail}");
    // Nothing aborted, so nothing was abandoned: the script itself tore the
    // call down, and the close had nothing to close.
    assert!(outcome.verdict.abandoned.is_none());

    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// **The inclusion semantic, on a real run** (§11.2): a declaration says what
/// the run MUST produce, not everything it may produce. A wire divergence BESIDE
/// the declared one — here a ladder our platform paces differently, the very
/// thing a withheld ACK causes — is carried, listed under `tolerated`, and the
/// negative case is still green.
#[tokio::test(start_paused = true)]
async fn a_wire_divergence_past_the_anchor_is_carried_by_the_negative_case() {
    let scene = api_scene("pivot-unacked-2xx-carried").await;
    let mut case = fixture("unacked-2xx-negative.v3.json");
    case.document.case.id = "unacked-2xx-carried".into();
    // `s12` is leg A's expect of the 200 to the BYE, past the `s6` anchor. A
    // non-INVITE final rides no ladder of its own (§17.2.2), so the document's
    // count IS the assertion there — one repeat declared, and the run sees none.
    declare_retransmits(&mut case, "s12", 1);
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_eq!(
        outcome.verdict.status,
        VerdictStatus::OkNegative,
        "failures: {:#?}",
        outcome.verdict.failures
    );
    assert!(outcome.verdict.failures.is_empty(), "{:#?}", outcome.verdict.failures);
    assert!(outcome.verdict.must_fail[0].observed.is_some());
    // Carried, and never invisible: the reader of the verdict sees it.
    let Some(Failure::RetransmitCountMismatch { step, declared, observed, .. }) =
        outcome.verdict.tolerated.first()
    else {
        panic!("{:#?}", outcome.verdict.tolerated)
    };
    assert_eq!((step.as_str(), *declared, *observed), ("s12", 1, 0));

    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The counter-proof: the SAME divergence BEFORE the anchor fails the run. Up to
/// the declared divergence the replay was still following the capture, so a
/// failure there is a defect of its own and no consequence of the declaration.
#[tokio::test(start_paused = true)]
async fn a_wire_divergence_before_the_anchor_still_fails_the_negative_case() {
    let scene = api_scene("pivot-unacked-2xx-early").await;
    let mut case = fixture("unacked-2xx-negative.v3.json");
    case.document.case.id = "unacked-2xx-early".into();
    // `s5` is leg A's expect of the 180, one step AHEAD of the anchor. An
    // unreliable provisional rides no ladder of its own, so the document's count
    // is the assertion there and the run sees none.
    declare_retransmits(&mut case, "s5", 1);
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert!(outcome.verdict.tolerated.is_empty(), "{:#?}", outcome.verdict.tolerated);
    assert_eq!(outcome.verdict.failed_step.as_deref(), Some("s5"));
    // The declared failure still matched: only the early divergence turned it red.
    assert!(outcome.verdict.must_fail[0].observed.is_some());

    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}
