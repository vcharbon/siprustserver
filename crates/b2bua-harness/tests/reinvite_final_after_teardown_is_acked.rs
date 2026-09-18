//! RFC 3261 §17.1.1.3 — the INVITE client transaction owes an ACK to every
//! non-2xx final it draws, and that obligation lives in the transaction layer,
//! not in the dialog: it survives the dialog's teardown. The shape that exposes
//! it is a relayed re-INVITE still pending (the callee has sent only a 100)
//! when the call is released. The callee owes that INVITE a 487 — for the
//! CANCEL the B2BUA sends it (§9.2) and for the BYE it takes (§15.1.2) — and
//! sends it only after the BYE is answered, so the 487 reaches the B2BUA on a
//! call it has already reaped.
//!
//! The cost of getting it wrong is observable: §17.2.1 keeps the callee's
//! INVITE server transaction in *Completed*, repeating the 487 on Timer G,
//! until the ACK arrives or Timer H (64·T1, ~32 s) fires.
//!
//! The transaction outlives the call by exactly its own timers (RFC 3261
//! §17.1.1.2, RFC 6026 §7.2): Timer D after a non-2xx, Timer M after a 2xx,
//! Timer B with nothing ever taken — then it is purged with the rest.
//!
//! ```text
//!   the_487_to_a_relayed_reinvite_after_the_bye_is_acked
//!       alice re-INVITE → relayed to bob → bob 100 → alice BYE → the SUT
//!       CANCELs the relayed re-INVITE and relays the BYE → bob 200(CANCEL),
//!       200(BYE), the call is reaped → bob 487(re-INVITE) → the SUT's ACK on
//!       the 487's branch
//!   a_2xx_…                the 2xx crossing the CANCEL, ACKed on a fresh branch
//!   a_late_487_…_timer_d   each Timer G repeat re-draws the hop ACK; purged
//!                          at Timer D, nothing left
//!   a_late_2xx_…_timer_m   each repeat of the 2xx re-draws the same ACK;
//!                          purged at Timer M
//!   …_purged_at_timer_b    the re-INVITE draws nothing: Timer A repeats it to
//!                          Timer B, then it is purged, no ACK ever sent
//! ```

use std::net::SocketAddr;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Harness, RunReport};
use sip_txn::timers::{T1, TIMER_B, TIMER_D, TIMER_M};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";

/// alice re-INVITEs; the relayed re-INVITE sits at bob on a 100 when alice
/// hangs up. The CANCEL and the BYE both reach bob, he answers them 200, the
/// call is reaped, and only then does bob answer the pending re-INVITE 487.
/// The SUT's INVITE client transaction toward bob must still ACK that final on
/// its own branch (§17.1.1.3), whatever became of the call it was relayed for.
#[tokio::test(start_paused = true)]
async fn the_487_to_a_relayed_reinvite_after_the_bye_is_acked() {
    let h = Harness::new("b2bua-reinvite-final-after-teardown");
    let alice = h.agent("alice", "127.0.0.1:5067").await;
    let bob = h.agent("bob", "127.0.0.1:5077").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5077).start(&h, "b2bua", "127.0.0.1:5087").await;

    // ── call setup ───────────────────────────────────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    assert_eq!(b2bua.active_calls(), 1, "call established");

    // ── alice re-INVITEs; bob acknowledges the relayed copy with a 100 only ──
    let mut reinv = alice_dialog.reinvite(Some(REOFFER)).await;
    let mut bob_reinv = bob.receive("INVITE").await;
    assert_eq!(bob_reinv.request().body(), REOFFER.as_bytes(), "alice's re-offer reached bob");
    bob_reinv.respond(100, "Trying").await;

    // ── alice hangs up on the pending renegotiation. The SUT CANCELs the
    //    relayed re-INVITE (§9.1: it has a provisional) and relays the BYE. ─────
    let mut alice_bye = alice_dialog.bye().await;
    alice_bye.expect(200).await;
    // §15.1.2 on the a-leg: the SUT holds alice's INVITE pending when her BYE
    // lands, so it answers that INVITE 487 itself.
    reinv.expect(487).await;
    bob.receive("CANCEL").await.respond(200, "OK").await;
    let mut bob_bye = bob.receive("BYE").await;
    bob_bye.respond(200, "OK").await;
    settle_until(|| b2bua.active_calls() == 0).await;

    // ── the call is gone when bob's own 487 (§9.2, §15.1.2) arrives ──────────
    bob_reinv.respond(487, "Request Terminated").await;
    // §17.1.1.3: the client transaction, not the dialog, owes this ACK.
    bob_reinv.expect_ack().await;

    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

