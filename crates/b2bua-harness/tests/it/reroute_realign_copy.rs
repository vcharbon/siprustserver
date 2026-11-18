//! An established call rerouted on its release event re-INVITEs the caller
//! onto the replacement leg (the a-realign). A copy of the caller's 2xx to an
//! earlier INVITE on her dialog (RFC 3261 §13.3.1.4, its ACK in flight)
//! arriving while that re-INVITE is pending answers the earlier CSeq, not the
//! realign (§12.2.1.2): the reroute waits for her answer to the realign, and
//! the copy draws the ACK its 2xx drew (§13.2.2.4).

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallReleaseResponse, NewCallResponse, ReleaseOutcome,
    ScriptedDecisionEngine,
};
use b2bua_harness::limiter::doubles::admit_all;
use b2bua_harness::{settle_until, B2buaSut};
use call::ReleaseEventKind;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const BOB_REOFFER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";
const ALICE_REANSWER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";
const MEDIA_ANSWER: &str = "v=0\r\no=media 7 7 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const ALICE_REALIGN: &str = "v=0\r\no=alice 1 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
/// The route's cap: short enough that the realign leaves while the caller's
/// earlier 2xx may still be repeated (64·T1).
const CAP_SEC: i64 = 5;

/// A route toward bob under a `CAP_SEC` cap subscribed to its release, whose
/// release answers a reroute toward the media server.
fn rerouting() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.features.platform.max_duration_ms = CAP_SEC * 1000;
                r.callback_context = Some("release-ctx".into());
                r.subscriptions = vec![ReleaseEventKind::MaxCallDuration];
                NewCallResponse::Route(r)
            })
            .on_release(|_| {
                ReleaseOutcome::Respond(CallReleaseResponse::Route(route_to("127.0.0.1", 5090)))
            })
            .build(),
    )
}

/// Bob's re-INVITE, relayed to alice, draws her 200 before the cap; the
/// reroute's realign re-INVITE then reaches her and she re-sends that 200.
/// Nothing reaches bob or the media server on the copy, the copy draws the
/// ACK of that 200, and the reroute completes on her answer to the realign.
#[tokio::test(start_paused = true)]
async fn a_copy_of_an_earlier_2xx_does_not_answer_the_reroute_realign() {
    let h = Harness::new("reroute-realign-earlier-2xx-copy");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let media = h.agent("media", "127.0.0.1:5090").await;
    let b2bua = B2buaSut::builder(rerouting())
        .limiter(admit_all())
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();
    let mut bob_reinvite = bob_dialog.request(InDialogMethod::Invite, Some(BOB_REOFFER)).await;
    let mut alice_earlier = alice.receive("INVITE").await;
    alice_earlier.respond(200, "OK").with_sdp(ALICE_REANSWER).await;
    bob_reinvite.expect(200).await;
    bob_dialog.ack(None).await;
    alice.receive("ACK").await;

    let acks_to_alice = |from: usize| -> Vec<Vec<u8>> {
        h.wire_entries()
            .into_iter()
            .skip(from)
            .filter(|e| e.from == b2bua.addr && e.to == alice.addr() && e.raw.starts_with(b"ACK "))
            .map(|e| e.raw)
            .collect()
    };
    let earlier_ack = acks_to_alice(0);
    assert_eq!(earlier_ack.len(), 1, "alice's 200 to bob's re-INVITE drew one ACK");

    let mut media_uas = media.receive("INVITE").await;
    media_uas.respond(200, "OK").with_sdp(MEDIA_ANSWER).await;
    media.receive("ACK").await;
    let mut realign = alice.receive("INVITE").await;

    let before_copy = h.wire_entries().len();
    alice_earlier.respond(200, "OK").with_sdp(ALICE_REANSWER).await;
    let mut heard = 0;
    for _ in 0..4 {
        h.advance(Duration::from_millis(100)).await;
        heard += bob.drain().await + media.drain().await;
    }
    assert_eq!(
        heard, 0,
        "a copy of alice's earlier 2xx answers no realign re-INVITE: nothing reaches bob or \
         the media server before she answers it (got {heard} datagrams)"
    );
    assert_eq!(
        acks_to_alice(before_copy),
        earlier_ack,
        "the copy draws the ACK that 2xx drew (RFC 3261 §13.2.2.4)"
    );
    alice.drain().await;

    realign.respond(200, "OK").with_sdp(ALICE_REALIGN).await;
    alice.receive("ACK").await;
    bob.receive("BYE").await.respond(200, "OK").await;

    let mut bye = alice_dialog.bye().await;
    media.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let _ = h.finish().await;
}
