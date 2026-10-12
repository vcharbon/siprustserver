//! Retransmissions: the absorbed repeat, the declared ladder in both directions, the ACK a repeated
//! final draws, and a ladder's own pacing.

use super::*;

/// §14 item 12 is unconditional: EVERY message, in wire order. A retransmission
/// the receive view absorbs is still a message the run saw, so it is in the
/// bundle — noted as absorbed — and it still never reaches an expect.
#[tokio::test(start_paused = true)]
async fn an_absorbed_retransmission_is_recorded_and_never_reaches_an_expect() {
    let scene = api_scene("pivot-retransmit-absorbed").await;
    let (outcome, dir) = replay(&scene, "retransmit-absorbed.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);

    let legs = outcome.recording.legs();
    let absorbed: Vec<_> = legs
        .values()
        .flatten()
        .filter(|m| m.note.as_deref().is_some_and(|n| n.starts_with("absorbed")))
        .collect();
    assert!(
        !absorbed.is_empty(),
        "the held ring draws a Timer-A retransmission; none is recorded: {legs:#?}"
    );
    // It is a repeat of something already there, and it carries its bytes.
    for message in &absorbed {
        assert!(message.step.is_none(), "an absorbed repeat satisfies no step");
        assert!(!text(message).is_empty(), "an absorbed repeat is recorded verbatim");
    }
    // And the bundle on disk carries it, not just the in-memory handle.
    let written = std::fs::read_to_string(dir.join("recording/B.jsonl")).unwrap();
    assert!(written.contains("absorbed"), "{written}");

    // The interpreter agent mode exposes BOTH views over that one
    // stream — the retransmission is in the wire view and not in the TU view.
    let wire = &outcome.wire_view;
    let tu = outcome.tu_view();
    assert!(
        wire.len() > tu.len(),
        "the wire view is the wider one: {} vs {}",
        wire.len(),
        tu.len()
    );
    let repeats: Vec<_> = wire.iter().filter(|e| e.is_repeat()).collect();
    assert_eq!(repeats.len(), absorbed.len(), "one wire-view repeat per recorded absorption");
    assert!(
        repeats.iter().all(|e| e.start_line().starts_with("INVITE")),
        "the held ring's Timer-A retransmission is the repeat: {repeats:#?}",
    );
    assert!(!tu.iter().any(|e| e.is_repeat()), "no absorbed repeat reached the transaction user",);

    scene.finish().await;
}

/// The `retransmits` COUNT (§6.9), both directions of one call and both halves
/// of the issue-22 two-view table.
///
/// The caller's own ladder is EMITTED — three byte-identical INVITEs, paced by
/// RFC 3261 §17.1.1.2's Timer A, because the document states how many repeats
/// there were and never when. Each one draws the platform's replayed 100
/// (§17.2.1), which reaches the transaction user and is counted on the leg that
/// sees it; meanwhile the platform's own Timer A re-dials the callee, and THOSE
/// repeats are absorbed by the callee's transaction layer and exist in the wire
/// view alone.
#[tokio::test(start_paused = true)]
async fn a_declared_retransmit_ladder_is_emitted_paced_and_counted_in_both_views() {
    let Some(case) = case_dir("bc-rc-retransmit-ladder") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    let scene = B2buaScene::new("pivot-retransmit-ladder").await;
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;
    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 13, "every step ran");

    // The verdict states every declared ladder, and what the run actually saw.
    let ladders: BTreeMap<&str, (u32, u32)> = outcome
        .verdict
        .retransmits
        .iter()
        .map(|note| (note.step.as_str(), (note.declared, note.observed)))
        .collect();
    assert_eq!(
        ladders,
        BTreeMap::from([("s1", (2, 2)), ("s2", (2, 2)), ("s3", (2, 2))]),
        "{:#?}",
        outcome.verdict.retransmits
    );

    let legs = outcome.recording.legs();
    // The emitted ladder: three identical INVITEs, the repeats pointing at the
    // first and paced by Timer A — T1, then 2·T1.
    let sent: Vec<_> = legs["A"].iter().filter(|m| text(m).starts_with("INVITE ")).collect();
    assert_eq!(sent.len(), 3, "{:#?}", legs["A"]);
    assert!(sent.iter().all(|m| text(m) == text(sent[0])), "byte-identical repeats");
    assert_eq!((sent[1].repeat_of, sent[2].repeat_of), (Some(sent[0].seq), Some(sent[0].seq)));
    assert_eq!(
        (sent[1].at_us - sent[0].at_us, sent[2].at_us - sent[0].at_us),
        (500_000, 1_500_000),
        "RFC 3261 §17.1.1.2: T1, then 2·T1"
    );
    assert!(sent.iter().all(|m| m.step.as_deref() == Some("s1")), "the step emitted all three");

    // The platform's replay of its cached 100 (§17.2.1) reaches the transaction
    // user: three on the leg, one claimed by s2 and two counted against it.
    let trying: Vec<_> = legs["A"].iter().filter(|m| text(m).starts_with("SIP/2.0 100")).collect();
    assert_eq!(trying.len(), 3, "{:#?}", legs["A"]);
    assert_eq!(trying[0].step.as_deref(), Some("s2"));
    for repeat in &trying[1..] {
        assert_eq!(repeat.step, None, "a repeat satisfies no second expect");
        assert_eq!(repeat.repeat_of, Some(trying[0].seq));
        assert!(repeat.note.as_deref().is_some_and(|n| n.contains("s2")), "{repeat:#?}");
    }

    // The platform's own Timer-A ladder toward the callee is ABSORBED: one
    // INVITE at the transaction user, two more in the wire view.
    let dialled: Vec<_> = legs["B"].iter().filter(|m| text(m).starts_with("INVITE ")).collect();
    assert_eq!(dialled.len(), 3, "{:#?}", legs["B"]);
    assert_eq!(dialled[0].step.as_deref(), Some("s3"));
    for repeat in &dialled[1..] {
        assert!(repeat.note.as_deref().is_some_and(|n| n.starts_with("absorbed")), "{repeat:#?}");
        assert_eq!(repeat.repeat_of, Some(dialled[0].seq));
    }

    // And the two views agree with the two counts. The absorbed ladder is the
    // §17.2 repeat; the replayed provisional is not one — a provisional is never
    // deduped, deliberately — so the count on s2 is the STEP's ladder, not the
    // seam's.
    let wire = &outcome.wire_view;
    let tu = outcome.tu_view();
    let count = |view: &[scenario_harness::absorption::WireEntry], prefix: &str, repeat: bool| {
        view.iter()
            .filter(|e| e.start_line().starts_with(prefix) && e.is_repeat() == repeat)
            .count()
    };
    assert_eq!(count(wire, "INVITE", true), 2, "the absorbed ladder");
    assert_eq!(count(&tu, "INVITE", true), 0, "and none of it reached the transaction user");
    assert_eq!(count(wire, "SIP/2.0 100", false), 3, "every replayed provisional surfaces");

    // The bundle on disk carries the ladder, not just the handle.
    let written = std::fs::read_to_string(dir.join("recording/A.jsonl")).unwrap();
    assert!(written.contains("\"repeat_of\""), "{written}");
    let verdict: pivot_interpreter::RunVerdict =
        serde_json::from_str(&std::fs::read_to_string(dir.join("verdict.json")).unwrap()).unwrap();
    assert_eq!(verdict.retransmits.len(), 3);

    scene.finish().await;
}

