//! What a datagram is matched or charged to: findings that do not abort, and answers on two
//! transactions.

use super::*;

/// **A finding is not an abort** (§9.1, §11.2): a gating inline check that does
/// not hold fails the VERDICT and the flow keeps walking — the matched message
/// is still one the next step composes over, so the run ends by its own
/// scripted teardown and the close has nothing to do.
#[tokio::test(start_paused = true)]
async fn a_failed_inline_check_fails_the_verdict_and_the_flow_walks_to_its_own_teardown() {
    let scene = api_scene("pivot-check-continues").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "check-continues".into();
    step_of(&mut case, "s7").checks.push(pivot_schema::check::Check {
        field: "header(X-Never-Present)".into(),
        op: pivot_schema::check::CheckOp::Exists,
        value: None,
        class: None,
    });
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert!(
        outcome.verdict.failures.iter().any(|f| matches!(f,
            Failure::CheckFailed { site, field, .. }
                if site == "step \"s7\"" && field == "header(X-Never-Present)")),
        "{:#?}",
        outcome.verdict.failures
    );
    // Every step ran, s7 and the whole teardown behind it included.
    assert_eq!(outcome.verdict.completed_steps.len(), 13, "{:#?}", outcome.verdict.completed_steps);
    assert!(
        outcome.verdict.abandoned.is_none(),
        "nothing was abandoned: {:#?}",
        outcome.verdict.abandoned
    );
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");
    assert_eq!(scene.b2bua.cdr_records().len(), 1, "the call was billed");
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// **A header set that differs names itself**: an asserted `expect` whose
/// datagram is one header short FAILS the run without abandoning the call to
/// say so.
///
/// Gating the whole match on the header set would refuse the 2xx outright: the
/// answer would go unanswered, the system would retransmit it to its ceiling and
/// tear both legs down, and the run would die of abandonment several steps past
/// the cause with nothing naming the header. Taking the datagram is what ANSWERS it,
/// so the dialog walks to its own teardown; the header is named with the value
/// the document froze and the value that arrived; and the verdict fails on that
/// finding rather than on the ladder behind it.
#[tokio::test(start_paused = true)]
async fn a_header_the_answer_omits_names_itself_and_the_call_reaches_its_teardown() {
    const OMITTED: &str = "Content-Disposition";
    let scene = api_scene("pivot-header-divergent").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "header-divergent".into();
    // The answer this system relays carries no such header, so the asserted
    // expect for it is exactly one header short.
    step_of(&mut case, "s7").msg.headers.push(Header {
        name: OMITTED.into(),
        value: "session;handling=required".into(),
        class: None,
    });
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;

    // The verdict fails, and the ONE failure is the header, both sides stated.
    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert_eq!(outcome.verdict.failures.len(), 1, "{:#?}", outcome.verdict.failures);
    assert!(
        matches!(&outcome.verdict.failures[0],
            Failure::CheckFailed { site, field, op, expected, observed }
                if site == "step \"s7\""
                    && field == "header(Content-Disposition)"
                    && op == "eq"
                    && expected == "session;handling=required"
                    && observed == "absent"),
        "{:#?}",
        outcome.verdict.failures
    );

    // The call went on: the step took the answer, so the ACK behind it and the
    // whole scripted teardown ran, and nothing was abandoned.
    assert_eq!(outcome.verdict.completed_steps.len(), 13, "{:#?}", outcome.verdict.completed_steps);
    assert!(outcome.verdict.abandoned.is_none(), "{:#?}", outcome.verdict.abandoned);
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");
    assert_eq!(scene.b2bua.cdr_records().len(), 1, "the call was billed");
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();

    // The reception is ATTRIBUTED to the step, which is what lets the post-run
    // confrontation pair it with its captured message and raise a per-header
    // probe of its own — the reporting an abandonment would swallow.
    let leg_a = &outcome.recording.legs()["A"];
    let answer = leg_a
        .iter()
        .find(|m| m.step.as_deref() == Some("s7"))
        .unwrap_or_else(|| panic!("s7 took the answer: {leg_a:#?}"));
    assert!(text(answer).starts_with("SIP/2.0 200 "), "{answer:#?}");
    assert!(!text(answer).contains(OMITTED), "the answer really omits it: {answer:#?}");
    // Answered means not retransmitted: no datagram on the leg repeats another.
    assert!(
        leg_a.iter().all(|m| m.repeat_of.is_none()),
        "the answered 2xx drew no ladder: {leg_a:#?}"
    );

    // And the bundle on disk says so, which is where a triage reads it.
    let written: serde_json::Value =
        serde_json::from_reader(std::fs::File::open(dir.join("verdict.json")).unwrap()).unwrap();
    assert_eq!(written["status"], "failed");
    assert_eq!(written["failures"][0]["field"], "header(Content-Disposition)", "{written:#}");
    assert_eq!(written["failures"][0]["observed"], "absent", "{written:#}");
    scene.finish().await;
}

/// Two answers one leg is owed on two transactions carry no order between them
/// (RFC 3261 §17). The caller BYEs the early dialog its reliably-answered
/// INVITE opened (§15.1, §12.1.2), and the system owes it 200 to the BYE and
/// 487 to the INVITE (§15.1.2). The document lists the 487 first — the order
/// one implementation emitted them in — and this system answers the BYE first:
/// the run takes each where it lands, and the flow completes.
#[tokio::test(start_paused = true)]
async fn two_answers_on_two_transactions_are_taken_in_either_order() {
    let scene = api_scene("pivot-early-bye-answer-order").await;
    let (outcome, dir) = replay(&scene, "early-bye-answer-order.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);
    for step in (1..=17).map(|n| format!("s{n}")) {
        assert!(
            outcome.verdict.completed_steps.contains(&step),
            "{step} never completed: {:#?}\nfailures: {:#?}",
            outcome.verdict.completed_steps,
            outcome.verdict.failures
        );
    }
    // The rung proves the inversion only while the wire shows it: on leg A the
    // BYE's 200 landed BEFORE the INVITE's 487 the document lists first.
    let a_leg = &outcome.recording.legs()["A"];
    let position = |start: &str, method: &str| {
        a_leg
            .iter()
            .position(|m| {
                let raw = text(m);
                raw.starts_with(start)
                    && raw.lines().any(|l| l.starts_with("CSeq:") && l.ends_with(method))
            })
            .unwrap_or_else(|| panic!("{start} to {method} was recorded: {a_leg:#?}"))
    };
    let bye_ok = position("SIP/2.0 200", " BYE");
    let terminated = position("SIP/2.0 487", " INVITE");
    assert!(
        bye_ok < terminated,
        "the system answered the BYE first: 200 at {bye_ok}, 487 at {terminated}"
    );
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// A response no armed expect matched is charged to the expect of ITS
/// transaction (RFC 3261 §17.1.3: CSeq method and number), whatever else is
/// armed on the leg; a final so charged retires that expect, and the flow runs
/// on. The callee rejects the INVITE (600) while the caller's PRACK is
/// unanswered; the system answers the PRACK 200 itself, where the document
/// names 481. One failure, on the PRACK step; the scripted ACK to the 600 goes
/// out; nothing is abandoned. The order the 600 and the 200 leave in is the
/// system's own, and the rung holds under either.
#[tokio::test(start_paused = true)]
async fn a_final_on_one_transaction_is_charged_to_that_transaction_s_expect_alone() {
    let scene = api_scene("pivot-answer-on-the-other-transaction").await;
    let (outcome, _dir) = replay(&scene, "answer-on-the-other-transaction.v3.json").await;
    assert_substituted_finals_run_on(&outcome, &[("s12", 481, "PRACK", 200)]);
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// Two finals on two transactions, both substituted: the callee's 600 where the
/// document names 486 to the INVITE, and the system's 200 where it names 481
/// to the PRACK. Each is charged to the expect of the transaction it rides,
/// never to the other's; both are retired; the ACK to the 600 is the scripted
/// one (a final of the same class leaves the dialog the tail was scripted for
/// where it was) and the flow completes.
#[tokio::test(start_paused = true)]
async fn two_substituted_finals_on_two_transactions_are_charged_one_to_each_expect() {
    let scene = api_scene("pivot-answers-on-two-transactions-substituted").await;
    let (outcome, _dir) = replay(&scene, "answers-on-two-transactions-substituted.v3.json").await;
    assert_substituted_finals_run_on(
        &outcome,
        &[("s11", 486, "INVITE", 600), ("s12", 481, "PRACK", 200)],
    );
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The shape the two rungs above share: every listed `(step, named, method,
/// arrived)` is exactly one `UnmatchedDatagram` on that step and no other
/// failure exists; the listed steps are the retired ones; every other step
/// completed; leg A's ACK went out under its scripted step, not the generic
/// close's; the 600 was never repeated (the ACK landed inside T1); the run
/// settled.
fn assert_substituted_finals_run_on(outcome: &Outcome, substituted: &[(&str, u16, &str, u16)]) {
    let verdict = &outcome.verdict;
    assert!(verdict.abandoned.is_none(), "nothing abandoned: {:#?}", verdict.abandoned);
    assert_eq!(verdict.failures.len(), substituted.len(), "failures: {:#?}", verdict.failures);
    for (step_id, named, method, arrived_status) in substituted {
        let charged = verdict
            .failures
            .iter()
            .find(|f| matches!(f, Failure::UnmatchedDatagram { step, .. } if step == step_id))
            .unwrap_or_else(|| panic!("{step_id} is charged: {:#?}", verdict.failures));
        let Failure::UnmatchedDatagram { gated_on, arrived, .. } = charged else { unreachable!() };
        assert!(
            matches!(gated_on, GatedOn::Response { status, cseq_method: Some(m) }
                if status == named && m == method),
            "{step_id} gated on {gated_on}"
        );
        assert!(
            matches!(arrived, Arrived::Response { status, cseq_method, .. }
                if status == arrived_status && cseq_method == method),
            "{step_id} took {arrived}"
        );
    }
    let mut retired = verdict.retired.clone();
    retired.sort();
    let mut expected: Vec<String> = substituted.iter().map(|(s, ..)| s.to_string()).collect();
    expected.sort();
    assert_eq!(retired, expected, "the charged steps are the retired ones");
    for step in (1..=13).map(|n| format!("s{n}")) {
        if expected.contains(&step) {
            continue;
        }
        assert!(
            verdict.completed_steps.contains(&step),
            "{step} never completed: {:#?}\nfailures: {:#?}",
            verdict.completed_steps,
            verdict.failures
        );
    }
    let a_leg = &outcome.recording.legs()["A"];
    let ladder: Vec<String> = a_leg
        .iter()
        .map(|m| {
            format!(
                "{:?} {} step={:?} repeat_of={:?} note={:?}",
                m.dir,
                text(m).lines().next().unwrap_or_default(),
                m.step,
                m.repeat_of,
                m.note
            )
        })
        .collect();
    let acks: Vec<_> =
        a_leg.iter().filter(|m| m.dir == Dir::Out && text(m).starts_with("ACK ")).collect();
    assert_eq!(acks.len(), 1, "one ACK on A: {ladder:#?}");
    assert_eq!(acks[0].step.as_deref(), Some("s13"), "the scripted ACK, not the close's");
    assert!(
        !a_leg.iter().any(|m| m.dir == Dir::In
            && text(m).starts_with("SIP/2.0 600")
            && m.repeat_of.is_some()),
        "the 600 was never repeated: {ladder:#?}"
    );
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");
}
