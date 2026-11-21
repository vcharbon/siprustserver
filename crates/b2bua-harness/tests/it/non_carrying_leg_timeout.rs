//! A transaction timeout on a leg that does not carry the call. A service
//! dials a media leg of its own beside the established call (unadopted, not
//! the caller's peer) and sends it a request the media server never answers.
//! The timeout terminates that leg's dialog (RFC 3261 §12.2.1.2, §14.1): the
//! media leg is BYE'd and the call between the caller and the callee stays up
//! until the caller hangs up.

use std::sync::Arc;
use std::time::Duration;

use b2bua::config::B2buaConfig;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const MEDIA_ANSWER: &str = "v=0\r\no=media 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const ORDINAL: &str = "w0";
const MEDIA_ADDR: &str = "127.0.0.1:5090";
/// Past Timer F and Timer B (64·T1 = 32 s) of the service's request.
const PAST_THE_TIMEOUT: Duration = Duration::from_secs(33);
/// Past the bound an INVITE client transaction that drew a provisional gives
/// up at.
const PAST_THE_INVITE_BOUND: Duration =
    Duration::from_secs(B2buaConfig::DEFAULT_INVITE_TXN_TIMEOUT_SEC as u64 + 1);
/// An OPTIONS keepalive interval past the timeout window: no probe rides it.
const KEEPALIVE_PAST_THE_TIMEOUT_SEC: i64 = 300;

/// A service that, on the caller's MESSAGE, dials a media leg of its own
/// toward the media server and, once the media server answers, ACKs it and
/// sends it a request of `METHOD`'s.
mod parked {
    use b2bua::rules::{
        Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult, ServiceDef,
        ServiceSeed, SERVICE_LAYER,
    };
    use b2bua_sdk::model::Body;
    use call::{helpers::leg_kind, Direction, LegKind};

    fn media_leg(ctx: &RuleContext) -> Option<String> {
        let leg = ctx.call.b_legs().iter().find(|l| leg_kind(l) == LegKind::Media)?;
        Some(leg.leg_id.clone())
    }

    fn dial(_ctx: &RuleContext) -> Option<RuleHandleResult> {
        let (host, port) = super::MEDIA_ADDR.split_once(':')?;
        Some(RuleHandleResult::new(vec![
            RuleAction::Respond {
                status: 200,
                reason: "OK".into(),
                body: vec![],
                content_type: None,
            },
            RuleAction::CreateLeg {
                destination: (host.to_string(), port.parse().ok()?),
                new_ruri: Some(format!("sip:media@{}", super::MEDIA_ADDR)),
                new_from: None,
                new_to: None,
                callback_context: None,
                body_override: Some(Body::from_leg(super::OFFER.as_bytes().to_vec(), "a")),
                header_updates: vec![],
                header_adds: vec![],
                kind: Some(LegKind::Media),
                attempt: None,
            },
        ]))
    }

    fn on_answer(ctx: &RuleContext, method: &str) -> Option<RuleHandleResult> {
        let media = media_leg(ctx).filter(|m| m == ctx.source_leg_id)?;
        let body =
            (method == "INVITE").then(|| Body::from_leg(super::OFFER.as_bytes().to_vec(), "a"));
        Some(RuleHandleResult::new(vec![
            RuleAction::ConfirmDialog { leg_id: media.clone() },
            RuleAction::AckLeg { leg_id: media.clone(), body: None },
            RuleAction::SendRequestToLeg {
                leg_id: media,
                method: method.into(),
                body,
                headers: vec![],
            },
        ]))
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

    fn answered() -> Match {
        Match::response().method("INVITE").status_class(2).direction(Direction::FromB)
    }

    fn dialing() -> RuleDefinition {
        rule("parked-dial", Match::request().method("MESSAGE").direction(Direction::FromA), dial)
    }

    fn info_rules() -> Vec<RuleDefinition> {
        vec![dialing(), rule("parked-info", answered(), |ctx| on_answer(ctx, "INFO"))]
    }

    fn reinvite_rules() -> Vec<RuleDefinition> {
        vec![dialing(), rule("parked-reinvite", answered(), |ctx| on_answer(ctx, "INVITE"))]
    }

    fn init(_call: &RuleCall) -> Option<ServiceSeed> {
        None
    }

    /// Sends the media leg an INFO.
    pub fn info() -> ServiceDef {
        ServiceDef { id: "parked", init, rules: info_rules }
    }

    /// Sends the media leg a re-INVITE.
    pub fn reinvite() -> ServiceDef {
        ServiceDef { id: "parked", init, rules: reinvite_rules }
    }
}

fn decision() -> Arc<ScriptedDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| NewCallResponse::Route(route_to("127.0.0.1", 5070)))
            .build(),
    )
}

