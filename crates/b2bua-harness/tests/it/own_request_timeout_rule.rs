//! The timeout of a request a service sent itself, read by the rules
//! (ADR-0055 §2): the request record names the rule that sent it, so a
//! service rule can claim that timeout and keep the call; without one, the
//! core default ends the session.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ORDINAL: &str = "w0";

/// A service that answers the caller's MESSAGE itself and sends the callee an
/// INFO of its own; with `keep`, a second rule claims that INFO's timeout and
/// keeps the call.
mod probe {
    use b2bua::rules::{
        Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult, ServiceDef,
        ServiceSeed, SERVICE_LAYER,
    };
    use call::Direction;

    pub const SEND: &str = "probe-send";

    fn send(ctx: &RuleContext) -> Option<RuleHandleResult> {
        let callee = ctx.call.b_legs().first()?.leg_id.clone();
        Some(RuleHandleResult::new(vec![
            RuleAction::Respond {
                status: 200,
                reason: "OK".into(),
                body: vec![],
                content_type: None,
            },
            RuleAction::SendRequestToLeg {
                leg_id: callee,
                method: "INFO".into(),
                body: None,
                headers: vec![],
            },
        ]))
    }

    /// The timeout of the INFO this service's `probe-send` sent: the call
    /// stays.
    fn keep(_ctx: &RuleContext) -> Option<RuleHandleResult> {
        Some(RuleHandleResult::new(vec![]))
    }

    fn sent_by_this_service(ctx: &RuleContext) -> bool {
        ctx.timed_out_record().is_some_and(|r| r.author.as_deref() == Some(SEND))
    }

    fn rule(
        id: &'static str,
        matcher: Match,
        handle: fn(&RuleContext) -> Option<RuleHandleResult>,
    ) -> RuleDefinition {
        RuleDefinition {
            id,
            layer: SERVICE_LAYER,
            overrides: &[],
            matcher,
            handle,
            machine: None,
            active_states: &[],
            transitions: &[],
            effects: &[],
            teardown: false,
        }
    }

    fn sending() -> Vec<RuleDefinition> {
        vec![rule(SEND, Match::request().method("MESSAGE").direction(Direction::FromA), send)]
    }

    fn keeping() -> Vec<RuleDefinition> {
        let mut rules = sending();
        rules.push(rule("probe-keep", Match::timeout().filter(sent_by_this_service), keep));
        rules
    }

    fn init(_call: &RuleCall) -> Option<ServiceSeed> {
        None
    }

    pub fn sends_only() -> ServiceDef {
        ServiceDef { id: "probe", init, rules: sending }
    }

    pub fn keeps_the_call() -> ServiceDef {
        ServiceDef { id: "probe", init, rules: keeping }
    }
}

fn decision() -> Arc<ScriptedDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| NewCallResponse::Route(route_to("127.0.0.1", 5070)))
            .build(),
    )
}

/// The service's INFO draws no final; past its Timer F the call is up when
/// the service keeps it, and torn down by the default when it does not.
async fn the_service_info_times_out(name: &str, service: b2bua::rules::ServiceDef, kept: bool) {
    let h = Harness::new(name);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua = B2buaSut::builder(decision())
        .services(vec![service])
        // No keepalive inside the Timer F window.
        .tune(|c| c.keepalive_interval_sec = 300)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let call_ref = call::derive_call_ref(ORDINAL, &call.call_id(), alice_dialog.local_tag());

    let mut message = alice_dialog.send_request(InDialogMethod::Message).send().await;
    message.expect(200).await;
    let _unanswered = bob.receive("INFO").await;

    // Past Timer F (64·T1 = 32 s) of the service's INFO.
    h.advance(Duration::from_secs(33)).await;
    bob.drain().await; // the INFO's retransmissions
    if kept {
        let live = b2bua.live_call(&call_ref).expect("the call is live");
        assert_eq!(live.state, call::CallModelState::Active, "the service keeps the call");
        let mut alice_bye = alice_dialog.bye().await;
        bob.receive("BYE").await.respond(200, "OK").await;
        alice_bye.expect(200).await;
    } else {
        alice.receive("BYE").await.respond(200, "OK").await;
        bob.receive("BYE").await.respond(200, "OK").await;
    }
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    h.finish().await;
}

/// A service rule claims the timeout of the INFO it sent and keeps the call.
#[tokio::test(start_paused = true)]
async fn a_service_keeps_the_call_when_its_own_info_times_out() {
    the_service_info_times_out("own-info-timeout-kept", probe::keeps_the_call(), true).await;
}

/// With no service rule claiming it, the default ends the session on the
/// timeout of the service's own INFO.
#[tokio::test(start_paused = true)]
async fn the_default_ends_the_session_when_an_own_info_times_out() {
    the_service_info_times_out("own-info-timeout-default", probe::sends_only(), false).await;
}
