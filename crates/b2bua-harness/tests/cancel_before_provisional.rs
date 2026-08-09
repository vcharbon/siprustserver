//! **RFC 3261 §9.1 — the b-leg CANCEL waits for the branch's first
//! provisional.** When the caller CANCELs while the callee has sent NOTHING on
//! the b-leg, the B2BUA's CANCEL must not reach the wire: a UAS that has not
//! built the INVITE server transaction answers it 481 while Timer-A INVITE
//! retransmits keep ringing a call that no longer exists. The transaction layer
//! holds the CANCEL and flushes it on the first provisional; a branch that
//! never draws one owes no CANCEL at all (`sip-txn/tests/cancel_hold.rs` pins
//! the layer seam; this pins the end-to-end callflow). Decision: ADR-0028.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// Caller CANCELs while the b-leg is response-less → the CANCEL is held; the
/// callee's first provisional releases it and the leg resolves 487.
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

    // ── the b-leg CANCEL must be HELD while the branch is response-less ──────
    // Bob keeps seeing Timer-A INVITE retransmits only — never a CANCEL.
    h.advance(Duration::from_secs(2)).await;
    assert!(
        bob.try_receive_tolerating("CANCEL", &["INVITE"]).await.is_none(),
        "no CANCEL may reach a response-less b-leg branch (RFC 3261 §9.1)"
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

/// Caller CANCELs and the callee never sends anything at all → the held CANCEL
/// is dropped with the b-leg transaction; the wire never carries it, and the
/// SUT still reaps the call on its own dead-call detection.
#[tokio::test(start_paused = true)]
async fn b_leg_cancel_is_dropped_when_callee_stays_silent() {
    let h = Harness::new("b2bua-cancel-dropped-silent-callee");
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

    // The branch never drew a provisional: the CANCEL died with the txn and
    // must never have reached the wire (bob saw INVITE retransmits only).
    assert!(
        bob.try_receive_tolerating("CANCEL", &["INVITE"]).await.is_none(),
        "a b-leg branch with no response ever is owed no CANCEL (RFC 3261 §9.1)"
    );
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}
