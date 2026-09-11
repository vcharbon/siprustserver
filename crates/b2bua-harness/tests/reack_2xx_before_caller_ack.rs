//! RFC 3261 §13.2.2.4 — the UAC core owes **one ACK per 2xx received**, including
//! the 2xx copies that arrive before the caller's own ACK has reached the B2BUA.
//!
//! The B2BUA ACKs an offer-carrying 2xx on receipt, so the first copy draws its
//! ACK before the caller has said anything — and the callee's §13.3.1.4 ladder
//! can still repeat that 2xx if the ACK is lost. Each copy is a 2xx received by
//! the UAC core and MUST draw its own ACK; they are byte-identical, on the one
//! client transaction the first ACK minted (`ack_branch` + the INVITE CSeq).
//!
//! The post-caller-ACK half of §13.2.2.4 is gated by `reack_retransmitted_2xx.rs`;
//! this is the pre-caller-ACK half, and the retransmitted 2xx must NOT re-enter
//! answer processing (one Answer CDR event, one bridge).
//!
//! `a_reinvite_2xx_repeated_before_the_ack_draws_one_ack_per_copy` is the same
//! obligation on an in-dialog re-INVITE: the leg is Confirmed long before, so
//! nothing re-confirms it, and the ACK's client transaction is minted — and its
//! first ACK sent — where the re-INVITE's 2xx is taken (`relay_response`).
//!
//! The DELAYED-OFFER shape is the one exception, and
//! `a_delayed_offer_2xx_repeated_before_the_ack_draws_one_ack_in_total` pins it:
//! an ACK that owes the answer cannot be composed from the dialog, so nothing is
//! sent and no client transaction is minted when the 2xx is taken
//! (`acked_invite_carries_offer`) — a copy inside the wait has no ACK to
//! re-send. What separates the shapes is the BODY the ACK owes, never whether it
//! confirms the dialog.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const BOB_ADDR: &str = "127.0.0.1:5073";

#[tokio::test(start_paused = true)]
async fn every_2xx_received_before_the_callers_ack_draws_its_own_ack() {
    let h = Harness::new("b2bua-reack-2xx-pre-ack");
    let alice = h.agent("alice", "127.0.0.1:5063").await;
    let bob = h.agent("bob", BOB_ADDR).await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5073).start(&h, "b2bua", "127.0.0.1:5083").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;

    // ── bob's §13.3.1.4 ladder runs while the caller still holds its ACK ──────
    // Two more copies of the same 200. Each is a 2xx the UAC core received, so
    // each owes an ACK. Kept inside the a-leg `AckOf2xx` ladder's first
    // rung (T1 = 500 ms) so the SUT's own 2xx ladder is not part of what is
    // counted.
    for _ in 0..2 {
        uas.respond(200, "OK").with_sdp(ANSWER).await;
        for _ in 0..2 {
            h.advance(Duration::from_millis(100)).await;
            bob.drain().await;
        }
    }

    // The caller ACKs at last — hop-local, since bob's own ACK went out on
    // receipt. The third copy's ACK is the one still queued for him.
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    for _ in 0..4 {
        h.advance(Duration::from_millis(100)).await;
        bob.drain().await;
    }

    // ── Teardown: clean BYE both ways; the confirmed call reaps (no leak) ─────
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;

    // One 2xx received = one answer processed: the repeats must not re-enter the
    // answer path (no duplicate Answer event, no re-bridge).
    let records = b2bua.cdr_records();
    assert_eq!(records.len(), 1, "one call record");
    let answers = records[0]
        .events
        .iter()
        .filter(|e| format!("{:?}", e.event_type).contains("Answer"))
        .count();
    assert_eq!(answers, 1, "exactly one Answer CDR event for three copies of one 200");
    assert_eq!(records[0].b_legs.len(), 1, "no re-bridge: one b-leg");

    b2bua.assert_fully_reaped();

    let report = h.finish().await;

    // §13.2.2.4: three 2xx received, three ACKs sent — byte-identical, one
    // client transaction (a fresh branch would mint a new one and never quiesce
    // bob's INVITE server transaction).
    let bob_addr: SocketAddr = BOB_ADDR.parse().unwrap();
    let acks: Vec<Vec<u8>> = report
        .entries()
        .iter()
        .filter(|e| e.from == b2bua.addr && e.to == bob_addr && e.raw.starts_with(b"ACK "))
        .map(|e| e.raw.clone())
        .collect();
    assert_eq!(acks.len(), 3, "one ACK per 2xx received (RFC 3261 §13.2.2.4): got {}", acks.len(),);
    assert!(
        acks.iter().all(|a| a == &acks[0]),
        "the ACK re-sent for a retransmitted 2xx is the SAME ACK",
    );
}

const REOFFER: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=bob 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30001 RTP/AVP 0\r\n";

const BOB_REINVITE_ADDR: &str = "127.0.0.1:5077";

