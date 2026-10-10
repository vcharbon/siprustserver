//! `background` policies (§5.1) beside an expect waiting for the same method.

use super::*;

/// **A relayed exchange of a policy-answered method** (§5.1): a `background`
/// policy answers traffic OUTSIDE the flow, so it must not absorb the arrival a
/// scripted expect is open on.
///
/// The caller polls its own dialog with an in-dialog OPTIONS and the system
/// carries it end to end — call behaviour a step owns — while the system's own
/// audits of the SAME method on the SAME leg are the policy's. Nothing on the
/// wire separates them: what does is the expect's window, which opens at its
/// anchor (`s101`, the caller's send) and is shut over every audit before it.
#[tokio::test(start_paused = true)]
async fn a_background_policy_does_not_absorb_the_arrival_an_open_expect_waits_for() {
    let scene = api_scene("pivot-relayed-options").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "relayed-options".into();
    let budget = case.document.timing.expect_budget_ms;
    // The poll sits three budgets into the talk phase, so the system's own
    // audits provably fire on both legs first.
    let mut send_options = step_of(&mut case, "s10").clone();
    send_options.id = "s101".into();
    send_options.msg.method = Some("OPTIONS".into());
    send_options.delay.ms = budget * 3;
    let mut expect_options = step_of(&mut case, "s11").clone();
    expect_options.id = "s102".into();
    expect_options.msg.method = Some("OPTIONS".into());
    expect_options.delay.from = "step:s101".parse().expect("s101 is a step id");
    let mut send_ok = step_of(&mut case, "s12").clone();
    send_ok.id = "s103".into();
    send_ok.msg.cseq_method = Some("OPTIONS".into());
    send_ok.delay.from = "step:s102".parse().expect("s102 is a step id");
    let mut expect_ok = step_of(&mut case, "s13").clone();
    expect_ok.id = "s104".into();
    expect_ok.msg.cseq_method = Some("OPTIONS".into());
    expect_ok.delay.from = "step:s103".parse().expect("s103 is a step id");
    // The teardown follows the poll it now sits behind.
    step_of(&mut case, "s10").delay.from = "step:s104".parse().expect("s104 is a step id");
    let at = case
        .document
        .flow
        .iter()
        .position(|node| matches!(node, pivot_schema::flow::FlowNode::Message(s) if s.id == "s10"))
        .expect("the teardown is a message node");
    for (offset, step) in [send_options, expect_options, send_ok, expect_ok].into_iter().enumerate()
    {
        case.document
            .flow
            .insert(at + offset, pivot_schema::flow::FlowNode::Message(Box::new(step)));
    }
    for actor in &mut case.document.actors {
        actor.background.push(BackgroundPolicy {
            r#match: BackgroundMatch { method: "OPTIONS".into() },
            respond: BackgroundResponse { status: 200 },
            count: Some(CountBound { at_least: Some(1), ..Default::default() }),
        });
    }
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.status, VerdictStatus::Ok, "{:#?}", outcome.verdict.failures);
    assert_eq!(outcome.verdict.completed_steps.len(), 17, "every step ran");
    // The two readings of one method really did both happen on leg B: the
    // relayed poll attributed to its step, the audits answered by the policy.
    let b_leg = &outcome.recording.legs()["B"];
    let options =
        |predicate: &dyn Fn(&pivot_schema::bundle::recording::RecordedMessage) -> bool| {
            b_leg.iter().filter(|m| text(m).starts_with("OPTIONS ") && predicate(m)).count()
        };
    assert_eq!(options(&|m| m.step.as_deref() == Some("s102")), 1, "the relay is the step's");
    assert!(
        options(&|m| m.note.as_deref().is_some_and(|n| n.starts_with("background"))) >= 1,
        "the audits are the policy's: {b_leg:#?}"
    );
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// **The same two readings, arriving in ONE instant** (§5.1): the system's own
/// audit and the relay of the caller's poll both land on the callee leg while
/// the scripted expect is open, and only the relay is the step's.
///
/// The window the sibling rung leans on is not enough here — both arrivals are
/// inside it — and arrival order is not evidence: the audit is a deployment
/// cadence that can fall anywhere, and here it falls first. What separates them
/// is the header the caller's poll carries and the audit does not, which the
/// expect freezes. Taking the audit costs the run twice over: the step answers
/// the wrong request, and the relayed answer then reaches the far leg before the
/// step gated on that answer is armed to receive it.
///
/// The poll is dwelled onto the audit cadence on purpose — just short of the
/// eighth 2 s interval — so the collision is the test and not an accident of
/// pacing.
#[tokio::test(start_paused = true)]
async fn an_open_expect_holds_out_for_the_relay_when_an_audit_lands_in_the_same_instant() {
    const POLL_MARK: &str = "P-Charging-Vector";
    let scene = api_scene("pivot-options-collision").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "options-collision".into();
    let mark = Header { name: POLL_MARK.into(), value: "icid-value=\"poll\"".into(), class: None };
    let mut send_options = step_of(&mut case, "s10").clone();
    send_options.id = "s101".into();
    send_options.msg.method = Some("OPTIONS".into());
    send_options.msg.headers = vec![mark.clone()];
    // Eight audit intervals into the talk phase, less the slack that puts the
    // poll's own send in front of the audit it collides with and the relay of
    // that poll behind it — the relay trails its send by the two transit hops it
    // crosses, so the audit has to fall inside those.
    send_options.delay.ms = 2000 * 8 - 300;
    let mut expect_options = step_of(&mut case, "s11").clone();
    expect_options.id = "s102".into();
    expect_options.msg.method = Some("OPTIONS".into());
    expect_options.msg.headers = vec![mark];
    expect_options.delay.from = "step:s101".parse().expect("s101 is a step id");
    let mut send_ok = step_of(&mut case, "s12").clone();
    send_ok.id = "s103".into();
    send_ok.msg.cseq_method = Some("OPTIONS".into());
    send_ok.delay.from = "step:s102".parse().expect("s102 is a step id");
    let mut expect_ok = step_of(&mut case, "s13").clone();
    expect_ok.id = "s104".into();
    expect_ok.msg.cseq_method = Some("OPTIONS".into());
    expect_ok.delay.from = "step:s103".parse().expect("s103 is a step id");
    step_of(&mut case, "s10").delay.from = "step:s104".parse().expect("s104 is a step id");
    let at = case
        .document
        .flow
        .iter()
        .position(|node| matches!(node, pivot_schema::flow::FlowNode::Message(s) if s.id == "s10"))
        .expect("the teardown is a message node");
    for (offset, step) in [send_options, expect_options, send_ok, expect_ok].into_iter().enumerate()
    {
        case.document
            .flow
            .insert(at + offset, pivot_schema::flow::FlowNode::Message(Box::new(step)));
    }
    for actor in &mut case.document.actors {
        actor.background.push(BackgroundPolicy {
            r#match: BackgroundMatch { method: "OPTIONS".into() },
            respond: BackgroundResponse { status: 200 },
            count: Some(CountBound { at_least: Some(1), ..Default::default() }),
        });
    }
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.status, VerdictStatus::Ok, "{:#?}", outcome.verdict.failures);
    assert_eq!(outcome.verdict.completed_steps.len(), 17, "every step ran");
    let b_leg = &outcome.recording.legs()["B"];
    let relay =
        b_leg.iter().find(|m| m.step.as_deref() == Some("s102")).expect("the relay is the step's");
    assert!(text(relay).contains(POLL_MARK), "the step took the poll, not an audit: {relay:#?}");
    // The collision is the test, not a bonus: an audit has to have landed on
    // this leg INSIDE the step's open window — after the poll that opened it,
    // before the relay it was competing with — and to have gone to the policy.
    let polled_at = outcome.recording.legs()["A"]
        .iter()
        .find(|m| m.step.as_deref() == Some("s101"))
        .expect("the caller polled")
        .at_us;
    let collided = b_leg.iter().any(|m| {
        text(m).starts_with("OPTIONS ")
            && !text(m).contains(POLL_MARK)
            && (polled_at..relay.at_us).contains(&m.at_us)
            && m.note.as_deref().is_some_and(|n| n.starts_with("background"))
    });
    assert!(
        collided,
        "an audit landed inside the open window, before the relay, and was the policy's: \
         polled {polled_at}, relayed {}, leg {b_leg:#?}",
        relay.at_us
    );
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}
