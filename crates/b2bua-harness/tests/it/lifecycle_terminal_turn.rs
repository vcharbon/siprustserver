//! The turn that ends a call still delivers the lifecycle events it caused,
//! and what a service writes on hearing them reaches the call's CDR: the
//! record is written from the call as the turn lands, lifecycle included.

use std::sync::Arc;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaSut};
use call::CallModelState;
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// A service that writes, under its call-ext slice, every leg it hears end.
mod listener {
    use b2bua::rules::{
        LifecycleEvent, Match, RuleAction, RuleCall, RuleContext, RuleHandleResult, ServiceSeed,
    };
    use b2bua::{define_service, sm_rule};

    pub const SLICE: &str = "leg-end-listener";

    define_service! {
        id: "leg-end-listener",
        machine: LISTENER,
        states: ListenerState { Listening },
        init: |_call: &RuleCall| Some(ServiceSeed::new(ListenerState::Listening.label())),
        rules: [ on_leg_ended() ],
    }

    fn on_leg_ended() -> b2bua::rules::RuleDefinition {
        sm_rule! {
            id: "leg-end-listener-heard",
            machine: LISTENER,
            active: [ ListenerState::Listening ],
            transitions: [],
            effects: [],
            matcher: Match::leg_ended(),
            handle: |ctx: &RuleContext| {
                let Some(LifecycleEvent::LegEnded { leg, .. }) = ctx.lifecycle() else {
                    return None;
                };
                let mut heard: Vec<serde_json::Value> = ctx
                    .call
                    .ext()
                    .and_then(|e| e.get(SLICE))
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                heard.push(leg.clone().into());
                Some(RuleHandleResult::new(vec![RuleAction::MergeCallExt {
                    ext: [(SLICE.to_string(), heard.into())].into_iter().collect(),
                }]))
            },
        }
    }
}

/// The callee refuses a call with no failover: the turn relays the refusal
/// and ends the call outright. The legs it ended reach the listener in that
/// turn, and its writes are on the call the CDR is written from.
#[tokio::test(start_paused = true)]
async fn a_lifecycle_write_on_the_call_s_last_turn_reaches_its_cdr() {
    let h = Harness::new("lifecycle-terminal-turn");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| NewCallResponse::Route(route_to("127.0.0.1", 5070)))
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .services(vec![listener::service_def()])
        .keep_terminated_calls()
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(486, "Busy Here").await;
    bob.receive("ACK").await;
    call.expect(486).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();

    let calls = b2bua.terminated_calls();
    assert_eq!(calls.len(), 1, "one CDR");
    assert_eq!(calls[0].state, CallModelState::Terminated);
    let heard = calls[0].ext.as_ref().and_then(|e| e.get(listener::SLICE)).cloned();
    assert_eq!(heard, Some(serde_json::json!(["a", "b-1"])), "the CDR's call carries the writes");
    let _ = h.finish().await;
}
