//! The timeout of a re-INVITE a service sent itself (RFC 3261 §17.1.1.2,
//! Timer B): the dialog's INVITE record settles before the rules read the
//! timeout, naming the rule that sent it, so a service rule can claim that
//! timeout and keep the call. The dialog then holds no INVITE in progress
//! (§14.1): a later re-INVITE leaves. Without a claiming rule, the core
//! default ends the session.

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
/// A keepalive interval past the Timer B window: no OPTIONS rides the test.
const KEEPALIVE_PAST_TIMER_B_SEC: i64 = 300;
/// Past Timer B (64·T1 = 32 s) of the service's re-INVITE.
const PAST_TIMER_B: Duration = Duration::from_secs(33);

/// A service that answers the caller's MESSAGE itself and re-INVITEs the
/// callee offering the caller's description; it ACKs the callee's 2xx. With
/// `keep`, a further rule claims that re-INVITE's timeout and keeps the call.
mod renegotiate {
    use b2bua::rules::{
        Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult, ServiceDef,
        ServiceSeed, SERVICE_LAYER,
    };
    use b2bua_sdk::model::Body;
    use call::Direction;

    pub const SEND: &str = "renegotiate-send";

    fn callee(ctx: &RuleContext) -> Option<String> {
        ctx.call.b_legs().first().map(|leg| leg.leg_id.clone())
    }

    fn send(ctx: &RuleContext) -> Option<RuleHandleResult> {
        let callee = callee(ctx)?;
        Some(RuleHandleResult::new(vec![
            RuleAction::Respond {
                status: 200,
                reason: "OK".into(),
                body: vec![],
                content_type: None,
            },
            RuleAction::SendRequestToLeg {
                leg_id: callee,
                method: "INVITE".into(),
                body: Some(Body::from_leg(super::OFFER.as_bytes().to_vec(), "a")),
                headers: vec![],
            },
        ]))
    }

    fn ack(ctx: &RuleContext) -> Option<RuleHandleResult> {
        let callee = callee(ctx)?;
        Some(RuleHandleResult::new(vec![RuleAction::AckLeg { leg_id: callee, body: None }]))
    }

    /// Whether the current 2xx answers the re-INVITE `renegotiate-send` sent,
    /// the INVITE the dialog last sent.
    fn answers_this_service(ctx: &RuleContext) -> bool {
        let sent = ctx.dialog().and_then(|d| d.sent_invite.as_ref());
        ctx.answers_last_sent_invite() && sent.is_some_and(|r| r.author.as_deref() == Some(SEND))
    }

    /// The timeout of the re-INVITE this service's `renegotiate-send` sent:
    /// the call stays.
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
        vec![
            rule(SEND, Match::request().method("MESSAGE").direction(Direction::FromA), send),
            rule(
                "renegotiate-ack",
                Match::response()
                    .method("INVITE")
                    .status_class(2)
                    .direction(Direction::FromB)
                    .filter(answers_this_service),
                ack,
            ),
        ]
    }

    fn keeping() -> Vec<RuleDefinition> {
        let mut rules = sending();
        rules.push(rule("renegotiate-keep", Match::timeout().filter(sent_by_this_service), keep));
        rules
    }

    fn init(_call: &RuleCall) -> Option<ServiceSeed> {
        None
    }

    pub fn sends_only() -> ServiceDef {
        ServiceDef { id: "renegotiate", init, rules: sending }
    }

    pub fn keeps_the_call() -> ServiceDef {
        ServiceDef { id: "renegotiate", init, rules: keeping }
    }
}

fn decision() -> Arc<ScriptedDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| NewCallResponse::Route(route_to("127.0.0.1", 5070)))
            .build(),
    )
}

/// The service's re-INVITE draws nothing from bob, whose silence is the
/// test's target; past its Timer B the call is up when the service keeps it,
/// and torn down by the default when it does not. A kept call's next re-INVITE
/// reaches bob, who still owes the first its final and so refuses the second
/// `500` with `Retry-After` (RFC 3261 §14.2); the call then ends by BYE.
async fn the_service_reinvite_times_out(name: &str, service: b2bua::rules::ServiceDef, kept: bool) {
    let h = Harness::new(name);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua = B2buaSut::builder(decision())
        .services(vec![service])
        .tune(|c| c.keepalive_interval_sec = KEEPALIVE_PAST_TIMER_B_SEC)
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
    let unanswered = bob.receive("INVITE").await;

    h.advance(PAST_TIMER_B).await;
    let _retransmissions = bob.drain().await;
    if kept {
        let live = b2bua.live_call(&call_ref).expect("the call is live");
        assert_eq!(live.state, call::CallModelState::Active, "the service keeps the call");
        let mut message = alice_dialog.send_request(InDialogMethod::Message).send().await;
        message.expect(200).await;
        let mut reinvite = bob.receive("INVITE").await;
        assert!(
            reinvite.request().cseq().seq() > unanswered.request().cseq().seq(),
            "a new INVITE transaction on the dialog"
        );
        reinvite.respond(500, "Server Internal Error").with_header("Retry-After", "5").await;
        bob.receive("ACK").await;
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

/// A service rule claims the timeout of the re-INVITE it sent and keeps the
/// call; the dialog takes the service's next re-INVITE.
#[tokio::test(start_paused = true)]
async fn a_service_keeps_the_call_when_its_own_reinvite_times_out() {
    the_service_reinvite_times_out(
        "own-reinvite-timeout-kept",
        renegotiate::keeps_the_call(),
        true,
    )
    .await;
}

/// With no service rule claiming it, the default ends the session on the
/// timeout of the service's own re-INVITE.
#[tokio::test(start_paused = true)]
async fn the_default_ends_the_session_when_an_own_reinvite_times_out() {
    the_service_reinvite_times_out(
        "own-reinvite-timeout-default",
        renegotiate::sends_only(),
        false,
    )
    .await;
}
