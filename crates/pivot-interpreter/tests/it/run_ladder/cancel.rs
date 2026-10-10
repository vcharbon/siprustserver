//! CANCEL: the unscripted CANCEL's obligations, the reject that answers it, the ACK to a reject,
//! and a CANCEL that crosses its INVITE's final.

use super::*;

#[tokio::test(start_paused = true)]
async fn rung_two_a_cancelled_attempt_tolerates_the_ring_and_takes_the_teardown_pair_in_any_order()
{
    let scene = api_scene("pivot-cancel-race").await;
    let (outcome, dir) = replay(&scene, "cancel-race.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);
    // The order-free group's two members both arrived, whichever way round.
    for step in ["s11", "s12"] {
        assert!(
            outcome.verdict.completed_steps.contains(&step.to_string()),
            "{step} missing from {:?}",
            outcome.verdict.completed_steps
        );
    }
    scene.finish().await;
}

/// The transaction obligation a datagram the flow scripts NO step for leaves on
/// its leg (RFC 3261 §9.2). Leg B's document writes no step for the CANCEL the
/// system relays, nor for the pair it draws: the endpoint answers `200` to the
/// CANCEL and `487` to the INVITE it names while the arrival stays the refusal
/// it is, and the flow walks on to the ACK those answers draw.
#[tokio::test(start_paused = true)]
async fn an_unscripted_cancel_is_refused_and_answered_200_then_487() {
    let scene = api_scene("pivot-unscripted-cancel").await;
    let (outcome, _dir) = replay(&scene, "unscripted-cancel.v3.json").await;

    // The refusal STANDS: the CANCEL is a divergence the verdict names.
    let refused = outcome
        .verdict
        .failures
        .iter()
        .find(|f| matches!(f, Failure::UnmatchedDatagram { leg, .. } if leg == "B"))
        .unwrap_or_else(|| panic!("the CANCEL was not refused: {:#?}", outcome.verdict.failures));
    let Failure::UnmatchedDatagram { arrived, .. } = refused else { unreachable!("matched above") };
    assert!(matches!(arrived, Arrived::Request { method, .. } if method == "CANCEL"), "{arrived}");

    // And the endpoint answered it all the same, out of leg B's own stack.
    let legs = outcome.recording.legs();
    let answers: Vec<_> = legs["B"]
        .iter()
        .filter(|m| m.dir == Dir::Out && m.note.as_deref().is_some_and(is_unscripted_answer))
        .collect();
    assert_eq!(answers.len(), 2, "the §9.2 pair: {:#?}", legs["B"]);
    assert!(
        text(answers[0]).starts_with("SIP/2.0 200") && text(answers[0]).contains("CSeq: 1 CANCEL"),
        "{}",
        text(answers[0])
    );
    assert!(
        text(answers[1]).starts_with("SIP/2.0 487") && text(answers[1]).contains("CSeq: 1 INVITE"),
        "{}",
        text(answers[1])
    );

    // No cursor moved and no expect was satisfied: neither answer belongs to a
    // step, and the CANCEL that drew them belongs to none either.
    assert!(answers.iter().all(|m| m.step.is_none()), "an answer owns no step: {answers:#?}");
    assert!(
        legs["B"]
            .iter()
            .all(|m| !(m.dir == Dir::In && text(m).starts_with("CANCEL ") && m.step.is_some())),
        "the refused CANCEL satisfied an expect: {:#?}",
        legs["B"]
    );
    // The script was never abandoned: the flow took the ACK the 487 drew and ran
    // to its end.
    assert!(outcome.verdict.abandoned.is_none(), "{:#?}", outcome.verdict.abandoned);
    for step in ["s10", "s11", "s12", "s13"] {
        assert!(
            outcome.verdict.completed_steps.contains(&step.to_string()),
            "{step} missing from {:?}",
            outcome.verdict.completed_steps
        );
    }
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The unscripted-cancel case with leg B's own final to the INVITE scripted
/// behind the CANCEL the system relays: `status` 500 ms after its ringing, once
/// the relayed CANCEL is in.
fn unscripted_cancel_then_final(status: u16, reason: &str) -> Case {
    unscripted_cancel_then_final_after(status, reason, 500)
}

/// [`unscripted_cancel_then_final`] with the scripted final `dwell_ms` behind
/// the 180.
fn unscripted_cancel_then_final_after(status: u16, reason: &str, dwell_ms: u64) -> Case {
    let base_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let text = std::fs::read_to_string(base_dir.join("unscripted-cancel.v3.json")).unwrap();
    let mut doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    doc["case"]["id"] = format!("unscripted-cancel-then-{status}").into();
    let flow = doc["flow"].as_array_mut().unwrap();
    let at = flow.iter().position(|s| s["id"] == "s4").unwrap() + 1;
    flow.insert(
        at,
        serde_json::json!({
            "id": "s4b", "leg": "B", "op": "send",
            "msg": { "cseq-method": "INVITE", "reason": reason, "status": status },
            "delay": { "compressible": true, "from": "step:s4", "ms": dwell_ms, "timer_linked": false }
        }),
    );
    let document = PivotV3::from_json(&doc.to_string()).expect("the case parses");
    Case { document, base_dir }
}

/// The finals leg B sent to its INVITE, first emissions only.
fn invite_finals(legs: &BTreeMap<String, Vec<RecordedMessage>>) -> Vec<String> {
    legs["B"]
        .iter()
        .filter(|m| m.dir == Dir::Out && m.repeat_of.is_none())
        .map(|m| text(m).to_string())
        .filter(|t| t.starts_with("SIP/2.0 ") && !t.starts_with("SIP/2.0 1"))
        .filter(|t| t.lines().any(|l| l.starts_with("CSeq:") && l.ends_with(" INVITE")))
        .map(|t| t.lines().next().unwrap_or_default().to_string())
        .collect()
}

/// The SUT's INVITE to a callee that never rings retransmits on Timer A until
/// a response (RFC 3261 §17.1.1.2). Here the only response is the `487` leg B
/// composes for the CANCEL the flow never scripted: an answer the run authored
/// ends that ladder exactly as a scripted one would, so the rungs it owes are
/// the ones before it: its dwell runs to the 487.
#[tokio::test(start_paused = true)]
async fn an_unscripted_answer_closes_the_ladder_of_the_request_it_ends() {
    let base_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let text_doc = std::fs::read_to_string(base_dir.join("unscripted-cancel.v3.json")).unwrap();
    let mut doc: serde_json::Value = serde_json::from_str(&text_doc).unwrap();
    doc["case"]["id"] = "unscripted-cancel-unrung".into();
    doc["flow"].as_array_mut().unwrap().retain(|s| s["id"] != "s4" && s["id"] != "s5");
    let document = PivotV3::from_json(&doc.to_string()).expect("the case parses");
    let scene = api_scene("pivot-unscripted-cancel-unrung").await;
    let (outcome, _dir) = replay_case(&scene, Case { document, base_dir }, BTreeMap::new()).await;
    let legs = outcome.recording.legs();
    assert_eq!(invite_finals(&legs), ["SIP/2.0 487 Request Terminated"], "{:#?}", legs["B"]);
    let note = outcome.verdict.retransmits.iter().find(|n| n.step == "s3").expect("s3's ladder");
    assert_eq!(note.dwell_us, Some(1_260_000), "the 487 closed the INVITE's ladder: {note:#?}");
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The generic close's act ends a ladder as a scripted send does: leg B never
/// answers the SUT's INVITE before its script is abandoned (it waits on an
/// INFO that never comes), and the final the close composes for that INVITE
/// (`PCAP2TEST_PIVOT_V3.md` §11.2: a request taken and never answered) is what
/// closes the INVITE's Timer A ladder.
#[tokio::test(start_paused = true)]
async fn the_generic_close_ends_the_ladder_of_the_request_it_answers() {
    let base_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let text_doc = std::fs::read_to_string(base_dir.join("linear-attempt.v3.json")).unwrap();
    let mut doc: serde_json::Value = serde_json::from_str(&text_doc).unwrap();
    doc["case"]["id"] = "linear-attempt-abandoned-unanswered".into();
    let flow = doc["flow"].as_array_mut().unwrap();
    flow.truncate(3);
    flow.push(serde_json::json!({
        "id": "s4", "leg": "B", "op": "expect", "check": "record",
        "msg": { "method": "INFO" },
        "delay": { "compressible": true, "from": "step:s3", "ms": 0, "timer_linked": false }
    }));
    let document = PivotV3::from_json(&doc.to_string()).expect("the case parses");
    let scene = api_scene("pivot-abandoned-unanswered").await;
    let (outcome, _dir) = replay_case(&scene, Case { document, base_dir }, BTreeMap::new()).await;
    assert!(
        outcome.verdict.abandoned.is_some(),
        "the script was abandoned: {:#?}",
        outcome.verdict
    );
    let note = outcome.verdict.retransmits.iter().find(|n| n.step == "s3").expect("s3's ladder");
    assert!(note.dwell_us.is_some(), "the close's final closed the INVITE's ladder: {note:#?}");
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// RFC 3261 §9.2 leaves the INVITE's final to the UAS — 487 SHOULD, any
/// non-2xx may — and §17.2.1 allows ONE. Where leg B scripts its own non-2xx
/// final behind a CANCEL the flow never scripted, that captured final is THE
/// answer: the CANCEL draws its `200` and no `487` of the stack's own, so the
/// INVITE is answered once, faithfully.
#[tokio::test(start_paused = true)]
async fn a_scripted_reject_answers_the_invite_an_unscripted_cancel_named() {
    let scene = api_scene("pivot-unscripted-cancel-reject").await;
    let (outcome, _dir) =
        replay_case(&scene, unscripted_cancel_then_final(486, "Busy Here"), BTreeMap::new()).await;
    let legs = outcome.recording.legs();
    assert_eq!(
        invite_finals(&legs),
        ["SIP/2.0 486 Busy Here"],
        "one final, the scripted one: {:#?}",
        legs["B"]
    );
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The CANCEL is answered when it arrives, whatever the script's dwell before
/// its own reject: leg B's 486 is scheduled 40 s behind its 180, yet the
/// CANCELled INVITE draws it at once — a UAS answers a CANCELled INVITE
/// promptly (RFC 3261 §9.2), and holding it would outlast the canceller's own
/// wait for that final (§17.1.1.2, 64*T1).
#[tokio::test(start_paused = true)]
async fn a_scripted_reject_behind_a_long_dwell_answers_the_cancel_at_once() {
    let scene = api_scene("pivot-unscripted-cancel-reject-dwell").await;
    let case = unscripted_cancel_then_final_after(486, "Busy Here", 40_000);
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;
    let legs = outcome.recording.legs();
    assert_eq!(invite_finals(&legs), ["SIP/2.0 486 Busy Here"], "{:#?}", legs["B"]);
    let at = |pred: &dyn Fn(&str) -> bool| {
        legs["B"].iter().find(|m| pred(&text(m))).map(|m| m.at_us).expect("on leg B")
    };
    let cancel = at(&|t| t.starts_with("CANCEL "));
    let reject = at(&|t| t.starts_with("SIP/2.0 486"));
    assert!(
        reject - cancel < 100_000,
        "the reject answers the CANCEL at once: {cancel} → {reject}"
    );
    let sent = legs["B"].iter().find(|m| text(m).starts_with("SIP/2.0 486")).expect("the 486");
    assert_eq!(sent.step.as_deref(), Some("s4b"), "it is the scripted step, drawn forward");
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The re-INVITE leg B sends is relayed to leg A, which answers `100` and
/// scripts a `487` 40 s later; leg B CANCELs it, so the SUT CANCELs leg A's
/// copy, which the flow never scripts. The `487` is drawn forward and the SUT
/// ACKs it. `scripts_ack` adds leg A's `expect` of that ACK.
fn cancelled_reinvite(scripts_ack: bool) -> Case {
    let base_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let text = std::fs::read_to_string(base_dir.join("reinvite-captured-cseq.v3.json")).unwrap();
    let mut doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    doc["case"]["id"] = format!("cancelled-reinvite-ack-{scripts_ack}").into();
    let d = |from: &str, ms: u64| serde_json::json!({ "compressible": true, "from": format!("step:{from}"), "ms": ms, "timer_linked": false });
    let flow = doc["flow"].as_array_mut().unwrap();
    flow.truncate(12);
    flow.push(serde_json::json!({ "id": "s12a", "leg": "A", "op": "send", "auto": true, "in_dialog": true,
        "msg": { "cseq-method": "INVITE", "reason": "Trying", "status": 100 }, "delay": d("s12", 0) }));
    flow.push(serde_json::json!({ "id": "s13", "leg": "B", "op": "send", "in_dialog": true,
        "msg": { "method": "CANCEL" }, "delay": d("s12", 30) }));
    flow.push(serde_json::json!({ "id": "s14", "leg": "A", "op": "send", "in_dialog": true,
        "msg": { "cseq-method": "INVITE", "reason": "Request Terminated", "status": 487 },
        "delay": d("s12a", 40_000) }));
    if scripts_ack {
        flow.push(serde_json::json!({ "id": "s14a", "leg": "A", "op": "expect", "auto": true,
            "check": "record", "in_dialog": true, "msg": { "method": "ACK" }, "delay": d("s14", 0) }));
    }
    flow.push(serde_json::json!({ "id": "u1", "op": "unordered", "steps": [
        { "id": "s15", "leg": "B", "op": "expect", "check": "record", "in_dialog": true,
          "msg": { "cseq-method": "CANCEL", "reason": "OK", "status": 200 }, "delay": d("s13", 0) },
        { "id": "s16", "leg": "B", "op": "expect", "check": "record", "in_dialog": true,
          "msg": { "cseq-method": "INVITE", "reason": "Request Terminated", "status": 487 },
          "delay": d("s13", 0) } ] }));
    flow.push(serde_json::json!({ "id": "s16a", "leg": "B", "op": "send", "auto": true,
        "in_dialog": true, "msg": { "method": "ACK" }, "delay": d("s16", 0) }));
    flow.push(serde_json::json!({ "id": "s17", "leg": "A", "op": "send", "in_dialog": true,
        "msg": { "method": "BYE" }, "delay": d("s16a", 50) }));
    flow.push(serde_json::json!({ "id": "s18", "leg": "B", "op": "expect", "check": "record",
        "in_dialog": true, "msg": { "method": "BYE" }, "delay": d("s17", 0) }));
    flow.push(serde_json::json!({ "id": "s19", "leg": "B", "op": "send", "in_dialog": true,
        "msg": { "cseq-method": "BYE", "reason": "OK", "status": 200 }, "delay": d("s18", 0) }));
    flow.push(serde_json::json!({ "id": "s20", "leg": "A", "op": "expect", "check": "record",
        "in_dialog": true, "msg": { "cseq-method": "BYE", "reason": "OK", "status": 200 },
        "delay": d("s19", 0) }));
    let document = PivotV3::from_json(&doc.to_string()).expect("the case parses");
    Case { document, base_dir }
}

/// The one failure a CANCEL no step scripts leaves: its own refusal (§6.7e).
fn assert_only_the_cancel_refused(failures: &[Failure], ladder: &[RecordedMessage]) {
    let refused = |f: &Failure| matches!(f, Failure::UnexpectedDatagram { arrived, .. } if format!("{arrived:?}").contains("CANCEL"));
    assert!(
        failures.len() == 1 && refused(&failures[0]),
        "only the CANCEL is refused: {failures:#?}\n{ladder:#?}"
    );
}

/// RFC 3261 §17.2.1: the ACK to a non-2xx final leg A sent is hop-by-hop and
/// belongs to that INVITE server transaction. No step scripts it, so the
/// transaction layer consumes it: no unexpected datagram, and the run is clean.
#[tokio::test(start_paused = true)]
async fn the_ack_to_a_reject_no_step_scripts_is_the_transactions_own() {
    let scene = api_scene("pivot-cancelled-reinvite-ack-unscripted").await;
    let (outcome, _dir) = replay_case(&scene, cancelled_reinvite(false), BTreeMap::new()).await;
    let legs = outcome.recording.legs();
    assert_only_the_cancel_refused(&outcome.verdict.failures, &legs["A"]);
    let ack = legs["A"]
        .iter()
        .find(|m| m.dir == Dir::In && text(m).starts_with("ACK "))
        .expect("the SUT ACKed the 487");
    assert_eq!(ack.step, None, "no step claims it");
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The same ACK, scripted: the document's `expect` takes it as today.
#[tokio::test(start_paused = true)]
async fn the_ack_to_a_reject_a_step_scripts_is_that_steps() {
    let scene = api_scene("pivot-cancelled-reinvite-ack-scripted").await;
    let (outcome, _dir) = replay_case(&scene, cancelled_reinvite(true), BTreeMap::new()).await;
    let legs = outcome.recording.legs();
    assert_only_the_cancel_refused(&outcome.verdict.failures, &legs["A"]);
    let ack = legs["A"]
        .iter()
        .find(|m| m.dir == Dir::In && text(m).starts_with("ACK "))
        .expect("the SUT ACKed the 487");
    assert_eq!(ack.step.as_deref(), Some("s14a"), "the scripted expect claims it");
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// A scripted 2xx cannot answer a CANCELled INVITE (§9.2: 487, never 2xx), so
/// the stack's `487` does, and the scripted 2xx — a second final on a
/// transaction already answered (§17.2.1) — is moot: never sent.
#[tokio::test(start_paused = true)]
async fn a_scripted_final_on_an_invite_already_answered_is_not_sent() {
    let scene = api_scene("pivot-unscripted-cancel-2xx").await;
    let (outcome, _dir) =
        replay_case(&scene, unscripted_cancel_then_final(200, "OK"), BTreeMap::new()).await;
    let legs = outcome.recording.legs();
    assert_eq!(
        invite_finals(&legs),
        ["SIP/2.0 487 Request Terminated"],
        "one final, the stack's 487: {:#?}",
        legs["B"]
    );
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// A CANCEL sent after its INVITE's non-2xx final reached the caller draws 200
/// from a UAS whose INVITE server transaction still lingers in Completed
/// (RFC 3261 §17.2.1, §9.2) and 481 from one that disposed of it. The document names the
/// 481; this system answers 200. The step takes it, the recording line says
/// why, and the call runs to its ACK with nothing retired and nothing
/// abandoned.
#[tokio::test(start_paused = true)]
async fn a_cancel_sent_after_its_invite_s_final_takes_200_or_481() {
    let scene = api_scene("pivot-late-cancel-final").await;
    let (outcome, dir) = replay(&scene, "late-cancel-final.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);
    let verdict = &outcome.verdict;
    for step in (1..=9).map(|n| format!("s{n}")) {
        assert!(
            verdict.completed_steps.contains(&step),
            "{step} never completed: {:#?}\nfailures: {:#?}",
            verdict.completed_steps,
            verdict.failures
        );
    }
    assert!(verdict.retired.is_empty(), "nothing retired: {:?}", verdict.retired);
    assert!(verdict.abandoned.is_none(), "nothing abandoned: {:#?}", verdict.abandoned);
    let a_leg = &outcome.recording.legs()["A"];
    let answer = a_leg
        .iter()
        .find(|m| {
            m.dir == Dir::In
                && text(m).lines().any(|l| l.starts_with("CSeq:") && l.ends_with(" CANCEL"))
        })
        .unwrap_or_else(|| panic!("the CANCEL was answered: {a_leg:#?}"));
    assert!(text(answer).starts_with("SIP/2.0 200"), "this system answers 200: {}", text(answer));
    assert_eq!(answer.step.as_deref(), Some("s8"), "the step naming 481 took the 200");
    assert_eq!(
        answer.note.as_deref(),
        Some(
            "tolerated: a final to a CANCEL sent after the INVITE's non-2xx final arrived on \
             this leg draws 200 while the server transaction lives and 481 once it is gone \
             (RFC 3261 §9.2, §17.2.1); 200 arrived where the step names 481"
        ),
        "the recording says why"
    );
    let acks: Vec<_> =
        a_leg.iter().filter(|m| m.dir == Dir::Out && text(m).starts_with("ACK ")).collect();
    assert_eq!(acks.len(), 1, "one ACK on A: {a_leg:#?}");
    assert_eq!(acks[0].step.as_deref(), Some("s9"), "the scripted ACK, not the close's");
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}
