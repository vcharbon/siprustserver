//! A route stating its ring deadline and its call's maximum duration at
//! `i64::MAX` ms: the call sets up, stays up past an hour, and ends by the
//! caller's BYE, every call deadline armed in the future and none wrapped
//! behind the clock.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The keepalive runs once a day, so no OPTIONS falls inside the hour the
/// call is held.
const KEEPALIVE_SEC: i64 = 86_400;
const HELD: Duration = Duration::from_secs(3_600);

#[tokio::test(start_paused = true)]
async fn a_route_stating_the_largest_timers_sets_up_and_ends_by_bye() {
    let h = Harness::new("b2bua-extreme-route-timers");
    let alice = h.agent("alice", "127.0.0.1:5069").await;
    let bob = h.agent("bob", "127.0.0.1:5079").await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut route = route_to("127.0.0.1", 5079);
                route.no_answer_ms = Some(i64::MAX);
                route.features.platform.max_duration_ms = i64::MAX;
                NewCallResponse::Route(route)
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .tune(|c| c.keepalive_interval_sec = KEEPALIVE_SEC)
        .start(&h, "b2bua", "127.0.0.1:5089")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    h.advance(HELD).await;
    assert_eq!(alice.drain().await, 0, "the call is up an hour after the answer");
    assert_eq!(bob.drain().await, 0, "the call is up an hour after the answer");

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}