/// The other half of the two-view table: a retransmitted FINAL and the ACKs it
/// draws, neither of which the transaction layer absorbs.
///
/// The callee repeats its 200 on §13.3.1.4's own pacing (T1, then 2·T1) and the
/// caller HOLDS its ACK past one rung — a held automatic is the step's `delay`,
/// never a deviation (§11) — so the platform runs its own 2xx ladder toward the
/// caller, which the run counts on the step that ladder repeats, and the
/// caller's ACK stops it. The b-leg ACK is the caller's, relayed: a copy that
/// lands before it is absorbed, and each copy that lands after it re-sends that
/// ACK (§13.2.2.4) on the one client transaction the first 2xx armed.
#[tokio::test(start_paused = true)]
async fn a_retransmitted_final_is_counted_and_each_2xx_after_the_relayed_ack_draws_it_again() {
    let Some(case) = case_dir("bc-rc-held-ack-final-ladder") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    let scene = B2buaScene::new("pivot-held-ack-final-ladder").await;
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;

    // The whole flow ran and the run settled: this is a counted ladder, not a
    // broken call.
    assert_eq!(outcome.verdict.completed_steps.len(), 13, "{:#?}", outcome.verdict.failures);
    assert!(outcome.timing.settled_at_ms.is_some());

    // The declared ladders and what the lane produced, side by side.
    let ladders: BTreeMap<&str, (u32, u32)> = outcome
        .verdict
        .retransmits
        .iter()
        .map(|note| (note.step.as_str(), (note.declared, note.observed)))
        .collect();
    assert_eq!(
        ladders,
        BTreeMap::from([("s6", (2, 2)), ("s7", (1, 1)), ("s9", (1, 1))]),
        "{:#?}",
        outcome.verdict.retransmits
    );
    // Every declared ladder holds, so the run is green.
    assert_eq!(outcome.verdict.status, VerdictStatus::Ok, "{:#?}", outcome.verdict.failures);
    assert!(outcome.verdict.failures.is_empty(), "{:#?}", outcome.verdict.failures);
    assert_eq!(outcome.verdict.failed_step, None);

    let legs = outcome.recording.legs();
    // The callee's own emitted ladder, paced by §13.3.1.4: T1, then 2·T1.
    let sent: Vec<_> = legs["B"]
        .iter()
        .filter(|m| text(m).starts_with("SIP/2.0 200") && m.step.as_deref() == Some("s6"))
        .collect();
    assert_eq!(sent.len(), 3, "{:#?}", legs["B"]);
    assert_eq!(
        (sent[1].at_us - sent[0].at_us, sent[2].at_us - sent[0].at_us),
        (500_000, 1_500_000)
    );
    // The relayed ACK, and one re-send for the copy that follows it: the copy at
    // T1 lands while the caller still holds its ACK and draws nothing of its own.
    // Both are the same bytes — one client transaction, so the callee's INVITE
    // server transaction quiesces on whichever arrives.
    let acks: Vec<_> = legs["B"].iter().filter(|m| text(m).starts_with("ACK ")).collect();
    assert_eq!(acks.len(), 2, "{:#?}", legs["B"]);
    assert!(acks.iter().all(|a| text(a) == text(acks[0])), "{:#?}", acks);
    assert!(acks[0].at_us > sent[1].at_us, "no ACK answers the copy the hold absorbed");
    assert!(acks[1].at_us > sent[2].at_us, "the re-send follows the copy it answers");

    // The platform's own ladder toward the held caller, counted where it lands:
    // one rung fits the hold, and the caller's ACK cancels the next.
    let to_invite: Vec<_> = legs["A"]
        .iter()
        .filter(|m| text(m).starts_with("SIP/2.0 200 ") && text(m).contains("CSeq: 1 INVITE"))
        .collect();
    assert_eq!(to_invite.len(), 2, "{:#?}", legs["A"]);
    assert_eq!(to_invite[1].repeat_of, Some(to_invite[0].seq));
    assert!(to_invite[1].step.is_none(), "a repeat satisfies no expect");

    // The trap row of the table: a 2xx retransmission is end-to-end, so the
    // repeat is in the wire view AND in the TU view.
    let tu = outcome.tu_view();
    let repeats = |view: &[scenario_harness::absorption::WireEntry], prefix: &str| {
        view.iter().filter(|e| e.start_line().starts_with(prefix) && e.is_repeat()).count()
    };
    assert_eq!(repeats(&outcome.wire_view, "SIP/2.0 200"), 1, "the platform's 2xx ladder");
    assert_eq!(repeats(&tu, "SIP/2.0 200"), 1, "2xx retransmission is never absorbed");
    assert!(dir.join("verdict.json").is_file(), "the run keeps its bundle");

    scene.finish().await;
}

