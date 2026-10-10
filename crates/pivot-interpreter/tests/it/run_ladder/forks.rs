//! Forks: reliable provisionals on several early dialogs, minted and observed fork tags.

use super::*;

/// The scene a RELIABLE-PROVISIONAL document replays against: the system keeps
/// the callee on 100rel, PRACKs each reliable provisional on the caller's
/// behalf, and shows the caller one bare 180.
///
/// It relays the dialled Request-URI userpart onto the b-leg — which the 18x
/// route decision alone does not do — so a `ruri-pos` claim still matches the
/// callee it names.
async fn reliable_provisional_scene(name: &str) -> B2buaScene {
    B2buaScene::with_b2bua(name, |bob_port| {
        B2buaSut::builder(std::sync::Arc::new(
            b2bua::decision::ScriptedDecisionEngine::builder()
                .fallback(move |request| {
                    let mut route = b2bua::decision::test_adapter::route_to_with_18x(
                        "127.0.0.1",
                        bob_port,
                        call::features::RelayFirst18xStrategy::FakePrack,
                    );
                    route.new_ruri = Some(callee_ruri(&request.ruri, bob_port));
                    b2bua::decision::NewCallResponse::Route(route)
                })
                .build(),
        ))
    })
    .await
}

/// The b-leg Request-URI: the userpart the caller dialled, at the callee's
/// socket. URI reading is `sip-message`'s.
fn callee_ruri(dialled: &str, bob_port: u16) -> String {
    match sip_message::header::Uri::parse(&sip_message::SipStr::owned(dialled))
        .ok()
        .and_then(|uri| uri.user().map(str::to_string))
        .filter(|user| !user.is_empty())
    {
        Some(user) => format!("sip:{user}@127.0.0.1:{bob_port}"),
        None => format!("sip:127.0.0.1:{bob_port}"),
    }
}

/// Rung three, the RELIABLE-PROVISIONAL rung: a draft whose callee rings two
/// forked early dialogs under RFC 3262, and whose system manages 100rel on the
/// callee's behalf.
///
/// The scripted UAS answers the INVITE twice over — a reliable 183 under early
/// dialog `f1`, a reliable 180 under `f2` — each with its own `RSeq` and its own
/// To-tag, and answers the PRACK the system originates for each. What the caller
/// sees is one bare 180 and then the answering fork's own SDP, so the rung
/// exercises `early` demux (§6.1), an RSeq the DOCUMENT states, and an RAck the
/// system composes.
///
/// Its residue is the lane-scoping one (§9.1): the draft states its CDR facts in
/// the vocabulary of the platform it was authored against — the event words and
/// the reason strings its 100rel service writes — and this lane spells all four
/// differently. Every one is classified, so all four are evaluated, recorded and
/// gate nothing.
///
/// It skips loudly when no case library is configured.
#[tokio::test(start_paused = true)]
async fn rung_three_a_forked_reliable_provisional_draft_prack_s_each_early_dialog() {
    let Some(case) = case_dir("forked-100rel-prack-per-early-dialog") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    let scene = reliable_provisional_scene("pivot-forked-100rel").await;
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;
    assert_bundle_is_complete(&outcome, &dir);
    for step in (1..=18).map(|n| format!("s{n}")) {
        let step = &step;
        assert!(
            outcome.verdict.completed_steps.contains(step),
            "{step} never completed: {:#?}\nfailures: {:#?}",
            outcome.verdict.completed_steps,
            outcome.verdict.failures
        );
    }

    // Two forks rang on one leg, and each PRACK reached its OWN early dialog:
    // the b-leg recording carries two reliable provisionals under two distinct
    // To-tags, and the two PRACKs that arrived acknowledge the RSeq each stated.
    let b_leg = &outcome.recording.legs()["B"];
    let rseqs: Vec<String> = b_leg
        .iter()
        .filter_map(|m| Some(text(m).split("RSeq: ").nth(1)?.split("\r\n").next()?.to_string()))
        .collect();
    assert_eq!(rseqs, ["1", "7001"], "each fork rings under its own RSeq: {rseqs:?}");
    let racks: Vec<String> = b_leg
        .iter()
        .filter_map(|m| Some(text(m).split("RAck: ").nth(1)?.split("\r\n").next()?.to_string()))
        .collect();
    assert_eq!(racks.len(), 2, "one PRACK per fork: {racks:?}");
    assert!(racks[0].starts_with("1 "), "fork 1's PRACK acknowledges RSeq 1: {racks:?}");
    assert!(racks[1].starts_with("7001 "), "fork 2's PRACK acknowledges RSeq 7001: {racks:?}");
    assert!(
        racks.iter().all(|rack| rack.ends_with(" INVITE")),
        "an RAck acknowledges the INVITE transaction: {racks:?}"
    );

    // The residue is the authoring platform's CDR vocabulary and nothing else:
    // two event words and two reason strings, all four classified, evaluated and
    // recorded rather than gating.
    assert!(outcome.verdict.failures.is_empty(), "{:#?}", outcome.verdict.failures);
    assert_eq!(outcome.verdict.informative.len(), 4, "{:#?}", outcome.verdict.informative);
    assert!(
        outcome.verdict.informative.iter().all(|note| note.class == CheckClass::CdrVocabulary
            && matches!(&note.finding, Failure::CdrMismatch { .. })),
        "{:#?}",
        outcome.verdict.informative
    );
    scene.finish().await;
}

