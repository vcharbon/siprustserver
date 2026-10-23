//! **RFC 3262 §3 — a PRACK names an unacknowledged provisional, or it draws
//! 481 from the UAS that sent the provisional.** On the face a reliable
//! provisional was shown, this stack is that UAS: the `RSeq` there is its own.
//! Once the party shown the provisional has acknowledged it, the provisional
//! is off the unacknowledged list, so a new PRACK naming it (a new
//! transaction, not a retransmission) draws 481 here and is never relayed.
//!
//! The answer is one fact of the call, read the same way while the call lives
//! and after it is released: a PRACK that acknowledged a provisional in-call
//! takes it off the list the released call keeps for late PRACKs too.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Harness, ServerTxn, WaiverScope};
use sip_message::generators::InDialogMethod;
use sip_net_testkit::RecordedSipEntry;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The callee's own `RSeq`, far from anything this stack mints.
const BOB_RSEQ: u32 = 4711;

fn reliable_180(uas: &mut ServerTxn) -> scenario_harness::Respond<'_> {
    uas.respond(180, "Ringing")
        .with_header("Require", "100rel")
        .with_header("RSeq", &BOB_RSEQ.to_string())
}

/// The PRACKs the SUT put on `to`'s wire, first copies only.
fn pracks_to(entries: &[RecordedSipEntry], from: SocketAddr, to: SocketAddr) -> usize {
    entries
        .iter()
        .filter(|e| e.from == from && e.to == to && e.raw.starts_with(b"PRACK "))
        .filter(|e| e.reemit.is_none())
        .count()
}

/// The caller PRACKs the reliable 180 she was shown and the callee answers the
/// relayed copy: acknowledged end to end. A new PRACK naming it again draws
/// 481 from this stack, and the callee receives no second PRACK.
#[tokio::test(start_paused = true)]
async fn a_new_prack_naming_an_acknowledged_provisional_draws_481_here() {
    let h = Harness::with_transit_delay("b2bua-prack-disposition-acknowledged", 0);
    let alice = h.agent("alice", "127.0.0.1:7501").await;
    let bob = h.agent("bob", "127.0.0.1:7502").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7502).start(&h, "b2bua", "127.0.0.1:7503").await;
    let bob_addr: SocketAddr = "127.0.0.1:7502".parse().unwrap();

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    reliable_180(&mut uas).await;
    let ringing = call.expect(180).await;

    let mut prack = call.try_prack(&ringing).await.expect("alice PRACKs the reliable 180");
    bob.receive("PRACK").await.respond(200, "OK").await;
    prack.expect(200).await;

    let mut again = call.try_prack(&ringing).await.expect("alice PRACKs it again");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        bob.try_receive_tolerating("PRACK", &[]).await.is_none(),
        "a new PRACK naming a provisional its party already acknowledged is not relayed",
    );
    again.expect(481).await;

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;
    assert_eq!(pracks_to(&report.entries(), b2bua.addr, bob_addr), 1, "one PRACK per RSeq");
}

/// The callee answers over a reliable ring the caller has not PRACKed yet, so
/// this stack PRACKs it; her PRACK, crossing the 2xx, is answered here. A new
/// PRACK naming the same provisional while the call lives draws 481.
#[tokio::test(start_paused = true)]
async fn a_second_caller_prack_after_one_answered_here_draws_481_in_call() {
    let h = Harness::with_transit_delay("b2bua-prack-disposition-in-call", 0);
    let alice = h.agent("alice", "127.0.0.1:7504").await;
    let bob = h.agent("bob", "127.0.0.1:7505").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7505).start(&h, "b2bua", "127.0.0.1:7506").await;
    let bob_addr: SocketAddr = "127.0.0.1:7505".parse().unwrap();

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    reliable_180(&mut uas).await;
    let ringing = call.expect(180).await;

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    bob.receive("PRACK").await.respond(200, "OK").await;
    call.expect(200).await;
    let mut first = call.try_prack(&ringing).await.expect("alice PRACKs the reliable 180");
    first.expect(200).await;
    let mut second = call.try_prack(&ringing).await.expect("alice PRACKs it again");
    second.expect(481).await;

    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;
    assert_eq!(pracks_to(&report.entries(), b2bua.addr, bob_addr), 1, "one PRACK per RSeq");
}

