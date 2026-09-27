//! Long call with keepalive (port of `tests/scenarios/keepalive-happy.ts`).
//!
//! After the call is established the B2BUA arms a keepalive timer. Each interval
//! it sends an in-dialog OPTIONS to *both* legs; each 200 OK is absorbed (not
//! relayed) and the timer is rescheduled. We drive two full cycles under a
//! paused clock to prove the call stays up and the timer re-arms, then tear it
//! down.
//!
//! Exercises the `keepalive` + `absorb-options-200` default rules. The source
//! backend configured a 15-min interval; the Rust default keepalive interval is
//! 30 s (`FeatureActivations.platform.keepalive.interval_sec`), so we advance in
//! 30 s steps — the behaviour (OPTIONS to both legs, absorb, re-arm) is identical.

use std::time::Duration;

use b2bua_harness::B2buaScene;

/// The Rust default keepalive interval (`KeepaliveActivation.interval_sec`).
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

#[tokio::test(start_paused = true)]
async fn keepalive_options_to_both_legs_two_cycles() {
    // The whole setup is the default alice ↔ b2bua(→bob) ↔ bob fixture.
    let s = B2buaScene::new("b2bua-keepalive").await;

    // ── Call setup ───────────────────────────────────────────────────────────
    let mut dialog = s.establish().await;

    // ── First keepalive cycle ────────────────────────────────────────────────
    s.h.advance(KEEPALIVE_INTERVAL).await;
    s.alice.receive("OPTIONS").await.respond(200, "OK").await;
    s.bob.receive("OPTIONS").await.respond(200, "OK").await;

    // ── Second cycle — confirms the timer re-armed ───────────────────────────
    s.h.advance(KEEPALIVE_INTERVAL).await;
    s.alice.receive("OPTIONS").await.respond(200, "OK").await;
    s.bob.receive("OPTIONS").await.respond(200, "OK").await;

    // ── Teardown ─────────────────────────────────────────────────────────────
    s.hangup(&mut dialog).await;

    let _report = s.finish().await;
}
