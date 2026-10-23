//! The generic close (§11.2): a script that ends, on a negative or a positive run, before its call
//! does.

use super::*;

/// **The generic close, doing real work** (§11.2): a negative case whose script
/// ends BEFORE the teardown it scripts, so the scripted ends terminate the call
/// themselves.
///
/// Gate `s7` on a 183 and the 200 that really arrives is a CONTRADICTING FINAL:
/// it ends the very transaction `s7` waits on, so the 183 can never come and the
/// run cannot go on. The delta is one step past the `s6` anchor and two steps
/// ahead of the ACK and the BYE the flow holds, so the close is what
/// acknowledges the 2xx (RFC 3261 §13.2.2.4), what closes the dialog leg A
/// opened (§15), and what answers the teardown on leg B, which ANSWERED its
/// dialog and therefore never starts one.
///
/// The declared ACK still arrives — the platform relays the close's own ACK —
/// and it arrives where no expect is armed, so no failure names it. §11.2
/// matches a declaration against the RECORDING, which holds it: the case is
/// green as a negative one, and the verdict says which of the two ways it was
/// observed.
#[tokio::test(start_paused = true)]
async fn a_script_that_ends_before_its_teardown_is_closed_by_the_scripted_ends() {
    let scene = api_scene("pivot-unacked-2xx-closed").await;
    let mut case = fixture("unacked-2xx-negative.v3.json");
    case.document.case.id = "unacked-2xx-closed".into();
    expect_status(&mut case, "s7", 183);
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_eq!(
        outcome.verdict.status,
        VerdictStatus::OkNegative,
        "failures: {:#?}\nrecording: {:#?}",
        outcome.verdict.failures,
        outcome.recording.legs()
    );
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");
    // Observed by the recording, not by a gate: the script had ended.
    let note = &outcome.verdict.must_fail[0];
    assert!(note.observed.is_none(), "no expect was armed to refuse it: {:#?}", note.observed);
    assert!(note.recorded.as_deref().is_some_and(|a| a.starts_with("ACK ")), "{note:#?}");
    // The delta itself is the carried one: a wire divergence past the anchor.
    assert_eq!(outcome.verdict.tolerated.len(), 1, "{:#?}", outcome.verdict.tolerated);

    // Three acts, and each is the RFC's own answer to what its leg held.
    let abandoned = outcome.verdict.abandoned.as_ref().expect("the script was abandoned");
    assert_eq!(abandoned.step.as_deref(), Some("s7"));
    let acts: Vec<(&str, CloseOwed)> =
        abandoned.closed.iter().map(|act| (act.leg.as_str(), act.owed)).collect();
    assert_eq!(
        acts,
        [("A", CloseOwed::Ack), ("A", CloseOwed::Bye), ("B", CloseOwed::Answer)],
        "{:#?}",
        abandoned.closed
    );
    assert!(abandoned.closed[1].sent.starts_with("BYE "), "{:?}", abandoned.closed[1].sent);

    assert_eq!(scene.b2bua.cdr_records().len(), 1, "the call was billed");
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The same close on a RED negative run: a case that fails for a reason of its
/// own must not leak a call either.
///
/// Two divergences, one on each side of the anchor. The `s7` delta ends the
/// script and the close tears the call down exactly as above; the `s3` ladder is
/// BEFORE the `s6` anchor, so it is a defect of its own and the run is red. The
/// close runs regardless — a run's calls end whatever its verdict says.
#[tokio::test(start_paused = true)]
async fn a_red_negative_run_is_still_closed_and_leaves_no_call_up() {
    let scene = api_scene("pivot-unacked-2xx-closed-red").await;
    let mut case = fixture("unacked-2xx-negative.v3.json");
    case.document.case.id = "unacked-2xx-closed-red".into();
    expect_status(&mut case, "s7", 183);
    // `s5` is leg A's expect of the 180, one step AHEAD of the anchor.
    declare_retransmits(&mut case, "s5", 1);
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert!(outcome.verdict.tolerated.is_empty(), "carrying is all or nothing");
    // The whole account, not a filtered half: the delta that ended the script
    // AND the divergence before the anchor that made the run red.
    assert_eq!(outcome.verdict.failures.len(), 2, "{:#?}", outcome.verdict.failures);
    assert!(
        outcome.verdict.failures.iter().any(|f| matches!(f,
            Failure::RetransmitCountMismatch { step, .. } if step == "s5")),
        "{:#?}",
        outcome.verdict.failures
    );
    // The declared divergence still happened, and is still stated.
    assert!(outcome.verdict.must_fail[0].recorded.is_some(), "{:#?}", outcome.verdict.must_fail);
    // And the call is over: the close ran, the CDR was written, nothing is up.
    let abandoned = outcome.verdict.abandoned.as_ref().expect("the script was abandoned");
    assert_eq!(abandoned.closed.len(), 3, "{:#?}", abandoned.closed);
    assert!(outcome.timing.settled_at_ms.is_some(), "a red run still settles");
    assert_eq!(scene.b2bua.cdr_records().len(), 1);
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The close's OTHER caller shape, on the other blocking trigger: a MISSING
/// MESSAGE. An INVITE with a provisional and no final is cancelled (RFC 3261
/// §9.1), and the callee end answers the INVITE it took.
///
/// Gate `s5` on a 183 and the 180 that really arrives is a delta the run walks
/// past: a provisional ends no transaction, so the 183 could still come. What
/// ends the script is `s5`'s own budget running out — and with `s6` dwelling on
/// `s5`, the call is still ringing when it does. Both failures are recorded, the
/// run is red (the delta is BEFORE the `s6` anchor and the declared ACK is never
/// produced), and the close is what ends the ringing call rather than a timer.
#[tokio::test(start_paused = true)]
async fn a_ringing_call_the_script_walked_away_from_is_cancelled_by_its_caller() {
    let scene = api_scene("pivot-unacked-2xx-cancelled").await;
    let mut case = fixture("unacked-2xx-negative.v3.json");
    case.document.case.id = "unacked-2xx-cancelled".into();
    expect_status(&mut case, "s5", 183);
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert_eq!(outcome.verdict.failed_step.as_deref(), Some("s5"));
    // BOTH failures, in the order they happened: the 180 the run walked past
    // BECAUSE a provisional ends no transaction, and then the budget that ran out
    // on the message that never came, which is what ended the script.
    let Some(Failure::UnmatchedDatagram { step, arrived, .. }) = outcome.verdict.failures.first()
    else {
        panic!("{:#?}", outcome.verdict.failures)
    };
    assert_eq!(step.as_str(), "s5");
    assert!(matches!(arrived, Arrived::Response { status: 180, .. }), "{arrived}");
    assert!(
        matches!(outcome.verdict.failures.get(1),
            Some(Failure::ExpectTimedOut { step, .. }) if step == "s5"),
        "{:#?}",
        outcome.verdict.failures
    );
    // Nothing answered the INVITE, so the platform sent no ACK to declare.
    assert!(outcome.verdict.must_fail[0].observed.is_none());
    assert!(outcome.verdict.must_fail[0].recorded.is_none());
    assert!(
        outcome
            .verdict
            .failures
            .iter()
            .any(|f| matches!(f, Failure::DeclaredFailureNotProduced { .. })),
        "{:#?}",
        outcome.verdict.failures
    );

    let abandoned = outcome.verdict.abandoned.as_ref().expect("the script was abandoned");
    let acts: Vec<(&str, CloseOwed)> =
        abandoned.closed.iter().map(|act| (act.leg.as_str(), act.owed)).collect();
    assert!(acts.contains(&("A", CloseOwed::Cancel)), "{:#?}", abandoned.closed);
    assert!(
        acts.iter().any(|(leg, owed)| *leg == "B" && *owed == CloseOwed::Answer),
        "the callee answers the INVITE it took: {:#?}",
        abandoned.closed
    );
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// **The close on a POSITIVE run** (§11.2): a document that declares nothing,
/// whose expected answer is a rejection instead.
///
/// The script says the callee answers and the caller takes a 200; the callee
/// rejects with a 486 and the platform relays it. That final ENDS the very
/// transaction `s7` waits on, so the 200 can never come and the run cannot go
/// on — which has nothing to do with the document's polarity. The script ends at
/// `s7`, the caller acknowledges the reject it took (RFC 3261 §17.1.1.3), the
/// system settles and the attempt is billed. The VERDICT still fails: this run
/// declared nothing, so nothing inverts, and the abandonment is stated beside
/// the failure rather than hiding it.
#[tokio::test(start_paused = true)]
async fn a_positive_run_whose_answer_is_a_reject_ends_its_script_and_closes() {
    let scene = api_scene("pivot-answer-rejected").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "answer-rejected".into();
    reject_at(&mut case, "s6", 486);
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert!(outcome.verdict.must_fail.is_empty(), "nothing was declared");
    assert_eq!(outcome.verdict.failed_step.as_deref(), Some("s7"));
    let Some(Failure::UnmatchedDatagram { step, leg, gated_on, arrived, .. }) =
        outcome.verdict.failures.first()
    else {
        panic!("{:#?}", outcome.verdict.failures)
    };
    assert_eq!((step.as_str(), leg.as_str()), ("s7", "A"));
    assert!(
        matches!(gated_on, GatedOn::Response { status: 200, cseq_method: Some(m) } if m == "INVITE"),
        "{gated_on}"
    );
    assert!(matches!(arrived, Arrived::Response { status: 486, .. }), "{arrived}");
    // The close ended the call; nothing is stacked on top of the real finding.
    assert_eq!(outcome.verdict.failures.len(), 1, "{:#?}", outcome.verdict.failures);

    // The abandonment is STATED on a positive run exactly as on a negative one.
    let abandoned = outcome.verdict.abandoned.as_ref().expect("the script was abandoned");
    assert_eq!((abandoned.leg.as_deref(), abandoned.step.as_deref()), (Some("A"), Some("s7")));
    for ran in ["s1", "s2", "s3", "s4", "s5", "s6"] {
        assert!(
            outcome.verdict.completed_steps.iter().any(|step| step == ran),
            "{ran} ran before the reject: {:?}",
            outcome.verdict.completed_steps
        );
    }
    // The refused answer's expect is RETIRED — the 486 ended its transaction
    // (§17.1.3) and was charged on it — and everything the caller's tail
    // scripted behind it is abandoned: the ACK that confirms the answer never
    // came, nor the BYE that ends it.
    assert_eq!(outcome.verdict.retired, ["s7"]);
    for never in ["s8", "s10", "s11", "s12", "s13"] {
        assert!(
            abandoned.pending.iter().any(|step| step == never),
            "{never} is abandoned: {:?}",
            abandoned.pending
        );
        assert!(
            outcome.verdict.completed_steps.iter().all(|step| step != never),
            "no step is both run and abandoned: {:?}",
            outcome.verdict.completed_steps
        );
    }
    // A rejected INVITE holds no dialog open, and the ACK §17.1.1.3 owes went out
    // where the reject was REFUSED — so the close is left with nothing, and the
    // leg that answered 486 holds nothing at all.
    assert!(abandoned.closed.is_empty(), "{:#?}", abandoned.closed);
    let ack = outcome.recording.legs()["A"]
        .iter()
        .find(|m| m.dir == Dir::Out && text(m).starts_with("ACK "))
        .cloned()
        .expect("the reject the flow never scripted was acknowledged");
    assert!(ack.note.as_deref().is_some_and(is_unscripted_answer), "{:?}", ack.note);
    assert!(ack.step.is_none(), "the ACK satisfied no expect: {ack:#?}");

    // And the run still owes its call an ending and its attempt a record.
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");
    assert_eq!(scene.b2bua.cdr_records().len(), 1, "the attempt was billed");
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// **A refused send closes the call** (§11.2): a deviation this interpreter
/// cannot emit on a MID-DIALOG send stops the script there — and the close, not
/// a timeout, is what ends the established call the script could no longer
/// script.
#[tokio::test(start_paused = true)]
async fn a_send_refused_mid_dialog_ends_the_script_and_the_close_ends_the_call() {
    let scene = api_scene("pivot-refused-send-closed").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "refused-send-closed".into();
    // The scripted BYE demands an emission fidelity no composition guarantees,
    // so the run refuses it AFTER the call is established.
    case.document.deviations.push(pivot_schema::deviation::Deviation {
        id: "d1".into(),
        kind: "verbatim-emission".into(),
        leg: Some("A".into()),
        step: Some("s10".into()),
        header: None,
        preserve: vec!["header-order".into(), "absolute-order".into()],
        retransmits: None,
        races: None,
        value: None,
    });
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert!(
        outcome.verdict.failures.iter().any(|f| matches!(f,
            Failure::DeviationUnimplemented { step, .. } if step.as_deref() == Some("s10"))),
        "{:#?}",
        outcome.verdict.failures
    );
    // The refusal is stated as the abandonment it is, and the close ended the
    // established dialog: the caller BYEs, the callee answers it.
    let abandoned = outcome.verdict.abandoned.as_ref().expect("the script was abandoned");
    assert_eq!((abandoned.leg.as_deref(), abandoned.step.as_deref()), (Some("A"), Some("s10")));
    let acts: Vec<(&str, CloseOwed)> =
        abandoned.closed.iter().map(|act| (act.leg.as_str(), act.owed)).collect();
    assert_eq!(acts, [("A", CloseOwed::Bye), ("B", CloseOwed::Answer)], "{:#?}", abandoned.closed);
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");
    assert_eq!(scene.b2bua.cdr_records().len(), 1, "the call was billed");
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// **A missing injector closes the call** (§11.2): an `inject` no lane binding
/// performs aborts the run by name — and the call the script had already
/// established is still torn down rather than left to a settle timeout.
#[tokio::test(start_paused = true)]
async fn a_missing_injector_ends_the_script_and_the_close_ends_the_call() {
    let scene = api_scene("pivot-injectorless-closed").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "injectorless-closed".into();
    // An external fault after the ACK lands, before the scripted teardown.
    let at = case
        .document
        .flow
        .iter()
        .position(|node| node.id() == "s10")
        .expect("the fixture has its BYE");
    case.document.flow.insert(
        at,
        pivot_schema::flow::FlowNode::Inject(pivot_schema::flow::Inject {
            id: "i1".into(),
            op: pivot_schema::flow::InjectOp::Inject,
            action: "node-kill".into(),
            target: None,
            after: vec!["s9".into()],
            delay: None,
        }),
    );
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert!(
        outcome.verdict.failures.iter().any(|f| matches!(f,
            Failure::InjectorMissing { node, action }
                if node == "i1" && action == "node-kill")),
        "{:#?}",
        outcome.verdict.failures
    );
    let abandoned = outcome.verdict.abandoned.as_ref().expect("the script was abandoned");
    assert_eq!(abandoned.step.as_deref(), Some("i1"));
    let acts: Vec<(&str, CloseOwed)> =
        abandoned.closed.iter().map(|act| (act.leg.as_str(), act.owed)).collect();
    assert_eq!(acts, [("A", CloseOwed::Bye), ("B", CloseOwed::Answer)], "{:#?}", abandoned.closed);
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");
    assert_eq!(scene.b2bua.cdr_records().len(), 1, "the call was billed");
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// Re-state a SEND as a rejection, in memory: the final it answers with, and no
/// body — a reject carries no answer to negotiate.
fn reject_at(case: &mut Case, step: &str, status: u16) {
    let held = step_of(case, step);
    held.msg.status = Some(status);
    held.msg.reason = None;
    held.msg.body = None;
}

/// Re-state the status an `expect` gates on, in memory: the status that then
/// arrives is a datagram the gate refuses.
fn expect_status(case: &mut Case, step: &str, status: u16) {
    let held = step_of(case, step);
    held.msg.status = Some(status);
    held.msg.reason = None;
}
