//! **A call's end deferred to a primary that then departs for good.** A
//! backup's takeover copy that ends a call while its primary is still a member
//! of the set defers the discharge to that primary (ADR-0020 X3): it keeps the
//! ended Element and writes no CDR. When the primary then leaves the set
//! without having reconciled it, the departure sweep discharges the ended
//! Element — CDR, limiter release, delete — once (ADR-0047 D8). The call's
//! limiter holds left earlier, at its end on the backup once the proxy's
//! verdict on the primary was confirmed; the discharge's release frees nothing
//! more (the limiter cell).
//!
//! ```text
//!   established · P serves · B synchronized
//!   P crashes in place (its endpoint stays in the set)
//!   alice ──BYE──▶ proxy ──▶ B   (takeover copy: ends the call, defers)
//!   P's endpoint leaves the set  (B's departure sweep discharges the call)
//! ```
//!
//! Beyond the failover sweep: the deferred end held until the departure, and the two-CDR residual
//! window.

use std::time::Duration;

use failover_harness::{
    assert_call_fully_over, assert_call_fully_released, cookie_field, total_cdrs_for,
    FailoverHarness, ReplicatedB2buaSut, WorkerHealth,
};

use crate::discharged_call_dropped_by_its_primary::{establish, Established};
use crate::limiter_release_by_call::{
    limited_decision, replicated_call_ref, spawn_workers, LimiterRig,
};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

#[tokio::test(start_paused = true)]
async fn an_end_deferred_to_a_primary_that_departs_is_discharged_by_the_sweep() {
    let Established {
        mut fh,
        alice,
        bob,
        proxy,
        mut w_b1,
        mut w_b2,
        pri_ord,
        call_ref,
        mut dialog,
    } = establish("deferred-end-of-a-departed-primary").await;
    {
        let primary = if pri_ord == "b1" { &mut w_b1 } else { &mut w_b2 };
        fh.mark(&pri_ord, None, "crash", "the primary crashes in place");
        primary.crash();
        proxy.set_health(&pri_ord, WorkerHealth::Dead);
    }
    fh.advance(Duration::from_millis(300)).await;
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(1)).await;
    let backup = if pri_ord == "b1" { &w_b2 } else { &w_b1 };
    assert_eq!(total_cdrs_for(&[backup], &call_ref), 0, "the backup defers to a member primary");
    assert!(backup.holds_any_trace(&call_ref).await, "and keeps the ended call for it");

    fh.depart(&pri_ord);
    let released = fh
        .settle_terminal(async || backup.memory_clean() && !backup.holds_any_trace(&call_ref).await)
        .await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(total_cdrs_for(&[backup], &call_ref), 1, "the sweep writes the call's one CDR");
    assert_eq!(backup.metrics().repl_terminal_lost_total(), 0, "no ended call is lost");
    assert!(released, "the ended call is deleted");
    assert!(
        fh.now_ms() < b2bua::ReplicaConfig::default().reboot_budget_sec * 1000,
        "discharged by the sweep, not reaped at the replica TTL"
    );
    assert_call_fully_released(&[backup], &call_ref).await;
    drop(proxy);
}

/// The residual of the sweep's discharge (ADR-0047): a primary that
/// reconciled a deferred end — its CDR written, its delete sent — and left the
/// set before the backup applied that delete. The departure parks the
/// backup's flows, so the delete never lands, and the sweep discharges the
/// ended Element again: two CDRs. The window runs from the copy's flush to the
/// park; here the primary's writes to the backup are held throughout.
#[tokio::test(start_paused = true)]
async fn an_end_reconciled_by_a_primary_that_departs_before_its_delete_lands_is_discharged_twice() {
    let Established { mut fh, alice, bob, proxy, w_b1, w_b2, pri_ord, call_ref, mut dialog } =
        establish("deferred-end-reconciled-then-departed").await;
    let bak_ord = if pri_ord == "b1" { "b2" } else { "b1" };
    let (primary, backup) = if pri_ord == "b1" { (&w_b1, &w_b2) } else { (&w_b2, &w_b1) };
    fh.stall(&pri_ord, bak_ord);
    // The proxy reads the running primary dead: the BYE reaches the backup.
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    let reconciled = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(10), async || {
            total_cdrs_for(&[primary], &call_ref) == 1
        })
        .await;
    assert!(reconciled, "the primary reconciled the backup's deferred end");
    assert!(backup.holds_any_trace(&call_ref).await, "its delete never reached the backup");

    fh.depart(&pri_ord);
    let released = fh
        .settle_terminal(async || {
            primary.memory_clean()
                && backup.memory_clean()
                && !primary.holds_any_trace(&call_ref).await
                && !backup.holds_any_trace(&call_ref).await
        })
        .await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert!(released, "nothing of the call is left");
    assert_eq!(total_cdrs_for(&[primary, backup], &call_ref), 2, "the residual: two CDRs");
    drop(proxy);
}

