//! A turn that breaks the model lands all the same (ADR-0052 §8): a service
//! moving its cursor along an edge it never declared is a defect the oracle
//! keeps, logs and counts, and the call runs to its end on the wire. The run
//! fails from the record, which this test, provoking the defect on purpose,
//! takes and judges itself.

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// A service seeded at S0 that hears its attempt end and moves its cursor to
/// S9, though it declares S0 → S1 alone.
mod drifting {
    use b2bua::rules::{
        MachineId, Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult,
        ServiceDef, ServiceSeed, StateLabel, SERVICE_LAYER,
    };

    pub const ID: &str = "drifting";

    static ACTIVE_S0: [StateLabel; 1] = [StateLabel::new("S0")];
    static S0_TO_S1: [(StateLabel, StateLabel); 1] =
        [(StateLabel::new("S0"), StateLabel::new("S1"))];

    fn on_attempt_ended(_ctx: &RuleContext) -> Option<RuleHandleResult> {
        Some(RuleHandleResult::new(vec![RuleAction::SetState {
            machine: MachineId::new(ID),
            to: StateLabel::new("S9"),
        }]))
    }

    fn rules() -> Vec<RuleDefinition> {
        vec![RuleDefinition {
            id: "drifting-on-attempt-ended",
            layer: SERVICE_LAYER,
            overrides: &[],
            matcher: Match::attempt_ended(),
            handle: on_attempt_ended,
            machine: Some(MachineId::new(ID)),
            active_states: &ACTIVE_S0,
            transitions: &S0_TO_S1,
            effects: &[],
            teardown: false,
        }]
    }

    fn init(_call: &RuleCall) -> Option<ServiceSeed> {
        Some(ServiceSeed::new(StateLabel::new("S0")))
    }

    pub fn service_def() -> ServiceDef {
        ServiceDef { id: ID, init, rules }
    }
}

#[tokio::test(start_paused = true)]
async fn an_undeclared_transition_lands_and_fails_the_run_from_the_record() {
    let h = Harness::new("model-defect-lands");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let mut b2bua = B2buaSut::route_all_to("127.0.0.1", 5070)
        .services(vec![drifting::service_def()])
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    // The callee's answer ends the attempt: the service hears it and drifts.
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.cdr_records().len() == 1).await;
    settle_until(|| b2bua.is_reaped()).await;
    let violations = b2bua.take_model_violations();
    assert_eq!(violations.len(), 1, "the defect is kept once: {violations:?}");
    let first = violations.first().map(String::as_str).unwrap_or_default();
    assert!(
        first.starts_with(
            "model defect rule-declares-its-transitions: rule 'drifting-on-attempt-ended'"
        ),
        "{first}"
    );
    assert_eq!(b2bua.metrics().model_defects_total("rule-declares-its-transitions"), 1);
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}
