//! RFC 3261 §13.3.1.4 (in-dialog) — an in-dialog **re-INVITE** whose 2xx is
//! never ACKed must be retransmitted, and if no ACK arrives by the give-up
//! deadline the call must be torn down cleanly (never an infinite retransmit).
//!
//! This is the re-INVITE twin of `unacked_2xx_reap.rs`. The B2BUA relays the
//! callee's re-INVITE 200 to the originator on the a-leg **server** transaction,
//! which goes `Completed` on that final and never retransmits it — so a lost
//! a-leg ACK strands the renegotiation. Under packet loss the caller's
//! renegotiation stalls and the call times out (`reinvited@connected`). The
//! RFC-correct B2BUA must:
//!   (a) **retransmit** the re-INVITE 2xx to the originator while its ACK is
//!       missing (the `ReinviteAckRetransmit` cadence, re-sending the cached
//!       byte-faithful copy raw), and
//!   (b) at the give-up deadline, **tear the call down** (BYE both legs), driving
//!       `active_calls` back to 0.
//!
//! Paused-clock; the harness pins a short `ack_timeout_sec` so the give-up
//! deadline is reached in a handful of `advance`s (CLAUDE.md test-runtime policy:
//! cut churn at the source — the window, not real time). The default-lane
//! paused-clock gate for the slow-lane `loadgen_loss_soak_all_bodies_recover`
//! `reinvite` / `reinvite_em` bodies.

use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=bob 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30001 RTP/AVP 0\r\n";

/// The harness-pinned ACK-timeout. RFC's 64·T1 is 32 s; the harness uses a
/// compact window so the paused-clock advances stay cheap.
const ACK_TIMEOUT_SEC: i64 = 6;

#[tokio::test(start_paused = true)]
async fn unacked_reinvite_2xx_is_retransmitted_then_byes_both_legs() {
    let h = Harness::new("b2bua-unacked-reinvite-2xx-reap");
    let alice = h.agent("alice", "127.0.0.1:5069").await;
    let bob = h.agent("bob", "127.0.0.1:5079").await;
    let b2bua = b2bua_with_ack_timeout(&h, "b2bua", "127.0.0.1:5089", 5079, ACK_TIMEOUT_SEC).await;

    // ── Call setup: INVITE → 180 → 200 → ACK (alice ACKs the INITIAL 2xx, so
    //    the initial-INVITE watchdog is cancelled and cannot be confused with the
    //    re-INVITE one). ──
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    assert_eq!(b2bua.active_calls(), 1, "call established");

    // ── alice re-INVITEs (offer in the re-INVITE); bob answers 200 — but alice
    //    NEVER ACKs the re-INVITE 2xx. ──
    let mut reinv = dialog.request(InDialogMethod::Invite, Some(REOFFER)).await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(REANSWER).await;
    reinv.expect(200).await; // alice receives the re-INVITE 200 — and stays silent.
    // No `dialog.ack(...)` — the a-leg re-INVITE ACK is withheld (the lost-ACK case).

    assert_eq!(b2bua.active_calls(), 1, "still one active call after the (un-ACKed) re-INVITE answer");

    // ── (a) The re-INVITE 2xx must be retransmitted to alice while her ACK is
    //    missing. Advance past the first retransmit cadence and confirm at least
    //    one re-sent 200 reached alice. ──
    h.advance(Duration::from_secs(2)).await;
    let retransmits = alice.drain().await;
    assert!(
        retransmits >= 1,
        "RFC 3261 §13.3.1.4 (in-dialog): the un-ACKed re-INVITE 2xx must be retransmitted to alice, got {retransmits}",
    );

    // ── (b) At the give-up deadline the B2BUA tears the call down (BYE both
    //    legs). Advance past the give-up (armed when the re-INVITE 2xx was
    //    relayed). ──
    h.advance(Duration::from_secs(ACK_TIMEOUT_SEC as u64)).await;
    // Discard alice's accumulated (un-ACKed) 2xx retransmits so the next request
    // she receives is the give-up BYE.
    alice.drain().await;
    bob.drain().await;
    alice.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}

/// The happy path under the watchdog: the a-leg ACK **does** arrive, so the
/// re-INVITE watchdog is cancelled (CSeq-matched) and NEVER fires — no spurious
/// retransmit, no give-up teardown. Guards the cancel seam (`relay-ack`).
#[tokio::test(start_paused = true)]
async fn reinvite_ack_cancels_the_watchdog() {
    let h = Harness::new("b2bua-reinvite-ack-cancels-watchdog");
    let alice = h.agent("alice", "127.0.0.1:5063").await;
    let bob = h.agent("bob", "127.0.0.1:5073").await;
    let b2bua = b2bua_with_ack_timeout(&h, "b2bua", "127.0.0.1:5083", 5073, ACK_TIMEOUT_SEC).await;

    // ── call setup ──
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── alice re-INVITEs; bob answers 200; alice ACKs promptly ──
    let mut reinv = dialog.request(InDialogMethod::Invite, Some(REOFFER)).await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(REANSWER).await;
    reinv.expect(200).await;
    dialog.ack(None).await;
    bob.receive("ACK").await; // the relayed re-INVITE ACK reaches bob.

    // Advance well past both the retransmit cadence and the give-up deadline: the
    // watchdog was cancelled by the a-leg ACK, so nothing re-sends and the call
    // is NOT torn down.
    h.advance(Duration::from_secs(ACK_TIMEOUT_SEC as u64 + 3)).await;
    assert_eq!(
        alice.drain().await,
        0,
        "a matching a-leg ACK cancels the re-INVITE watchdog — no spurious 2xx retransmit",
    );
    assert_eq!(b2bua.active_calls(), 1, "the call stays up (no give-up teardown)");

    // ── normal teardown proves the dialog is still healthy ──
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;

    let _report = h.finish().await;
}

/// A B2BUA that routes every call to `dest_port` with a short `ack_timeout_sec`
/// so the un-ACKed-2xx give-up deadline is reached in a few paused-clock steps.
async fn b2bua_with_ack_timeout(
    h: &Harness,
    name: &str,
    addr: &str,
    dest_port: u16,
    ack_timeout_sec: i64,
) -> B2buaSut {
    let decision = Arc::new(b2bua::decision::ScriptedDecisionEngine::route_all_to("127.0.0.1", dest_port));
    B2buaSut::builder(decision)
        .tune(move |c| {
            c.ack_timeout_sec = ack_timeout_sec;
        })
        .start(h, name, addr)
        .await
}
