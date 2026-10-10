//! Reroute and no-answer hunting, and how this lane arms a ring timer.

use super::*;

/// The scene a REROUTE document replays against: a system whose decision backend
/// walks the ordered `X-Api-Call` route plan the lane injected (ADR-0017), so a
/// b-leg that fails — or that never answers inside its own ring timer — advances
/// the call to the chain's next route instead of ending it.
async fn hunting_scene(name: &str) -> B2buaScene {
    B2buaScene::with_b2bua(name, |bob_port| B2buaSut::route_api_call("127.0.0.1", bob_port)).await
}

/// Rung three, the REROUTE rung: the first destination answers 486 and the
/// system hunts on to the alternate, which answers.
///
/// Two attempts on ONE branch and one socket, told apart by the Request-URI
/// user part each is dialled under — the relayed one for the attempt the caller
/// dialled, the registry's own number for the reroute the system composed — so
/// each INVITE reaches the leg the document names for it.
///
/// It skips loudly when no case library is configured.
#[tokio::test(start_paused = true)]
async fn rung_three_a_reroute_draft_hunts_past_a_busy_destination() {
    let Some(case) = case_dir("reroute-on-486") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    let scene = hunting_scene("pivot-reroute-486").await;
    let bob = scene.bob.clone();
    let (outcome, dir) =
        replay_case(&scene, case, BTreeMap::from([("uas2".to_string(), bob)])).await;
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
    // The first attempt really was left behind and the second really was a
    // different destination: two b-leg INVITEs, each under its own callee's
    // Request-URI, and the hunt's own negative final in between.
    let legs = outcome.recording.legs();
    let dialled = |leg: &str| {
        legs[leg]
            .iter()
            .map(text)
            .find(|m| m.starts_with("INVITE sip:"))
            .and_then(|m| m.split_whitespace().nth(1).map(str::to_string))
            .unwrap_or_default()
    };
    assert_eq!(dialled("B"), "sip:+1999999bob5070@127.0.0.1:5070", "the dialled callee");
    assert_eq!(dialled("C"), "sip:+15550900005@127.0.0.1:5070", "the rerouted callee");

    // The residue is the authoring platform's CDR vocabulary and nothing else:
    // two event words it spells in PascalCase and one `b_legs.count` field this
    // lane does not publish at all.
    assert!(outcome.verdict.failures.is_empty(), "{:#?}", outcome.verdict.failures);
    assert_eq!(outcome.verdict.informative.len(), 3, "{:#?}", outcome.verdict.informative);
    assert!(
        outcome.verdict.informative.iter().all(|note| note.class == CheckClass::CdrVocabulary
            && matches!(&note.finding, Failure::CdrMismatch { .. })),
        "{:#?}",
        outcome.verdict.informative
    );
    scene.finish().await;
}

/// Rung three, the NO-ANSWER rung: the first destination rings and never
/// answers, the ring timer the lane armed from the document's own
/// `no_answer_ms` fires, the system CANCELs that attempt and hunts on.
///
/// The dwell is the DOCUMENT's (§4.1): the driver lowers `no_answer_ms` to the
/// route's whole-second `no_answer_timeout_sec` and nothing else picks it. On a
/// paused runtime the callee simply stays silent, so the only thing that can
/// end the ring is the system's own timer.
///
/// It skips loudly when no case library is configured.
#[tokio::test(start_paused = true)]
async fn rung_three_a_no_answer_draft_rings_out_and_hunts_on() {
    let Some(case) = case_dir("reroute-on-no-answer") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    let scene = hunting_scene("pivot-reroute-no-answer").await;
    let bob = scene.bob.clone();
    let (outcome, dir) =
        replay_case(&scene, case, BTreeMap::from([("uas2".to_string(), bob)])).await;
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
    // The ring was the DOCUMENT's: the abandoned attempt's INVITE and the
    // CANCEL that ended it sit `no_answer_ms` apart on the b-leg's own ladder,
    // and nothing else on the wire could have ended it — the callee never
    // answered.
    let b_leg = &outcome.recording.legs()["B"];
    let at = |predicate: &dyn Fn(&str) -> bool| {
        b_leg.iter().find(|m| predicate(&text(m))).map(|m| m.at_us).expect("recorded")
    };
    let invited = at(&|raw: &str| raw.starts_with("INVITE sip:+1999999bob5070"));
    let cancelled = at(&|raw: &str| raw.starts_with("CANCEL "));
    assert_eq!(
        cancelled - invited,
        1_000_000,
        "the system rang for exactly the `no_answer_ms` the document states"
    );

    // And the abandoned leg was closed the way RFC 3261 §17.1.1.3 owes it: the
    // 487 the CANCEL drew, ACKed on the INVITE's own branch.
    let ack =
        b_leg.iter().find(|m| text(m).starts_with("ACK sip:")).expect("the 487 draws its hop ACK");
    let branch = |raw: &str| {
        raw.split("branch=").nth(1).and_then(|r| r.split(['\r', ';']).next()).map(str::to_string)
    };
    let first_invite = b_leg
        .iter()
        .find(|m| text(m).starts_with("INVITE sip:+1999999bob5070"))
        .expect("the abandoned INVITE is recorded");
    assert_eq!(
        branch(&text(ack)),
        branch(&text(first_invite)),
        "the ACK to a non-2xx rides its own INVITE's branch"
    );

    assert!(outcome.verdict.failures.is_empty(), "{:#?}", outcome.verdict.failures);
    assert_eq!(outcome.verdict.informative.len(), 3, "{:#?}", outcome.verdict.informative);
    assert!(
        outcome.verdict.informative.iter().all(|note| note.class == CheckClass::CdrVocabulary
            && matches!(&note.finding, Failure::CdrMismatch { .. })),
        "{:#?}",
        outcome.verdict.informative
    );
    scene.finish().await;
}

