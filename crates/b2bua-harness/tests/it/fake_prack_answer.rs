//! fake-prack — the descriptions the B2BUA builds when the callee re-offers
//! inside the early dialog it acknowledges on the caller's behalf.
//!
//! The callee's UPDATE offer (RFC 3311 §5.1) is answered by the B2BUA as the
//! caller would (RFC 3264 §6): the caller's own description, one m-line per
//! offered stream, each live stream carrying the offerer's preferred format the
//! caller also offered plus a common `telephone-event`, the direction the two
//! ends allow, the origin the next version of the caller's session in that
//! dialog (§8). A final without a description then gives the caller the
//! callee's side of that exchange, answered the same way against her offer; a
//! final carrying its own description is relayed as it is. Once the B2BUA has
//! stated a version of the caller's session, the caller's later descriptions
//! are restated above it.

use b2bua_harness::{settle_until, B2buaSut};
use call::features::RelayFirst18xStrategy;
use scenario_harness::{Agent, ClientInvite, Harness, ServerTxn};
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8 18 101\r\na=rtpmap:8 PCMA/8000\r\na=rtpmap:18 G729/8000\r\na=rtpmap:101 telephone-event/8000\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\n";

async fn b2bua_fake_prack(h: &Harness, name: &str, addr: &str, dest_port: u16) -> B2buaSut {
    B2buaSut::route_all_to_with_18x("127.0.0.1", dest_port, RelayFirst18xStrategy::FakePrack)
        .start(h, name, addr)
        .await
}