/// The callee rejects over a reliable ring, which this stack PRACKs; the call
/// is released. The caller's first late PRACK is answered 200 from the
/// released call's book; a new one naming the same provisional draws the 481
/// it draws in-call.
#[tokio::test(start_paused = true)]
async fn a_second_caller_prack_after_release_draws_the_481_it_draws_in_call() {
    let h = Harness::with_transit_delay("b2bua-prack-disposition-released", 0);
    let alice = h.agent("alice", "127.0.0.1:7507").await;
    let bob = h.agent("bob", "127.0.0.1:7508").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7508).start(&h, "b2bua", "127.0.0.1:7509").await;
    let bob_addr: SocketAddr = "127.0.0.1:7508".parse().unwrap();

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    reliable_180(&mut uas).await;
    let ringing = call.expect(180).await;

    uas.respond(486, "Busy Here").await;
    bob.receive("ACK").await;
    bob.receive("PRACK").await.respond(200, "OK").await;
    call.expect(486).await;
    h.advance(Duration::from_millis(70)).await;
    let mut first = call.try_prack(&ringing).await.expect("alice PRACKs the reliable 180");
    first.expect(200).await;
    let mut second = call.try_prack(&ringing).await.expect("alice PRACKs it again");
    second.expect(481).await;

    h.advance(Duration::from_secs(1)).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;
    assert_eq!(pracks_to(&report.entries(), b2bua.addr, bob_addr), 1, "one PRACK per RSeq");
}

/// Two PRACKs naming one shown provisional cross: the first is relayed, the
/// second (a new transaction) arrives before the callee answers the first.
/// The first PRACK took the provisional off the unacknowledged list as it
/// arrived (RFC 3262 §3), so the second draws 481 here at once, and the first
/// its relayed 200.
#[tokio::test(start_paused = true)]
async fn a_prack_crossing_the_relayed_one_draws_481_here() {
    let h = Harness::with_transit_delay("b2bua-prack-disposition-crossing", 0);
    let alice = h.agent("alice", "127.0.0.1:7564").await;
    let bob = h.agent("bob", "127.0.0.1:7565").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7565).start(&h, "b2bua", "127.0.0.1:7566").await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    reliable_180(&mut uas).await;
    let ringing = call.expect(180).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    let mut first = call.try_prack(&ringing).await.expect("alice PRACKs the reliable 180");
    let mut bob_prack = bob.receive("PRACK").await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    let mut second = call.try_prack(&ringing).await.expect("alice PRACKs it again");
    second.expect(481).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    bob_prack.respond(200, "OK").await;
    first.expect(200).await;

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// The callee's offer, in the reliable 183 to an offerless INVITE.
const BOB_OFFER: &str = "v=0\r\no=bob 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
/// The caller's answer to it.
const ALICE_ANSWER: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// An offerless INVITE; the callee's reliable 183 carries the offer, shown to
/// the caller, and the callee answers 200 before any PRACK: the offer waits
/// for the ACK. The caller's PRACK, carrying her answer, then arrives: the
/// callee is still owed a PRACK on a dialog that carries it, so hers is
/// relayed with her answer (RFC 3262 §5), and the ACK that follows is bare.
#[tokio::test(start_paused = true)]
async fn a_caller_prack_after_the_offer_was_deferred_is_relayed_with_her_answer() {
    let h = Harness::with_transit_delay("b2bua-prack-disposition-deferred", 0);
    h.waive(
        WaiverScope::rule(
            "delay-2xx-on-unacked-reliable-1xx-with-sdp",
            "bob deliberately answers 200 before his reliable offer is PRACKed",
        )
        .on_party("bob"),
    );
    h.waive(
        WaiverScope::rule(
            "delay-2xx-on-unacked-reliable-1xx-with-sdp",
            "the SUT relays bob's early 200 toward alice before she PRACKed the 183 it showed her",
        )
        .on_party("b2bua"),
    );
    let alice = h.agent("alice", "127.0.0.1:7567").await;
    let bob = h.agent("bob", "127.0.0.1:7568").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7568).start(&h, "b2bua", "127.0.0.1:7569").await;
    let mut call =
        alice.invite(&bob).with_header("Supported", "100rel").through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress").reliable(BOB_RSEQ).with_sdp(BOB_OFFER).await;
    let progress = call.expect(183).await;
    let rseq = b2bua_harness::stated_by_response(&progress, "RSeq").expect("a reliable 183");
    uas.respond(200, "OK").with_sdp(BOB_OFFER).await;
    call.expect(200).await;
    let mut prack = call
        .send_request(InDialogMethod::Prack)
        .with_rack(&format!("{rseq} {} INVITE", progress.cseq().seq()))
        .with_sdp(ALICE_ANSWER)
        .try_send()
        .await
        .expect("alice PRACKs with her answer");
    let mut bob_prack = bob.receive("PRACK").await;
    assert_eq!(
        String::from_utf8_lossy(bob_prack.request().body()),
        ALICE_ANSWER,
        "her answer reaches bob in the relayed PRACK"
    );
    bob_prack.respond(200, "OK").await;
    prack.expect(200).await;
    let mut dialog = call.ack().await;
    let ack = bob.receive("ACK").await;
    assert!(ack.request().body().is_empty(), "the exchange closed in the PRACK: a bare ACK");
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