/// The **re-INVITE** twin: a renegotiation's 2xx repeated while the originator's
/// ACK is still owed. §13.2.2.4 counts copies, not dialogs, so each one draws its
/// own ACK on the one client transaction — and here no `ConfirmDialog` runs to
/// mint that transaction, since the leg was confirmed by the initial INVITE.
#[tokio::test(start_paused = true)]
async fn a_reinvite_2xx_repeated_before_the_ack_draws_one_ack_per_copy() {
    let h = Harness::new("b2bua-reack-reinvite-2xx-pre-ack");
    let alice = h.agent("alice", "127.0.0.1:5067").await;
    let bob = h.agent("bob", BOB_REINVITE_ADDR).await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5077).start(&h, "b2bua", "127.0.0.1:5087").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── alice renegotiates; bob answers and then ladders his un-ACKed 200 ─────
    let mut reinv = dialog.reinvite(Some(REOFFER)).await;
    let reinvite_cseq = dialog.local_cseq();
    let mut bob_reinv = bob.receive("INVITE").await;
    bob_reinv.respond(200, "OK").with_sdp(REANSWER).await;
    reinv.expect(200).await;
    bob_reinv.respond(200, "OK").with_sdp(REANSWER).await;
    h.advance(Duration::from_millis(100)).await;

    // ── alice ACKs once; bob is owed one ACK per copy ─────────────────────────
    dialog.ack_for(reinvite_cseq, None).await;
    bob.receive("ACK").await;
    bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.active_calls() == 0).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();

    let report = h.finish().await;

    // The two ACKs for the re-INVITE 2xx are ONE client transaction: a fresh
    // branch per copy would never quiesce bob's re-INVITE server txn.
    let bob_addr: SocketAddr = BOB_REINVITE_ADDR.parse().unwrap();
    let reinvite_acks: Vec<Vec<u8>> = report
        .entries()
        .iter()
        .filter(|e| e.from == b2bua.addr && e.to == bob_addr && e.raw.starts_with(b"ACK "))
        .map(|e| e.raw.clone())
        .filter(|raw| sip_message::sniff::cseq_number(raw) == Some(reinvite_cseq))
        .collect();
    assert_eq!(reinvite_acks.len(), 2, "one ACK per re-INVITE 2xx received (§13.2.2.4)");
    assert_eq!(
        reinvite_acks[0], reinvite_acks[1],
        "the re-ACK for a retransmitted re-INVITE 2xx is the SAME ACK",
    );
}

const BOB_DELAYED_OFFER_ADDR: &str = "127.0.0.1:5079";

/// The **delayed-offer** twin, and the one shape where the copies draw NOTHING:
/// alice's INVITE carries no offer, so bob's 2xx holds it and the answer rides
/// the ACK (RFC 3261 §13.2.1, RFC 3264 §4). That ACK cannot be composed from
/// the dialog alone, so no client transaction is minted when the 2xx is taken
/// (`acked_invite_carries_offer`) and a copy arriving while the answer is still
/// owed has no ACK to re-send. Alice's ACK supplies it, ONE goes out, and it is
/// what mints the branch every LATER copy re-sends.
#[tokio::test(start_paused = true)]
async fn a_delayed_offer_2xx_repeated_before_the_ack_draws_one_ack_in_total() {
    let h = Harness::new("b2bua-reack-delayed-offer-pre-ack");
    let alice = h.agent("alice", "127.0.0.1:5069").await;
    let bob = h.agent("bob", BOB_DELAYED_OFFER_ADDR).await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5079).start(&h, "b2bua", "127.0.0.1:5089").await;

    // ── alice INVITEs bodyless: the offer is bob's to make ────────────────────
    let mut call = alice.invite(&bob).through(b2bua.addr).send().await;
    let invite_cseq = call.invite_cseq();
    let mut uas = bob.receive("INVITE").await;
    assert!(
        uas.request().body().is_empty(),
        "the offerless INVITE reached bob with a substituted body",
    );
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(OFFER).await;
    call.expect(200).await;

    // ── bob's §13.3.1.4 ladder runs while alice still owes her answer ─────────
    uas.respond(200, "OK").with_sdp(OFFER).await;
    for _ in 0..2 {
        h.advance(Duration::from_millis(100)).await;
        bob.drain().await;
    }

    // ── alice ACKs once, carrying the answer; ONE ACK reaches bob ─────────────
    let mut dialog = call.ack_with(Some(ANSWER)).await;
    let first = bob.receive("ACK").await;
    assert!(
        !first.request().body().is_empty(),
        "the b-leg ACK must carry alice's answer to bob's offer",
    );

    // ── a copy landing once the ACK EXISTS draws its own, on that branch ──────
    uas.respond(200, "OK").with_sdp(OFFER).await;
    bob.receive("ACK").await;

    // ── Teardown: clean BYE both ways; the confirmed call reaps (no leak) ─────
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.active_calls() == 0).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();

    let report = h.finish().await;

    // Three 2xx received, TWO ACKs sent: the copy inside the wait drew none, the
    // one after it drew its own — byte-identical, one client transaction.
    let bob_addr: SocketAddr = BOB_DELAYED_OFFER_ADDR.parse().unwrap();
    let acks: Vec<Vec<u8>> = report
        .entries()
        .iter()
        .filter(|e| e.from == b2bua.addr && e.to == bob_addr && e.raw.starts_with(b"ACK "))
        .map(|e| e.raw.clone())
        .filter(|raw| sip_message::sniff::cseq_number(raw) == Some(invite_cseq))
        .collect();
    assert_eq!(
        acks.len(),
        2,
        "the held copy draws no ACK; the one after the ACK exists draws its own: got {}",
        acks.len(),
    );
    // One client transaction, one datagram: the re-ACK is alice's ACK repeated,
    // so it reuses her branch AND carries the answer she supplied — a copy that
    // drew a bare ACK would leave bob's offer unanswered
    // (`reack_delayed_offer_after_caller_ack.rs`).
    assert_eq!(
        String::from_utf8_lossy(&acks[1]),
        String::from_utf8_lossy(&acks[0]),
        "the re-ACK is the SAME ACK, answer included (RFC 3261 §13.2.2.4)",
    );
}
