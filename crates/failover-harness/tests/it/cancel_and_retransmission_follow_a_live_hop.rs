//! **A CANCEL and a retransmission go where their transaction lives.** The
//! front proxy sends a CANCEL to the node its INVITE went to while that node
//! is alive (RFC 3261 §9.1, §16.10), and repeats a retransmission's target
//! only while that target is alive: a retransmission whose first copy went
//! to a node now dead is placed again by the call's cookie.
//!
//! ```text
//!   cancel   the proxy reads P dead (P keeps running) · the caller's
//!            re-INVITE → C, relayed, the callee's 100 · the proxy reads P
//!            alive again · the caller's CANCEL → C: 200 + 487, the callee's
//!            side cancelled and its 487 ACKed
//!   rtx      the callee's 2xx to the initial INVITE replicates but never
//!            reaches the caller · P is killed, the peers are not told · the
//!            caller's INVITE retransmission → C, which answers the 2xx it
//!            holds; the caller's ACK reaches the callee once
//! ```
//!
//! Every cell ends with the caller's BYE, one CDR and clean memory.

use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use failover_harness::{worker_ordinals, FailoverHarness, WorkerHealth};
use sip_message::Method;

use crate::departure_support::{bring_up_deciding, bring_up_reactive, placed, CountedDecision};
use crate::inflight_resend::{
    assert_not_taken_over, assert_taken_over_once, bring_up, establish, kill_serving_node,
    last_request_sent, reboot_and_reclaim, round_invites_sent_to, settle_to_one_cdr, split_mut,
    survivor_of, synchronized_call_ref, REOFFER, REPLICATE,
};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