const REANSWER: &str = "v=0\r\no=bob 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30001 RTP/AVP 0\r\n";

/// The 2xx sibling: bob's 200 to the relayed re-INVITE crosses the SUT's
/// CANCEL on the wire and lands on a reaped call. §9.1 — a UAC whose CANCEL
/// is beaten by a 2xx still ACKs it — and §13.2.2.4 own that ACK whatever the
/// dialog's fate; without it bob ladders the 200 to 64·T1 (§13.3.1.4). The
/// round is closed (offer in the re-INVITE, answer in the 200), so the ACK is
/// bare and composable by the SUT alone.
///
/// The crossing is genuine: bob sends his first provisional only with his BYE
/// 200, so the CANCEL — held for that provisional (ADR-0028) — leaves the SUT
/// as the call is reaped and reaches bob one transit after he has already
/// answered 200 (§9.2 lets a UAS that has answered ignore the CANCEL's
/// intent and answer the CANCEL itself 200).
#[tokio::test(start_paused = true)]
async fn a_2xx_to_a_relayed_reinvite_after_the_bye_is_acked() {
    let h = Harness::new("b2bua-reinvite-2xx-after-teardown");
    let alice = h.agent("alice", "127.0.0.1:5068").await;
    let bob = h.agent("bob", "127.0.0.1:5078").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5078).start(&h, "b2bua", "127.0.0.1:5088").await;

    // ── call setup ───────────────────────────────────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    assert_eq!(b2bua.active_calls(), 1, "call established");

    // ── alice re-INVITEs; bob sits on the relayed copy without a provisional ─
    let mut reinv = alice_dialog.reinvite(Some(REOFFER)).await;
    let mut bob_reinv = bob.receive("INVITE").await;

    // ── alice hangs up; the SUT relays the BYE and holds the CANCEL for the
    //    relayed re-INVITE's first provisional (§9.1) ──────────────────────────
    let mut alice_bye = alice_dialog.bye().await;
    alice_bye.expect(200).await;
    reinv.expect(487).await;
    let mut bob_bye = bob.receive("BYE").await;
    // bob's 100 and his BYE 200 leave together: the 100 releases the CANCEL,
    // the 200 releases the call, both one transit later at the SUT.
    bob_reinv.respond(100, "Trying").await;
    bob_bye.respond(200, "OK").await;
    settle_until(|| b2bua.active_calls() == 0).await;

    // ── bob's 200 leaves before the CANCEL reaches him and lands on a reaped
    //    call: the SUT owes it an ACK all the same ────────────────────────────
    bob_reinv.respond(200, "OK").with_sdp(REANSWER).await;
    bob.receive("CANCEL").await.respond(200, "OK").await;
    let ack = bob.receive("ACK").await;
    assert_eq!(
        ack.request().cseq().seq(),
        bob_reinv.request().cseq().seq(),
        "§13.2.2.4: the ACK echoes the re-INVITE's CSeq",
    );
    assert!(ack.request().body().is_empty(), "the round is closed, so the ACK is bare");

    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// Every ACK the SUT put on `callee`'s socket, in wire order.
fn acks_to(report: &RunReport, sut: SocketAddr, callee: SocketAddr) -> Vec<Vec<u8>> {
    report
        .entries()
        .iter()
        .filter(|e| e.from == sut && e.to == callee && e.raw.starts_with(b"ACK "))
        .map(|e| e.raw.clone())
        .collect()
}

