//! A 2xx that reaches the B2BUA after its call was released (RFC 6026 §7.2,
//! RFC 3261 §13.2.2.4): the INVITE client transaction it answers is held
//! Accepted to Timer M, so the 2xx still draws its ACK. A dialog the call
//! ACKed gets the call's own ACK again and nothing else; a dialog the call
//! never saw, opened by the initial INVITE, is ACKed and BYEd. A 2xx to an
//! INVITE this node never sent draws nothing.

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Agent, Harness};
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20001 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
const STRAGGLER_ANSWER: &str = "v=0\r\no=bob 2 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";

const WINNER: &str = "bobfork1";
const STRAGGLER: &str = "bobfork2";

/// Nothing more reaches `bob` within one T1 ladder's worth of time.
async fn bob_hears_nothing_more(h: &Harness, bob: &Agent) {
    for _ in 0..10 {
        h.advance(Duration::from_millis(100)).await;
    }
    assert!(bob.take_queued().await.is_none(), "nothing more reaches bob");
}

/// bob's ACK was lost, as far as bob knows: after alice hung up and the call
/// was reaped, bob repeats his 2xx (RFC 3261 §13.3.1.4). It draws the ACK the
/// call relayed, byte for byte, and no BYE: the call's own hangup ended that
/// dialog.
#[tokio::test(start_paused = true)]
async fn a_2xx_repeated_after_release_draws_the_calls_own_ack_and_no_bye() {
    let h = Harness::new("b2bua-2xx-after-release-acked-dialog");
    let alice = h.agent("alice", "127.0.0.1:6101").await;
    let bob = h.agent("bob", "127.0.0.1:6102").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 6102).start(&h, "b2bua", "127.0.0.1:6103").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    let relayed = bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let again = bob.try_receive("ACK").await.expect("the repeat after release is ACKed");
    assert_eq!(
        again.request().image(),
        relayed.request().image(),
        "with the ACK the call relayed, byte for byte"
    );
    bob_hears_nothing_more(&h, &bob).await;

    b2bua.assert_fully_reaped();
    assert_eq!(b2bua.cdr_records().len(), 1, "one CDR for the one call");
    alice.drain().await;
    let _report = h.finish().await;
}

/// A delayed offer: bob's 200 carries the offer, so its ACK owes the answer
/// alice's ACK would carry. alice hangs up on the 200 before she ACKs it (her
/// declared fault): the BYE relayed to bob leaves behind his ACK, answering
/// his offer with every stream rejected (RFC 3264 §6); he answers the BYE and
/// the call is reaped. His next repeat of the 2xx (RFC 3261 §13.3.1.4) draws
/// one ACK and no second BYE: the call already ended that dialog.
#[tokio::test(start_paused = true)]
async fn a_2xx_repeated_after_a_bye_that_beat_its_ack_draws_an_ack_and_no_bye() {
    let h = Harness::new("b2bua-2xx-after-release-byed-unacked");
    let alice = h.agent("alice", "127.0.0.1:6131").await;
    let bob = h.agent("bob", "127.0.0.1:6132").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 6132).start(&h, "b2bua", "127.0.0.1:6133").await;
    h.allow_violation(
        "no-ack-to-dialog-creating-2xx",
        "alice hangs up on the 200 without ACKing it: the peer fault under test",
    );

    let mut call = alice.invite(&bob).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(OFFER).await;
    call.expect(200).await;
    let mut bye = call.send_request(InDialogMethod::Bye).send().await;
    let ack = bob.receive("ACK").await;
    assert_eq!(ack.request().cseq().seq(), uas.request().cseq().seq());
    let answer = String::from_utf8_lossy(ack.request().body()).to_string();
    assert!(answer.contains("m=audio 0 "), "the ACK rejects every stream of the offer: {answer}");
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;

    uas.respond(200, "OK").with_sdp(OFFER).await;
    let ack = bob.try_receive("ACK").await.expect("the repeat after release is ACKed");
    assert_eq!(ack.request().cseq().seq(), uas.request().cseq().seq());
    bob_hears_nothing_more(&h, &bob).await;

    b2bua.assert_fully_reaped();
    assert_eq!(b2bua.cdr_records().len(), 1, "one CDR for the one call");
    alice.drain().await;
    let _report = h.finish().await;
}

/// A forked INVITE: the first fork answers and the call bridges, alice hangs
/// up and the call is reaped; only then does the second fork answer. Its 2xx
/// is ACKed and its dialog, which nothing holds, BYEd (RFC 6026 §7.2).
#[tokio::test(start_paused = true)]
async fn a_forked_2xx_after_release_is_acked_and_its_dialog_byed() {
    let h = Harness::new("b2bua-2xx-after-release-fork");
    let alice = h.agent("alice", "127.0.0.1:6111").await;
    let bob = h.agent("bob", "127.0.0.1:6112").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 6112).start(&h, "b2bua", "127.0.0.1:6113").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").with_to_tag(WINNER).await;
    call.expect(180).await;
    uas.respond(180, "Ringing").with_to_tag(STRAGGLER).await;
    call.expect(180).await;
    uas.adopt_to_tag(WINNER);
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;

    uas.respond(200, "OK").with_to_tag(STRAGGLER).with_sdp(STRAGGLER_ANSWER).await;
    let ack = bob.try_receive("ACK").await.expect("the straggler's 2xx is ACKed");
    assert_eq!(ack.request().to().tag(), Some(STRAGGLER), "the ACK names the straggler");
    let mut release = bob.try_receive("BYE").await.expect("the straggler's dialog is BYEd");
    assert_eq!(release.request().to().tag(), Some(STRAGGLER), "the BYE ends the straggler");
    release.respond(200, "OK").await;
    bob_hears_nothing_more(&h, &bob).await;

    b2bua.assert_fully_reaped();
    assert_eq!(b2bua.cdr_records().len(), 1, "one CDR for the one call");
    alice.drain().await;
    let _report = h.finish().await;
}

/// A 2xx on a branch this node never sent an INVITE on matches no
/// transaction and no call: nothing answers it.
#[tokio::test(start_paused = true)]
async fn a_2xx_to_an_invite_never_sent_draws_no_ack() {
    let h = Harness::new("b2bua-2xx-after-release-unsolicited");
    let alice = h.agent("alice", "127.0.0.1:6121").await;
    let bob = h.agent("bob", "127.0.0.1:6122").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 6122).start(&h, "b2bua", "127.0.0.1:6123").await;
    h.allow_violation(
        "unacked-2xx-not-cleared",
        "the peer's stray 2xx answers no INVITE the SUT sent, so the SUT rightly never ACKs it",
    );

    let stray = "SIP/2.0 200 OK\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:6123;branch=z9hG4bK-never-sent\r\n\
                 From: <sip:alice@127.0.0.1:6121>;tag=never\r\n\
                 To: <sip:bob@127.0.0.1:6122>;tag=stray\r\n\
                 Call-ID: never-sent@127.0.0.1\r\n\
                 CSeq: 1 INVITE\r\n\
                 Contact: <sip:bob@127.0.0.1:6122>\r\n\
                 Content-Length: 0\r\n\r\n";
    bob.try_send_datagram(stray.as_bytes(), b2bua.addr).await.expect("the stray 2xx leaves");
    bob_hears_nothing_more(&h, &bob).await;

    b2bua.assert_fully_reaped();
    assert!(b2bua.cdr_records().is_empty(), "no call, no CDR");
    alice.drain().await;
    let _report = h.finish().await;
}
