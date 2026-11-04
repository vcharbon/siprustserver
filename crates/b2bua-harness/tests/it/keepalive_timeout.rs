//! Auto-cutoff when one party stops answering keepalives (port of
//! `tests/scenarios/options-keepalive-timeout.ts`).
//!
//! The call is up. The B2BUA fires its keepalive and sends an in-dialog OPTIONS
//! to both legs. Bob answers 200; **alice does not**. When the probe's own
//! transaction times out (Timer F, RFC 3261 §17.1.2.2) the B2BUA gives up on
//! the call and BYEs BOTH legs: the healthy peer (bob), and alice — RFC 3261
//! §12.2.1.2 ends a dialog whose request drew no answer by sending a BYE,
//! since the OPTIONS may have been lost while the signalling path is alive.
//! Alice stays silent on the BYE too; her leg resolves by the BYE's own
//! transaction timeout and the call is reaped, nothing left live. Exercises
//! `keepalive` → the probe's Timer F → `handle-timeout` → `begin-termination`.
//!
//! The harness probes every 30 s, so the next interval's probe leaves before
//! the first one's Timer F: the healthy party answers it.

use std::time::Duration;

use b2bua_harness::{establish, settle_until, B2buaSut};
use call::CdrEventType;
use scenario_harness::Harness;

/// The harness keepalive interval.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
/// Timer F of the unanswered probe's client transaction (RFC 3261 §17.1.2.2), plus one.
const PROBE_TIMEOUT: Duration = Duration::from_secs(33);

/// The `b2bua_peer_failures_total{kind="keepalive_timeout"}` count against `peer`.
fn keepalive_failures(b2bua: &B2buaSut, peer: &str) -> u64 {
    let (peer, kind) = (format!("peer=\"{peer}\""), "kind=\"keepalive_timeout\"");
    let text = b2bua.metrics().prometheus_text();
    text.lines()
        .filter(|l| l.starts_with("b2bua_peer_failures_total{"))
        .filter(|l| l.contains(&peer) && l.contains(kind))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<u64>().ok())
        .sum()
}
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

    // ── Cutoff: alice's probe times out → BYE to bob AND to alice ────────────
    h.advance(PROBE_TIMEOUT).await;
    assert_eq!(
        keepalive_failures(&b2bua, "127.0.0.1:5065"),
        1,
        "the silent hop's keepalive failure"
    );
    assert_eq!(keepalive_failures(&b2bua, "127.0.0.1:5075"), 0, "none for the healthy hop");
    bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    // Alice's BYE arrives behind the probes; she stays silent on it as well.
    let _silent_bye = alice.receive_absorbing("BYE", &["OPTIONS"]).await;

    // ── Alice never answers the BYE: its timeout resolves her leg, the call reaps ─
    h.advance(BYE_TIMEOUT).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();

    // The CDR records the keepalive-driven teardown.
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one CDR for the timed-out call");
    let kinds: Vec<CdrEventType> = cdrs[0].events.iter().map(|e| e.event_type).collect();
    assert!(kinds.contains(&CdrEventType::Bye), "bye event from keepalive timeout: {kinds:?}");

    let _report = h.finish().await;
}

/// A leg silent on its probe, whose BYE still awaits its answer when the
/// probe's Timer F fires, is owed the BYE's own deadline: the probe's
/// timeout resolves nothing on a leg awaiting its BYE answer, so a 200 that
/// arrives after it confirms the BYE and the call ends on that answer.
#[tokio::test(start_paused = true)]
async fn a_silent_leg_answering_its_bye_after_the_probes_timer_f_is_confirmed() {
    let h = Harness::new("b2bua-keepalive-timeout-late-bye-200");
    let alice = h.agent("alice", "127.0.0.1:5165").await;
    let bob = h.agent("bob", "127.0.0.1:5175").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5175).start(&h, "b2bua", "127.0.0.1:5185").await;
    let mut dialog = establish(&alice, &bob, b2bua.addr).await;

    // t = 30 s: the probe. Bob silent, alice healthy.
    h.advance(KEEPALIVE_INTERVAL).await;
    alice.receive("OPTIONS").await.respond(200, "OK").await;
    let _silent = bob.receive("OPTIONS").await;

    // t = 35 s: alice hangs up; bob holds the BYE relayed to him.
    h.advance(Duration::from_secs(5)).await;
    let mut bye = dialog.bye().await;
    let mut bob_bye = bob.receive_absorbing("BYE", &["OPTIONS"]).await;

    // t = 63 s: the probe's Timer F (t = 62 s) has fired; bob's leg still
    // awaits its BYE answer, so the call is still there.
    h.advance(Duration::from_secs(28)).await;
    settle_until(|| false).await;
    assert_eq!(b2bua.active_calls(), 1, "the probe's timeout does not resolve the BYE-sent leg");

    // Bob answers his BYE before its own Timer F (t = 67 s): the call ends
    // on that answer.
    bob_bye.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one CDR for the call");

    let _report = h.finish().await;
}
