use super::*;
use pivot_schema::bundle::Dir;
use pivot_schema::violation::RfcRule;

/// The callee's 200, as leg B put it on the wire.
const ANSWER: &str = "SIP/2.0 200 OK\r\n\
        Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-b\r\n\
        From: <sip:alice@example.test>;tag=a1\r\n\
        To: <sip:bob@example.test>;tag=b1\r\n\
        Call-ID: call-b\r\n\
        CSeq: 1 INVITE\r\n\
        Content-Length: 0\r\n\r\n";

/// The platform's own ACK to it — the datagram no step of the document holds.
const ACK: &str = "ACK sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
        Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-c\r\n\
        From: <sip:alice@example.test>;tag=a1\r\n\
        To: <sip:bob@example.test>;tag=b1\r\n\
        Call-ID: call-b\r\n\
        CSeq: 1 ACK\r\n\
        Content-Length: 0\r\n\r\n";

/// The callee's reliable 183, as leg B put it on the wire.
const PROGRESS: &str = "SIP/2.0 183 Session Progress\r\n\
        Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-b\r\n\
        From: <sip:alice@example.test>;tag=a1\r\n\
        To: <sip:bob@example.test>;tag=b1\r\n\
        Call-ID: call-b\r\n\
        CSeq: 1 INVITE\r\n\
        Require: 100rel\r\n\
        RSeq: 7\r\n\
        Content-Length: 0\r\n\r\n";

/// The platform's own PRACK to it — the datagram no step of the document holds.
const PRACK: &str = "PRACK sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
        Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-d\r\n\
        From: <sip:alice@example.test>;tag=a1\r\n\
        To: <sip:bob@example.test>;tag=b1\r\n\
        Call-ID: call-b\r\n\
        CSeq: 2 PRACK\r\n\
        RAck: 7 1 INVITE\r\n\
        Content-Length: 0\r\n\r\n";

/// A document whose leg-B step `s2` SENDS the 200, declaring `block` at the
/// top level where the canonical key order puts `must_fail`.
fn plan(block: &str) -> Plan {
    let text = format!(
        r#"{{
              "pivot_version": 3,
              "case": {{ "id": "negative", "title": "t", "family": "transparent",
                        "variant": "repro", "origin": "authored",
                        "lanes": {{ "upstream-fake": "ok" }} }},
              "identities": [ {{ "name": "called-0-0", "kind": "site", "forms": ["e164"] }} ],
              "calls": [ {{ "id": "c1", "caller_leg": "A", "attempts": [
                 {{ "branch": 0, "position": 0, "leg": "B",
                    "callee": {{ "identity": "called-0-0" }} }} ] }} ],
              "endpoints": [ {{ "id": "ep0", "observed": "127.0.0.1:5060",
                               "side": "peer", "binding": "dedicated" }} ],
              "actors": [ {{ "id": "uac1", "kind": "uac", "endpoint": "ep0" }},
                          {{ "id": "uas1", "kind": "uas", "endpoint": "ep0" }} ],
              "legs": [ {{ "id": "A", "actor": "uac1", "dir": "out" }},
                        {{ "id": "B", "actor": "uas1", "dir": "in" }} ],
              "flow": [
                {{ "id": "s1", "leg": "A", "op": "send", "msg": {{ "method": "INVITE" }},
                   "delay": {{ "ms": 0, "from": "trigger", "compressible": true,
                              "timer_linked": false }} }},
                {{ "id": "s2", "leg": "B", "op": "send",
                   "msg": {{ "status": 200, "cseq-method": "INVITE" }},
                   "delay": {{ "ms": 0, "from": "trigger", "compressible": true,
                              "timer_linked": false }} }} ],
              {block}
              "postconditions": {{ "cdr": {{ "absent": "unit test" }} }},
              "timing": {{ "expect_budget_ms": 1000, "settle_budget_ms": 1000 }}
            }}"#
    );
    Plan::compile(pivot_schema::PivotV3::from_json(&text).expect("the fixture parses"))
        .expect("the fixture compiles")
}