/// Rung three, the SUB-SECOND ring: a `no_answer_ms` this lane's whole-second
/// route decision cannot arm exactly, replayed under a stated timing tolerance
/// (§9.2).
///
/// A captured platform states its ring where its timer actually fired —
/// 15 139 ms here, not a round 15 000 — and no lane arms every
/// grain. The driver arms the NEAREST value it can, the run states the window it
/// will accept, and the difference is REPORTED rather than rounded away: the
/// verdict's timing note carries declared, observed and the window that covered
/// the gap, so a reader sees what the run absorbed.
///
/// The document is the no-answer draft with ONE number changed, built here
/// rather than authored beside it: a second twenty-step copy of one flow to vary
/// one field is a document nobody would maintain. Its own `case.id` keeps its
/// bundle its own.
///
/// It skips loudly when no case library is configured.
#[tokio::test(start_paused = true)]
async fn a_ring_this_lane_cannot_arm_exactly_rides_the_run_s_stated_tolerance() {
    /// What the capture measured, and what no whole-second lane arms.
    const RING_MS: u64 = 15_139;
    /// What this lane arms instead: the nearest second.
    const ARMED_MS: u64 = 15_000;

    let Some(mut case) = case_dir("reroute-on-no-answer") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    case.document.case.id = format!("{}-subsecond", case.document.case.id);
    for call in &mut case.document.calls {
        for attempt in &mut call.attempts {
            if attempt.no_answer_ms.is_some() {
                attempt.no_answer_ms = Some(RING_MS);
                if let Some(final_) = &mut attempt.r#final {
                    final_.at_ms = RING_MS + 80;
                }
            }
        }
    }
    for node in &mut case.document.flow {
        if let pivot_schema::flow::FlowNode::Message(step) = node {
            if step.delay.timer_linked {
                step.delay.ms = RING_MS;
            }
        }
    }

    let scene = hunting_scene("pivot-reroute-subsecond").await;
    let bob = scene.bob.clone();
    let directives = LaneDirectives { timing_tolerance_ms: 200, ..Default::default() };
    let (outcome, dir) =
        replay_directed(&scene, case, BTreeMap::from([("uas2".to_string(), bob)]), directives)
            .await;
    assert_bundle_is_complete(&outcome, &dir);

    // The reading, in the verdict: what the document declares, what the system's
    // timer actually measured, and the window that covered the difference.
    assert_eq!(outcome.verdict.timings.len(), 1, "{:#?}", outcome.verdict.timings);
    let note = &outcome.verdict.timings[0];
    assert_eq!(note.step, "s6");
    assert_eq!(note.leg, "B");
    assert_eq!(note.declared_ms, RING_MS);
    assert_eq!(note.observed_ms, ARMED_MS);
    assert_eq!(note.delta_ms, -139, "the system rang 139 ms short of the document");
    assert_eq!(note.tolerance_ms, 200);

    // And the wire agrees: the ring really was the armed 15 s, not the declared
    // 15.139 s — the tolerance absorbs a difference, it does not invent one.
    let b_leg = &outcome.recording.legs()["B"];
    let at = |predicate: &dyn Fn(&str) -> bool| {
        b_leg.iter().find(|m| predicate(&text(m))).map(|m| m.at_us).expect("recorded")
    };
    let invited = at(&|raw: &str| raw.starts_with("INVITE sip:+1999999bob5070"));
    let cancelled = at(&|raw: &str| raw.starts_with("CANCEL "));
    assert_eq!(cancelled - invited, ARMED_MS * 1000);

    assert!(outcome.verdict.failures.is_empty(), "{:#?}", outcome.verdict.failures);
    scene.finish().await;
}

/// The lane's own arming rule (§9.2), on the grain it actually has: a whole
/// second. What it cannot arm exactly it arms nearest, and what the window does
/// not cover it REFUSES rather than rounding.
#[test]
fn a_ring_between_two_seconds_arms_at_the_nearest_one_inside_the_stated_window() {
    assert_eq!(no_answer_sec("called-0-0", 15_000, 0), 15, "an armable ring needs no window");
    assert_eq!(no_answer_sec("called-0-0", 15_139, 200), 15, "139 ms below the nearest second");
    assert_eq!(no_answer_sec("called-0-0", 14_800, 200), 15, "200 ms above it, at the edge");
}

#[test]
#[should_panic(expected = "this lane arms whole seconds")]
fn a_ring_the_window_does_not_cover_is_refused_rather_than_rounded() {
    no_answer_sec("called-0-0", 15_139, 100);
}