/// The MINTED-FORK rung: leg B answers one INVITE twice over — a 180 under
/// early dialog `f1`, a 180 under `f2` — and rejects under `f1`'s tag, while
/// the caller's expects name no fork at all (§6.1, RFC 3261 §12.1.1).
///
/// The proof is on the b-leg wire: two provisionals of ONE transaction under
/// two distinct To-tags, and a 486 riding the first of them, all relayed and
/// the attempt properly rejected end to end.
#[tokio::test(start_paused = true)]
async fn a_callee_side_fork_rings_twice_under_two_minted_tags() {
    let scene = api_scene("pivot-forked-ring").await;
    let (outcome, dir) = replay(&scene, "forked-ring.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);

    // Two forks of one leg reached the wire as two To-tags, and the final rode
    // the first fork's.
    let b_leg = &outcome.recording.legs()["B"];
    let to_tags: Vec<String> = b_leg
        .iter()
        .map(text)
        .filter(|m| m.starts_with("SIP/2.0 180") || m.starts_with("SIP/2.0 486"))
        .filter_map(|m| {
            let to = m.lines().find(|line| line.starts_with("To:"))?;
            let tag = to.split("tag=").nth(1)?;
            Some(tag.split(';').next().unwrap_or(tag).trim().to_string())
        })
        .collect();
    assert_eq!(to_tags.len(), 3, "two rings and a final: {to_tags:?}");
    assert_ne!(to_tags[0], to_tags[1], "two forks are two dialogs: {to_tags:?}");
    assert!(to_tags[0].ends_with("-early-f1") && to_tags[1].ends_with("-early-f2"), "{to_tags:?}");
    assert_eq!(to_tags[2], to_tags[0], "the 486 answers under f1's tag: {to_tags:?}");

    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The INTERLEAVED-FORK rung: both forks ring reliably BEFORE either is
/// PRACKed, so each PRACK must ride its own early dialog — the fork's To-tag
/// and the fork's RAck on ONE message, and a CSeq numbered in that fork's own
/// space (RFC 3261 §12.2.1.1, RFC 3262 §7.2). The INVITE was each dialog's
/// CSeq 1, so BOTH PRACKs are CSeq 2; a leg-wide counter would number the
/// second one 3 and hand it the wrong To-tag.
///
/// The proof is the a-leg wire: the caller composes both PRACKs against the
/// two OBSERVED forks r1/r2 it learned from the system's a-facing tags.
#[tokio::test(start_paused = true)]
async fn an_interleaved_fork_s_prack_rides_its_own_early_dialog() {
    let scene = api_scene("pivot-forked-ring-interleaved").await;
    let (outcome, dir) = replay(&scene, "forked-ring-interleaved.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);
    for step in (1..=19).map(|n| format!("s{n}")) {
        assert!(
            outcome.verdict.completed_steps.contains(&step),
            "{step} never completed: {:#?}\nfailures: {:#?}",
            outcome.verdict.completed_steps,
            outcome.verdict.failures
        );
    }

    let a_leg = &outcome.recording.legs()["A"];
    let header = |raw: &str, name: &str| -> Option<String> {
        let wanted = format!("{}:", name.to_ascii_lowercase());
        raw.lines()
            .find(|line| line.to_ascii_lowercase().starts_with(&wanted))
            .and_then(|line| line.split_once(':').map(|(_, v)| v.trim().to_string()))
    };
    let to_tag = |raw: &str| {
        header(raw, "To").and_then(|to| {
            to.split_once("tag=").map(|(_, t)| t.split(';').next().unwrap_or(t).trim().to_string())
        })
    };
    // The two reliable provisionals, in ring order: each fork's a-facing
    // To-tag and the RSeq the system minted toward the caller.
    let rings: Vec<(String, String)> = a_leg
        .iter()
        .filter(|m| text(m).starts_with("SIP/2.0 183") || text(m).starts_with("SIP/2.0 180"))
        .filter_map(|m| Some((to_tag(&text(m))?, header(&text(m), "RSeq")?)))
        .collect();
    assert_eq!(rings.len(), 2, "two reliable forks rang: {rings:?}");
    assert_ne!(rings[0].0, rings[1].0, "two forks are two dialogs: {rings:?}");
    // The two PRACKs, in send order: the To-tag and the RAck name ONE dialog,
    // and each fork numbers from its own INVITE-seeded space.
    let pracks: Vec<(String, String, String)> = a_leg
        .iter()
        .filter(|m| text(m).starts_with("PRACK "))
        .filter_map(|m| {
            Some((to_tag(&text(m))?, header(&text(m), "RAck")?, header(&text(m), "CSeq")?))
        })
        .collect();
    assert_eq!(pracks.len(), 2, "one PRACK per fork: {pracks:?}");
    assert_eq!(pracks[0].0, rings[0].0, "fork 1's PRACK rides fork 1's dialog: {pracks:?}");
    assert_eq!(pracks[1].0, rings[1].0, "fork 2's PRACK rides fork 2's dialog: {pracks:?}");
    assert_eq!(pracks[0].1, format!("{} 1 INVITE", rings[0].1), "fork 1's own RSeq: {pracks:?}");
    assert_eq!(pracks[1].1, format!("{} 1 INVITE", rings[1].1), "fork 2's own RSeq: {pracks:?}");
    assert_eq!(pracks[0].2, "2 PRACK", "fork 1's space: the INVITE was its CSeq 1");
    assert_eq!(pracks[1].2, "2 PRACK", "fork 2's space, not fork 1's leavings");

    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The OBSERVED-FORK rung — the deliverable that closes the collapse hole: the
/// caller's two provisional expects name two observed forks `r1` and `r2`,
/// each of which must be a dialog of its own, and the wire carries only ONE.
///
/// The callee rings one fork twice (180 then 183 under `f1`), so toward the
/// caller both provisionals ride the single To-tag the system minted — at the
/// caller, exactly the wire a B2BUA that collapses two early dialogs into one
/// produces (the demo B2BUA itself is fork-transparent: the minted-fork rung
/// above shows each b-leg fork reaching the caller under its own tag). The
/// second expect MUST be refused — the tag already rides `r1` — so a document
/// claiming two dialogs over one tag goes red instead of silently passing.
#[tokio::test(start_paused = true)]
async fn a_caller_side_collapse_of_two_observed_forks_fails_by_name() {
    let scene = api_scene("pivot-forked-ring-collapsed").await;
    let (outcome, dir) = replay(&scene, "forked-ring-collapsed.v3.json").await;

    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    let unmatched = outcome
        .verdict
        .failures
        .iter()
        .find(|f| matches!(f, Failure::UnmatchedDatagram { .. }))
        .unwrap_or_else(|| {
            panic!("no unmatched-datagram failure: {:#?}", outcome.verdict.failures)
        });
    let Failure::UnmatchedDatagram { step, leg, reason, arrived, .. } = unmatched else {
        unreachable!("matched above")
    };
    assert_eq!((step.as_str(), leg.as_str()), ("s7", "A"));
    assert!(matches!(arrived, Arrived::Response { status: 183, .. }), "{arrived}");
    // The refusal names the fork, the tag, and the id that already rides it —
    // the exact account the triage taxonomy quotes.
    assert!(
        reason.starts_with(
            "gated on early dialog \"r2\", which must be a dialog of its own; To-tag \""
        ),
        "{reason}"
    );
    assert!(reason.contains("\" already rides \"r1\""), "{reason}");

    // A red run still ends its call and keeps its evidence.
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");
    scene.b2bua.assert_fully_reaped();
    for file in ["pivot.json", "run-config.json", "verdict.json", "timing.json"] {
        assert!(dir.join(file).is_file(), "{file} missing from a failed run's bundle");
    }

    scene.finish().await;
}