const DECLARED: &str = r#""must_fail": [ { "failure": "unexpected-ack", "step": "s2",
                              "derived_from": "no-ack-to-dialog-creating-2xx" } ],"#;

const DECLARED_PRACK: &str = r#""must_fail": [ { "failure": "unexpected-prack",
                              "step": "s2",
                              "derived_from": "unacked-reliable-provisional" } ],"#;

const DECLARED_CANCEL: &str = r#""must_fail": [ { "failure": "unexpected-cancel",
                              "step": "s2",
                              "derived_from": "no-cancel-after-final" } ],"#;

/// The platform's own CANCEL for the transaction the anchor's final ended —
/// the datagram no step of the document holds. §9.1 names the INVITE by
/// Call-ID and CSeq number, and its To carries no tag.
const CANCEL: &str = "CANCEL sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
        Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-b\r\n\
        From: <sip:alice@example.test>;tag=a1\r\n\
        To: <sip:bob@example.test>\r\n\
        Call-ID: call-b\r\n\
        CSeq: 1 CANCEL\r\n\
        Content-Length: 0\r\n\r\n";

/// A recording of the anchor's reliable 183 out, and whatever `arrival`
/// states in.
fn recording_prack(arrival: Option<(&str, Option<&str>)>) -> Recording {
    let rec = Recording::new();
    rec.push("B", Dir::Out, 0, PROGRESS, Some("s2"), None);
    if let Some((raw, step)) = arrival {
        rec.push("B", Dir::In, 10, raw, step, None);
    }
    rec
}

/// A recording of the anchor's 200 out, and whatever `arrival` states in.
fn recording(arrival: Option<(&str, Option<&str>)>) -> Recording {
    let rec = Recording::new();
    rec.push("B", Dir::Out, 0, ANSWER, Some("s2"), None);
    if let Some((raw, step)) = arrival {
        rec.push("B", Dir::In, 10, raw, step, None);
    }
    rec
}

fn refused(leg: &str, raw: &str) -> Failure {
    let message = parse(raw.as_bytes()).expect("the fixture parses");
    Failure::UnexpectedDatagram {
        leg: leg.to_string(),
        arrived: Inbound::of(&message).arrived(),
        detail: None,
    }
}

#[test]
fn the_declared_failure_leaves_failures_and_the_run_reads_ok_negative() {
    let plan = plan(DECLARED);
    let recording = recording(Some((ACK, None)));
    let mut verdict = RunVerdict::ok("negative", "upstream-demo");
    verdict.fail(refused("B", ACK));

    invert(&mut verdict, &plan, &recording);

    assert_eq!(verdict.status, VerdictStatus::OkNegative);
    assert!(verdict.failures.is_empty(), "{:#?}", verdict.failures);
    assert_eq!(verdict.failed_step, None);
    // Moved, not rewritten: the site and the evidence the gate stated stand.
    let note = &verdict.must_fail[0];
    assert_eq!(note.failure, DeclaredFailure::UnexpectedAck);
    assert_eq!(note.step, "s2");
    assert_eq!(note.derived_from, RfcRule::NoAckToDialogCreating2xx);
    assert_eq!(note.observed, Some(refused("B", ACK)));
    assert!(verdict.passed(), "green-as-negative is a run that passed");
}

/// The other direction, and the reason the format exists: the platform
/// stopped producing the failure, so the negative case is what turns red.
#[test]
fn a_declaration_the_run_never_produced_fails_the_run() {
    let plan = plan(DECLARED);
    let mut verdict = RunVerdict::ok("negative", "upstream-demo");

    invert(&mut verdict, &plan, &recording(None));

    assert_eq!(verdict.status, VerdictStatus::Failed);
    assert!(!verdict.passed());
    assert_eq!(verdict.failed_step.as_deref(), Some("s2"));
    assert!(verdict.must_fail[0].observed.is_none());
    let Some(Failure::DeclaredFailureNotProduced { declared, detail, .. }) =
        verdict.failures.first()
    else {
        panic!("{:#?}", verdict.failures)
    };
    assert_eq!(*declared, DeclaredFailure::UnexpectedAck);
    assert!(detail.contains("no unclaimed ACK"), "{detail}");
}