/// The proxy misreads the primary dead while it holds the caller's INVITE
/// undecided; the caller's retransmission then follows the cookie to the
/// backup, which holds no answered record of the call and drops the copy
/// rather than dial a second call. The primary decides, dials the callee
/// once and answers the caller: one b-leg INVITE, one CDR. Told or not, the
/// false dead verdict leaves the undecided call alone (ADR-0047 D4).
#[tokio::test(start_paused = true)]
async fn a_retransmission_sent_to_the_backup_of_a_live_primary_dials_no_second_call() {
    retransmission_to_the_backup_of_a_live_primary(
        "retransmission-to-the-backup-of-a-live-primary",
        false,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_dead_verdict_on_a_live_primary_leaves_its_undecided_call_alone() {
    retransmission_to_the_backup_of_a_live_primary("live-primary-read-dead-undecided", true).await;
}

async fn retransmission_to_the_backup_of_a_live_primary(name: &str, told: bool) {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let route = std::sync::Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let (decision, _consults) = CountedDecision::counting(route, Duration::from_secs(3));
    let (mut proxy, w_b1, w_b2) = bring_up_deciding(&mut fh, decision).await;
    if !told {
        proxy.tell_no_dead_verdict();
    }

    // ── the caller hears nothing; the primary holds the INVITE undecided ────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    fh.cut_signalling("alice", alice.addr());
    let (pri_ord, _call_ref) = placed(&fh, &w_b1, &w_b2).await;
    fh.mark(&pri_ord, None, "misread dead", "the proxy reads the live primary dead");
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    fh.restore_signalling("alice", alice.addr());

    // ── the caller's Timer A rung reaches the backup ────────────────────────
    let (wire, dst) = last_request_sent(&fh, alice.addr(), &Method::Invite);
    alice.try_send_datagram(&wire, dst).await.expect("the caller's re-send leaves");
    fh.advance(Duration::from_millis(200)).await;
    proxy.set_health(&pri_ord, WorkerHealth::Alive);

    // ── the primary decides and dials once; the caller is answered ──────────
    let mut uas = bob.receive_tolerating("INVITE", &["OPTIONS"]).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive_tolerating("ACK", &["OPTIONS", "INVITE"]).await;
    fh.advance(Duration::from_secs(1)).await;
    assert_eq!(round_invites_sent_to(&fh, bob.addr(), OFFER), 1, "one b-leg INVITE");
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    assert!(
        survivor.metrics().foreign_invite_copies_dropped_total() >= 1,
        "the backup dropped the copy"
    );
    assert_eq!(survivor.metrics().dead_verdicts_taken_total(), u64::from(told));
    assert_eq!(survivor.metrics().repl_takeover_hydrated_total(), 0, "the backup took nothing");
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    let drained = fh.settle_terminal(async || w_b1.memory_clean() && w_b2.memory_clean()).await;
    assert!(drained, "the cluster drains the call");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    let cdrs: usize = [&w_b1, &w_b2].iter().map(|n| n.cdr_records().len()).sum();
    assert_eq!(cdrs, 1, "one CDR");
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}

/// After the misread above, the primary reads alive again and relays the
/// callee's 180; the caller's CANCEL goes where its INVITE went, the live
/// primary, not to the backup the dropped retransmission was sent to. One
/// 487 reaches the caller and is ACKed; the backup takes nothing over, told
/// the false dead verdict or not.
#[tokio::test(start_paused = true)]
async fn a_cancel_after_a_misread_retransmission_goes_to_the_live_primary() {
    cancel_after_a_misread_retransmission("cancel-after-a-misread-retransmission", false).await;
}

#[tokio::test(start_paused = true)]
async fn a_cancel_after_a_told_misread_goes_to_the_live_primary() {
    cancel_after_a_misread_retransmission("cancel-after-a-told-misread", true).await;
}

async fn cancel_after_a_misread_retransmission(name: &str, told: bool) {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let route = std::sync::Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let (decision, _consults) = CountedDecision::counting(route, Duration::from_secs(3));
    let (mut proxy, w_b1, w_b2) = bring_up_deciding(&mut fh, decision).await;
    if !told {
        proxy.tell_no_dead_verdict();
    }

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    fh.cut_signalling("alice", alice.addr());
    let (pri_ord, _call_ref) = placed(&fh, &w_b1, &w_b2).await;
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    fh.restore_signalling("alice", alice.addr());
    let (wire, dst) = last_request_sent(&fh, alice.addr(), &Method::Invite);
    alice.try_send_datagram(&wire, dst).await.expect("the caller's re-send leaves");
    fh.advance(Duration::from_millis(200)).await;
    proxy.set_health(&pri_ord, WorkerHealth::Alive);
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    let hydrated = survivor.metrics().repl_takeover_hydrated_total();

    let mut uas = bob.receive_tolerating("INVITE", &["OPTIONS"]).await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    fh.advance(Duration::from_millis(500)).await;
    let mut cancel = call.cancel().await;
    cancel.expect(200).await;
    bob.receive_absorbing("CANCEL", &["INVITE", "OPTIONS"]).await.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    call.expect(487).await;
    bob.receive_absorbing("ACK", &["INVITE", "OPTIONS"]).await;
    fh.advance(Duration::from_secs(5)).await;

    let finals: std::collections::HashSet<_> = fh
        .sip_entries()
        .into_iter()
        .filter(|e| e.to == alice.addr() && e.opens_with("SIP/2.0 487"))
        .map(|e| e.raw)
        .collect();
    assert_eq!(finals.len(), 1, "one 487 reaches the caller");
    assert_eq!(
        survivor.metrics().repl_takeover_hydrated_total(),
        hydrated,
        "the backup took nothing over"
    );
    assert_eq!(survivor.metrics().dead_verdicts_taken_total(), u64::from(told));
    let drained = fh.settle_terminal(async || w_b1.memory_clean() && w_b2.memory_clean()).await;
    assert!(drained, "the cluster drains the call");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    let cdrs: usize = [&w_b1, &w_b2].iter().map(|n| n.cdr_records().len()).sum();
    assert_eq!(cdrs, 1, "one CDR");
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}

/// The answered call's 2xx is lost on its way to the caller and its node is
/// killed; the front proxy restarts and has no memo of the INVITE, so the
/// caller's retransmission is placed afresh on the backup with a cookie of
/// its own. The backup holds the call's answered replica: it answers the
/// copy with the replicated 2xx and dials the callee no second time.
#[tokio::test(start_paused = true)]
async fn a_retransmission_placed_afresh_on_the_backup_draws_the_replicated_2xx() {
    let name = "retransmission-placed-afresh-on-the-backup";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut w_b1, mut w_b2) = bring_up_reactive(&mut fh).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    fh.cut_signalling("alice", alice.addr());
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak) = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    fh.advance(REPLICATE).await;
    let call_ref = survivor_of(&pri_ord, &w_b1, &w_b2)
        .scan_one_backed_up(&pri_ord)
        .await
        .expect("the answered call replicated");

    // ── P is killed; the front proxy restarts and its memo is lost ──────────
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let hydrated_before = kill_serving_node(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    drop(proxy);
    fh.advance(Duration::from_millis(10)).await;
    let proxy = crate::departure_support::spawn_front_proxy(&fh).await;
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    fh.restore_signalling("alice", alice.addr());

    // ── the caller's re-send is placed afresh on the backup ─────────────────
    let (wire, dst) = last_request_sent(&fh, alice.addr(), &Method::Invite);
    alice.try_send_datagram(&wire, dst).await.expect("the caller's re-send leaves");
    call.expect(200).await;
    assert_taken_over_once(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before);
    let mut dialog = call.ack().await;
    bob.receive_tolerating("ACK", &["INVITE"]).await;
    fh.advance(Duration::from_secs(4)).await;
    assert_eq!(round_invites_sent_to(&fh, bob.addr(), OFFER), 1, "one b-leg INVITE");

    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(60)).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}