/// How the media server leaves the service's request.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Silence {
    /// No response at all: Timer F or Timer B ends the transaction.
    NoResponse,
    /// A 180, then no final: the INVITE bound ends the transaction.
    AfterRinging,
}

/// The caller and the callee are in a call; the service's media leg takes
/// `method` and leaves it as `silence` says. Past its timeout the media leg
/// alone is BYE'd; the caller's BYE then ends the call.
async fn the_media_leg_request_times_out(
    name: &str,
    service: b2bua::rules::ServiceDef,
    method: &str,
    silence: Silence,
) {
    let h = Harness::new(name);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let media = h.agent("media", MEDIA_ADDR).await;
    let b2bua = B2buaSut::builder(decision())
        .services(vec![service])
        .tune(|c| c.keepalive_interval_sec = KEEPALIVE_PAST_THE_TIMEOUT_SEC)
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
    let mut media_inv = media.receive("INVITE").await;
    media_inv.respond(200, "OK").with_sdp(MEDIA_ANSWER).await;
    media.receive("ACK").await;
    let mut unanswered = media.receive(method).await;
    let deadline = match silence {
        Silence::NoResponse => PAST_THE_TIMEOUT,
        Silence::AfterRinging => {
            unanswered.respond(180, "Ringing").await;
            PAST_THE_INVITE_BOUND
        }
    };

    h.advance(deadline).await;
    let _retransmissions = media.drain().await;
    media.receive("BYE").await.respond(200, "OK").await;
    if method == "INVITE" {
        unanswered.respond(487, "Request Terminated").await;
    }
    let live = b2bua.live_call(&call_ref).expect("the call is live");
    assert_eq!(
        live.state,
        call::CallModelState::Active,
        "a timeout on a leg that does not carry the call leaves the call up",
    );

    let mut alice_bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    h.finish().await;
}

/// A service's own INFO to its media leg draws no final: past Timer F the
/// media leg is BYE'd, the call stands.
#[tokio::test(start_paused = true)]
async fn an_own_info_timeout_on_a_media_leg_ends_that_leg_alone() {
    the_media_leg_request_times_out(
        "non-carrying-info-timeout",
        parked::info(),
        "INFO",
        Silence::NoResponse,
    )
    .await;
}

/// A service's own re-INVITE to its media leg draws no response: past Timer B
/// the media leg is BYE'd, the call stands.
#[tokio::test(start_paused = true)]
async fn an_own_reinvite_timeout_on_a_media_leg_ends_that_leg_alone() {
    the_media_leg_request_times_out(
        "non-carrying-reinvite-timeout",
        parked::reinvite(),
        "INVITE",
        Silence::NoResponse,
    )
    .await;
}

/// A service's re-INVITE to its media leg draws a 180 and no final: at the
/// INVITE bound, which no request record names, the media leg is BYE'd and the
/// call stands.
#[tokio::test(start_paused = true)]
async fn an_own_reinvite_given_up_after_ringing_on_a_media_leg_ends_that_leg_alone() {
    the_media_leg_request_times_out(
        "non-carrying-reinvite-bound",
        parked::reinvite(),
        "INVITE",
        Silence::AfterRinging,
    )
    .await;
}