/// `unexpected-prack`, both directions: the PRACK whose RAck names the
/// anchor's own RSeq and INVITE CSeq satisfies the declaration, and its
/// absence turns the negative case red.
#[test]
fn the_declared_prack_inverts_and_its_absence_fails_the_run() {
    let plan = plan(DECLARED_PRACK);
    let mut verdict = RunVerdict::ok("negative", "upstream-demo");
    verdict.fail(refused("B", PRACK));
    invert(&mut verdict, &plan, &recording_prack(Some((PRACK, None))));
    assert_eq!(verdict.status, VerdictStatus::OkNegative, "{:#?}", verdict.failures);
    let note = &verdict.must_fail[0];
    assert_eq!(note.failure, DeclaredFailure::UnexpectedPrack);
    assert_eq!(note.derived_from, RfcRule::UnackedReliableProvisional);

    let mut verdict = RunVerdict::ok("negative", "upstream-demo");
    invert(&mut verdict, &plan, &recording_prack(None));
    assert_eq!(verdict.status, VerdictStatus::Failed);
    let Some(Failure::DeclaredFailureNotProduced { detail, .. }) = verdict.failures.first() else {
        panic!("{:#?}", verdict.failures)
    };
    assert!(detail.contains("no unclaimed PRACK"), "{detail}");
}

/// `unexpected-cancel`, both directions: the CANCEL naming the anchor's own
/// call and INVITE CSeq satisfies the declaration, and its absence turns the
/// negative case red.
#[test]
fn the_declared_cancel_inverts_and_its_absence_fails_the_run() {
    let plan = plan(DECLARED_CANCEL);
    let mut verdict = RunVerdict::ok("negative", "upstream-demo");
    verdict.fail(refused("B", CANCEL));
    invert(&mut verdict, &plan, &recording(Some((CANCEL, None))));
    assert_eq!(verdict.status, VerdictStatus::OkNegative, "{:#?}", verdict.failures);
    let note = &verdict.must_fail[0];
    assert_eq!(note.failure, DeclaredFailure::UnexpectedCancel);
    assert_eq!(note.derived_from, RfcRule::NoCancelAfterFinal);
    assert_eq!(note.observed, Some(refused("B", CANCEL)));

    let mut verdict = RunVerdict::ok("negative", "upstream-demo");
    invert(&mut verdict, &plan, &recording(None));
    assert_eq!(verdict.status, VerdictStatus::Failed);
    let Some(Failure::DeclaredFailureNotProduced { detail, .. }) = verdict.failures.first() else {
        panic!("{:#?}", verdict.failures)
    };
    assert!(detail.contains("no unclaimed CANCEL"), "{detail}");
}

/// The dialog is the join here too: a CANCEL of another call's transaction
/// satisfies nothing, whatever leg it lands on.
#[test]
fn a_cancel_of_another_transaction_satisfies_no_declaration() {
    let plan = plan(DECLARED_CANCEL);
    let mut verdict = RunVerdict::ok("negative", "upstream-demo");
    let elsewhere = CANCEL.replace("Call-ID: call-b", "Call-ID: call-z");
    verdict.fail(refused("B", &elsewhere));
    invert(&mut verdict, &plan, &recording(Some((&elsewhere, None))));
    assert_eq!(verdict.status, VerdictStatus::Failed, "{:#?}", verdict.failures);
}