/// A deferred terminal of a primary that departs is released once. The call
/// holds `[x, x, y]`; the primary crashes in place and the proxy reads it dead,
/// the caller's BYE fails over to the backup, which defers the terminal's CDR
/// to it and releases the call at its end; then the primary's endpoint leaves
/// the set, and the backup's departure sweep discharges the deferral: one CDR,
/// and a second release that frees nothing. The primary reboots inside the
/// replica TTL and finds nothing to reclaim, and nothing releases the call again
/// past the TTL: the witnesses are intact.
#[tokio::test(start_paused = true)]
async fn a_deferred_terminal_of_a_departing_primary_is_released_once() {
    let mut fh = FailoverHarness::new("limiter-release-by-call-reclaim-and-reap", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let rig = LimiterRig::serve().await;
    let (proxy, mut w_b1, mut w_b2) =
        spawn_workers(&mut fh, &rig, limited_decision(&["x", "x", "y"])).await;
    let reboot_budget_ms = b2bua::ReplicaConfig::default().reboot_budget_sec * 1000;

    // ── establish on the primary; replicate ──────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let primary_ord = cookie_field(uas.request(), "w_pri").unwrap_or_default();
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(rig.holds(), [2, 1, 0], "the call holds x twice and y");
    let backup_sut = if primary_ord == "b1" { &w_b2 } else { &w_b1 };
    let call_ref = replicated_call_ref(&fh, backup_sut, &primary_ord).await;

    let (primary, backup): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if primary_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };

    // ── the primary crashes in place ─────────────────────────────────────
    primary.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(300)).await;

    // ── the BYE fails over to the backup, which defers the terminal ──────
    let creations_before = backup.metrics().creations_total();
    let deferred_at = fh.now_ms();
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(1)).await;
    assert!(backup.metrics().creations_total() > creations_before, "backup served the BYE");
    assert_eq!(rig.holds(), [0, 0, 0], "the backup releases at the end: its primary is read dead");
    assert!(backup.cdr_records().is_empty(), "and defers the CDR to the primary");
    let releases_deferred = rig.store.stats().releases_total;

    // ── the primary departs: the backup's sweep discharges the deferral ──
    backup.simulate_peer_removed(&primary_ord);
    fh.advance(Duration::from_secs(1)).await;
    assert_eq!(backup.cdr_records().len(), 1, "the sweep wrote the deferred call's CDR");
    let releases_swept = rig.store.stats().releases_total;
    assert_eq!(releases_swept, releases_deferred + 1, "the discharge releases the key again");
    assert_eq!(rig.holds(), [0, 0, 0], "which frees nothing more: the witnesses are intact");

    // ── inside the TTL: the primary reboots and finds nothing to reclaim ─
    let reboot_at = deferred_at + reboot_budget_ms - 40_000;
    while fh.now_ms() < reboot_at {
        fh.advance(Duration::from_secs(10)).await;
    }
    let new_addr = primary.reboot().await;
    for _ in 0..40 {
        fh.advance(Duration::from_millis(500)).await;
        if primary.is_ready() {
            break;
        }
    }
    assert!(primary.is_ready(), "rebooted primary re-hydrated from the backup");
    proxy.set_address(&primary_ord, new_addr);
    fh.note_worker_rebound(&primary_ord, new_addr);
    proxy.set_health(&primary_ord, WorkerHealth::Alive);
    backup.simulate_peer_added(&primary_ord);

    // ── past the TTL: nothing of the call is released again ──────────────
    let past_ttl = deferred_at + reboot_budget_ms + 90_000;
    while fh.now_ms() < past_ttl {
        fh.advance(Duration::from_secs(30)).await;
    }
    assert!(primary.cdr_records().is_empty(), "the reborn primary reclaimed nothing");
    assert_eq!(rig.store.stats().releases_total, releases_swept, "no further release");
    assert_eq!(backup.metrics().repl_terminal_lost_total(), 0, "no ended call is lost");
    assert_eq!(rig.holds(), [0, 0, 0], "the witnesses on x and y are intact");
    rig.release_witnesses();
    assert_call_fully_over(&[&w_b1, &w_b2], &call_ref, &rig.store).await;
}
