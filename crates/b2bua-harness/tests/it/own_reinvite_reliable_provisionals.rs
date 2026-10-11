//! **RFC 3262 §4 — every reliable provisional answering a re-INVITE this
//! stack originated is PRACKed by this stack, in order.** No party is shown a
//! provisional to this stack's own re-INVITE, so the PRACK it is owed is this
//! stack's, one per `RSeq` — whatever this stack sent on the dialog since the
//! re-INVITE left, its own first PRACK included.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallReleaseResponse, NewCallResponse, ReleaseOutcome, RouteDecision, ScriptedDecisionEngine,
};
use b2bua_harness::{settle_until, stated, B2buaSut};
use call::ReleaseEventKind;
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

/// The decision declares `Supported: 100rel` toward the originator, so the
/// re-INVITE this stack originates toward her offers reliable provisionals.
fn offering_100rel_to_the_caller(r: &mut RouteDecision) {
    r.features.advertise_capabilities = Some(call::features::AdvertiseCapabilitiesFeature {
        toward_originator: Some(call::features::AdvertisedCapabilities {
            allow: None,
            supported: Some(vec!["100rel".to_string()]),
        }),
        toward_originated: None,
    });
}

/// A release reroute realigns the caller with a re-INVITE of this stack's
/// own. She answers it with a reliable 183 (`RSeq 1`), which this stack
/// PRACKs, then a reliable 180 (`RSeq 2`): the second is PRACKed too, on the
/// same INVITE, though the first PRACK moved the dialog's own sequence.
#[tokio::test(start_paused = true)]
async fn each_reliable_provisional_to_an_own_reinvite_is_pracked() {
    let h = Harness::with_transit_delay("b2bua-own-reinvite-reliable-provisionals", 0);
    let alice = h.agent("alice", "127.0.0.1:7560").await;
    let bob = h.agent("bob", "127.0.0.1:7561").await;
    let carol = h.agent("carol", "127.0.0.1:7562").await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 7561);
                r.features.platform.max_duration_ms = 60_000;
                r.callback_context = Some("ctx".into());
                r.subscriptions = vec![ReleaseEventKind::MaxCallDuration];
                offering_100rel_to_the_caller(&mut r);
                NewCallResponse::Route(r)
            })
            .on_release(|_| {
                let mut r = route_to("127.0.0.1", 7562);
                offering_100rel_to_the_caller(&mut r);
                ReleaseOutcome::Respond(CallReleaseResponse::Route(r))
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
        })
        .start(&h, "b2bua", "127.0.0.1:7563")
        .await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    // The max-duration release reroutes to carol; the realign re-INVITE
    // toward alice is this stack's own.
    h.advance(Duration::from_secs(61)).await;
    let mut carol_uas = carol.receive("INVITE").await;
    carol_uas.respond(200, "OK").with_sdp(ANSWER).await;
    let tag = carol_uas.dialog().local_tag().to_string();
    while let Some(mut again) = carol.try_receive_tolerating("INVITE", &[]).await {
        again.respond(200, "OK").with_sdp(ANSWER).with_to_tag(&tag).await;
    }
    carol.receive("ACK").await;
    let mut realign = alice.receive("INVITE").await;
    let realign_cseq = realign.request().cseq().seq();

    realign.respond(183, "Session Progress").reliable(1).await;
    let mut first = alice.receive("PRACK").await;
    assert_eq!(
        stated(first.request(), "RAck").as_deref(),
        Some(format!("1 {realign_cseq} INVITE").as_str()),
    );
    first.respond(200, "OK").await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    realign.respond(180, "Ringing").reliable(2).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut second = alice
        .try_receive_tolerating("PRACK", &["BYE"])
        .await
        .expect("the second in-order reliable provisional to the own re-INVITE is PRACKed");
    assert_eq!(
        stated(second.request(), "RAck").as_deref(),
        Some(format!("2 {realign_cseq} INVITE").as_str()),
    );
    second.respond(200, "OK").await;
    realign.respond(200, "OK").with_sdp(OFFER).await;
    alice.receive("ACK").await;
    bob.receive("BYE").await.respond(200, "OK").await;

    h.advance(Duration::from_secs(5)).await;
    let mut bye = alice_dialog.bye().await;
    carol.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