/// The RAck is the join: a PRACK acknowledging another provisional is not
/// the one the anchor's RSeq draws, however unexpected it was.
#[test]
fn a_prack_naming_another_provisional_satisfies_nothing() {
    let plan = plan(DECLARED_PRACK);
    let other = PRACK.replace("RAck: 7 1 INVITE", "RAck: 9 1 INVITE");
    let mut verdict = RunVerdict::ok("negative", "upstream-demo");
    verdict.fail(refused("B", &other));
    invert(&mut verdict, &plan, &recording_prack(Some((&other, None))));
    assert_eq!(verdict.status, VerdictStatus::Failed);
    assert!(verdict.must_fail[0].observed.is_none());
}

/// An ACK a flow step CLAIMED is an ACK the document expected. The lint
/// refuses that document; the run refuses to be satisfied by it either.
#[test]
fn an_ack_a_step_matched_does_not_satisfy_a_declaration() {
    let plan = plan(DECLARED);
    let mut verdict = RunVerdict::ok("negative", "upstream-demo");

    invert(&mut verdict, &plan, &recording(Some((ACK, Some("s3")))));

    assert_eq!(verdict.status, VerdictStatus::Failed);
    assert!(verdict.must_fail[0].observed.is_none());
}

/// The precision the recording buys: an unexpected datagram that is not the
/// ACK the anchor's own 2xx draws satisfies nothing, however loudly it failed.
#[test]
fn an_unexpected_datagram_of_another_dialog_or_another_method_satisfies_nothing() {
    let other_dialog = ACK.replace("Call-ID: call-b", "Call-ID: call-z");
    let other_cseq = ACK.replace("CSeq: 1 ACK", "CSeq: 4 ACK");
    let other_tag = ACK.replace("tag=b1", "tag=zz");
    let not_an_ack = ANSWER.to_string();
    for stray in [&other_dialog, &other_cseq, &other_tag, &not_an_ack] {
        let plan = plan(DECLARED);
        let mut verdict = RunVerdict::ok("negative", "upstream-demo");
        verdict.fail(refused("B", stray));
        invert(&mut verdict, &plan, &recording(Some((stray, None))));
        assert_eq!(verdict.status, VerdictStatus::Failed, "{stray}");
        // The stray failure is still in the list: nothing was absorbed.
        assert_eq!(verdict.failures.len(), 2, "{:#?}", verdict.failures);
    }
}

/// The declared ACK on the WRONG leg is a different fact about the run.
#[test]
fn the_declared_ack_counts_only_on_the_anchor_s_own_leg() {
    let plan = plan(DECLARED);
    let recording = Recording::new();
    recording.push("B", Dir::Out, 0, ANSWER, Some("s2"), None);
    recording.push("A", Dir::In, 10, ACK, None, None);
    let mut verdict = RunVerdict::ok("negative", "upstream-demo");
    verdict.fail(refused("A", ACK));

    invert(&mut verdict, &plan, &recording);

    assert_eq!(verdict.status, VerdictStatus::Failed);
    assert_eq!(verdict.failures.len(), 2, "{:#?}", verdict.failures);
}

/// The ruled split, both directions in one place. A WIRE divergence beside
/// the declared one is what a diverging tail produces, so the negative case
/// carries it; a SETTLE failure beside it is the call not ending, which no
/// declaration excuses.
#[test]
fn a_wire_failure_beside_the_declaration_is_carried_and_a_settle_failure_is_not() {
    let ladder = || Failure::RetransmitCountMismatch {
        step: "s2".into(),
        leg: "B".into(),
        declared: 3,
        expected: 3,
        observed: 0,
    };

    let plan = plan(DECLARED);
    let mut carried = RunVerdict::ok("negative", "upstream-demo");
    carried.fail(refused("B", ACK));
    carried.fail(ladder());
    invert(&mut carried, &plan, &recording(Some((ACK, None))));
    assert_eq!(carried.status, VerdictStatus::OkNegative);
    assert!(carried.failures.is_empty(), "{:#?}", carried.failures);
    assert_eq!(carried.failed_step, None);
    assert_eq!(carried.tolerated, vec![ladder()], "moved, and still stated");
    assert!(carried.must_fail[0].observed.is_some());

    let mut settled = RunVerdict::ok("negative", "upstream-demo");
    settled.fail(refused("B", ACK));
    settled.fail(Failure::SettleTimedOut { budget_ms: 1000, open: vec!["B".into()] });
    invert(&mut settled, &plan, &recording(Some((ACK, None))));
    assert_eq!(settled.status, VerdictStatus::Failed);
    assert_eq!(settled.failures.len(), 1);
    assert!(matches!(settled.failures[0], Failure::SettleTimedOut { .. }));
    assert!(settled.tolerated.is_empty());
    assert!(settled.must_fail[0].observed.is_some(), "the declared one still matched");
}

