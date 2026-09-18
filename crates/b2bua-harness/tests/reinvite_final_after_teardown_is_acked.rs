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
//! ```text
//!   the_487_to_a_relayed_reinvite_after_the_bye_is_acked
//!       alice re-INVITE → relayed to bob → bob 100 → alice BYE → the SUT
//!       CANCELs the relayed re-INVITE and relays the BYE → bob 200(CANCEL),
//!       200(BYE), the call is reaped → bob 487(re-INVITE) → the SUT's ACK on
//!       the 487's branch
//! ```

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;

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

/// The 2xx sibling: bob answers the relayed re-INVITE 200 after the CANCEL and
/// the BYE were both answered, so the 200 crosses onto a reaped call. §9.1 —
/// a UAC whose CANCEL is beaten by a 2xx still ACKs it — and §13.2.2.4 own
/// that ACK whatever the dialog's fate; without it bob ladders the 200 to
/// 64·T1 (§13.3.1.4). The round is closed (offer in the re-INVITE, answer in
/// the 200), so the ACK is bare and composable by the SUT alone.
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

    // ── alice re-INVITEs; bob acknowledges the relayed copy with a 100 only ──
    let mut reinv = alice_dialog.reinvite(Some(REOFFER)).await;
    let mut bob_reinv = bob.receive("INVITE").await;
    bob_reinv.respond(100, "Trying").await;

    // ── alice hangs up; the SUT CANCELs the relayed re-INVITE and relays the
    //    BYE; bob answers both 200 and the call is reaped ──────────────────────
    let mut alice_bye = alice_dialog.bye().await;
    alice_bye.expect(200).await;
    reinv.expect(487).await;
    bob.receive("CANCEL").await.respond(200, "OK").await;
    let mut bob_bye = bob.receive("BYE").await;
    bob_bye.respond(200, "OK").await;
    settle_until(|| b2bua.active_calls() == 0).await;

    // ── bob's 200 beats his 487: the SUT owes it an ACK all the same ─────────
    bob_reinv.respond(200, "OK").with_sdp(REANSWER).await;
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
