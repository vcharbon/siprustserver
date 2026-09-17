//! Auto-cutoff when one party stops answering keepalives (port of
//! `tests/scenarios/options-keepalive-timeout.ts`).
//!
//! The call is up. The B2BUA fires its keepalive and sends an in-dialog OPTIONS
//! to both legs. Bob answers 200; **alice does not**. After the per-leg
//! keepalive timeout the B2BUA gives up on the call and BYEs BOTH legs: the
//! healthy peer (bob), and alice — RFC 3261 §12.2.1.2 ends a dialog whose
//! request drew no answer by sending a BYE, since the OPTIONS may have been
//! lost while the signalling path is alive. Alice stays silent on the BYE too;
//! her leg resolves by the BYE's own transaction timeout and the call is
//! reaped, nothing left live. Exercises `keepalive` → `keepalive-timeout` →
//! `begin-termination` → `handle-timeout`.
//!
//! The source backend used a 2 s/3 s interval/timeout; the Rust defaults are a
//! 30 s keepalive interval and a hard-coded 5 s per-leg keepalive timeout
//! (`TimerRules`), so we advance 30 s to trigger the probe and 5 s more to trip
//! the cutoff.

use std::time::Duration;

use b2bua_harness::{establish, settle_until, B2buaSut};
use call::CdrEventType;
use scenario_harness::Harness;

/// Rust default keepalive interval; the per-leg keepalive timeout is a fixed 5 s.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(5);
/// A non-INVITE client transaction's Timer F (RFC 3261 §17.1.2.2, 64×T1) — the
/// unanswered BYE's deadline, one more than `TERMINATING_TIMEOUT_MS`'s worth.
const BYE_TIMEOUT: Duration = Duration::from_secs(33);

#[tokio::test(start_paused = true)]
async fn unanswered_keepalive_byes_both_peers_and_reaps() {
    let h = Harness::new("b2bua-keepalive-timeout");
    let alice = h.agent("alice", "127.0.0.1:5065").await;
    let bob = h.agent("bob", "127.0.0.1:5075").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5075).start(&h, "b2bua", "127.0.0.1:5085").await;

    // ── Call setup ───────────────────────────────────────────────────────────
    let _dialog = establish(&alice, &bob, b2bua.addr).await;

    // ── Keepalive probe ──────────────────────────────────────────────────────
    h.advance(KEEPALIVE_INTERVAL).await;
    // Alice receives the OPTIONS but stays silent (no respond).
    let _silent = alice.receive("OPTIONS").await;
    // Bob answers, so only alice's leg is unhealthy.
    bob.receive("OPTIONS").await.respond(200, "OK").await;

    // ── Cutoff: alice's keepalive times out → BYE to bob AND to alice ─────────
    h.advance(KEEPALIVE_TIMEOUT).await;
    bob.receive("BYE").await.respond(200, "OK").await;
    // Alice's BYE arrives behind the still-retransmitting OPTIONS; she stays
    // silent on it as well.
    let _silent_bye = alice.receive_tolerating("BYE", &["OPTIONS"]).await;

    // ── Alice never answers the BYE: its timeout resolves her leg, the call reaps ─
    h.advance(BYE_TIMEOUT).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    // The CDR records the keepalive-driven teardown.
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one CDR for the timed-out call");
    let kinds: Vec<CdrEventType> = cdrs[0].events.iter().map(|e| e.event_type).collect();
    assert!(kinds.contains(&CdrEventType::Bye), "bye event from keepalive timeout: {kinds:?}");

    let _report = h.finish().await;
}

/// A leg silent on its probe, then on its BYE for a while, is still owed the
/// BYE's own deadline: the probe's Timer F (32 s after the OPTIONS, 5 s before
/// the BYE's) resolves nothing on a leg awaiting its BYE answer, so a 200 that
/// arrives in between confirms the BYE and the call ends on that answer.
#[tokio::test(start_paused = true)]
async fn a_silent_leg_answering_its_bye_after_the_probes_timer_f_is_confirmed() {
    let h = Harness::new("b2bua-keepalive-timeout-late-bye-200");
    let alice = h.agent("alice", "127.0.0.1:5165").await;
    let bob = h.agent("bob", "127.0.0.1:5175").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5175).start(&h, "b2bua", "127.0.0.1:5185").await;
    let _dialog = establish(&alice, &bob, b2bua.addr).await;

    // t = 30 s: the probe. Alice silent, bob healthy.
    h.advance(KEEPALIVE_INTERVAL).await;
    let _silent = alice.receive("OPTIONS").await;
    bob.receive("OPTIONS").await.respond(200, "OK").await;

    // t = 35 s: the cutoff, a BYE to each leg. Bob answers; alice holds hers.
    h.advance(KEEPALIVE_TIMEOUT).await;
    bob.receive("BYE").await.respond(200, "OK").await;
    let mut alice_bye = alice.receive_tolerating("BYE", &["OPTIONS"]).await;

    // t = 63 s: the probe's Timer F (t = 62 s) has fired; alice's leg still
    // awaits its BYE answer, so the call is still there.
    h.advance(Duration::from_secs(28)).await;
    settle_until(|| false).await;
    assert_eq!(b2bua.active_calls(), 1, "the probe's timeout does not resolve the BYE-sent leg");

    // Alice answers her BYE before its own Timer F (t = 67 s): the call ends
    // on that answer.
    alice_bye.respond(200, "OK").await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one CDR for the call");

    let _report = h.finish().await;
}