/// A wire failure BEFORE the divergence is nobody's consequence: the replay
/// was still following the capture there, so it is a defect of its own.
#[test]
fn a_wire_failure_before_the_divergence_still_fails_the_run() {
    let plan = plan(DECLARED);
    let mut verdict = RunVerdict::ok("negative", "upstream-demo");
    verdict.fail(refused("B", ACK));
    // `s1` is the caller's INVITE, one step ahead of the `s2` anchor.
    verdict.fail(Failure::RetransmitCountMismatch {
        step: "s1".into(),
        leg: "A".into(),
        declared: 2,
        expected: 2,
        observed: 0,
    });

    invert(&mut verdict, &plan, &recording(Some((ACK, None))));

    assert_eq!(verdict.status, VerdictStatus::Failed);
    assert!(verdict.tolerated.is_empty(), "{:#?}", verdict.tolerated);
    assert_eq!(verdict.failed_step.as_deref(), Some("s1"));
}

/// Every structural failure gates, whatever the document declared. The list
/// is one per family of the classification, and CDR and settle are in it
/// because this project never deactivates their verification.
#[test]
fn a_structural_failure_beside_the_declaration_always_fails_the_run() {
    let structural = [
        Failure::CdrMismatch { expected: "1 CDR".into(), observed: "0".into() },
        Failure::FlowIncomplete { pending: vec!["s2".into()] },
        Failure::CheckFailed {
            site: "postcondition".into(),
            field: "active_calls".into(),
            op: "eq".into(),
            expected: "0".into(),
            observed: "1".into(),
        },
        Failure::SendFailed { step: "s2".into(), leg: "B".into(), detail: "no route".into() },
        Failure::RunUnwound { detail: "the RFC gate refused the trace".into() },
    ];
    for failure in structural {
        let plan = plan(DECLARED);
        let mut verdict = RunVerdict::ok("negative", "upstream-demo");
        verdict.fail(refused("B", ACK));
        verdict.fail(failure.clone());
        invert(&mut verdict, &plan, &recording(Some((ACK, None))));
        assert_eq!(verdict.status, VerdictStatus::Failed, "{failure:#?}");
        assert_eq!(verdict.failures, vec![failure.clone()], "{failure:#?}");
        assert!(verdict.tolerated.is_empty(), "{failure:#?}");
    }
}

/// All or nothing: one structural failure and the wire ones stay where the
/// run wrote them, because a red run's `failures` is the whole account.
#[test]
fn a_structural_failure_keeps_the_wire_ones_in_failures_too() {
    let plan = plan(DECLARED);
    let mut verdict = RunVerdict::ok("negative", "upstream-demo");
    verdict.fail(refused("B", ACK));
    verdict.fail(Failure::RetransmitCountMismatch {
        step: "s2".into(),
        leg: "B".into(),
        declared: 3,
        expected: 3,
        observed: 0,
    });
    verdict.fail(Failure::SettleTimedOut { budget_ms: 1000, open: vec!["B".into()] });

    invert(&mut verdict, &plan, &recording(Some((ACK, None))));

    assert_eq!(verdict.status, VerdictStatus::Failed);
    assert_eq!(verdict.failures.len(), 2, "{:#?}", verdict.failures);
    assert!(verdict.tolerated.is_empty());
}

