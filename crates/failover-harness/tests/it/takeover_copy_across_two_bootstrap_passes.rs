//! **A takeover copy live across two cold bootstrap passes stands down on the
//! end of the call its primary ended between them** (ADR-0014 §8, ADR-0047
//! D7). b2 backs b1's calls X and Y up and stays routable throughout. Its
//! Backup flow falls off b1's compacted change log, so its next stream is a
//! cold pass, which carries X and is cut. While the cut holds, the proxy reads
//! b1 dead and routes the caller's re-INVITE of X to b2, whose takeover copy
//! relays it; the proxy reads b1 alive again, and the callee's BYE ends X on
//! b1. The callee refuses the re-INVITE through the copy, which stays live.
//! The second cold pass carries Y alone and catches up: b2 drops X's Element
//! as b1's delete would have, and the copy, holding no request of its own,
//! stands down on that delete. b1 dies, b2 serves Y; past the replica's TTL,
//! b2 holds nothing of X, has sent no BYE in its dialogs, and X has its one
//! CDR, b1's.
//!
//! ```text
//!   b1 answers X and Y, each w_bak=b2
//!   b1 ✂ b2 for 100 s     b1's log toward b2 drops: b2's resume is reset
//!   heal, b1 → b2 500 ms  pass 1 (cold) lands X; b1 ✂ b2
//!   proxy: b1 dead        alice ─re-INVITE(X)─▶ b2 ─▶ bob, unanswered
//!   proxy: b1 alive       bob ─BYE(X)─▶ b1 ─▶ alice: X ends on b1, one CDR
//!   bob ─487─▶ b2 ─▶ alice  the round ends; the copy stays live
//!   heal                  pass 2 carries Y, catches up: X's Element dropped,
//!                         the copy stands down on the delete
//!   b1 dies               alice ─BYE(Y)─▶ b2; past the replica TTL
//! ```
//!
//! Timing axes: the cut of pass 1 lands within 100 ms of X, at least 400 ms
//! before its next frame. The re-INVITE reaches b2 200 ms after the proxy
//! reads b1 dead, the callee's BYE b1 200 ms after it reads b1 alive, and the
//! callee's 487 follows the BYE 500 ms later. The heal comes 500 ms after
//! the 487, well inside the puller's 10 s hard timer, with the copy not yet
//! shed and its round answered; pass 2 catches up on its own `Noop`. Every
//! step is 100 ms or more apart, so no verdict depends on in-order draining.
//!
//! Beyond the failover sweep: a takeover copy across two cold bootstrap passes.

use std::net::SocketAddr;
use std::time::Duration;

use failover_harness::{
    total_cdrs_for, FailoverHarness, Partition, ProxySut, ReplicatedB2buaSut, WorkerHealth,
};
use repl_net::frame::Frame;
use repl_net_testkit::Direction;
use scenario_harness::Agent;

use crate::away_backup_support::{
    answered_with, bring_up, callee_hangs_up, caller_hangs_up, pod_deleted, Answered, ALICE, BOB,
    OFFER,
};
use crate::call_ended_between_two_bootstrap_passes::{
    cold_pulls, noops_on, FRAME_DELAY_MS, PAST_THE_REPLICA_TTL,
};
use crate::departure_support::call_id_placed_on;

/// Past b1's dead-peer TTL, which drops its change log toward an unreached
/// peer.
const PAST_DEAD_PEER: Duration = Duration::from_secs(100);

/// The calls the `Data` frames of the stream whose local end is `local`
/// carried, in order.
fn carried_on(fh: &FailoverHarness, local: SocketAddr) -> Vec<String> {
    fh.repl_report()
        .frames
        .iter()
        .filter(|f| f.dir == Direction::Received && f.from == local)
        .filter_map(|f| match &f.frame {
            Frame::Data { call_ref, .. } => Some(call_ref.clone()),
            _ => None,
        })
        .collect()
}

/// The BYEs `node` sent inside a dialog of `call`, toward either party.
fn byes_of(fh: &FailoverHarness, node: SocketAddr, call: &Answered) -> usize {
    let tags = [call.dialog.local_tag(), call.bob_dialog.local_tag()];
    fh.sip_entries()
        .iter()
        .filter(|e| e.from == node && e.opens_with("BYE "))
        .filter(|e| {
            let raw = String::from_utf8_lossy(&e.raw);
            tags.iter().any(|t| raw.contains(t))
        })
        .count()
}

/// A call through the proxy answered on b1, whose Call-ID places it there.
async fn on_b1(
    fh: &FailoverHarness,
    alice: &Agent,
    bob: &Agent,
    proxy: &ProxySut,
    b1: &ReplicatedB2buaSut,
    label: &str,
) -> Answered {
    let cid = call_id_placed_on("b1", label);
    answered_with(fh, alice, bob, proxy, b1, Some(&cid)).await
}

