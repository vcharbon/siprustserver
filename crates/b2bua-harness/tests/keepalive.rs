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

use b2bua_harness::{establish_call, B2buaSut};
use scenario_harness::Harness;

/// The Rust default keepalive interval (`KeepaliveActivation.interval_sec`).
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

#[tokio::test(start_paused = true)]
async fn keepalive_options_to_both_legs_two_cycles() {
    let h = Harness::new("b2bua-keepalive");
    let alice = h.agent("alice", "127.0.0.1:5064").await;
    let bob = h.agent("bob", "127.0.0.1:5074").await;
    let b2bua = B2buaSut::route_all_to(&h, "b2bua", "127.0.0.1:5084", "127.0.0.1", 5074).await;

    // ── Call setup ───────────────────────────────────────────────────────────
    let mut dialog = establish_call(&alice, &bob, b2bua.addr).await;

    // ── First keepalive cycle ────────────────────────────────────────────────
    h.advance(KEEPALIVE_INTERVAL).await;
    alice.receive("OPTIONS").await.respond(200, "OK").await;
    bob.receive("OPTIONS").await.respond(200, "OK").await;

    // ── Second cycle — confirms the timer re-armed ───────────────────────────
    h.advance(KEEPALIVE_INTERVAL).await;
    alice.receive("OPTIONS").await.respond(200, "OK").await;
    bob.receive("OPTIONS").await.respond(200, "OK").await;

    // ── Teardown ─────────────────────────────────────────────────────────────
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
}
