//! Long call with keepalive.
//!
//! After the call is established the B2BUA arms a keepalive timer. Each interval
//! it sends an in-dialog OPTIONS to *both* legs; each 200 OK is absorbed (not
//! relayed) and the timer is rescheduled. We drive two full cycles under a
//! paused clock to prove the call stays up and the timer re-arms, then tear it
//! down.
//!
//! Exercises the `keepalive` + `absorb-options-200` default rules. The harness
//! baseline sets `B2buaConfig::keepalive_interval_sec` to 30 s, so we advance in
//! 30 s steps.

use std::time::Duration;

use b2bua_harness::B2buaScene;

/// The harness baseline keepalive interval (`B2buaConfig::keepalive_interval_sec`).
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
