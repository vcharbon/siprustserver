//! Acts the RFC makes the endpoint's own whatever the document scripts: PRACK, the ACK to a non-2xx
//! final, and the answer to a BYE.

use super::*;

/// The act RFC 3262 §3 makes the endpoint's own: a PRACK no step scripts is
/// answered `200` when its RAck names the reliable provisional this leg sent,
/// while the arrival stays the refusal it is. The document declares it — the
/// source never PRACKed its callee's reliable 180, and this platform relays the
/// caller's PRACK end to end — so the callee's `200` is what lets the relayed
/// `200 PRACK` reach the caller and the flow walk on to the 2xx, the ACK the
/// document also declares, and the scripted teardown.
#[tokio::test(start_paused = true)]
async fn an_unscripted_prack_is_refused_and_answered_200_so_the_relay_walks_on() {
    let scene = api_scene("pivot-unscripted-prack").await;
    let (outcome, _dir) = replay(&scene, "unscripted-prack-negative.v3.json").await;

    assert_eq!(
        outcome.verdict.status,
        VerdictStatus::OkNegative,
        "failures: {:#?}\nrecording: {:#?}",
        outcome.verdict.failures,
        outcome.recording.legs()
    );
    assert!(outcome.verdict.failures.is_empty(), "{:#?}", outcome.verdict.failures);

    // Both declarations were produced, the PRACK first: the refusal STANDS,
    // raised with its site, and the verdict inverts on it.
    let declared: Vec<DeclaredFailure> =
        outcome.verdict.must_fail.iter().map(|note| note.failure).collect();
    assert_eq!(declared, [DeclaredFailure::UnexpectedPrack, DeclaredFailure::UnexpectedAck]);
    for note in &outcome.verdict.must_fail {
        let observed = note.observed.as_ref().unwrap_or_else(|| panic!("{note:#?}"));
        assert_eq!(arrival_of(observed).0, "B", "the anchor's own leg: {note:#?}");
    }

    // And the endpoint answered the PRACK all the same, out of leg B's own
    // stack, right behind the arrival it refused.
    let legs = outcome.recording.legs();
    let prack = legs["B"]
        .iter()
        .position(|m| m.dir == Dir::In && text(m).starts_with("PRACK "))
        .unwrap_or_else(|| panic!("no PRACK on leg B: {:#?}", legs["B"]));
    assert_eq!(legs["B"][prack].step, None, "no step claimed it — that is the failure");
    let answer = &legs["B"][prack + 1];
    assert!(
        answer.dir == Dir::Out
            && text(answer).starts_with("SIP/2.0 200")
            && text(answer).contains("CSeq: 2 PRACK")
            && answer.note.as_deref().is_some_and(is_unscripted_answer),
        "{answer:#?}"
    );
    assert_eq!(answer.step, None, "an answer owns no step");

    // The caller's own `200 PRACK` expect was satisfied by the relay, so the
    // script ran to its end and nothing was abandoned.
    assert!(outcome.verdict.abandoned.is_none(), "{:#?}", outcome.verdict.abandoned);
    assert_eq!(outcome.verdict.completed_steps.len(), 14, "{:?}", outcome.verdict.completed_steps);

    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The other act the RFC makes the endpoint's own (§17.1.1.3): a non-2xx final
/// no step scripts is ACKed on the INVITE's own branch, where the arrival is
/// refused — not left for the generic close to discover.
#[tokio::test(start_paused = true)]
async fn an_unscripted_non_2xx_final_is_refused_and_acked_on_the_invite_s_branch() {
    let scene = api_scene("pivot-unscripted-final").await;
    let (outcome, _dir) = replay(&scene, "unscripted-final.v3.json").await;

    // The refusal STANDS: the document waited for a ring and a final arrived.
    assert_eq!(outcome.verdict.failed_step.as_deref(), Some("s5"));
    let refused = outcome
        .verdict
        .failures
        .iter()
        .find(|f| matches!(f, Failure::UnmatchedDatagram { leg, .. } if leg == "A"))
        .unwrap_or_else(|| panic!("the final was not refused: {:#?}", outcome.verdict.failures));
    let Failure::UnmatchedDatagram { arrived, .. } = refused else { unreachable!("matched above") };
    let Arrived::Response { status, .. } = arrived else { panic!("{arrived}") };
    assert!((300..700).contains(status), "a non-2xx final: {arrived}");

    // The ACK it owes went out where the refusal happened, on the transaction's
    // own branch, and it belongs to no step.
    let legs = outcome.recording.legs();
    let out = |prefix: &str| {
        legs["A"]
            .iter()
            .find(|m| m.dir == Dir::Out && text(m).starts_with(prefix))
            .unwrap_or_else(|| panic!("no outbound {prefix} on leg A: {:#?}", legs["A"]))
            .clone()
    };
    let ack = out("ACK ");
    assert!(ack.note.as_deref().is_some_and(is_unscripted_answer), "{:?}", ack.note);
    assert!(ack.step.is_none(), "the ACK owns no step: {ack:#?}");
    assert!(text(&ack).contains("CSeq: 1 ACK"), "{}", text(&ack));
    assert_eq!(
        via_branch(&text(&ack)),
        via_branch(&text(&out("INVITE "))),
        "§17.1.1.3: same branch"
    );

    // And ONLY the act the RFC names: the ACK the system sends leg B for its own
    // 486 is no step's, so leg B's server transaction consumes it (§17.2.1) —
    // no failure, and nothing is invented in reply.
    assert!(
        !outcome.verdict.failures.iter().any(|f| matches!(
            f,
            Failure::UnexpectedDatagram { leg, arrived: Arrived::Request { method, .. }, .. }
                if leg == "B" && method == "ACK"
        )),
        "{:#?}",
        outcome.verdict.failures
    );
    assert!(
        legs["B"].iter().any(|m| m.dir == Dir::In
            && text(m).starts_with("ACK ")
            && m.note.as_deref().is_some_and(|n| n.contains("§17.2.1"))),
        "leg B's transaction consumed the ACK: {:#?}",
        legs["B"]
    );
    assert!(
        legs["B"]
            .iter()
            .all(|m| !(m.dir == Dir::Out && m.note.as_deref().is_some_and(is_unscripted_answer))),
        "leg B answered something no rule makes its own: {:#?}",
        legs["B"]
    );

    // The close never had to compose it: the obligation was discharged at the
    // arrival, so what the abandoned script closed holds no ACK for leg A.
    let abandoned = outcome.verdict.abandoned.as_ref().expect("the script was abandoned");
    assert!(
        !abandoned.closed.iter().any(|act| act.leg == "A" && act.owed == CloseOwed::Ack),
        "the close re-ACKed what the refusal had already answered: {:#?}",
        abandoned.closed
    );
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// RFC 3261 §15.1.2 makes the answer to a BYE the endpoint's own whether or
/// not its flow is still running. The callee's script ends at the ACK, the
/// caller's BYE is the flow's last step, and the system relays that BYE onto
/// leg B once the flow has completed: the arrival is the late datagram it is —
/// the finding stands, the verdict is what it was — AND leg B answers it `200`
/// out of its own stack, so the system's BYE transaction ends there instead of
/// retransmitting to Timer F and the run settles as soon as the call is gone.
#[tokio::test(start_paused = true)]
async fn a_bye_taken_after_the_flow_completed_is_answered_200_and_still_a_late_arrival() {
    let scene = api_scene("pivot-late-bye").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "linear-attempt-late-bye".to_string();
    case.document.flow.retain(|node| {
        !matches!(node, pivot_schema::flow::FlowNode::Message(step)
            if ["s11", "s12", "s13"].contains(&step.id.as_str()))
    });
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    // The finding stands, and it is the only one: the flow completed and a BYE
    // arrived on a leg that scripted nothing for it.
    assert_eq!(outcome.verdict.completed_steps.len(), 10, "{:?}", outcome.verdict.completed_steps);
    assert!(outcome.verdict.abandoned.is_none(), "{:#?}", outcome.verdict.abandoned);
    let late: Vec<&Failure> = outcome
        .verdict
        .failures
        .iter()
        .filter(|f| matches!(f, Failure::DatagramAfterFlow { .. }))
        .collect();
    assert!(
        matches!(
            late.as_slice(),
            [Failure::DatagramAfterFlow { leg, arrived: Arrived::Request { method, .. } }]
                if leg == "B" && method == "BYE"
        ),
        "{:#?}",
        outcome.verdict.failures
    );
    assert_eq!(outcome.verdict.failures.len(), 1, "{:#?}", outcome.verdict.failures);

    // The endpoint answered it all the same, out of leg B's own stack, right
    // behind the arrival it recorded as late.
    let legs = outcome.recording.legs();
    let bye = legs["B"]
        .iter()
        .position(|m| m.dir == Dir::In && text(m).starts_with("BYE "))
        .unwrap_or_else(|| panic!("no BYE on leg B: {:#?}", legs["B"]));
    let taken = &legs["B"][bye];
    assert!(
        taken.note.as_deref().is_some_and(|note| note.contains("after the flow completed")),
        "{taken:#?}"
    );
    let taken_text = text(taken);
    let cseq = taken_text
        .lines()
        .find_map(|line| line.strip_prefix("CSeq: "))
        .expect("the BYE carries a CSeq");
    let answer =
        legs["B"].get(bye + 1).unwrap_or_else(|| panic!("nothing answered the BYE: {taken:#?}"));
    assert!(
        answer.dir == Dir::Out
            && text(answer).starts_with("SIP/2.0 200")
            && text(answer).contains(&format!("\r\nCSeq: {cseq}\r\n"))
            && answer.note.as_deref().is_some_and(is_unscripted_answer),
        "{answer:#?}"
    );
    assert_eq!(answer.step, None, "an answer owns no step");
    assert_eq!(
        via_branch(&text(answer)),
        via_branch(&text(taken)),
        "§17.2.2: the BYE's own branch"
    );

    // Answered once, the system's transaction never retransmitted, and the run
    // settled as soon as the call was gone — well inside the first rung of the
    // Timer E ladder a silent leg would have drawn.
    assert!(
        legs["B"]
            .iter()
            .all(|m| !(m.dir == Dir::In && text(m).starts_with("BYE ") && m.repeat_of.is_some())),
        "the BYE retransmitted: {:#?}",
        legs["B"]
    );
    let settled = outcome.timing.settled_at_ms.expect("the run settled");
    let taken_ms = taken.at_us / 1000;
    assert!(
        settled < taken_ms + 500,
        "settled at {settled} ms, the BYE arrived at {taken_ms} ms: the settle waited on a ladder"
    );
    // And the caller's own BYE drew the final §15.1.2 owes it, which the
    // document never scripted either.
    assert!(
        legs["A"].iter().any(|m| m.dir == Dir::In
            && text(m).starts_with("SIP/2.0 200")
            && m.note.as_deref().is_some_and(|note| note.contains("15.1.2"))),
        "{:#?}",
        legs["A"]
    );
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The linear call with its teardown CROSSING: the captured trace shows the
/// callee's own BYE 400 ms behind the caller's, and no BYE reaching the
/// callee. Leg B scripts its BYE (s11) and that BYE's 200 (s12), nothing for a
/// BYE it takes. 400 ms outlasts this lane's 200 ms relay of the caller's BYE,
/// so the relay reaches leg B first.
fn crossing_bye_case() -> Case {
    let base_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let text = std::fs::read_to_string(base_dir.join("linear-attempt.v3.json")).unwrap();
    let mut doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    doc["case"]["id"] = "linear-attempt-crossing-bye".into();
    let delay = |from: &str, ms: u64| serde_json::json!({ "compressible": true, "from": from, "ms": ms, "timer_linked": false });
    let flow = doc["flow"].as_array_mut().unwrap();
    flow.truncate(10);
    flow.push(serde_json::json!({
        "id": "s11", "leg": "B", "op": "send", "in_dialog": true,
        "msg": { "method": "BYE" }, "delay": delay("step:s10", 400)
    }));
    flow.push(serde_json::json!({
        "id": "s12", "leg": "B", "op": "expect", "check": "record", "in_dialog": true,
        "msg": { "cseq-method": "BYE", "reason": "OK", "status": 200 },
        "delay": delay("step:s11", 0)
    }));
    flow.push(serde_json::json!({
        "id": "s13", "leg": "A", "op": "expect", "check": "record", "in_dialog": true,
        "msg": { "cseq-method": "BYE", "reason": "OK", "status": 200 },
        "delay": delay("step:s10", 0)
    }));
    let document = PivotV3::from_json(&doc.to_string()).expect("the crossing case parses");
    Case { document, base_dir }
}

/// RFC 3261 §15.1.1 / §15.1.2: once a UA has answered the peer's BYE 200 the
/// dialog is gone, so a BYE it would send on it names no dialog and draws a
/// 481. Where the system's relay of the caller's BYE reaches the callee
/// before the callee's own crossing BYE goes out, the callee answers the
/// relay (an unscripted arrival) and its scripted BYE is moot: it is never
/// put on the wire.
#[tokio::test(start_paused = true)]
async fn a_scripted_bye_on_a_dialog_the_leg_already_ended_is_not_sent() {
    let scene = api_scene("pivot-crossing-bye").await;
    let (outcome, _dir) = replay_case(&scene, crossing_bye_case(), BTreeMap::new()).await;

    let legs = outcome.recording.legs();
    let answered = legs["B"]
        .iter()
        .position(|m| {
            m.dir == Dir::Out && text(m).starts_with("SIP/2.0 200") && text(m).contains(" BYE\r\n")
        })
        .unwrap_or_else(|| panic!("leg B answered the relayed BYE: {:#?}", legs["B"]));
    assert!(
        legs["B"][answered + 1..]
            .iter()
            .all(|m| !(m.dir == Dir::Out && text(m).starts_with("BYE "))),
        "leg B sent its BYE on the dialog its own 200 had ended: {:#?}",
        legs["B"]
    );
    assert!(
        legs["B"].iter().all(|m| !(m.dir == Dir::In && text(m).starts_with("SIP/2.0 481"))),
        "a 481 answered a BYE leg B should never have sent: {:#?}",
        legs["B"]
    );
    // The verdict names the one divergence — the relayed BYE the document never
    // scripted on leg B — and nothing of the moot BYE: no timeout on its final,
    // which is retired.
    let failures = &outcome.verdict.failures;
    assert!(
        failures.iter().all(|f| matches!(f, Failure::UnexpectedDatagram { leg, .. } if leg == "B")),
        "{failures:#?}"
    );
    assert_eq!(failures.len(), 1, "{failures:#?}");
    assert_eq!(outcome.verdict.retired, ["s12"], "the moot BYE's final is retired");
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// A BYE the document scripts AFTER the peer's BYE it already answered is the
/// captured peer's own act, 481 and all (RFC 3261 §15.1.2): leg B takes the
/// caller's BYE, answers it, then sends the BYE it scripts behind, and the
/// system answers 481. Nothing is moot about it — it is what was captured.
#[tokio::test(start_paused = true)]
async fn a_bye_the_document_scripts_after_the_peers_is_sent() {
    let base_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let text_doc = std::fs::read_to_string(base_dir.join("linear-attempt.v3.json")).unwrap();
    let mut doc: serde_json::Value = serde_json::from_str(&text_doc).unwrap();
    doc["case"]["id"] = "linear-attempt-bye-after-bye".into();
    let delay = |from: &str, ms: u64| serde_json::json!({ "compressible": true, "from": from, "ms": ms, "timer_linked": false });
    let flow = doc["flow"].as_array_mut().unwrap();
    flow.truncate(13);
    flow.push(serde_json::json!({
        "id": "s14", "leg": "B", "op": "send", "in_dialog": true,
        "msg": { "method": "BYE" }, "delay": delay("step:s12", 50)
    }));
    flow.push(serde_json::json!({
        "id": "s15", "leg": "B", "op": "expect", "check": "record", "in_dialog": true,
        "msg": { "cseq-method": "BYE", "reason": "Call/Transaction Does Not Exist", "status": 481 },
        "delay": delay("step:s14", 0)
    }));
    let document = PivotV3::from_json(&doc.to_string()).expect("the case parses");
    let scene = api_scene("pivot-bye-after-bye").await;
    let (outcome, _dir) = replay_case(&scene, Case { document, base_dir }, BTreeMap::new()).await;
    let legs = outcome.recording.legs();
    assert!(
        legs["B"].iter().any(|m| m.dir == Dir::Out && text(m).starts_with("BYE ")),
        "leg B sent the BYE the document scripts: {:#?}",
        legs["B"]
    );
    assert!(
        outcome.verdict.completed_steps.contains(&"s15".to_string()),
        "the 481 it drew matched: {:#?}",
        outcome.verdict
    );
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// RFC 3261 §15.1.2, last paragraph: a UAS that answered a BYE still responds
/// to every request pending on that dialog, 487 recommended.
///
/// The caller re-INVITEs, then hangs up without waiting for the answer. The
/// system relays both onto a callee that scripts neither: the re-INVITE is
/// refused (nothing answers an unscripted INVITE — its answer is the
/// document's call decision, §14.2), then the BYE lands on the held dialog and
/// draws its 200 — and the re-INVITE its 487, out of leg B's own stack, so the
/// system's INVITE client transaction ends on a final instead of Timer B.
///
/// The 487 holds leg B's server transaction in Completed until the ACK the
/// system owes it on the INVITE's branch (§17.1.1.3, §17.2.1), so the settle
/// waits for that ACK — it is milliseconds behind — and records it as the
/// transaction's own closer, not as a datagram nothing scripted.
#[tokio::test(start_paused = true)]
async fn a_re_invite_pending_when_the_bye_is_answered_draws_487_behind_the_200() {
    let scene = api_scene("pivot-reinvite-under-bye").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "linear-attempt-reinvite-under-bye".to_string();
    // The callee's BYE steps go: leg B scripts nothing past the confirming ACK.
    case.document.flow.retain(|node| {
        !matches!(node, pivot_schema::flow::FlowNode::Message(step)
            if ["s11", "s12"].contains(&step.id.as_str()))
    });
    // The re-INVITE (the caller's offer again) and its 100, 50 ms after the ACK.
    let mut reinvite = step_of(&mut case, "s1").clone();
    reinvite.id = "s101".into();
    reinvite.in_dialog = true;
    reinvite.msg.ruri = None;
    reinvite.msg.from = None;
    reinvite.msg.to = None;
    reinvite.msg.headers.clear();
    reinvite.delay.from = "step:s9".parse().expect("s9 is a step id");
    reinvite.delay.ms = 50;
    let mut trying = step_of(&mut case, "s2").clone();
    trying.id = "s102".into();
    trying.in_dialog = true;
    trying.msg.cseq = Some(2);
    trying.delay.from = "step:s101".parse().expect("s101 is a step id");
    // The hang-up follows the 100 by 100 ms, well ahead of any answer.
    let bye = step_of(&mut case, "s10");
    bye.delay.from = "step:s102".parse().expect("s102 is a step id");
    bye.delay.ms = 100;
    step_of(&mut case, "s13").delay.from = "step:s10".parse().expect("s10 is a step id");
    let at = case
        .document
        .flow
        .iter()
        .position(|node| matches!(node, pivot_schema::flow::FlowNode::Message(s) if s.id == "s10"))
        .expect("the teardown is a message node");
    for (offset, step) in [reinvite, trying].into_iter().enumerate() {
        case.document
            .flow
            .insert(at + offset, pivot_schema::flow::FlowNode::Message(Box::new(step)));
    }
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    let legs = outcome.recording.legs();
    let b = &legs["B"];
    let position =
        |predicate: &dyn Fn(&pivot_schema::bundle::recording::RecordedMessage) -> bool| {
            b.iter().position(predicate).unwrap_or_else(|| panic!("not on leg B: {b:#?}"))
        };
    let reinvite_at = position(&|m| {
        m.dir == Dir::In
            && text(m).starts_with("INVITE ")
            && text(m).contains("\r\nCSeq: 2 INVITE\r\n")
    });
    let bye_at = position(&|m| m.dir == Dir::In && text(m).starts_with("BYE "));
    assert!(reinvite_at < bye_at, "the re-INVITE landed before the BYE: {b:#?}");
    let bye_text = text(&b[bye_at]);
    let bye_cseq = bye_text
        .lines()
        .find_map(|line| line.strip_prefix("CSeq: "))
        .expect("the BYE carries a CSeq");
    // The 200 to the BYE, then the 487 to the pending INVITE, both the leg's own.
    let ok = b.get(bye_at + 1).unwrap_or_else(|| panic!("nothing answered the BYE: {b:#?}"));
    assert!(
        ok.dir == Dir::Out
            && text(ok).starts_with("SIP/2.0 200")
            && text(ok).contains(&format!("\r\nCSeq: {bye_cseq}\r\n"))
            && ok.note.as_deref().is_some_and(is_unscripted_answer),
        "{ok:#?}"
    );
    let terminated =
        b.get(bye_at + 2).unwrap_or_else(|| panic!("nothing answered the re-INVITE: {b:#?}"));
    assert!(
        terminated.dir == Dir::Out
            && text(terminated).starts_with("SIP/2.0 487")
            && text(terminated).contains("\r\nCSeq: 2 INVITE\r\n")
            && terminated.note.as_deref().is_some_and(is_unscripted_answer),
        "{terminated:#?}"
    );
    assert_eq!(
        via_branch(&text(terminated)),
        via_branch(&text(&b[reinvite_at])),
        "§17.2.3: the INVITE's own branch"
    );
    // Both answered in the instant the BYE landed.
    assert_eq!(ok.at_us, b[bye_at].at_us, "{ok:#?}");
    assert_eq!(terminated.at_us, b[bye_at].at_us, "{terminated:#?}");
    // The system ACKed the 487 on the INVITE's own branch (§17.1.1.3), and the
    // recording holds that ACK behind the 487 — past the INVITE repeat its
    // Timer A had already put on the wire — noted as the closer the settle
    // waited for. The run's one late-arrival finding is the caller's 487,
    // raised before this ACK lands, so the note is what pins the absorption.
    let ack = b[bye_at + 3..]
        .iter()
        .find(|m| m.dir == Dir::In && text(m).starts_with("ACK "))
        .unwrap_or_else(|| panic!("no ACK followed the 487 on leg B: {b:#?}"));
    assert!(
        text(ack).contains("\r\nCSeq: 2 ACK\r\n")
            && ack.note.as_deref().is_some_and(|note| note.contains("17.1.1.3")),
        "{ack:#?}"
    );
    assert_eq!(
        via_branch(&text(ack)),
        via_branch(&text(terminated)),
        "§17.1.1.3: the INVITE's branch"
    );
    // And the run settled on the teardown at once: the ACK is milliseconds
    // behind the 487, and nothing else held the run open past it.
    let settled = outcome.timing.settled_at_ms.expect("the run settled");
    assert!(
        settled < b[bye_at].at_us / 1000 + 500,
        "settled at {settled} ms: the settle waited on a ladder"
    );
    assert!(
        settled >= ack.at_us / 1000,
        "settled at {settled} ms, before the ACK at {} us",
        ack.at_us
    );
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// A message's topmost Via branch.
fn via_branch(raw: &str) -> String {
    raw.lines()
        .find_map(|line| line.split_once("branch="))
        .map(|(_, branch)| branch.trim().to_string())
        .expect("every emission carries a Via branch")
}