/// The caller's CANCEL of a re-INVITE round the takeover copy relays reaches
/// that copy, though the proxy reads the primary alive again by then.
#[tokio::test(start_paused = true)]
async fn a_cancel_follows_its_reinvite_to_the_copy_after_the_primary_reads_alive_again() {
    let name = "cancel-follows-its-reinvite-to-the-copy";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, w_b1, w_b2) = bring_up(&mut fh).await;
    let (mut dialog, pri_ord) = establish(&fh, &alice, &bob, &proxy).await;
    let call_ref = synchronized_call_ref(&pri_ord, &w_b1, &w_b2).await;
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    let hydrated_before = survivor.metrics().repl_takeover_hydrated_total();

    // ── the proxy reads P dead; the round goes to C, which relays it ────────
    fh.mark(&pri_ord, None, "misread dead", "the proxy reads the running primary dead");
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    let mut reinvite = dialog.reinvite(Some(REOFFER)).await;
    reinvite.expect(100).await;
    let mut at_bob = bob.receive("INVITE").await;
    at_bob.respond(100, "Trying").await;
    fh.advance(REPLICATE).await;
    assert_taken_over_once(survivor, hydrated_before);

    // ── the proxy reads P alive again; the caller CANCELs the round ─────────
    fh.mark(&pri_ord, None, "alive", "the proxy reads the primary alive again");
    proxy.set_health(&pri_ord, WorkerHealth::Alive);
    let mut cancel = reinvite.cancel().await;
    cancel.expect(200).await;
    bob.receive_absorbing("CANCEL", &["INVITE"]).await.respond(200, "OK").await;
    at_bob.respond(487, "Request Terminated").await;
    reinvite.expect(487).await;
    bob.receive_absorbing("ACK", &["INVITE"]).await;
    assert_eq!(round_invites_sent_to(&fh, bob.addr(), REOFFER), 1, "one b-leg INVITE");

    // ── the call lives on and is hung up ─────────────────────────────────────
    fh.advance(Duration::from_secs(1)).await;
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}

/// The caller's initial INVITE retransmitted after its node was killed
/// unannounced reaches the backup, which answers it with the replicated 2xx.
#[tokio::test(start_paused = true)]
async fn an_initial_invite_retransmitted_after_an_unannounced_kill_draws_the_replicated_2xx() {
    let name = "initial-invite-retransmitted-after-a-kill";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut w_b1, mut w_b2) = bring_up_reactive(&mut fh).await;

    // ── the caller hears nothing: her 100 and her 2xx are lost ───────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    fh.cut_signalling("alice", alice.addr());
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak) = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    fh.advance(REPLICATE).await;
    let call_ref = survivor_of(&pri_ord, &w_b1, &w_b2)
        .scan_one_backed_up(&pri_ord)
        .await
        .expect("the answered call replicated");

    // ── P is killed, the peers are not told ─────────────────────────────────
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let hydrated_before = kill_serving_node(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    fh.restore_signalling("alice", alice.addr());
    assert_not_taken_over(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before, &call_ref);

    // ── the caller's Timer A rung reaches the backup ────────────────────────
    let (wire, dst) = last_request_sent(&fh, alice.addr(), &Method::Invite);
    alice.try_send_datagram(&wire, dst).await.expect("the caller's re-send leaves");
    call.expect(200).await;
    assert_taken_over_once(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before);
    let mut dialog = call.ack().await;
    bob.receive_tolerating("ACK", &["INVITE"]).await;

    // ── the caller hangs up; the primary returns and meets the record ───────
    fh.advance(Duration::from_secs(4)).await;
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(60)).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}