/// The 487 comes late and bob's Timer G repeats it: each repeat re-draws the
/// hop ACK (§17.1.1.2 Completed). The orphaned transaction is held for Timer
/// D from the first 487 — the earliest moment the SUT may forget it — and the
/// layer is empty afterwards: no transaction, no timer, no call.
#[tokio::test(start_paused = true)]
async fn a_late_487_is_re_acked_on_each_repeat_and_purged_at_timer_d() {
    let h = Harness::new("b2bua-reinvite-487-timer-d");
    let alice = h.agent("alice", "127.0.0.1:5069").await;
    let bob = h.agent("bob", "127.0.0.1:5079").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5079).start(&h, "b2bua", "127.0.0.1:5089").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut reinv = alice_dialog.reinvite(Some(REOFFER)).await;
    let mut bob_reinv = bob.receive("INVITE").await;
    bob_reinv.respond(100, "Trying").await;
    let mut alice_bye = alice_dialog.bye().await;
    alice_bye.expect(200).await;
    reinv.expect(487).await;
    bob.receive("CANCEL").await.respond(200, "OK").await;
    let mut bob_bye = bob.receive("BYE").await;
    bob_bye.respond(200, "OK").await;
    settle_until(|| b2bua.active_calls() == 0).await;
    assert_eq!(
        b2bua.txn_metrics().orphaned_transactions(),
        1,
        "the pending relayed re-INVITE is orphaned, not cut short"
    );

    // ── the 487 arrives well after the call is gone ──────────────────────────
    h.advance(Duration::from_millis(2_000)).await;
    bob_reinv.respond(487, "Request Terminated").await;
    bob_reinv.expect_ack().await;
    let first_ack_at = tokio::time::Instant::now();

    // ── bob's Timer G repeat (the first ACK "was lost") draws the hop ACK again
    h.advance(Duration::from_millis(T1)).await;
    bob_reinv.respond(487, "Request Terminated").await;
    bob_reinv.expect_ack().await;
    assert_eq!(
        b2bua.txn_metrics().orphaned_transactions(),
        1,
        "the orphaned INVITE client transaction is held in Completed for Timer D"
    );

    // ── Timer D, from the first 487: the orphan is purged, and with it the
    //    last transaction state the call left behind ─────────────────────────
    let elapsed = first_ack_at.elapsed();
    h.advance(Duration::from_millis(TIMER_D + 100) - elapsed).await;
    assert_eq!(b2bua.txn_metrics().orphaned_transactions(), 0, "the orphan is gone at Timer D");
    assert_eq!(b2bua.txn_metrics().active_transactions(), 0, "no transaction survives Timer D");
    assert_eq!(b2bua.txn_metrics().timer_queue_len(), 0, "no timer survives the purge");

    // ── a 487 after the purge matches nothing and draws nothing ──────────────
    bob.drain().await;
    bob_reinv.respond(487, "Request Terminated").await;
    h.advance(Duration::from_millis(500)).await;
    assert_eq!(bob.drain().await, 0, "a final after Timer D draws nothing at all");
    alice.drain().await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;
    let acks = acks_to(&report, b2bua.addr, bob.addr());
    assert_eq!(acks.len(), 3, "the dialog's ACK, then one hop ACK per 487 taken inside Timer D");
    assert_eq!(acks[1], acks[2], "§17.1.1.3: the same hop ACK, on the INVITE's branch, each time");
}

