//! Auto-cutoff when one party stops answering keepalives (port of
//! `tests/scenarios/options-keepalive-timeout.ts`).
//!
//! The call is up. The B2BUA fires its keepalive and sends an in-dialog OPTIONS
//! to both legs. Bob answers 200; **alice does not**. After the per-leg
//! keepalive timeout the B2BUA gives up on alice, terminates her leg and BYEs
//! the still-healthy peer (bob) — the "caller walked away, B2BUA reaps the
//! call" shape. Exercises `keepalive` → `keepalive-timeout` → `begin-termination`.
//!
//! The source backend used a 2 s/3 s interval/timeout; the Rust defaults are a
//! 30 s keepalive interval and a hard-coded 5 s per-leg keepalive timeout
//! (`TimerRules`), so we advance 30 s to trigger the probe and 5 s more to trip
//! the cutoff. Alice deliberately never answers her OPTIONS — her dialog is
//! still live on the B2BUA at the end, exactly as the source's
//! `.skipFinalSweep()` documents.

use std::time::Duration;

use b2bua_harness::{establish, settle_until, B2buaSut};
use call::CdrEventType;
use scenario_harness::Harness;

/// Rust default keepalive interval; the per-leg keepalive timeout is a fixed 5 s.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::test(start_paused = true)]
async fn unanswered_keepalive_byes_the_healthy_peer() {
    let h = Harness::new("b2bua-keepalive-timeout");
    let alice = h.agent("alice", "127.0.0.1:5065").await;
    let bob = h.agent("bob", "127.0.0.1:5075").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5075).start(&h, "b2bua", "127.0.0.1:5085").await;

    // ── Call setup ───────────────────────────────────────────────────────────
    let _dialog = establish(&alice, &bob, b2bua.addr).await;

    // ── Keepalive probe ──────────────────────────────────────────────────────
    h.advance(KEEPALIVE_INTERVAL).await;
    // Alice receives the OPTIONS but stays silent (no respond).
    let _silent = alice.receive("OPTIONS").await;
    // Bob answers, so only alice's leg is unhealthy.
    bob.receive("OPTIONS").await.respond(200, "OK").await;

    // ── Cutoff: alice's keepalive times out → BYE to bob ─────────────────────
    h.advance(KEEPALIVE_TIMEOUT).await;
    bob.receive("BYE").await.respond(200, "OK").await;

    // The CDR records the keepalive-driven teardown.
    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one CDR for the timed-out call");
    let kinds: Vec<CdrEventType> = cdrs[0].events.iter().map(|e| e.event_type).collect();
    assert!(kinds.contains(&CdrEventType::Bye), "bye event from keepalive timeout: {kinds:?}");

    let _report = h.finish().await;
}
