//! Expect budgets and dwells (§6.8): when a budget opens, and a dwell anchored on a block.

use super::*;

/// **A talk phase longer than the expect budget** (§6.8): an expect's budget
/// opens at the later of its own dwell (`anchor + delay.ms`) and its leg
/// reaching it — never across the dwell.
///
/// Leg B is at `s11` — the BYE it is to receive — the instant `s9` completes,
/// but that BYE is `s10`'s, dwelling six times the document's 5 s budget away.
/// Arming on the frontier alone would spend the whole budget waiting for a
/// message the flow has not yet been asked to provoke, so EVERY captured call
/// whose talk phase outlasts `expect_budget_ms` would fail on a faithful
/// document.
#[tokio::test(start_paused = true)]
async fn an_expect_gated_on_a_dwelling_send_does_not_spend_its_budget_waiting_for_it() {
    let scene = api_scene("pivot-long-talk-phase").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "long-talk-phase".into();
    let budget = case.document.timing.expect_budget_ms;
    step_of(&mut case, "s10").delay.ms = budget * 6;
    // A call held up this long is POLLED: the system keepalives both endpoints
    // while it is up, which every captured document of a long call carries as
    // background traffic (§5.1) rather than as flow.
    for actor in &mut case.document.actors {
        actor.background.push(BackgroundPolicy {
            r#match: BackgroundMatch { method: "OPTIONS".into() },
            respond: BackgroundResponse { status: 200 },
            count: Some(CountBound { at_least: Some(1), ..Default::default() }),
        });
    }
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 13, "every step ran");
    // And the dwell really was slept: the teardown sits a whole talk phase after
    // the answer, which is the fact the budget must not have been counting over.
    let a_leg = &outcome.recording.legs()["A"];
    let at = |predicate: &dyn Fn(&str) -> bool| {
        a_leg.iter().find(|m| predicate(&text(m))).map(|m| m.at_us).expect("recorded")
    };
    let acked = at(&|raw: &str| raw.starts_with("ACK "));
    let byed = at(&|raw: &str| raw.starts_with("BYE "));
    assert!(byed - acked >= budget * 6 * 1000, "the BYE dwelled: {acked} → {byed}");
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The same rule on the OTHER shape (§6.8): the gap is the expect's OWN, not a
/// cross-leg send's.
///
/// `s11` waits for the BYE and declares the whole talk phase as its own dwell,
/// which is what the generator writes for a message the system originates. A
/// budget opening at the anchor's completion rather than at `anchor + ms` would
/// still expire a whole talk phase before the BYE is due — the same defect
/// wearing a different anchor.
#[tokio::test(start_paused = true)]
async fn an_expect_declaring_its_own_gap_opens_its_budget_at_the_end_of_it() {
    let scene = api_scene("pivot-long-gap-same-leg").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "long-gap-same-leg".into();
    let budget = case.document.timing.expect_budget_ms;
    // The caller dwells the talk phase before its BYE; the callee's expect
    // states the SAME gap against its own leg's previous step.
    step_of(&mut case, "s10").delay.ms = budget * 6;
    let s11 = step_of(&mut case, "s11");
    s11.delay.from = "step:s9".parse().expect("s9 is a step id");
    s11.delay.ms = budget * 6;
    for actor in &mut case.document.actors {
        actor.background.push(BackgroundPolicy {
            r#match: BackgroundMatch { method: "OPTIONS".into() },
            respond: BackgroundResponse { status: 200 },
            count: Some(CountBound { at_least: Some(1), ..Default::default() }),
        });
    }
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 13, "every step ran");
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// A dwell anchored on a BLOCK (§6.8). Branch scoping refuses a reference into
/// an `alt` from outside, so the alt's own id is the ONLY legal way to anchor on
/// the branch that ran — and a block a run cannot time is a block nothing may be
/// anchored on.
///
/// The tolerated absence is what makes this observable: §6.5 releases it when
/// its own budget expires, and that is the only rule that keeps the steps behind
/// it from waiting forever. A block that never settles never opens the budget,
/// so the release never comes and the run dies `flow-incomplete` on a document
/// that should pass.
#[tokio::test(start_paused = true)]
async fn a_dwell_anchored_on_an_alt_settles_when_the_block_completes() {
    let scene = api_scene("pivot-alt-anchored-dwell").await;
    let mut case = fixture("alt-answered.v3.json");
    case.document.case.id = "alt-anchored-dwell".into();
    // A 481 to the BYE the callee already answered 200: nothing will ever match
    // it, so only the budget can retire it.
    let mut absent = case
        .document
        .flow
        .iter()
        .flat_map(pivot_schema::flow::FlowNode::steps)
        .find(|step| step.id == "s11")
        .expect("the answered branch ends on the 200 to the BYE")
        .clone();
    absent.id = "s99".into();
    absent.msg.status = Some(481);
    absent.msg.reason = None;
    absent.optional = true;
    absent.within_ms = Some(250);
    absent.delay.from = "step:a1".parse().expect("a1 is the alt's own id");
    absent.delay.ms = 0;
    case.document.flow.push(pivot_schema::flow::FlowNode::Message(Box::new(absent)));
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_bundle_is_complete(&outcome, &dir);
    assert!(
        outcome.verdict.released_optional.iter().any(|step| step == "s99"),
        "the tolerated absence was released by its budget: {:#?}",
        outcome.verdict.released_optional
    );
    scene.finish().await;
}
