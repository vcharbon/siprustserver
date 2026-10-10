//! The overall call ceiling under `MaxDurationAnchor::Answer`: the cap bounds
//! the ESTABLISHED call and runs from the answer the caller receives. A callee
//! that rings longer than the cap is not torn down at the cap (the setup is the
//! `SetupTimeout` deadline's to bound); the call is torn down `max_duration_sec`
//! after the answer. Under the default `Creation` anchor the same ring is reaped
//! at the cap (`setup_stall_global_duration_reap`). A call the cap's release
//! reroutes runs under the reroute's cap from the replacement leg's answer.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallReleaseResponse, NewCallResponse, ReleaseOutcome, ScriptedDecisionEngine,
};
use b2bua_harness::{settle_until, B2buaSut};
use call::features::MaxDurationAnchor;
use call::ReleaseEventKind;
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// A cap shorter than the ring that precedes the answer, so the two anchors are
/// told apart: from creation it expires while the callee still rings; from the
/// answer it expires `MAX_DURATION_SEC` into the established call.
const MAX_DURATION_SEC: i64 = 20;
const RING: Duration = Duration::from_secs(30);

#[tokio::test(start_paused = true)]
async fn the_cap_runs_from_the_answer_under_the_answer_anchor() {
    let h = Harness::new("b2bua-max-duration-anchor-answer");
    let alice = h.agent("alice", "127.0.0.1:5069").await;
    let bob = h.agent("bob", "127.0.0.1:5079").await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut route = route_to("127.0.0.1", 5079);
                route.features.platform.max_duration_sec = MAX_DURATION_SEC;
                route.features.platform.max_duration_anchor = MaxDurationAnchor::Answer;
                NewCallResponse::Route(route)
            })
            .build(),
    );
    // The default configuration bounds the setup (`setup_timeout_sec` 150 s), so
    // the cap is armed at the answer alone.
    let b2bua = B2buaSut::builder(decision).start(&h, "b2bua", "127.0.0.1:5089").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    // The callee rings past the cap: nothing is torn down.
    h.advance(RING).await;
    assert_eq!(bob.drain().await, 0, "no CANCEL reaches a callee ringing past the cap");
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _dialog = call.ack().await;
    bob.receive("ACK").await;

    // The cap runs from the answer: half of it later the call is still up ...
    h.advance(Duration::from_secs(MAX_DURATION_SEC as u64 / 2)).await;
    assert_eq!(bob.drain().await, 0, "the established call is not torn down before the cap");
    assert_eq!(alice.drain().await, 0, "the established call is not torn down before the cap");

    // ... and at the cap both legs are BYE'd.
    h.advance(Duration::from_secs(MAX_DURATION_SEC as u64 / 2 + 1)).await;
    alice.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}

/// The cap of an established call under the answer anchor raises its release,
/// answered by a reroute whose features restate the anchor and which
/// subscribes no release: the replacement leg's answer arms the reroute's
/// cap, which fires on the rerouted call and tears it down.
#[tokio::test(start_paused = true)]
async fn a_rerouted_call_runs_under_the_reroute_s_cap_from_its_answer() {
    const MEDIA_ANSWER: &str = "v=0\r\no=media 7 7 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
    let h = Harness::new("b2bua-max-duration-anchor-answer-reroute");
    let alice = h.agent("alice", "127.0.0.1:5069").await;
    let bob = h.agent("bob", "127.0.0.1:5079").await;
    let media = h.agent("media", "127.0.0.1:5099").await;
    let releases = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = releases.clone();
    let anchored = |mut route: b2bua::decision::RouteDecision| {
        route.features.platform.max_duration_sec = MAX_DURATION_SEC;
        route.features.platform.max_duration_anchor = MaxDurationAnchor::Answer;
        route
    };
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut route = anchored(route_to("127.0.0.1", 5079));
                route.callback_context = Some("release-ctx".into());
                route.subscriptions = vec![ReleaseEventKind::MaxCallDuration];
                NewCallResponse::Route(route)
            })
            .on_release(move |_| {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                ReleaseOutcome::Respond(CallReleaseResponse::Route(anchored(route_to(
                    "127.0.0.1",
                    5099,
                ))))
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", "127.0.0.1:5089")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _dialog = call.ack().await;
    bob.receive("ACK").await;

    // The cap raises the release; the reroute toward the media server lands.
    h.advance(Duration::from_secs(MAX_DURATION_SEC as u64 + 1)).await;
    let mut media_uas = media.receive("INVITE").await;
    media_uas.respond(200, "OK").with_sdp(MEDIA_ANSWER).await;
    media.receive("ACK").await;
    let mut realign = alice.receive("INVITE").await;
    realign.respond(200, "OK").with_sdp(OFFER).await;
    alice.receive("ACK").await;
    bob.receive("BYE").await.respond(200, "OK").await;

    // The reroute's cap runs from the replacement leg's answer.
    h.advance(Duration::from_secs(MAX_DURATION_SEC as u64 + 1)).await;
    alice.receive("BYE").await.respond(200, "OK").await;
    media.receive("BYE").await.respond(200, "OK").await;
    assert_eq!(releases.load(std::sync::atomic::Ordering::SeqCst), 1, "one release consult");
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}