/// A carried divergence is never invisible: `verdict.json` states it, so a
/// reader of a green negative case sees what else the replay diverged on.
#[test]
fn a_carried_divergence_survives_the_bundle() {
    let plan = plan(DECLARED);
    let mut verdict = RunVerdict::ok("negative", "upstream-demo");
    verdict.fail(refused("B", ACK));
    verdict.fail(Failure::UnexpectedDatagram {
        leg: "B".into(),
        arrived: Arrived::Request { method: "OPTIONS".into(), cseq: 9 },
        detail: None,
    });

    invert(&mut verdict, &plan, &recording(Some((ACK, None))));

    assert_eq!(verdict.status, VerdictStatus::OkNegative);
    let text = serde_json::to_string(&verdict).expect("a verdict serializes");
    assert!(text.contains("\"tolerated\":[{\"failure\":\"unexpected-datagram\""), "{text}");
    assert!(text.contains("\"kind\":\"request\",\"method\":\"OPTIONS\",\"cseq\":9"), "{text}");
    assert_eq!(serde_json::from_str::<RunVerdict>(&text).unwrap(), verdict);
}

/// A run that died BEFORE the anchor ever emitted its 2xx is no negative
/// pass: the declaration was never testable, and the verdict says both
/// facts — the earlier failure, and the declaration nothing produced.
#[test]
fn a_run_that_never_reached_the_anchor_fails_with_both_facts_stated() {
    let plan = plan(DECLARED);
    let empty = Recording::new();
    let mut verdict = RunVerdict::ok("negative", "upstream-demo");
    verdict.fail(Failure::SettleTimedOut { budget_ms: 1000, open: vec!["A".into()] });

    invert(&mut verdict, &plan, &empty);

    assert_eq!(verdict.status, VerdictStatus::Failed);
    assert!(verdict.must_fail[0].observed.is_none());
    assert_eq!(verdict.failures.len(), 2, "{:#?}", verdict.failures);
    assert!(
        verdict.failures.iter().any(|f| matches!(
            f,
            Failure::DeclaredFailureNotProduced { detail, .. } if detail.contains("no datagram")
        )),
        "{:#?}",
        verdict.failures
    );
}

/// Tolerance runs only where EVERY declaration was produced. One absent,
/// and the wire co-failures stay in `failures` beside it: a run that
/// failed for another reason states the whole list, tolerating nothing.
#[test]
fn a_missing_declaration_keeps_every_wire_co_failure_in_failures() {
    let two = r#""must_fail": [
            { "failure": "unexpected-ack", "step": "s2",
              "derived_from": "no-ack-to-dialog-creating-2xx" },
            { "failure": "unexpected-ack", "step": "s1",
              "derived_from": "no-ack-to-dialog-creating-2xx" } ],"#;
    let plan = plan(two);
    let stray = ACK.replace("Call-ID: call-b", "Call-ID: call-z");
    let mut verdict = RunVerdict::ok("negative", "upstream-demo");
    verdict.fail(refused("B", ACK));
    verdict.fail(refused("B", &stray));

    invert(&mut verdict, &plan, &recording(Some((ACK, None))));

    assert_eq!(verdict.status, VerdictStatus::Failed);
    assert!(verdict.tolerated.is_empty(), "{:#?}", verdict.tolerated);
    // The produced declaration still moved to its note; what remains is
    // the stray wire failure and the absent declaration, both stated.
    assert_eq!(verdict.failures.len(), 2, "{:#?}", verdict.failures);
    assert!(verdict.failures.iter().any(|f| matches!(f, Failure::UnexpectedDatagram { .. })));
    assert!(verdict
        .failures
        .iter()
        .any(|f| matches!(f, Failure::DeclaredFailureNotProduced { .. })));
}

