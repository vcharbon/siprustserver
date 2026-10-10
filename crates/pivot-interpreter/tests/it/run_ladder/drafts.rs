//! Authored drafts from the case library: origin-lane vocabulary, a re-INVITE crossing a BYE,
//! transfers, and accessor-composed headers.

use super::*;

/// Rung three, over an authored case LIBRARY outside this crate: a draft, its
/// own `resources/`, and nothing about it rewritten. **The lane-scoping
/// exemplar** (§9.1) and the `rfc_violations` one (§11.1).
///
/// A draft's SIP choreography — fifteen steps over two legs, a CANCEL crossing
/// the answer, an order-free teardown pair — runs to completion against a real
/// B2BUA and the run is GREEN, carrying two facts it does not gate on:
///
/// - the draft asserts CDR events in the vocabulary of the deployment it was
///   written for (`InviteReceived`, `Bye`) while this system spells them
///   `invite_received` and `bye`. Both checks state `cdr-vocabulary`, this lane
///   is not the draft's `origin_lane`, so both are evaluated and RECORDED —
///   the document keeps the fact and the run does not turn red for replaying it
///   somewhere else;
/// - the RFC 3261 §9.2 violation the case exists for is emitted by a scripted
///   peer, so it is listed loudly and gates nothing.
///
/// It skips loudly when no library is configured — an empty corpus must never
/// read as a pass.
#[tokio::test(start_paused = true)]
async fn rung_three_a_draft_runs_green_with_its_origin_lane_s_vocabulary_recorded() {
    let Some(case) = case_dir("bc-rc-cancel-xing-200ok-invite") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    let scene = B2buaScene::new("pivot-cancel-xing").await;
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;
    assert_bundle_is_complete(&outcome, &dir);

    // Every one of the draft's steps ran, in its own document's terms.
    for step in (1..=15).map(|n| format!("s{n}")) {
        let step = &step;
        assert!(
            outcome.verdict.completed_steps.contains(step),
            "{step} never completed: {:#?}\nfailures: {:#?}",
            outcome.verdict.completed_steps,
            outcome.verdict.failures
        );
    }
    // The order-free pair on the b-leg: the platform's ACK and its teardown BYE
    // both arrived, in whichever order.
    assert!(outcome.verdict.completed_steps.contains(&"s13".to_string()));
    assert!(outcome.verdict.completed_steps.contains(&"s14".to_string()));
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");

    // The violation the case exists to reproduce is LISTED and gates nothing:
    // the callee that answers after taking the CANCEL is a scripted peer, and
    // reproducing a peer's non-compliance is what the document is for.
    assert_eq!(outcome.verdict.rfc_violations.len(), 1, "{:#?}", outcome.verdict.rfc_violations);
    let note = &outcome.verdict.rfc_violations[0];
    assert_eq!(note.rule.to_string(), "no-200-after-cancel");
    assert_eq!(note.step, "s11");
    assert!(!note.gating, "a scripted peer's violation never gates");

    // And the residue gates nothing: the draft's two CDR event regexes are the
    // authoring lane's PascalCase vocabulary read off a snake_case stream, both
    // classified, both evaluated, both recorded as informative.
    assert!(outcome.verdict.failures.is_empty(), "{:#?}", outcome.verdict.failures);
    assert_eq!(outcome.verdict.informative.len(), 2, "{:#?}", outcome.verdict.informative);
    assert!(
        outcome.verdict.informative.iter().all(|note| note.class == CheckClass::CdrVocabulary
            && matches!(&note.finding, Failure::CdrMismatch { .. })),
        "{:#?}",
        outcome.verdict.informative
    );

    // The bundle on disk carries both sections, because reading them there is
    // the whole point of recording rather than gating.
    let written: serde_json::Value =
        serde_json::from_reader(std::fs::File::open(dir.join("verdict.json")).unwrap()).unwrap();
    assert_eq!(written["status"], "ok");
    assert_eq!(written["informative"].as_array().map(Vec::len), Some(2), "{written:#}");
    assert_eq!(written["rfc_violations"][0]["rule"], "no-200-after-cancel");

    scene.finish().await;
}