#[tokio::test(start_paused = true)]
async fn a_takeover_copy_live_across_two_bootstrap_passes_stands_down_on_the_end_of_its_call() {
    let name = "takeover-copy-across-two-passes";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (mut proxy, mut b1, b2) = bring_up(&mut fh).await;
    proxy.tell_no_dead_verdict();
    let mut calls = [
        on_b1(&fh, &alice, &bob, &proxy, &b1, "x").await,
        on_b1(&fh, &alice, &bob, &proxy, &b1, "y").await,
    ];
    for call in &calls {
        assert_eq!(call.cookie, ("b1".to_string(), "b2".to_string()), "b1 serves, b2 backs up");
    }

    fh.partition("b1", "b2");
    let mut short = on_b1(&fh, &alice, &bob, &proxy, &b1, "short").await;
    caller_hangs_up(&mut short.dialog, &bob).await;
    fh.advance(PAST_DEAD_PEER).await;
    let earlier = cold_pulls(&fh, "b2", Partition::Bak).len();
    fh.heal("b1", "b2");
    fh.delay("b1", "b2", FRAME_DELAY_MS);
    let landed = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(10), async || {
            let pulls = cold_pulls(&fh, "b2", Partition::Bak);
            pulls.get(earlier).is_some_and(|pass1| !carried_on(&fh, *pass1).is_empty())
        })
        .await;
    assert!(landed, "b2's stream is reset, and its cold pass 1 carries a call");
    let pass1 = cold_pulls(&fh, "b2", Partition::Bak)[earlier];
    let first = carried_on(&fh, pass1);
    assert_eq!(first.len(), 1, "pass 1 carried one call before the cut: {first:?}");
    assert_eq!(noops_on(&fh, pass1), 0, "pass 1 has not caught up when it is cut");
    fh.cut_streams("b1", "b2");
    let x = calls.iter().position(|c| c.call_ref == first[0]).expect("pass 1 carried X or Y");
    let x_ref = calls[x].call_ref.clone();
    let y_ref = calls[1 - x].call_ref.clone();

    proxy.set_health("b1", WorkerHealth::Dead);
    fh.advance(Duration::from_millis(200)).await;
    let mut reinvite = calls[x].dialog.reinvite(Some(OFFER)).await;
    let mut at_bob = bob.receive_tolerating("INVITE", &["OPTIONS"]).await;
    at_bob.respond(100, "Trying").await;
    reinvite.expect(100).await;
    fh.advance(Duration::from_millis(300)).await;
    assert!(b2.serves(&x_ref), "b2's takeover copy relays the re-INVITE");
    proxy.set_health("b1", WorkerHealth::Alive);
    fh.advance(Duration::from_millis(200)).await;
    callee_hangs_up(&mut calls[x].bob_dialog, &alice).await;
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(total_cdrs_for(&[&b1], &x_ref), 1, "X ended on b1, its CDR there");
    at_bob.respond(487, "Request Terminated").await;
    bob.receive_absorbing("ACK", &["INVITE"]).await;
    reinvite.expect(487).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(b2.serves(&x_ref), "the copy is live at the heal, its round answered");

    fh.heal("b1", "b2");
    let caught_up = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(5), async || {
            let pulls = cold_pulls(&fh, "b2", Partition::Bak);
            pulls.get(earlier + 1..).is_some_and(|p| p.iter().any(|s| noops_on(&fh, *s) > 0))
        })
        .await;
    assert!(caught_up, "pass 2 caught up on its own Noop");
    assert_eq!(b2.scan_backed_up("b1"), vec![y_ref.clone()], "b2 holds b1's live set: Y");
    fh.advance(Duration::from_millis(200)).await;
    assert!(!b2.serves(&x_ref), "the copy stands down on the delete");
    assert_eq!(b2.metrics().repl_stand_down_total(), 1);

    pod_deleted(&mut fh, &mut b1);
    proxy.set_health("b1", WorkerHealth::Dead);
    fh.advance(Duration::from_secs(1)).await;
    caller_hangs_up(&mut calls[1 - x].dialog, &bob).await;
    fh.pump_until(Duration::from_secs(30), PAST_THE_REPLICA_TTL, async || false).await;
    assert!(
        b2.memory_clean() && !b2.holds_any_trace(&x_ref).await && !b2.holds_any_trace(&y_ref).await,
        "b2 releases both calls, past the replica's TTL"
    );
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    let nodes = [&b1, &b2];
    assert_eq!(total_cdrs_for(&nodes, &y_ref), 1, "Y served by b2 with one CDR");
    assert_eq!(byes_of(&fh, b2.sip_addr(), &calls[x]), 0, "b2 sends no BYE inside X's dialogs");
    assert_eq!(total_cdrs_for(&nodes, &x_ref), 1, "X keeps its one CDR, b1's");
    fh.assert_sip_rfc_clean(name);
}