/// The caller-side half of the same table: a count on an auto ACK step, which
/// the WIRE draws rather than a timer paces (§6.3).
///
/// The callee answers ONCE, so every repeat of the 200 on the caller's leg is
/// the platform's own un-ACKed-2xx watchdog and every ACK beyond the first is
/// one the caller's own core owed. Both copies arrive while the ACK is HELD:
/// RFC 3261 §13.2.2.4 owes one ACK per 2xx RECEIVED, so the hold releases all
/// three at once, and the count on the ACK step is what says so.
///
/// The callee's leg draws NO count from any of it: one final received there is
/// one ACK owed, and an ACK the far leg sends is that dialog's obligation, not
/// this one's — so `s9` declares no ladder and reports no note.
#[tokio::test(start_paused = true)]
async fn a_held_ack_s_count_is_drawn_by_the_repeats_of_the_final_its_transaction_drew() {
    let Some(case) = case_dir("bc-rc-drawn-ack-per-2xx") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    let scene = B2buaScene::new("pivot-drawn-ack-per-2xx").await;
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_eq!(outcome.verdict.completed_steps.len(), 13, "{:#?}", outcome.verdict.failures);
    assert!(outcome.timing.settled_at_ms.is_some());
    let ladders: BTreeMap<&str, (u32, u32)> = outcome
        .verdict
        .retransmits
        .iter()
        .map(|note| (note.step.as_str(), (note.declared, note.observed)))
        .collect();
    assert_eq!(
        ladders,
        BTreeMap::from([("s7", (2, 2)), ("s8", (2, 2))]),
        "{:#?}",
        outcome.verdict.retransmits
    );
    assert_eq!(outcome.verdict.status, VerdictStatus::Ok, "{:#?}", outcome.verdict.failures);

    let legs = outcome.recording.legs();
    // The callee answered once: nothing on ITS leg repeats the final.
    let answered: Vec<_> = legs["B"]
        .iter()
        .filter(|m| text(m).starts_with("SIP/2.0 200 ") && text(m).contains("CSeq: 1 INVITE"))
        .collect();
    assert_eq!(answered.len(), 1, "{:#?}", legs["B"]);

    // Three 200s reach the held caller, and the three ACKs it drew all leave
    // AFTER the last of them: the hold is what makes the catch-up visible.
    let finals: Vec<_> = legs["A"]
        .iter()
        .filter(|m| text(m).starts_with("SIP/2.0 200 ") && text(m).contains("CSeq: 1 INVITE"))
        .collect();
    assert_eq!(finals.len(), 3, "{:#?}", legs["A"]);
    assert!(finals[1..].iter().all(|m| m.repeat_of == Some(finals[0].seq)));
    let acks: Vec<_> = legs["A"].iter().filter(|m| text(m).starts_with("ACK ")).collect();
    assert_eq!(acks.len(), 3, "one ACK per 2xx received: {:#?}", legs["A"]);
    assert!(acks.iter().all(|a| a.step.as_deref() == Some("s8")), "{acks:#?}");
    assert!(acks.iter().all(|a| text(a) == text(acks[0])), "one client transaction, one ACK");
    assert!(acks[1..].iter().all(|a| a.repeat_of == Some(acks[0].seq)));
    assert!(acks[0].at_us > finals[2].at_us, "the ACK was held past the last copy");

    // The callee took ONE final, so it is owed ONE ACK: the caller's re-passes
    // discharge the caller dialog's obligation and never reach this leg.
    let relayed: Vec<_> = legs["B"].iter().filter(|m| text(m).starts_with("ACK ")).collect();
    assert_eq!(relayed.len(), 1, "one ACK per final received: {:#?}", legs["B"]);
    assert!(dir.join("verdict.json").is_file(), "the run keeps its bundle");

    scene.finish().await;
}