/// Alice offers [`OFFER`]; bob answers it in a reliable 183 the B2BUA PRACKs
/// itself (alice sees a bare 180).
async fn ringing(alice: &Agent, bob: &Agent, b2bua: &B2buaSut) -> (ClientInvite, ServerTxn) {
    let mut call = alice
        .invite(bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", "1")
        .with_sdp(ANSWER)
        .await;
    call.expect(180).await;
    bob.receive("PRACK").await.respond(200, "OK").await;
    (call, uas)
}

/// Bob re-offers `offer` in his early dialog; the B2BUA's answer body.
async fn reoffer(bob_dialog: &mut scenario_harness::Dialog, offer: &str) -> String {
    let mut update = bob_dialog.request(InDialogMethod::Update, Some(offer)).await;
    let resp = update.expect(200).await;
    String::from_utf8(resp.body().to_vec()).expect("an SDP answer")
}

/// Alice re-offers `offer` on her confirmed dialog; bob answers `answer`. What
/// each end received: bob's offer, alice's answer.
async fn caller_reoffer(
    dialog: &mut scenario_harness::Dialog,
    bob: &Agent,
    offer: &str,
    answer: &str,
) -> (String, String) {
    let mut reinv = dialog.reinvite(Some(offer)).await;
    let cseq = dialog.local_cseq();
    let mut at_bob = bob.receive("INVITE").await;
    let seen_by_bob = String::from_utf8(at_bob.request().body().to_vec()).unwrap();
    at_bob.respond(200, "OK").with_sdp(answer).await;
    let ok = reinv.expect(200).await;
    let seen_by_alice = String::from_utf8(ok.body().to_vec()).unwrap();
    dialog.ack_for(cseq, None).await;
    bob.receive("ACK").await;
    (seen_by_bob, seen_by_alice)
}

async fn hang_up(dialog: &mut scenario_harness::Dialog, bob: &Agent, b2bua: &B2buaSut) {
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// The answer keeps ONE format, the first of the offer's the caller also
/// offered, plus the common `telephone-event`; the caller's port, connection
/// and other lines; the direction answering the offer's `sendonly`; the
/// caller's origin one version up. The caller's 200 answers her offer the same
/// way from the callee's re-offer, under the callee's origin. Her next
/// re-offer reaches the callee one version above the B2BUA's.
#[tokio::test]
async fn a_reoffer_is_answered_with_the_offerers_preferred_common_format() {
    let h = Harness::with_transit_delay("fake-prack-answer-preferred", 0);
    let alice = h.agent("alice", "127.0.0.1:5801").await;
    let bob = h.agent("bob", "127.0.0.1:5802").await;
    let b2bua = b2bua_fake_prack(&h, "b2bua", "127.0.0.1:5803", 5802).await;
    let (mut call, mut uas) = ringing(&alice, &bob, &b2bua).await;

    let update = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0 18 8 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:18 G729/8000\r\na=rtpmap:8 PCMA/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendonly\r\n";
    assert_eq!(
        reoffer(&mut uas.dialog(), update).await,
        "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 18 101\r\na=rtpmap:18 G729/8000\r\na=rtpmap:101 telephone-event/8000\r\na=recvonly\r\n",
    );

    uas.respond(200, "OK").await;
    let ok = call.expect(200).await;
    assert_eq!(
        String::from_utf8_lossy(ok.body()),
        "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 18 101\r\na=rtpmap:18 G729/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendonly\r\n",
    );
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let reoffer = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 18\r\na=rtpmap:18 G729/8000\r\n";
    let reanswer = "v=0\r\no=bob 1 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 18\r\na=rtpmap:18 G729/8000\r\n";
    let (at_bob, at_alice) = caller_reoffer(&mut dialog, &bob, reoffer, reanswer).await;
    assert_eq!(at_bob, reoffer.replace("o=alice 1 2", "o=alice 1 3"), "one above the B2BUA's");
    assert_eq!(at_alice, reanswer, "the callee's own session crosses as written");

    hang_up(&mut dialog, &bob, &b2bua).await;
    let _ = h.finish().await;
}

/// A second re-offer in the same early dialog is answered as the next version
/// again, and the caller's 200 is built from the latest one.
#[tokio::test]
async fn a_second_reoffer_is_answered_as_the_next_version() {
    let h = Harness::with_transit_delay("fake-prack-answer-second", 0);
    let alice = h.agent("alice", "127.0.0.1:5811").await;
    let bob = h.agent("bob", "127.0.0.1:5812").await;
    let b2bua = b2bua_fake_prack(&h, "b2bua", "127.0.0.1:5813", 5812).await;
    let (mut call, mut uas) = ringing(&alice, &bob, &b2bua).await;

    let mut bob_dialog = uas.dialog();
    let first = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\n";
    let answer = reoffer(&mut bob_dialog, first).await;
    assert!(answer.contains("o=alice 1 2 "), "{answer}");
    let second = "v=0\r\no=bob 1 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20004 RTP/AVP 18 101\r\na=rtpmap:18 G729/8000\r\na=rtpmap:101 telephone-event/8000\r\n";
    assert_eq!(
        reoffer(&mut bob_dialog, second).await,
        "v=0\r\no=alice 1 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 18 101\r\na=rtpmap:18 G729/8000\r\na=rtpmap:101 telephone-event/8000\r\n",
    );

    uas.respond(200, "OK").await;
    let ok = call.expect(200).await;
    assert_eq!(
        String::from_utf8_lossy(ok.body()),
        "v=0\r\no=bob 1 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20004 RTP/AVP 18 101\r\na=rtpmap:18 G729/8000\r\na=rtpmap:101 telephone-event/8000\r\n",
    );
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    hang_up(&mut dialog, &bob, &b2bua).await;
    let _ = h.finish().await;
}

/// No format in common rejects the stream (port 0, RFC 3264 §6) instead of
/// refusing the whole offer; the caller's 200 then rejects her stream too.
#[tokio::test]
async fn no_common_format_rejects_the_stream() {
    let h = Harness::with_transit_delay("fake-prack-answer-no-common", 0);
    let alice = h.agent("alice", "127.0.0.1:5821").await;
    let bob = h.agent("bob", "127.0.0.1:5822").await;
    let b2bua = b2bua_fake_prack(&h, "b2bua", "127.0.0.1:5823", 5822).await;
    let (mut call, mut uas) = ringing(&alice, &bob, &b2bua).await;

    let opus = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 96\r\na=rtpmap:96 opus/48000/2\r\n";
    assert_eq!(
        reoffer(&mut uas.dialog(), opus).await,
        "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 0 RTP/AVP 96\r\n",
    );

    uas.respond(200, "OK").await;
    let ok = call.expect(200).await;
    assert_eq!(
        String::from_utf8_lossy(ok.body()),
        "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 0 RTP/AVP 8 18 101\r\n",
    );
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    hang_up(&mut dialog, &bob, &b2bua).await;
    let _ = h.finish().await;
}

/// A stream the re-offer adds is answered rejected (§6: one m-line per offered
/// stream) and is not in the caller's 200 (her offer had one stream). Her next
/// re-offer keeps the rejected stream in place toward the callee (§8), and the
/// callee's answer comes back to her with her one stream.
#[tokio::test]
async fn a_stream_the_reoffer_adds_is_rejected() {
    let h = Harness::with_transit_delay("fake-prack-answer-extra-stream", 0);
    let alice = h.agent("alice", "127.0.0.1:5831").await;
    let bob = h.agent("bob", "127.0.0.1:5832").await;
    let b2bua = b2bua_fake_prack(&h, "b2bua", "127.0.0.1:5833", 5832).await;
    let (mut call, mut uas) = ringing(&alice, &bob, &b2bua).await;

    let video = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\nm=video 20002 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n";
    assert_eq!(
        reoffer(&mut uas.dialog(), video).await,
        "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\nm=video 0 RTP/AVP 96\r\n",
    );

    uas.respond(200, "OK").await;
    let ok = call.expect(200).await;
    assert_eq!(
        String::from_utf8_lossy(ok.body()),
        "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\n",
    );
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let reoffer = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\n";
    let reanswer = "v=0\r\no=bob 1 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\nm=video 0 RTP/AVP 96\r\n";
    let (at_bob, at_alice) = caller_reoffer(&mut dialog, &bob, reoffer, reanswer).await;
    assert_eq!(
        at_bob,
        "v=0\r\no=alice 1 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\nm=video 0 RTP/AVP 96\r\n",
    );
    assert_eq!(
        at_alice,
        "v=0\r\no=bob 1 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\n",
    );

    hang_up(&mut dialog, &bob, &b2bua).await;
    let _ = h.finish().await;
}

/// A final that carries its own description is relayed as it is: only a final
/// without one takes the description the B2BUA built.
#[tokio::test]
async fn a_final_with_its_own_description_is_relayed_as_written() {
    let h = Harness::with_transit_delay("fake-prack-answer-final-sdp", 0);
    let alice = h.agent("alice", "127.0.0.1:5841").await;
    let bob = h.agent("bob", "127.0.0.1:5842").await;
    let b2bua = b2bua_fake_prack(&h, "b2bua", "127.0.0.1:5843", 5842).await;
    let (mut call, mut uas) = ringing(&alice, &bob, &b2bua).await;

    let update = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8 9\r\na=rtpmap:8 PCMA/8000\r\na=rtpmap:9 G722/8000\r\n";
    reoffer(&mut uas.dialog(), update).await;

    // The 183's transport plan (RFC 3261 §13.2.1) under a new version of bob's
    // session (RFC 3264 §8), unlike what the B2BUA would build (`o=bob 1 2`,
    // no `ptime`).
    let own = "v=0\r\no=bob 1 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=ptime:20\r\n";
    uas.respond(200, "OK").with_sdp(own).await;
    let ok = call.expect(200).await;
    assert_eq!(String::from_utf8_lossy(ok.body()), own, "the callee's own final description");
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    hang_up(&mut dialog, &bob, &b2bua).await;
    let _ = h.finish().await;
}

/// Forking: the B2BUA answered a re-offer inside the first early dialog, the
/// second one answers. The caller's session in the answering dialog was never
/// stated by the B2BUA, so her re-offer crosses it as she wrote it.
#[tokio::test]
async fn a_stated_version_belongs_to_its_early_dialog() {
    let h = Harness::with_transit_delay("fake-prack-answer-forked", 0);
    let alice = h.agent("alice", "127.0.0.1:5851").await;
    let bob = h.agent("bob", "127.0.0.1:5852").await;
    let b2bua = b2bua_fake_prack(&h, "b2bua", "127.0.0.1:5853", 5852).await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond_early("f1", 183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", "1")
        .with_sdp(ANSWER)
        .await;
    call.expect(180).await;
    bob.receive("PRACK").await.respond(200, "OK").await;

    let fork1 = uas.early_tag("f1");
    uas.adopt_to_tag(&fork1);
    let update = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\n";
    let answer = reoffer(&mut uas.dialog(), update).await;
    assert!(answer.contains("o=alice 1 2 "), "{answer}");

    let b2 = "v=0\r\no=carol 7 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20010 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\n";
    uas.respond_early("f2", 183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", "7")
        .with_sdp(b2)
        .await;
    bob.receive("PRACK").await.respond(200, "OK").await;
    uas.win("f2");
    uas.respond(200, "OK").with_sdp(b2).await;
    let ok = call.expect(200).await;
    assert_eq!(String::from_utf8_lossy(ok.body()), b2);
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let reoffer = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\n";
    let reanswer = "v=0\r\no=carol 7 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20010 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\n";
    let (at_bob, at_alice) = caller_reoffer(&mut dialog, &bob, reoffer, reanswer).await;
    assert_eq!(at_bob, reoffer, "the answering dialog never saw a version of the B2BUA's");
    assert_eq!(at_alice, reanswer);

    hang_up(&mut dialog, &bob, &b2bua).await;
    let _ = h.finish().await;
}

/// The route replaced the offer the callee was sent: the answer to his
/// re-offer speaks for THAT offer (its origin, port and formats), and the
/// caller's 200 still answers her own.
#[tokio::test]
async fn the_answer_speaks_for_the_offer_the_callee_was_sent() {
    use b2bua::decision::test_adapter::route_to_with_18x;
    use b2bua::decision::{BodyUpdate, NewCallResponse, ScriptedDecisionEngine};
    use std::sync::Arc;

    const SENT: &str = "v=0\r\no=media 5 1 IN IP4 192.0.2.50\r\ns=-\r\nc=IN IP4 192.0.2.50\r\nt=0 0\r\nm=audio 11000 RTP/AVP 18 8\r\na=rtpmap:18 G729/8000\r\na=rtpmap:8 PCMA/8000\r\n";
    let h = Harness::with_transit_delay("fake-prack-answer-replaced-offer", 0);
    let alice = h.agent("alice", "127.0.0.1:5861").await;
    let bob = h.agent("bob", "127.0.0.1:5862").await;
    let b2bua = B2buaSut::builder(Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| {
                let mut r = route_to_with_18x("127.0.0.1", 5862, RelayFirst18xStrategy::FakePrack);
                r.update_body = BodyUpdate::Replace(SENT.into());
                NewCallResponse::Route(r)
            })
            .build(),
    ))
    .start(&h, "b2bua", "127.0.0.1:5863")
    .await;

    let (mut call, mut uas) = ringing(&alice, &bob, &b2bua).await;
    assert_eq!(
        String::from_utf8_lossy(uas.request().body()),
        SENT,
        "the callee got the route's offer"
    );
    let update = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\n";
    assert_eq!(
        reoffer(&mut uas.dialog(), update).await,
        "v=0\r\no=media 5 2 IN IP4 192.0.2.50\r\ns=-\r\nc=IN IP4 192.0.2.50\r\nt=0 0\r\nm=audio 11000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\n",
    );
    uas.respond(200, "OK").await;
    let ok = call.expect(200).await;
    assert_eq!(
        String::from_utf8_lossy(ok.body()),
        "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\n",
    );
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    hang_up(&mut dialog, &bob, &b2bua).await;
    let _ = h.finish().await;
}
