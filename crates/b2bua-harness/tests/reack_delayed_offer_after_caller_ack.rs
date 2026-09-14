//! RFC 3261 §13.2.2.4 — a copy of a **delayed-offer** 2xx that lands AFTER the
//! caller's ACK has been relayed re-sends THAT ACK, body and all.
//!
//! The b-leg ACK is the caller's, relayed end to end, and on a delayed offer it
//! is also the only carrier of the answer to the callee's offer (RFC 3261
//! §13.2.1, RFC 3264 §4). §13.2.2.4 says "the ACK MUST be passed to the client
//! transport every time a retransmission of the 2xx final response that
//! triggered the ACK arrives" — **the** ACK, the one that 2xx triggered. A
//! freshly composed one is not it: it reaches the callee answerless, so the
//! offer/answer exchange never completes and media is dead on exactly the calls
//! where the first ACK was lost.
//!
//! The pre-caller-ACK half of the same shape is gated by
//! `repeated_2xx_before_caller_ack.rs` (a copy inside the wait draws nothing,
//! since there is no ACK yet to re-send); the bare, offer-in-INVITE half by
//! `reack_retransmitted_2xx.rs`. This file is the after-the-relayed-ACK half.

use std::net::SocketAddr;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

const ALICE_ADDR: &str = "127.0.0.1:5991";
const BOB_ADDR: &str = "127.0.0.1:5992";

/// The defect gate: the callee repeats its offer-carrying 200 once the relayed
/// ACK is already on the wire, and the re-ACK must be that ACK repeated — the
/// same Via branch AND the same answer.
#[tokio::test(start_paused = true)]
async fn a_delayed_offer_2xx_repeated_after_the_relayed_ack_re_sends_that_ack() {
    let h = Harness::new("b2bua-reack-delayed-offer-post-ack");
    let alice = h.agent("alice", ALICE_ADDR).await;
    let bob = h.agent("bob", BOB_ADDR).await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5992).start(&h, "b2bua", "127.0.0.1:5993").await;

    // ── alice INVITEs bodyless: the offer is bob's to make ────────────────────
    let mut call = alice.invite(&bob).through(b2bua.addr).send().await;
    let invite_cseq = call.invite_cseq();
    let mut uas = bob.receive("INVITE").await;
    assert!(
        uas.request().body().is_empty(),
        "the offerless INVITE reached bob with a substituted body"
    );
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(OFFER).await;
    call.expect(200).await;

    // ── alice's ACK carries the answer and is relayed end to end ─────────────
    let mut dialog = call.ack_with(Some(ANSWER)).await;
    let first = bob.receive("ACK").await;
    assert!(
        !first.request().body().is_empty(),
        "the b-leg ACK carries alice's answer to bob's offer"
    );

    // ── bob never saw it: his §13.3.1.4 ladder repeats the 200 ────────────────
    uas.respond(200, "OK").with_sdp(OFFER).await;
    let re_ack = bob.receive("ACK").await;
    assert!(
        !re_ack.request().body().is_empty(),
        "RFC 3261 §13.2.2.4: the re-ACK is the ACK that 2xx triggered, so it still carries the answer",
    );

    // ── Teardown: clean BYE both ways; the confirmed call reaps (no leak) ─────
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.active_calls() == 0).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();

    let report = h.finish().await;

    // Two copies received, two ACKs sent, and the second is the first repeated:
    // byte-identical, so it is one datagram re-passed to the transport and not a
    // second composition that happens to share a branch.
    let acks = acks_to(&report, b2bua.addr, BOB_ADDR, invite_cseq);
    assert_eq!(acks.len(), 2, "one ACK per 2xx received once the ACK exists: got {}", acks.len());
    assert_eq!(
        String::from_utf8_lossy(&acks[1]),
        String::from_utf8_lossy(&acks[0]),
        "RFC 3261 §13.2.2.4: the re-ACK is the SAME ACK, answer included",
    );
}

const REOFFER: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=bob 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30001 RTP/AVP 0\r\n";

const REINVITE_ALICE_ADDR: &str = "127.0.0.1:5994";
const REINVITE_BOB_ADDR: &str = "127.0.0.1:5995";

/// The ACK a copy re-sends belongs to ONE INVITE transaction. A re-INVITE opens
/// the next one, and its 2xx carries the answer, so the ACK relayed for it is
/// bare — re-sending the initial delayed-offer ACK's answer here would replay a
/// spent negotiation onto a renegotiated session.
#[tokio::test(start_paused = true)]
async fn a_reinvite_2xx_copy_re_sends_its_own_bare_ack_not_the_initial_answer() {
    let h = Harness::new("b2bua-reack-delayed-offer-then-reinvite");
    let alice = h.agent("alice", REINVITE_ALICE_ADDR).await;
    let bob = h.agent("bob", REINVITE_BOB_ADDR).await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5995).start(&h, "b2bua", "127.0.0.1:5996").await;

    let mut call = alice.invite(&bob).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(OFFER).await;
    call.expect(200).await;
    let mut dialog = call.ack_with(Some(ANSWER)).await;
    bob.receive("ACK").await;

    // ── alice renegotiates, offering this time, and ACKs bare ────────────────
    let mut reinv = dialog.reinvite(Some(REOFFER)).await;
    let reinvite_cseq = dialog.local_cseq();
    let mut bob_reinv = bob.receive("INVITE").await;
    bob_reinv.respond(200, "OK").with_sdp(REANSWER).await;
    reinv.expect(200).await;
    dialog.ack_for(reinvite_cseq, None).await;
    bob.receive("ACK").await;

    // ── a copy of the re-INVITE's 200 ────────────────────────────────────────
    bob_reinv.respond(200, "OK").with_sdp(REANSWER).await;
    bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.active_calls() == 0).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();

    let report = h.finish().await;

    let reinvite_acks = acks_to(&report, b2bua.addr, REINVITE_BOB_ADDR, reinvite_cseq);
    assert_eq!(reinvite_acks.len(), 2, "one ACK per re-INVITE 2xx received (§13.2.2.4)");
    assert_eq!(
        String::from_utf8_lossy(&reinvite_acks[1]),
        String::from_utf8_lossy(&reinvite_acks[0]),
        "the re-ACK of a re-INVITE 2xx is that re-INVITE's own ACK repeated",
    );
    assert!(
        reinvite_acks.iter().all(|a| a.ends_with(b"Content-Length: 0\r\n\r\n")),
        "the relayed bare ACK stays bare: the initial transaction's answer is not replayed onto it",
    );
}

/// Every ACK the SUT sent to `peer` on one INVITE's CSeq, in wire order.
fn acks_to(
    report: &scenario_harness::RunReport,
    sut: SocketAddr,
    peer: &str,
    cseq: u32,
) -> Vec<Vec<u8>> {
    let peer: SocketAddr = peer.parse().unwrap();
    report
        .entries()
        .iter()
        .filter(|e| e.from == sut && e.to == peer && e.raw.starts_with(b"ACK "))
        .map(|e| e.raw.clone())
        .filter(|raw| sip_message::sniff::cseq_number(raw) == Some(cseq))
        .collect()
}