/// A ladder paces ONE leg's transaction and gates nothing else.
///
/// The rungs wait BESIDE the loop, never inside it: sleeping them out in the
/// send path parks the whole run for the ladder's length, so every other leg's
/// dwell is pushed back by it and the peer this ladder is aimed at answers into
/// a loop that cannot hear it until the last rung is out.
#[tokio::test(start_paused = true)]
async fn a_ladder_paces_its_own_leg_and_parks_no_other() {
    let scene = api_scene("pivot-ladder-beside-the-loop").await;
    let (outcome, dir) = replay(&scene, "ladder-beside-the-loop.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);

    let legs = outcome.recording.legs();
    // The ladder still runs, and still on Timer E: two BYEs, T1 apart.
    let byes: Vec<_> = legs["A"].iter().filter(|m| text(m).starts_with("BYE ")).collect();
    assert_eq!(byes.len(), 2, "{:#?}", legs["A"]);
    assert_eq!(byes[1].repeat_of, Some(byes[0].seq));
    assert_eq!(
        byes[1].at_us - byes[0].at_us,
        500_000,
        "RFC 3261 §17.1.2.2: the rung is T1 past the first copy"
    );

    // And the teardown reached the callee — and was answered — WHILE the rung
    // was still owed, not queued behind it.
    let taken = legs["B"]
        .iter()
        .find(|m| text(m).starts_with("BYE "))
        .expect("the callee took the teardown");
    // Well inside the rung, not merely before it: a run parked by the ladder
    // relays the teardown only once the rung is out, so the midpoint separates
    // the two readings by more than this scene's own relay latency.
    let midpoint = byes[0].at_us + (byes[1].at_us - byes[0].at_us) / 2;
    assert!(
        taken.at_us < midpoint,
        "the teardown waited out the ladder: sent at {} us, relayed at {} us, rung at {} us",
        byes[0].at_us,
        taken.at_us,
        byes[1].at_us
    );

    scene.finish().await;
}
