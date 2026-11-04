//! The harness verbs of ADR-0055 §13: a call placed on a named primary and
//! backup, a turn held at each of its sub-points, the parties' traffic steered
//! to a named role through the proxy, and the nodes named by role.

use std::time::Duration;

use failover_harness::cluster::{Cluster, ClusterRecipe};
use failover_harness::sweep::{ALICE, BOB};
use failover_harness::{assert_call_fully_over, Cause, Released, Role, TurnAt, TurnPoint};
use scenario_harness::{Agent, ClientInvite, Dialog};
use sip_message::generators::InDialogMethod;

use crate::call_terminate_on_backup::{ANSWER, OFFER};

async fn cluster(name: &str) -> (Cluster, Agent, Agent) {
    let recipe = ClusterRecipe::routing_to(BOB.parse().unwrap());
    let cluster = Cluster::start(name, &recipe).await;
    let alice = cluster.fh.agent("alice", ALICE).await;
    let bob = cluster.fh.agent("bob", BOB).await;
    (cluster, alice, bob)
}

/// Answer the INVITE bob holds and confirm the call: (alice's, bob's) dialogs.
async fn answer(
    call: &mut ClientInvite,
    uas: &mut scenario_harness::ServerTxn,
    bob: &Agent,
) -> (Dialog, Dialog) {
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    bob.receive("ACK").await;
    (dialog, uas.dialog())
}

/// The caller hangs up and the cluster forgets the call: one CDR, nothing left.
async fn hang_up(cluster: &Cluster, dialog: &mut Dialog, bob: &Agent, alice: &Agent) {
    let mut bye = dialog.bye().await;
    bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    let call_ref = cluster.fh.turn_points().first_call().expect("a call took a turn").0;
    let drained = cluster.fh.settle_terminal(async || cluster.is_clear_of(&call_ref).await).await;
    assert!(drained, "the cluster forgets the call");
    cluster.fh.linger_peers(&[alice, bob], Duration::from_secs(3)).await;
    assert_call_fully_over(&cluster.nodes(), &call_ref, &cluster.limiter).await;
}

/// A call placed on either worker lands there: its first request reaches the
/// callee under the cookie naming that primary and the other worker as backup.
#[tokio::test(start_paused = true)]
async fn a_call_lands_on_the_primary_it_is_placed_on() {
    for primary in ["b1", "b2"] {
        let (cluster, alice, bob) = cluster(&format!("placed-on-{primary}")).await;
        let placement = cluster.place_next_call(&alice, primary);
        let backup = if primary == "b1" { "b2" } else { "b1" };
        assert_eq!(placement.backup, backup);
        let mut call =
            alice.invite(&bob).with_sdp(OFFER).through(cluster.proxy.addr()).send().await;
        let mut uas = bob.receive("INVITE").await;
        Cluster::assert_placed(&placement, uas.request());
        let (mut dialog, _) = answer(&mut call, &mut uas, &bob).await;
        hang_up(&cluster, &mut dialog, &bob, &alice).await;
    }
}

/// A turn held where its write is handed to replication sends nothing: the
/// INVITE reaches the callee only once the turn is released.
#[tokio::test(start_paused = true)]
async fn a_turn_held_where_its_write_is_logged_has_sent_nothing() {
    let (cluster, alice, bob) = cluster("held-at-logged").await;
    cluster.place_next_call(&alice, "b1");
    let reached = cluster.fh.turn_points().arm(TurnAt { turn: 1, point: TurnPoint::Logged });
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(cluster.proxy.addr()).send().await;
    let held = reached.await.expect("the first turn is reached");
    assert_eq!(held.node, "b1", "the turn is held at the primary");
    cluster.fh.advance(Duration::from_millis(300)).await;
    assert!(bob.take_queued().await.is_none(), "nothing of the held turn left the primary");
    held.release();
    let mut uas = bob.receive("INVITE").await;
    let (mut dialog, _) = answer(&mut call, &mut uas, &bob).await;
    hang_up(&cluster, &mut dialog, &bob, &alice).await;
}

/// The applied sub-point of a turn is reached at the backup, once it holds
/// the turn's write; under the `applied` sync level the turn's effects wait
/// for it, and leave once the backup reported applying (ADR-0055 §3, §13).
#[tokio::test(start_paused = true)]
async fn a_turns_applied_sub_point_is_the_backup_holding_its_write() {
    let (cluster, alice, bob) = cluster("held-at-applied").await;
    let placement = cluster.place_next_call(&alice, "b1");
    let reached = cluster.fh.turn_points().arm(TurnAt { turn: 1, point: TurnPoint::Applied });
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(cluster.proxy.addr()).send().await;
    let held = reached.await.expect("the backup applies the first turn's write");
    assert_eq!(held.node, placement.backup);
    assert!(
        cluster.node(&placement.backup).scan_backed_up(&placement.primary).contains(&held.call_ref),
        "the backup holds the call's replica"
    );
    assert!(bob.take_queued().await.is_none(), "the turn's INVITE waits for the backup");
    let call_ref = held.call_ref.clone();
    held.release();
    let mut uas = bob.receive("INVITE").await;
    let turn = cluster.fh.turn_points().turns(&call_ref)[0];
    assert_eq!(turn.released, Some(Released::Applied), "it left on the backup's report");
    let (mut dialog, _) = answer(&mut call, &mut uas, &bob).await;
    hang_up(&cluster, &mut dialog, &bob, &alice).await;
}

