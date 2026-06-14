//! RFC 3261 §13.3.1.4 — an answered INVITE whose **2xx is never ACKed** must be
//! retransmitted (T1, doubling, capped T2) and, if no ACK arrives by 64·T1, the
//! UAS MUST clear the just-created dialog with a BYE.
//!
//! For the B2BUA this is a real latent leak: it relays bob's 200 to alice, but
//! the a-leg INVITE **server** transaction moves to `Completed` the moment the
//! final is sent, so the transaction layer never retransmits the 2xx
//! proactively (it only replays the stored 200 on a *retransmitted* INVITE), and
//! at Timer H the un-ACKed server txn is deleted **silently** — no BYE, no b-leg
//! teardown. The bridged, billable call then leaks until the 1 h GlobalDuration
//! cap (or bob's keepalive-timeout). `active_calls` stays pinned at 1.
//!
//! This scenario answers the call, then alice goes silent (never ACKs). The
//! RFC-correct B2BUA must:
//!   (a) **retransmit** the 2xx to alice at least once inside the ACK window, and
//!   (b) at the ACK-timeout deadline, **BYE the a-leg AND tear down the b-leg**
//!       (BYE to bob), driving `active_calls` back to 0.
//!
//! Paused-clock; the harness pins a short `ack_timeout_sec` so the give-up
//! deadline is reached in a handful of `advance`s (CLAUDE.md test-runtime policy:
//! cut churn at the source — the window, not real time).

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The harness-pinned ACK-timeout (set via `tune` below). RFC's 64·T1 is 32 s;
/// the harness uses a compact window so the paused-clock advances stay cheap.
/// The give-up rule fires `ack_timeout_sec` after the 2xx is relayed.
const ACK_TIMEOUT_SEC: i64 = 6;

#[tokio::test(start_paused = true)]
async fn unacked_2xx_is_retransmitted_then_byes_both_legs() {
    let h = Harness::new("b2bua-unacked-2xx-reap");
    let alice = h.agent("alice", "127.0.0.1:5067").await;
    let bob = h.agent("bob", "127.0.0.1:5077").await;
    let b2bua = b2bua_with_ack_timeout(&h, "b2bua", "127.0.0.1:5087", 5077, ACK_TIMEOUT_SEC).await;

    // ── Call setup: alice INVITEs, bob answers 200, but alice NEVER ACKs ──────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await; // alice receives the 200 — and deliberately stays silent.
    // alice never ACKs, so the B2BUA never relays an ACK to bob (no `bob.receive("ACK")`).

    assert_eq!(
        b2bua.metrics().creations_total() - b2bua.metrics().removals_total(),
        1,
        "exactly one active call after the (un-ACKed) answer",
    );

    // ── (a) The 2xx must be retransmitted to alice while her ACK is missing ──
    // Advance partway into the ACK window (past the first retransmit cadence) and
    // confirm at least one re-sent 200 reached alice.
    h.advance(Duration::from_secs(2)).await;
    let retransmits = alice.drain().await;
    assert!(
        retransmits >= 1,
        "RFC 3261 §13.3.1.4: the un-ACKed 2xx must be retransmitted to alice, got {retransmits}",
    );

    // ── (b) At the ACK-timeout deadline the B2BUA clears BOTH legs ───────────
    // Advance past the give-up deadline (armed when the 2xx was relayed). The
    // B2BUA BYEs the just-created a-leg dialog AND tears down the b-leg. The
    // caller (whose ACK was lost but is still reachable) and bob both answer, so
    // the call reaps without needing the 32 s Terminating safety net.
    h.advance(Duration::from_secs(ACK_TIMEOUT_SEC as u64)).await;
    // Discard alice's accumulated (un-ACKed) 2xx retransmits so the next request
    // she receives is the give-up BYE (the BYE client txn retransmits, so one is
    // still in flight after the drain).
    alice.drain().await;
    alice.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

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
    use std::sync::Arc;
    let decision = Arc::new(b2bua::decision::ScriptedDecisionEngine::route_all_to("127.0.0.1", dest_port));
    B2buaSut::builder(decision)
        .tune(move |c| {
            c.ack_timeout_sec = ack_timeout_sec;
        })
        .start(h, name, addr)
        .await
}