/// Rung three, the IN-DIALOG rung: a draft whose callee re-INVITEs an
/// established dialog, whose caller's BYE crosses that re-INVITE, and whose
/// 481 and teardown BYE reach the callee in either order.
///
/// It is the re-INVITE drive end to end — an in-dialog request composed on the
/// dialog the leg already owns, its 100 Trying, its relayed copy on the far leg,
/// an in-dialog NEGATIVE final and the hop ACK RFC 3261 §17.1.1.3 owes it — over
/// a real B2BUA, with the crossing teardown taken order-free.
///
/// It skips loudly when no case library is configured.
#[tokio::test(start_paused = true)]
async fn rung_three_a_re_invite_draft_crosses_a_bye_and_takes_the_481_order_free() {
    let Some(case) = case_dir("bc-rc-reinvite-then-bye") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    let scene = B2buaScene::new("pivot-reinvite-then-bye").await;
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;
    assert_bundle_is_complete(&outcome, &dir);
    for step in (1..=20).map(|n| format!("s{n}")) {
        let step = &step;
        assert!(
            outcome.verdict.completed_steps.contains(step),
            "{step} never completed: {:#?}\nfailures: {:#?}",
            outcome.verdict.completed_steps,
            outcome.verdict.failures
        );
    }

    // The callee's re-INVITE and the caller's crossing BYE really crossed: the
    // b-leg carries an outbound in-dialog INVITE on the dialog it already owns,
    // and the ACK the 481 owes rides that same transaction (§17.1.1.3).
    let b_leg = &outcome.recording.legs()["B"];
    let re_invite = b_leg
        .iter()
        .find(|m| m.step.as_deref() == Some("s10"))
        .expect("the callee's re-INVITE is recorded");
    assert!(text(re_invite).starts_with("INVITE sip:"), "{}", text(re_invite));
    assert!(text(re_invite).contains("CSeq: 1 INVITE"), "{}", text(re_invite));
    let hop_ack = b_leg
        .iter()
        .find(|m| m.step.as_deref() == Some("s19"))
        .expect("the ACK to the 481 is recorded");
    assert!(text(hop_ack).contains("CSeq: 1 ACK"), "{}", text(hop_ack));
    let branch = |raw: &str| {
        raw.split("branch=").nth(1).and_then(|r| r.split(['\r', ';']).next()).map(str::to_string)
    };
    assert_eq!(
        branch(&text(hop_ack)),
        branch(&text(re_invite)),
        "the ACK to a non-2xx rides its own INVITE's branch"
    );

    // The residue is the authoring platform's CDR vocabulary and nothing else.
    assert!(outcome.verdict.failures.is_empty(), "{:#?}", outcome.verdict.failures);
    assert_eq!(outcome.verdict.informative.len(), 2, "{:#?}", outcome.verdict.informative);
    assert!(
        outcome.verdict.informative.iter().all(|note| note.class == CheckClass::CdrVocabulary
            && matches!(&note.finding, Failure::CdrMismatch { .. })),
        "{:#?}",
        outcome.verdict.informative
    );
    scene.finish().await;
}

