//! What the settle waits for on the scripted legs' own INVITE transactions:
//! the final an INVITE a leg sent is still owed, and the ACK the final it
//! receives is owed in turn (RFC 3261 §17.1.1).

use super::*;

use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, NewCallRequest, NewCallResponse, ScriptedDecisionEngine,
};

/// A caller whose script ends at the 100 still holds an INVITE client
/// transaction in Proceeding (RFC 3261 §17.1.1.2), so the settle waits for
/// its final.
///
/// The callee's `send 486` is the flow's last node. The system ACKs it on leg
/// B — the settle window's first arrival, the closer of a final a STEP composed
/// — and relays the 486 to leg A, which arrives after the flow completed (the
/// late arrival the verdict states) and is ACKed on the INVITE's own branch
/// (§17.1.1.3) before the run settles.
#[tokio::test(start_paused = true)]
async fn a_caller_whose_script_ends_at_the_100_waits_for_its_final_and_acks_it() {
    let scene = api_scene("pivot-invite-awaits-final").await;
    let mut case = fixture("unscripted-final.v3.json");
    case.document.case.id = "unscripted-final-cut-at-the-reject".into();
    drop_steps(&mut case, &["s5"]);
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    let legs = outcome.recording.legs();
    let (a, b) = (&legs["A"], &legs["B"]);
    // Leg B: the step's 486, then the system's ACK on its branch, noted as the
    // closer the settle awaited.
    let reject = b
        .iter()
        .find(|m| m.dir == Dir::Out && text(m).starts_with("SIP/2.0 486"))
        .unwrap_or_else(|| panic!("no 486 left leg B: {b:#?}"));
    assert_eq!(reject.step.as_deref(), Some("s4"));
    let b_ack = b
        .iter()
        .find(|m| m.dir == Dir::In && text(m).starts_with("ACK "))
        .unwrap_or_else(|| panic!("the system never ACKed leg B's 486: {b:#?}"));
    assert!(b_ack.note.as_deref().is_some_and(|n| n.contains("17.1.1.3")), "{b_ack:#?}");
    assert_eq!(via_branch(&text(b_ack)), via_branch(&text(reject)), "§17.1.1.3: same branch");

    // Leg A: the relayed 486 is on the recording, and leg A ACKed it on the
    // INVITE's own branch.
    let invite = out_on(a, "INVITE ");
    let final_at = a
        .iter()
        .position(|m| m.dir == Dir::In && text(m).starts_with("SIP/2.0 486"))
        .unwrap_or_else(|| panic!("the settle closed before leg A's final: {a:#?}"));
    let ack = out_on(a, "ACK ");
    assert!(ack.at_us >= a[final_at].at_us, "{a:#?}");
    assert!(text(&ack).contains("\r\nCSeq: 1 ACK\r\n"), "{}", text(&ack));
    assert_eq!(via_branch(&text(&ack)), via_branch(&text(&invite)), "§17.1.1.3: same branch");
    assert!(ack.step.is_none(), "no step scripts it: {ack:#?}");

    // The one finding is the late final itself; the run settled behind the ACK.
    assert_eq!(outcome.verdict.completed_steps.len(), 4, "{:?}", outcome.verdict.completed_steps);
    let late: Vec<(&str, &Arrived)> = outcome.verdict.failures.iter().map(arrival_of).collect();
    assert!(
        matches!(late.as_slice(), [("A", Arrived::Response { status: 486, .. })]),
        "{:#?}",
        outcome.verdict.failures
    );
    let settled = outcome.timing.settled_at_ms.expect("the run settled");
    assert!(settled >= ack.at_us / 1000, "settled at {settled} ms, before the ACK");
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// A document that ends at the ring leaves the caller's INVITE in Proceeding
/// and the callee's ringing. The system's no-answer timer then ends the
/// attempt inside the settle window: it CANCELs leg B, which answers 200 and
/// 487 and takes the system's ACK, and consults its routing backend, whose
/// answer puts a 503 on leg A. The callee's side emptying does not settle the
/// run while leg A's final is still owed: the final is recorded and ACKed on
/// the INVITE's own branch (RFC 3261 §17.1.1.3) before the run settles.
///
/// The scene depends on one ordering: the consult answers in the instant the
/// system ACKs leg B's 487, so the 503 and that ACK leave together and the
/// system holds no call once the ACK is read ([`FAILURE_CONSULT_MS`]).
#[tokio::test(start_paused = true)]
async fn a_ring_the_system_ends_after_the_flow_settles_on_the_caller_s_final() {
    let scene = B2buaScene::with_b2bua("pivot-ring-ended-in-settle", |bob_port| {
        let routing = Arc::new(ScriptedDecisionEngine::route_by_api_call("127.0.0.1", bob_port));
        B2buaSut::builder(Arc::new(SlowFailureConsult { inner: routing }))
    })
    .await;
    let mut case = fixture("unscripted-final.v3.json");
    case.document.case.id = "ring-ended-in-settle".into();
    let ring = step_of(&mut case, "s4");
    ring.msg.status = Some(180);
    ring.msg.reason = Some("Ringing".into());
    let route = serde_json::json!({
        "routes": [{
            "destination": { "host": "127.0.0.1", "port": BOB_PORT },
            "new_ruri": format!("sip:bob{BOB_PORT}@127.0.0.1:{BOB_PORT}"),
            "no_answer_ms": 2000
        }],
        "on_exhausted": { "action": "reject", "code": 503, "reason": "Service Unavailable" }
    });
    let invite = step_of(&mut case, "s1");
    let header = invite
        .msg
        .headers
        .iter_mut()
        .find(|h| h.name == "X-Api-Call")
        .expect("the fixture states its destination");
    header.value = route.to_string();
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    let legs = outcome.recording.legs();
    let (a, b) = (&legs["A"], &legs["B"]);
    // Leg B: the system's CANCEL, the leg's 200 and 487, the system's ACK.
    let cancel_at = b
        .iter()
        .position(|m| m.dir == Dir::In && text(m).starts_with("CANCEL "))
        .unwrap_or_else(|| panic!("the no-answer timer never CANCELled leg B: {b:#?}"));
    let terminated = b[cancel_at..]
        .iter()
        .find(|m| m.dir == Dir::Out && text(m).starts_with("SIP/2.0 487"))
        .unwrap_or_else(|| panic!("leg B never ended its INVITE: {b:#?}"));
    let b_ack = b[cancel_at..]
        .iter()
        .find(|m| m.dir == Dir::In && text(m).starts_with("ACK "))
        .unwrap_or_else(|| panic!("the system never ACKed leg B's 487: {b:#?}"));
    assert_eq!(via_branch(&text(b_ack)), via_branch(&text(terminated)));

    // Leg A: the system's final, and leg A's ACK on the INVITE's own branch.
    let invite = out_on(a, "INVITE ");
    let final_at = a
        .iter()
        .position(|m| {
            m.dir == Dir::In
                && text(m).starts_with("SIP/2.0 ")
                && status_of(&text(m)) >= 300
                && text(m).contains("\r\nCSeq: 1 INVITE\r\n")
        })
        .unwrap_or_else(|| panic!("the settle closed before leg A's final: {a:#?}"));
    let ack = out_on(a, "ACK ");
    assert!(ack.at_us >= a[final_at].at_us, "{a:#?}");
    assert_eq!(via_branch(&text(&ack)), via_branch(&text(&invite)), "§17.1.1.3: same branch");

    // The run states the teardown it never scripted, and settled behind the ACK.
    assert_eq!(outcome.verdict.completed_steps.len(), 5, "{:?}", outcome.verdict.completed_steps);
    assert!(
        matches!(outcome.verdict.failures.as_slice(),
            [Failure::DatagramAfterFlow { leg, arrived: Arrived::Request { method, .. } }]
                if leg == "B" && method == "CANCEL"),
        "{:#?}",
        outcome.verdict.failures
    );
    let settled = outcome.timing.settled_at_ms.expect("the run settled");
    assert!(settled >= ack.at_us / 1000, "settled at {settled} ms, before the ACK");
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// How long the routing backend takes to answer a failure consult: the delay
/// the demo system's ACK to a b-leg's non-2xx final lands behind that final.
const FAILURE_CONSULT_MS: u64 = 200;

/// A routing backend whose failure consult answers [`FAILURE_CONSULT_MS`]
/// late, everything else at once.
struct SlowFailureConsult {
    inner: Arc<ScriptedDecisionEngine>,
}

#[async_trait::async_trait]
impl CallDecisionEngine for SlowFailureConsult {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        self.inner.new_call(req).await
    }

    async fn call_failure(
        &self,
        req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        tokio::time::sleep(std::time::Duration::from_millis(FAILURE_CONSULT_MS)).await;
        self.inner.call_failure(req).await
    }

    async fn call_refer(
        &self,
        req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        self.inner.call_refer(req).await
    }
}

/// Remove message steps from a case, by id.
fn drop_steps(case: &mut Case, ids: &[&str]) {
    case.document.flow.retain(|node| {
        !matches!(node, pivot_schema::flow::FlowNode::Message(step) if ids.contains(&step.id.as_str()))
    });
}

/// The first datagram `ladder` sent whose start line begins with `prefix`.
fn out_on(ladder: &[RecordedMessage], prefix: &str) -> RecordedMessage {
    ladder
        .iter()
        .find(|m| m.dir == Dir::Out && text(m).starts_with(prefix))
        .unwrap_or_else(|| panic!("no outbound {prefix}: {ladder:#?}"))
        .clone()
}

/// A response's status code.
fn status_of(raw: &str) -> u16 {
    raw.split_whitespace().nth(1).and_then(|code| code.parse().ok()).unwrap_or(0)
}
