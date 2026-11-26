//! The timeout of a re-INVITE a service sent itself. At its Timer B, with no
//! response at all (RFC 3261 §17.1.1.2), the rules read the dialog's INVITE
//! record as the timed-out one, naming the rule that sent it, so a service
//! rule can claim that timeout and keep the call. The dialog then holds no
//! INVITE in progress (§14.1): a later re-INVITE leaves, and a late 2xx to the
//! timed-out one is owed nothing and draws no ACK (RFC 6026 §7.2). Without a
//! claiming rule, the core default ends the session. A re-INVITE that drew a
//! provisional is still in progress when it gives up at the INVITE bound: no
//! rule claims it.

use std::sync::Arc;
use std::time::Duration;

use b2bua::config::B2buaConfig;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Agent, Dialog, Harness, ServerTxn};
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
/// Alice's next description of her session (RFC 3264 §8).
const ALICE_2: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";
const ORDINAL: &str = "w0";
/// A keepalive interval past every test's end: no OPTIONS rides a test.
const KEEPALIVE_PAST_THE_TEST_SEC: i64 = 3_000;
/// Past Timer B (64·T1 = 32 s) of the service's re-INVITE.
const PAST_TIMER_B: Duration = Duration::from_secs(33);
/// Past a round trip on the harness's 100 ms transit: a message reaches the
/// SUT and its answer comes back.
const PAST_A_ROUND_TRIP: Duration = Duration::from_millis(250);
/// Past the bound an INVITE client transaction that drew a provisional gives
/// up at.
const PAST_THE_INVITE_BOUND: Duration =
    Duration::from_secs(B2buaConfig::DEFAULT_INVITE_TXN_TIMEOUT_SEC as u64 + 1);

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

/// An answered call whose service, on alice's MESSAGE, re-INVITEs bob.
struct Renegotiating {
    h: Harness,
    alice: Agent,
    bob: Agent,
    b2bua: B2buaSut,
    alice_dialog: Dialog,
    /// Bob's server transaction of the service's re-INVITE.
    reinvite: ServerTxn,
    call_ref: String,
}

impl Renegotiating {
    /// The call set up through `service`, the re-INVITE received by bob.
    async fn start(name: &str, service: b2bua::rules::ServiceDef) -> Self {
        let h = Harness::new(name);
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;
        let b2bua = B2buaSut::builder(decision())
            .services(vec![service])
            .tune(|c| c.keepalive_interval_sec = KEEPALIVE_PAST_THE_TEST_SEC)
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
        let reinvite = bob.receive("INVITE").await;
        Renegotiating { h, alice, bob, b2bua, alice_dialog, reinvite, call_ref }
    }

    /// The call is up and Active.
    fn assert_kept(&self) {
        let live = self.b2bua.live_call(&self.call_ref).expect("the call is live");
        assert_eq!(live.state, call::CallModelState::Active, "the service keeps the call");
    }

    /// Alice hangs up; bob answers the BYE.
    async fn alice_hangs_up(&mut self) {
        let mut alice_bye = self.alice_dialog.bye().await;
        self.bob.receive("BYE").await.respond(200, "OK").await;
        alice_bye.expect(200).await;
    }

    /// Bob, his dialog ended by BYE, answers the re-INVITE still pending at
    /// him `487` (RFC 3261 §15.1.2), which the transaction ACKs.
    async fn bob_ends_the_pending_reinvite(&mut self) {
        self.reinvite.respond(487, "Request Terminated").await;
        self.bob.receive("ACK").await;
    }

    async fn finish(self) {
        settle_until(|| self.b2bua.is_reaped()).await;
        self.b2bua.assert_fully_reaped();
        self.h.finish().await;
    }
}