/// A document that declares nothing is not touched: no note, no status
/// change, and the ordinary green path is byte-identical to before.
#[test]
fn a_document_declaring_nothing_is_left_exactly_as_the_run_left_it() {
    let plan = plan("");
    let mut green = RunVerdict::ok("plain", "upstream-demo");
    invert(&mut green, &plan, &recording(Some((ACK, None))));
    assert_eq!(green.status, VerdictStatus::Ok);
    assert!(green.must_fail.is_empty());

    let mut red = RunVerdict::ok("plain", "upstream-demo");
    red.fail(refused("B", ACK));
    invert(&mut red, &plan, &recording(Some((ACK, None))));
    assert_eq!(red.status, VerdictStatus::Failed);
    assert_eq!(red.failures.len(), 1, "an undeclared failure stays where it was");
}

/// The declared datagram arriving AFTER the script ended (§11.2): no expect
/// was armed, so no gate refused it and no failure names it. The RECORDING
/// holds it, which is what a declaration is matched against, so it is
/// satisfied — and stated as `recorded` rather than as a failure the run
/// never raised.
#[test]
fn a_declared_arrival_the_ended_script_never_gated_is_satisfied_by_the_recording() {
    let plan = plan(DECLARED);
    let mut verdict = RunVerdict::ok("negative", "upstream-demo");
    // The delta that ended the script: a wire failure at the anchor itself.
    verdict.fail(Failure::UnmatchedDatagram {
        step: "s2".into(),
        leg: "B".into(),
        gated_on: pivot_schema::bundle::GatedOn::Response { status: 200, cseq_method: None },
        reason: "gated on response 200; 180 arrived".into(),
        arrived: Arrived::Response {
            status: 180,
            reason: "Ringing".into(),
            cseq_method: "INVITE".into(),
            cseq: 1,
        },
    });

    invert(&mut verdict, &plan, &recording(Some((ACK, None))));

    assert_eq!(verdict.status, VerdictStatus::OkNegative, "{:#?}", verdict.failures);
    let note = &verdict.must_fail[0];
    assert!(note.observed.is_none(), "no expect was armed to refuse it");
    assert_eq!(note.recorded.as_deref(), Some("ACK (CSeq 1 ACK)"));
    // And the delta that ended the script is carried, not lost.
    assert_eq!(verdict.tolerated.len(), 1, "{:#?}", verdict.tolerated);
}

/// The other half of that rule: a recording with no such datagram satisfies
/// nothing, whether or not the script ended early.
#[test]
fn an_ended_script_does_not_excuse_a_declaration_the_wire_never_carried() {
    let plan = plan(DECLARED);
    let mut verdict = RunVerdict::ok("negative", "upstream-demo");
    verdict.fail(Failure::UnmatchedDatagram {
        step: "s2".into(),
        leg: "B".into(),
        gated_on: pivot_schema::bundle::GatedOn::Response { status: 200, cseq_method: None },
        reason: "gated on response 200; 180 arrived".into(),
        arrived: Arrived::Response {
            status: 180,
            reason: "Ringing".into(),
            cseq_method: "INVITE".into(),
            cseq: 1,
        },
    });

    invert(&mut verdict, &plan, &recording(None));

    assert_eq!(verdict.status, VerdictStatus::Failed);
    assert!(verdict.must_fail[0].recorded.is_none());
    assert!(verdict.tolerated.is_empty(), "a run carrying an absent declaration carries nothing");
}

/// A verdict carrying a declaration survives the bundle: the note is what a
/// reader of `verdict.json` sees instead of an unexplained failure.
#[test]
fn a_negative_verdict_round_trips_through_its_bundle_form() {
    let plan = plan(DECLARED);
    let mut verdict = RunVerdict::ok("negative", "upstream-demo");
    verdict.fail(refused("B", ACK));
    invert(&mut verdict, &plan, &recording(Some((ACK, None))));

    let text = serde_json::to_string(&verdict).expect("a verdict serializes");
    assert!(text.contains("\"status\":\"ok-negative\""), "{text}");
    assert!(text.contains("\"unexpected-ack\""), "{text}");
    assert_eq!(serde_json::from_str::<RunVerdict>(&text).unwrap(), verdict);
}
