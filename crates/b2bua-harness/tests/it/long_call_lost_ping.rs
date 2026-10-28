//! The `long_call` shape through the B2BUA when the relayed OPTIONS ping is
//! lost before the callee: the B2BUA's Timer E resends it (RFC 3261
//! §17.1.2.2), and the caller's hold starts once the ping is answered, so her
//! BYE reaches the callee after the ping and the callee takes the dialog's
//! requests in CSeq order (§12.2.1.1). The callee's dialog table gates the
//! run in `finish`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::{B2buaScene, B2buaSut};
use scenario_harness::actor::{
    ActorCall, ActorScenario, GoalStep, Observation, ObservedState, ResponseFact,
};
use scenario_harness::realcall::{run_actor_asserting, CallEnv};
use scenario_harness::Harness;
use sip_net::{Arrival, PreIngressAction};

#[tokio::test(start_paused = true)]
async fn a_lost_relayed_ping_is_answered_before_the_bye() {
    let h = Harness::new("long-call-lost-ping");
    let alice = h.agent_auto("alice").await;
    let copies = Arc::new(AtomicUsize::new(0));
    let counter = copies.clone();
    let bob_at = h.next_addr().to_string();
    let bob = h
        .agent_with_pre_ingress(
            "bob",
            &bob_at,
            Arc::new(move |arrival: &mut Arrival, _src, _depth| {
                if arrival.raw().starts_with(b"OPTIONS ")
                    && counter.fetch_add(1, Ordering::SeqCst) == 0
                {
                    return PreIngressAction::Drop;
                }
                PreIngressAction::Accept
            }),
        )
        .await;
    let at = h.next_addr().to_string();
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", bob.addr().port()).start(&h, "b2bua", &at).await;
    let scene = B2buaScene { h, alice, bob, b2bua };

    let mut env = CallEnv::for_functional(
        &scene.alice,
        &scene.bob,
        None,
        scene.b2bua.addr,
        "X-Loadgen-Id",
        "long-call-lost-ping-tok",
    );
    env.long_hold = Duration::from_millis(100);
    let shape = callshapes::shapes::long_call(callshapes::shapes::default_binder());
    assert_eq!(run_actor_asserting(&shape, &env).await, "answered");
    assert_eq!(copies.load(Ordering::SeqCst), 2, "the lost ping went again once");

    let report = scene.finish().await;
    assert!(report.passed(), "the RFC audit failed");
}

/// Whether the caller's BYE goal of `call` opens once both legs are confirmed
/// and her OPTIONS ping drew `status` (`None`: no answer yet).
fn bye_opens(call: &ActorCall, status: Option<u16>) -> bool {
    let obs = ObservedState::new();
    let now = tokio::time::Instant::now();
    obs.record(Observation::LegConfirmed { leg: "alice" }, now);
    obs.record(Observation::LegConfirmed { leg: "bob" }, now);
    if let Some(status) = status {
        let fact = ResponseFact {
            status,
            reason: String::new(),
            cseq_method: "OPTIONS".to_string(),
            body_len: 0,
            body_is_sdp: false,
            early_tag: None,
            typed: None,
        };
        obs.record(Observation::LegResponse { leg: "alice", fact }, now);
    }
    let alice = call.actors.iter().find(|a| a.role == "alice").expect("alice");
    let bye = alice.goals.iter().find(|g| matches!(g.step, GoalStep::Bye)).expect("her BYE");
    obs.with_snapshot(|s| bye.guard.holds(s))
}

/// Any final to the ping opens the shape's hold: a 405 leaves the dialog to
/// the BYE, where waiting for a 2xx would hold the call to the step timeout.
#[tokio::test(start_paused = true)]
async fn any_final_to_the_ping_starts_the_hold() {
    let h = Harness::new("long-call-ping-final");
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let env = CallEnv::for_functional(&alice, &bob, None, bob.addr(), "X-Loadgen-Id", "t");
    let shape = callshapes::shapes::long_call(callshapes::shapes::default_binder());
    let call = shape.build(&env).expect("built");
    assert!(!bye_opens(&call, None), "the hold waits for the ping's final");
    assert!(bye_opens(&call, Some(200)));
    assert!(bye_opens(&call, Some(405)), "a non-2xx final opens the hold too");
    h.finish().await;
}