/// Rung three, the TRANSFER rung, and a **SUT CAPABILITY FINDING**: the
/// INFO-intake transfer draft does not replay on the upstream demo B2BUA, and
/// the run says exactly where it stops.
///
/// The draft's transferor asks for the transfer with an in-dialog INFO
/// (its step `s8`). This system has no INFO intake: `relay-info`
/// (`b2bua/src/rules/defaults/core_rules.rs`) forwards an in-dialog INFO to the
/// peer leg like any other transparent method, so the caller's leg receives it
/// and no transferee is ever dialled. The demo's only transfer intake is REFER
/// (`b2bua/src/rules/refer_transfer/`), and its failure treatment is the
/// release reroute, not a transfer-failure reroute.
///
/// The rung pins that outcome by NAME rather than skipping the case: the
/// document is not wrong, the lane cannot play it, and `case.lanes` records
/// `blocked:no-info-intake-transfer` for `upstream-demo`. Everything up to s8
/// DOES run — the dialog establishes and the in-dialog INFO reaches the wire
/// with its frozen body — so the bundle is real evidence of where the gap is.
#[tokio::test(start_paused = true)]
async fn rung_three_an_info_intake_transfer_finds_no_intake_on_this_lane() {
    let Some(case) = case_dir("info-transfer-caller-fail-reroute") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    // The frozen body's own content type, read from the document rather than
    // spelled here: a deployment's private vocabulary lives in its case library.
    let content_type = case
        .document
        .flow
        .iter()
        .find_map(|node| match node {
            pivot_schema::flow::FlowNode::Message(step) if step.id == "s8" => {
                match &step.msg.body {
                    Some(pivot_schema::body::Body::Resource(resource)) => {
                        resource.content_type.clone()
                    }
                    _ => None,
                }
            }
            _ => None,
        })
        .expect("the transfer request states its frozen body's content type");
    let scene = B2buaScene::new("pivot-info-transfer").await;
    let transferees = scene.h.agent("transferees", "127.0.0.1:5071").await;
    let extra = BTreeMap::from([
        ("uas2".to_string(), transferees.clone()),
        ("uas3".to_string(), transferees),
    ]);
    let (outcome, dir) = replay_case(&scene, case, extra).await;

    // The establishment and the transfer request itself ran.
    for step in (1..=8).map(|n| format!("s{n}")) {
        let step = &step;
        assert!(
            outcome.verdict.completed_steps.contains(step),
            "{step} never completed: {:#?}\nfailures: {:#?}",
            outcome.verdict.completed_steps,
            outcome.verdict.failures
        );
    }
    let info = text(
        outcome.recording.legs()["B"]
            .iter()
            .find(|m| m.step.as_deref() == Some("s8"))
            .expect("the transferor's INFO reached the wire"),
    );
    assert!(info.starts_with("INFO sip:"), "{info}");
    assert!(info.contains(&format!("Content-Type: {content_type}")), "{info}");

    // And then the lane's own answer: the INFO comes BACK on the caller's leg,
    // which is a relay, not an intake.
    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    let relayed = outcome
        .verdict
        .failures
        .iter()
        .find(|f| matches!(f, Failure::UnexpectedDatagram { .. }))
        .unwrap_or_else(|| panic!("no unexpected datagram: {:#?}", outcome.verdict.failures));
    let Failure::UnexpectedDatagram { leg, arrived, .. } = relayed else {
        unreachable!("matched above")
    };
    assert_eq!(leg, "A", "the INFO was relayed to the caller");
    assert!(matches!(arrived, Arrived::Request { method, .. } if method == "INFO"), "{arrived}");
    // The relay stopped nothing: the run went on recording what arrived until
    // the 200 to the INFO that never comes ran out its budget — which is what
    // ends the script (§11.2), so the finding is the WHOLE gap and not its first
    // packet.
    assert!(
        outcome
            .verdict
            .failures
            .iter()
            .any(|f| matches!(f, Failure::ExpectTimedOut { step, .. } if step == "s9")),
        "{:#?}",
        outcome.verdict.failures
    );
    // No transferee was ever dialled: legs C and D heard nothing at all.
    let legs = outcome.recording.legs();
    assert!(legs["C"].is_empty() && legs["D"].is_empty(), "{legs:#?}");
    assert!(dir.join("verdict.json").is_file(), "the finding keeps its bundle");

    // A lane that cannot play the case still owes it a teardown: the close ended
    // the established dialog the script walked away from.
    let abandoned = outcome.verdict.abandoned.as_ref().expect("the script was abandoned");
    assert!(
        abandoned.closed.iter().any(|act| act.leg == "A" && act.owed == CloseOwed::Bye),
        "{:#?}",
        abandoned.closed
    );
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// Rung three, the ACCESSOR rung: a header composed out of the lane's own
/// number allocation reaches the wire, and a platform that was told to process
/// this call's transfers itself refuses a Refer-To no reader accepts.
///
/// The draft's transferor REFERs with a deliberately unclosed name-addr
/// (`malformed-header` deviation d1) whose number is `${num:transferee:e164}` —
/// a registry identity NO attempt dials, so it exists only because §4.3 binds
/// every declared identity. The draft asserts 400, which is an answer only a
/// platform that terminates the REFER can give: the rung ARMS local REFER
/// processing on this lane (§4.3 — the document states the flow, the runner
/// states the scene), because a platform that relays REFER on would hand the
/// header to the caller instead of answering it.
#[tokio::test(start_paused = true)]
async fn rung_three_an_accessor_composed_refer_to_is_refused_by_name() {
    let Some(case) = case_dir("refer-malformed-refer-to") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    let scene = api_scene("pivot-refer-malformed").await;
    let transferee = scene.h.agent("transferee", "127.0.0.1:5071").await;
    let extra = BTreeMap::from([("uas2".to_string(), transferee)]);
    let directives = LaneDirectives { local_refer: true, ..Default::default() };
    let (outcome, dir) = replay_directed(&scene, case, extra, directives).await;

    // The composed header is on the wire, verbatim as the deviation states it:
    // the lane's number for an identity no call dials, inside an unclosed
    // name-addr no reader accepts.
    let refer = text(
        outcome.recording.legs()["B"]
            .iter()
            .find(|m| m.step.as_deref() == Some("s8"))
            .expect("the REFER reached the wire"),
    );
    let refer_to = refer
        .split("Refer-To: ")
        .nth(1)
        .and_then(|rest| rest.split("\r\n").next())
        .expect("the REFER carries a Refer-To");
    assert!(
        refer_to.starts_with("<sip:transferee@"),
        "the lane's number for an identity no call dials: {refer_to}"
    );
    assert!(!refer_to.ends_with('>'), "the name-addr stays unclosed: {refer_to}");

    // And the answer the draft states is the answer on the wire: 400, on the
    // transferor's own leg, at the step that asserts it.
    assert_bundle_is_complete(&outcome, &dir);
    let refusal = text(
        outcome.recording.legs()["B"]
            .iter()
            .find(|m| m.step.as_deref() == Some("s9"))
            .expect("the REFER was answered"),
    );
    assert!(refusal.starts_with("SIP/2.0 400"), "{refusal}");

    // No transfer started: the transferee was never dialled and the transferor
    // heard no NOTIFY — the document's two `exactly: 0` counters, and legs C/D
    // stayed silent.
    let legs = outcome.recording.legs();
    for leg in ["C", "D"] {
        assert!(legs.get(leg).is_none_or(|messages| messages.is_empty()), "{legs:#?}");
    }
    assert!(
        !legs["B"].iter().any(|m| text(m).starts_with("NOTIFY")),
        "a refused REFER opens no subscription: {:#?}",
        legs["B"]
    );

    scene.finish().await;
}
