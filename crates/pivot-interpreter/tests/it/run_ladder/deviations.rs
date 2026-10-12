//! Deviations: one this interpreter cannot emit, and the preserved block and jumped CSeq it can.

use super::*;

/// The false-success guard, end to end: a document whose defect IS the emission
/// must never run green while the interpreter emits a compliant message. The
/// fixture asks a `verbatim-emission` to preserve the ABSOLUTE header order —
/// the interleaving of the stored block with the tier-1 lines, which the
/// document does not store and this emission cannot restore — so the run stops
/// at that step, before anything reaches the wire.
#[tokio::test(start_paused = true)]
async fn a_deviation_this_interpreter_cannot_emit_stops_the_run_before_the_wire() {
    let scene = api_scene("pivot-verbatim-refused").await;
    let (outcome, dir) = replay(&scene, "verbatim-emission-refused.v3.json").await;

    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert_eq!(outcome.verdict.failed_step.as_deref(), Some("s1"));
    let refusal = outcome
        .verdict
        .failures
        .iter()
        .find(|f| matches!(f, Failure::DeviationUnimplemented { .. }))
        .unwrap_or_else(|| panic!("no deviation refusal: {:#?}", outcome.verdict.failures));
    let Failure::DeviationUnimplemented { deviation, kind, step, reason } = refusal else {
        unreachable!("matched above")
    };
    assert_eq!((deviation.as_str(), kind.as_str()), ("d1", "verbatim-emission"));
    assert_eq!(step.as_deref(), Some("s1"));
    assert!(reason.contains("does not guarantee"), "{reason}");
    assert!(
        outcome.recording.is_empty(),
        "nothing reached the wire: {:#?}",
        outcome.recording.legs()
    );
    assert!(dir.join("verdict.json").is_file());

    scene.finish().await;
}

/// Rung five, the EMISSION-PATH rung: the three deviation kinds §11 defines for
/// what an emission LOOKS like, in one call.
///
/// - `verbatim-emission` on the opening INVITE: the stored block rides as the
///   document holds it — two `Accept` rows in order, `user-agent` and
///   `supported` in the lower case the document spells them, and a trailing
///   header that stays trailing. The tier-1 lines around it are still the
///   stack's, because the document stores none of them (§8).
/// - `raw-order` on the re-INVITE: the same guarantee over the stored block,
///   asked for on its own, with a duplicate header opening and closing it.
/// - `cseq-override` on the BYE, in its relative form: the re-INVITE's CSeq
///   plus three. The leg's numbering CONTINUES from what went out (RFC 3261
///   §12.2.1.1), which is why the override sets it rather than decorating one
///   message.
///
/// It skips loudly when no case library is configured.
#[tokio::test(start_paused = true)]
async fn rung_five_a_preserved_block_and_a_jumped_cseq_reach_the_wire() {
    let Some(case) = case_dir("bc-rc-cseq-jump-raw-order") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    // The second `Accept` row as the document stores it: its media type is the
    // case library's choice, its position and casing are what the rung proves.
    let stated = serde_json::to_value(&case.document).expect("the document serialises");
    let accepts: Vec<String> = stated["flow"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|node| node["id"] == "s1")
        .and_then(|node| node["msg"]["headers"].as_array())
        .into_iter()
        .flatten()
        .filter(|header| header["name"] == "Accept")
        .filter_map(|header| header["value"].as_str().map(str::to_string))
        .collect();
    let second_accept =
        format!("\r\nAccept: {}\r\n", accepts.get(1).expect("s1 stores two Accept rows"));
    let scene = B2buaScene::new("pivot-cseq-jump-raw-order").await;
    // The `cseq-override` IS the non-compliance the case reproduces (RFC 3261
    // §12.2.1.1), and it is the scripted CALLER that emits it: the waiver is
    // scoped to that party so the same rule stays gated on the system's own
    // output.
    scene.h.waive(
        scenario_harness::WaiverScope::rule(
            "cseq-in-dialog-order",
            "the document's cseq-override deviation: the scripted caller jumps its dialog CSeq \
             by three, which is what this case exists to replay",
        )
        .on_party("alice"),
    );
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
    let a_leg = &outcome.recording.legs()["A"];
    let sent = |step: &str| {
        text(
            a_leg
                .iter()
                .find(|m| m.step.as_deref() == Some(step))
                .unwrap_or_else(|| panic!("{step} is recorded")),
        )
    };

    // The stored block, in the document's own order and casing.
    let invite = sent("s1");
    let block = [
        "\r\nuser-agent: PivotReplay/1.0\r\n",
        "\r\nP-Asserted-Identity: <sip:0009001@pivot.invalid>\r\n",
        "\r\nAccept: application/sdp\r\n",
        second_accept.as_str(),
        "\r\nsupported: timer, replaces\r\n",
        "\r\nX-Pivot-Order: stored-last\r\n",
    ];
    let mut at = 0usize;
    for line in block {
        let found = invite[at..]
            .find(line)
            .unwrap_or_else(|| panic!("{line:?} out of order or absent in:\n{invite}"));
        // Stop on the CRLF this line ENDS with: it is the one the next line
        // begins with.
        at += found + line.len() - 2;
    }
    assert!(!invite.contains("User-Agent: PivotReplay"), "the document's casing survived");
    assert!(invite.contains("\r\nVia: SIP/2.0/UDP "), "tier-1 is still the stack's");

    // `raw-order` over a duplicate header: both rows ride, in their positions.
    let re_invite = sent("s10");
    assert!(re_invite.contains("CSeq: 2 INVITE"), "{re_invite}");
    let first = re_invite.find("X-Pivot-Order: first").expect("the opening row");
    let expires = re_invite.find("session-expires: 1800").expect("the stated spelling");
    let last = re_invite.find("X-Pivot-Order: last").expect("the closing row");
    assert!(first < expires && expires < last, "{re_invite}");

    // The override is the number that went out, and the leg keeps it.
    let bye = sent("s17");
    assert!(bye.starts_with("BYE sip:"), "{bye}");
    assert!(bye.contains("CSeq: 5 BYE"), "the BYE is numbered 2 + 3:\n{bye}");

    // The draft was authored FOR this lane, so its CDR vocabulary is this
    // system's own and nothing downgrades.
    assert!(outcome.verdict.failures.is_empty(), "{:#?}", outcome.verdict.failures);
    assert!(outcome.verdict.informative.is_empty(), "{:#?}", outcome.verdict.informative);
    scene.finish().await;
}
