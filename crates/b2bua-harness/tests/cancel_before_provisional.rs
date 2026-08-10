//! **RFC 3261 §9.1 bounded by ADR-0028 — the b-leg CANCEL waits for the
//! branch's first provisional, but only for the grace window.** When the
//! caller CANCELs while the callee has sent NOTHING on the b-leg, the B2BUA's
//! CANCEL is held (a UAS that has not built the INVITE server transaction
//! answers it 481 while Timer-A INVITE retransmits keep ringing a call that no
//! longer exists) and flushed on the first provisional. A branch that stays
//! response-less past the grace window gets the CANCEL REGARDLESS — a callee
//! that answers nothing must still hear the cancellation
//! (`sip-txn/tests/cancel_hold.rs` pins the layer seam; this pins the
//! end-to-end callflow). Decision: ADR-0028.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// The transaction layer's default held-CANCEL grace window (ms) — the SUT is
/// spawned with defaults, so tests time their advances against this.
const GRACE_MS: u64 = sip_txn::timers::CANCEL_HOLD_GRACE;

/// Caller CANCELs while the b-leg is response-less → the CANCEL is held; the
/// callee's first provisional (inside the grace window) releases it and the
/// leg resolves 487.
#[tokio::test(start_paused = true)]
async fn b_leg_cancel_is_held_until_first_provisional() {
    let h = Harness::new("b2bua-cancel-held-until-1xx");
    let alice = h.agent("alice", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5071").await;
    let b2bua = B2buaSut::builder(Arc::new(ScriptedDecisionEngine::route_all_to(
        "127.0.0.1",
        5071,
    )))
    .start(&h, "b2bua", "127.0.0.1:5081")
    .await;

    // ── alice INVITEs through the B2BUA; bob receives but stays silent ───────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    h.advance(Duration::from_millis(300)).await;
    let mut b_inv = bob.receive("INVITE").await;

    // ── alice hangs up before ANY b-leg response ──────────────────────────────
    // The a-leg is answered by the transaction layer at once (200 to the
    // CANCEL, 487 to the INVITE) — alice is released regardless of bob.
    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;

    // ── the b-leg CANCEL is HELD while inside the grace window ───────────────
    // Bob keeps seeing Timer-A INVITE retransmits only — no CANCEL yet.
    h.advance(Duration::from_millis(GRACE_MS / 2)).await;
    assert!(
        bob.try_receive_tolerating("CANCEL", &["INVITE"]).await.is_none(),
        "no CANCEL may reach a response-less b-leg branch inside the grace window (RFC 3261 §9.1)"
    );

    // ── bob's first provisional releases the held CANCEL ─────────────────────
    b_inv.respond(180, "Ringing").await;
    let mut b_cxl = bob.receive("CANCEL").await;
    b_cxl.respond(200, "OK").await;
    b_inv.respond(487, "Request Terminated").await;
    bob.receive("ACK").await; // the B2BUA completes bob's 487 txn (§17.1.1.3)

    // ── fully reaped ──────────────────────────────────────────────────────────
    h.advance(Duration::from_secs(1)).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}

/// Caller CANCELs and the callee sends nothing at all → the grace expiry puts
/// the CANCEL on the wire anyway (ADR-0028: the §9.1 wait is a courtesy, never
/// a veto), the callee 487s, and the call resolves promptly — no ride on the
/// terminating backstop.
#[tokio::test(start_paused = true)]
async fn b_leg_cancel_is_sent_at_grace_expiry_when_callee_stays_silent() {
    let h = Harness::new("b2bua-cancel-grace-silent-callee");
    let alice = h.agent("alice", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5071").await;
    let b2bua = B2buaSut::builder(Arc::new(ScriptedDecisionEngine::route_all_to(
        "127.0.0.1",
        5071,
    )))
    .start(&h, "b2bua", "127.0.0.1:5081")
    .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    h.advance(Duration::from_millis(300)).await;
    let mut b_inv = bob.receive("INVITE").await; // delivered; bob answers nothing

    // alice hangs up pre-provisional; the a-leg resolves immediately (ADR-0022).
    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;

    // ── cross exactly the grace deadline: the CANCEL reaches bob pre-1xx ─────
    h.advance(Duration::from_millis(GRACE_MS + 300)).await;
    let mut b_cxl = bob.receive_absorbing("CANCEL", &["INVITE"]).await;

    // Bob's UAS has the server transaction (it got the INVITE), so the CANCEL
    // matches: 200 + 487, auto-ACKed — the abandoned callee stops ringing
    // within ~grace instead of the 32 s terminating backstop.
    b_cxl.respond(200, "OK").await;
    b_inv.respond(487, "Request Terminated").await;
    bob.receive_absorbing("ACK", &["INVITE"]).await;

    h.advance(Duration::from_secs(1)).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}

/// STRICT policy (`cancel_strict_rfc3261_wait`) — the literal §9.1 wait kept
/// selectable: caller CANCELs and the callee never sends anything at all → the
/// held CANCEL is dropped with the b-leg transaction; the wire never carries
/// it, and the SUT still reaps the call on its own dead-call detection (the
/// pre-amendment ADR-0028 shape, preserved as a test case).
#[tokio::test(start_paused = true)]
async fn strict_policy_never_cancels_a_silent_callee() {
    let h = Harness::new("b2bua-cancel-strict-silent-callee");
    let alice = h.agent("alice", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5071").await;
    let b2bua = B2buaSut::builder(Arc::new(ScriptedDecisionEngine::route_all_to(
        "127.0.0.1",
        5071,
    )))
    .tune(|c| c.cancel_strict_rfc3261_wait = true)
    .start(&h, "b2bua", "127.0.0.1:5081")
    .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    h.advance(Duration::from_millis(300)).await;
    let _b_inv = bob.receive("INVITE").await; // delivered; bob never answers

    // alice hangs up pre-provisional; the a-leg resolves immediately (ADR-0022).
    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;

    // The terminating backstop (armed when the caller's CANCEL moved the call
    // to Terminating) is the deadline that reaps the silent b-leg — advance
    // exactly past it, so a regression that falls back to the 150 s
    // SetupTimeout fails here instead of passing under a longer pump.
    h.advance(Duration::from_millis(
        call::helpers::TERMINATING_TIMEOUT_MS as u64 + 1_000,
    ))
    .await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;

    // The branch never drew a provisional: under the strict policy the CANCEL
    // died with the txn and never reached the wire (bob saw INVITE
    // retransmits only).
    assert!(
        bob.try_receive_tolerating("CANCEL", &["INVITE"]).await.is_none(),
        "strict §9.1: a b-leg branch with no response ever is owed no CANCEL"
    );
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}

/// Caller CANCELs and the callee is a black hole — it never answers the INVITE
/// *or* the grace-sent CANCEL (host dead mid-setup). The CANCEL still reaches
/// the wire (the always-send guarantee is unconditional), and the SUT still
/// reaps the call on its own dead-call detection.
#[tokio::test(start_paused = true)]
async fn grace_sent_cancel_to_a_dead_callee_still_reaps_on_the_backstop() {
    let h = Harness::new("b2bua-cancel-grace-dead-callee");
    let alice = h.agent("alice", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5071").await;
    let b2bua = B2buaSut::builder(Arc::new(ScriptedDecisionEngine::route_all_to(
        "127.0.0.1",
        5071,
    )))
    .start(&h, "b2bua", "127.0.0.1:5081")
    .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    h.advance(Duration::from_millis(300)).await;
    let _b_inv = bob.receive("INVITE").await; // delivered; bob never answers anything

    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;

    // The grace expiry sends the CANCEL even though bob answered NOTHING.
    h.advance(Duration::from_millis(GRACE_MS + 300)).await;
    assert!(
        bob.try_receive_tolerating("CANCEL", &["INVITE"]).await.is_some(),
        "the grace expiry must put the CANCEL on the wire toward a fully silent callee (ADR-0028)"
    );

    // Bob ignores it. The terminating backstop (armed when the caller's CANCEL
    // moved the call to Terminating) reaps the dead b-leg — advance exactly
    // past it, so a regression that falls back to the 150 s SetupTimeout fails
    // here instead of passing under a longer pump.
    h.advance(Duration::from_millis(
        call::helpers::TERMINATING_TIMEOUT_MS as u64 + 1_000,
    ))
    .await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}