/// The crossing 2xx, repeated: bob re-sends his 200 (§13.3.1.4) and each
/// repeat re-draws the very same ACK (§13.2.2.4). The orphaned transaction is
/// held in Accepted for Timer M from the first 2xx (RFC 6026 §7.2) and is
/// purged with its ACK when it fires.
#[tokio::test(start_paused = true)]
async fn a_late_2xx_is_re_acked_on_each_repeat_and_purged_at_timer_m() {
    let h = Harness::new("b2bua-reinvite-2xx-timer-m");
    let alice = h.agent("alice", "127.0.0.1:5066").await;
    let bob = h.agent("bob", "127.0.0.1:5076").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5076).start(&h, "b2bua", "127.0.0.1:5086").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut reinv = alice_dialog.reinvite(Some(REOFFER)).await;
    let mut bob_reinv = bob.receive("INVITE").await;
    let mut alice_bye = alice_dialog.bye().await;
    alice_bye.expect(200).await;
    reinv.expect(487).await;
    let mut bob_bye = bob.receive("BYE").await;
    bob_reinv.respond(100, "Trying").await;
    bob_bye.respond(200, "OK").await;
    settle_until(|| b2bua.active_calls() == 0).await;

    // ── the 200 crosses the CANCEL and lands on the reaped call ──────────────
    bob_reinv.respond(200, "OK").with_sdp(REANSWER).await;
    bob.receive("CANCEL").await.respond(200, "OK").await;
    let ack = bob.receive("ACK").await;
    let first_ack_at = tokio::time::Instant::now();
    let reinvite_branch = bob_reinv.request().top_via().branch().unwrap().to_string();
    assert_ne!(
        ack.request().top_via().branch().unwrap(),
        reinvite_branch,
        "§17.1.1.3: a 2xx ACK is its own transaction, so it carries a fresh branch",
    );

    // ── bob repeats the 200: the same ACK, re-passed ─────────────────────────
    h.advance(Duration::from_millis(T1)).await;
    bob_reinv.respond(200, "OK").with_sdp(REANSWER).await;
    bob.receive("ACK").await;
    assert_eq!(
        b2bua.txn_metrics().orphaned_transactions(),
        1,
        "the orphaned INVITE client transaction is held in Accepted for Timer M"
    );

    // ── Timer M, from the first 2xx: purged ──────────────────────────────────
    let elapsed = first_ack_at.elapsed();
    h.advance(Duration::from_millis(TIMER_M + 100) - elapsed).await;
    assert_eq!(b2bua.txn_metrics().orphaned_transactions(), 0, "the orphan is gone at Timer M");
    assert_eq!(b2bua.txn_metrics().active_transactions(), 0, "no transaction survives Timer M");
    assert_eq!(b2bua.txn_metrics().timer_queue_len(), 0, "no timer survives the purge");
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;
    let acks = acks_to(&report, b2bua.addr, bob.addr());
    assert_eq!(acks.len(), 3, "the dialog's ACK, then one ACK per 2xx taken inside Timer M");
    assert_eq!(acks[1], acks[2], "§13.2.2.4: the same ACK datagram re-passed on the repeat");
}

/// Nothing ever comes back for the relayed re-INVITE: bob answers the BYE and
/// is deaf to everything else. The orphaned transaction keeps its Timer A
/// ladder (§17.1.1.2 Calling — the request may not have arrived), gives up at
/// Timer B (64·T1 from the INVITE) and is purged having ACKed nothing.
#[tokio::test(start_paused = true)]
async fn a_relayed_reinvite_that_draws_nothing_is_purged_at_timer_b() {
    let h = Harness::new("b2bua-reinvite-timer-b");
    let alice = h.agent("alice", "127.0.0.1:5064").await;
    let bob = h.agent("bob", "127.0.0.1:5074").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5074).start(&h, "b2bua", "127.0.0.1:5084").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut reinv = alice_dialog.reinvite(Some(REOFFER)).await;
    let bob_reinv = bob.receive("INVITE").await;
    let relayed_at = tokio::time::Instant::now();
    let mut alice_bye = alice_dialog.bye().await;
    alice_bye.expect(200).await;
    reinv.expect(487).await;
    let mut bob_bye = bob.receive("BYE").await;
    bob_bye.respond(200, "OK").await;
    settle_until(|| b2bua.active_calls() == 0).await;
    assert_eq!(b2bua.txn_metrics().orphaned_transactions(), 1, "the orphan is still resident");

    // ── Timer B, from the relayed INVITE: the orphan is purged having ACKed
    //    nothing; the BYE's own Timer J, armed a transit later, ends last ─────
    let elapsed = relayed_at.elapsed();
    h.advance(Duration::from_millis(TIMER_B + 100) - elapsed).await;
    assert_eq!(b2bua.txn_metrics().orphaned_transactions(), 0, "the orphan is gone at Timer B");
    h.advance(Duration::from_millis(T1)).await;
    assert_eq!(b2bua.txn_metrics().active_transactions(), 0, "no transaction survives Timer J");
    assert_eq!(b2bua.txn_metrics().timer_queue_len(), 0, "no timer survives the purge");
    drop(bob_reinv);
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;
    let acks = acks_to(&report, b2bua.addr, bob.addr());
    assert_eq!(acks.len(), 1, "only the dialog's own ACK ever crossed to bob");
}