/// Held at its applied sub-point past the cap, the turn's effects leave at the
/// cap, and the harness's own hold is named as the cause.
#[tokio::test(start_paused = true)]
async fn a_cap_release_behind_the_harnesss_applied_hold_is_flagged_as_its_own() {
    let (cluster, alice, bob) = cluster("capped-at-applied").await;
    cluster.place_next_call(&alice, "b1");
    let reached = cluster.fh.turn_points().arm(TurnAt { turn: 1, point: TurnPoint::Applied });
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(cluster.proxy.addr()).send().await;
    let held = reached.await.expect("the backup applies the first turn's write");
    let call_ref = held.call_ref.clone();
    cluster.fh.advance(Duration::from_millis(150)).await;
    let mut uas = bob.receive("INVITE").await;
    let turn = cluster.fh.turn_points().turns(&call_ref)[0];
    assert_eq!(turn.released, Some(Released::Capped), "it left at the cap");
    assert!(turn.capped_by_harness, "the harness's applied hold caused the cap release");
    held.release();
    let (mut dialog, _) = answer(&mut call, &mut uas, &bob).await;
    hang_up(&cluster, &mut dialog, &bob, &alice).await;
}

/// Steered to the backup by the primary's health, the caller's next request
/// is served by the backup's copy of the call; restored, the primary serves
/// the hang-up.
#[tokio::test(start_paused = true)]
async fn a_request_steered_by_health_is_served_by_the_backups_copy() {
    let (mut cluster, alice, bob) = cluster("steered-by-health").await;
    let placement = cluster.place_next_call(&alice, "b1");
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(cluster.proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (mut dialog, _) = answer(&mut call, &mut uas, &bob).await;
    cluster.fh.advance(Duration::from_millis(500)).await;
    let call_ref = cluster.fh.turn_points().first_call().expect("a call").0;
    assert!(cluster.view(&placement, Role::Copy, &call_ref).is_none(), "no copy before the steer");

    cluster.steer(&placement, Role::Backup, Cause::Health);
    let mut info = dialog.request(InDialogMethod::Info, None).await;
    bob.receive("INFO").await.respond(200, "OK").await;
    info.expect(200).await;
    assert!(
        cluster.view(&placement, Role::Copy, &call_ref).is_some(),
        "the backup's copy served it"
    );

    cluster.restore(&placement, Role::Primary);
    cluster.fh.advance(Duration::from_millis(500)).await;
    hang_up(&cluster, &mut dialog, &bob, &alice).await;
}

/// A turn that ends the call hands the call's delete to replication before
/// any of its effects leaves (ADR-0055 §3): held where its write is logged,
/// the turn counts as one that wrote, and the backup drops the call's replica.
#[tokio::test(start_paused = true)]
async fn a_terminal_turn_logs_its_delete_before_its_effects_leave() {
    let (cluster, alice, bob) = cluster("terminal-turn-logged").await;
    let placement = cluster.place_next_call(&alice, "b1");
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(cluster.proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (mut dialog, _) = answer(&mut call, &mut uas, &bob).await;
    cluster.fh.advance(Duration::from_millis(500)).await;
    let call_ref = cluster.fh.turn_points().first_call().expect("a call").0;
    let backup = cluster.node(&placement.backup);
    assert!(backup.scan_backed_up(&placement.primary).contains(&call_ref));

    let mut bye = dialog.bye().await;
    let mut bye_in = bob.receive_tolerating("BYE", &["OPTIONS"]).await;
    let terminal = cluster.fh.turn_points().turns(&call_ref).len() + 1;
    let reached = cluster.fh.turn_points().arm(TurnAt { turn: terminal, point: TurnPoint::Logged });
    bye_in.respond(200, "OK").await;
    let held = reached.await.expect("the terminal turn is reached");
    assert_eq!(held.node, placement.primary, "the turn is held at the primary");
    cluster.fh.advance(Duration::from_millis(300)).await;
    let turn = cluster.fh.turn_points().turns(&call_ref)[terminal - 1];
    assert!(turn.wrote, "the terminal turn wrote the call's delete");
    let cdrs = failover_harness::total_cdrs_for(&cluster.nodes(), &call_ref);
    assert_eq!(cdrs, 1, "the CDR was enqueued before the delete");
    assert!(
        !backup.scan_backed_up(&placement.primary).contains(&call_ref),
        "the backup applied the delete while the turn's effects wait"
    );
    held.release();
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    let drained = cluster.fh.settle_terminal(async || cluster.is_clear_of(&call_ref).await).await;
    assert!(drained, "the cluster forgets the call");
    cluster.fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_call_fully_over(&cluster.nodes(), &call_ref, &cluster.limiter).await;
}