/// The service's re-INVITE draws nothing from bob, whose silence is the
/// test's target; past its Timer B the call is up when the service keeps it,
/// and torn down by the default when it does not. A kept call's next re-INVITE
/// reaches bob, who still owes the first its final and so refuses the second
/// `500` with `Retry-After` (RFC 3261 §14.2); the call then ends by BYE.
async fn the_service_reinvite_times_out(name: &str, service: b2bua::rules::ServiceDef, kept: bool) {
    let mut call = Renegotiating::start(name, service).await;
    call.h.advance(PAST_TIMER_B).await;
    let _retransmissions = call.bob.drain().await;
    if kept {
        call.assert_kept();
        let mut message = call.alice_dialog.send_request(InDialogMethod::Message).send().await;
        message.expect(200).await;
        let mut reinvite = call.bob.receive("INVITE").await;
        assert!(
            reinvite.request().cseq().seq() > call.reinvite.request().cseq().seq(),
            "a new INVITE transaction on the dialog"
        );
        reinvite.respond(500, "Server Internal Error").with_header("Retry-After", "5").await;
        call.bob.receive("ACK").await;
        call.alice_hangs_up().await;
    } else {
        call.alice.receive("BYE").await.respond(200, "OK").await;
        call.bob.receive("BYE").await.respond(200, "OK").await;
    }
    call.bob_ends_the_pending_reinvite().await;
    call.finish().await;
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

/// Bob answers the re-INVITE `200` past its Timer B, the call kept: the
/// transaction that gave up owes it nothing (RFC 6026 §7.2), so it draws no
/// ACK, the service's ACK rule never reads it, and the call stays up.
#[tokio::test(start_paused = true)]
async fn a_2xx_past_timer_b_to_a_kept_calls_reinvite_is_dropped() {
    let mut call =
        Renegotiating::start("own-reinvite-late-2xx", renegotiate::keeps_the_call()).await;
    call.h.advance(PAST_TIMER_B).await;
    let _retransmissions = call.bob.drain().await;
    call.assert_kept();
    call.reinvite.respond(200, "OK").with_sdp(ANSWER).await;
    call.h.advance(PAST_A_ROUND_TRIP).await;
    assert_eq!(call.bob.drain_tu().await, 0, "no ACK answers the late 2xx");
    assert_eq!(call.b2bua.txn_metrics().late_2xx_dropped(), 1, "the transaction absorbs it");
    call.assert_kept();
    call.alice_hangs_up().await;
    call.finish().await;
}

/// Bob rings the re-INVITE and answers nothing more: its client transaction
/// left Calling, so Timer B never ends it, and it gives up at the INVITE
/// bound still in progress at bob (§14.1). No rule claims that give-up: the
/// default ends the call, and bob answers the pending re-INVITE `487`
/// (§15.1.2).
#[tokio::test(start_paused = true)]
async fn a_reinvite_giving_up_after_a_provisional_ends_the_call() {
    let mut call =
        Renegotiating::start("own-reinvite-proceeding-give-up", renegotiate::keeps_the_call())
            .await;
    call.reinvite.respond(180, "Ringing").await;
    call.h.advance(PAST_THE_INVITE_BOUND).await;
    call.alice.receive("BYE").await.respond(200, "OK").await;
    call.bob.receive("BYE").await.respond(200, "OK").await;
    call.bob_ends_the_pending_reinvite().await;
    call.finish().await;
}

/// Past the Timer B of the service's re-INVITE, the call kept, alice
/// re-INVITEs: bob's dialog holds no INVITE of this stack in progress, so the
/// glare check lets the relay leave (RFC 3261 §14.2) and bob, still owing the
/// first its final, refuses it `500` with `Retry-After`, relayed to alice.
#[tokio::test(start_paused = true)]
async fn a_peer_reinvite_past_timer_b_is_relayed() {
    let mut call =
        Renegotiating::start("own-reinvite-timer-b-glare", renegotiate::keeps_the_call()).await;
    call.h.advance(PAST_TIMER_B).await;
    let _retransmissions = call.bob.drain().await;
    call.assert_kept();
    let mut alice_reinvite = call.alice_dialog.reinvite(Some(ALICE_2)).await;
    let mut relayed = call.bob.receive("INVITE").await;
    relayed.respond(500, "Server Internal Error").with_header("Retry-After", "5").await;
    call.bob.receive("ACK").await;
    alice_reinvite.expect(500).await;
    call.alice_hangs_up().await;
    call.bob_ends_the_pending_reinvite().await;
    call.finish().await;
}
