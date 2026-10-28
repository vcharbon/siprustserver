//! **A new call whose node dies while its routing decision is in flight.** The
//! caller heard the node's 100 Trying through the stateless front proxy
//! (RFC 3261 §16.11), so its client transaction is `Proceeding`: it runs no
//! Timer B and sends nothing more (§17.1.1.2). The call reached its backup at
//! its birth; the decision did not, and dies with the node that asked for it.
//!
//! ```text
//!   alice ─INVITE─▶ proxy ─▶ primary ──(decision in flight)       backup: the born call
//!   alice ◀──100── proxy ◀── primary
//!                            ✗ primary killed ✗
//!   departure announced → backup takes the call over, answers 503 → alice ACKs
//!   the primary departed: the backup writes the one CDR and deletes the call
//!   primary restarts: nothing of the call is left to reclaim
//! ```
//!
//! The caller gets a 503 with no Retry-After (its next hop is the proxy, which
//! places a retry on a live worker at once); the decision is never asked
//! again, and nothing reaches the callee. Three variants: a CANCEL with the
//! departure unannounced is answered by the backup the CANCEL takes the call
//! over to; a primary restarted in place, the departure never announced,
//! reclaims the call and answers it 503 itself; and a caller retransmission the
//! proxy placed on the survivor as a call of its own before the departure is
//! announced is not answered on that call's transaction.

use std::sync::atomic::Ordering;
use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use failover_harness::{
    assert_call_fully_released, total_cdrs_for, FailoverHarness, Transitional, WorkerHealth,
};
use scenario_harness::agent::Agent;
use sip_message::header::HeaderName;

use crate::departure_support::{
    bring_up_deciding, call_id_placed_on, kill_announced, placed, spawn_front_proxy,
    CountedDecision,
};
use crate::inflight_resend::{reboot_and_reclaim, settle_to_one_cdr, split_mut, survivor_of};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

/// How long the routing decision takes to answer: longer than every step a
/// cell takes before the kill.
const DECISION_DELAY: Duration = Duration::from_secs(3);
/// How soon after the departure the caller's final arrives.
const ANSWER_BOUND: Duration = Duration::from_millis(500);

/// Whether anything was sent to `agent`'s address this run.
fn reached(fh: &FailoverHarness, agent: &Agent) -> bool {
    fh.sip_entries().iter().any(|e| e.to == agent.addr())
}

/// How many distinct finals starting `status_line` reached `agent` this run:
/// a retransmission, byte for byte the same, counts once.
fn finals_to(fh: &FailoverHarness, agent: &Agent, status_line: &[u8]) -> usize {
    fh.sip_entries()
        .iter()
        .filter(|e| e.to == agent.addr() && e.start_line().as_bytes().starts_with(status_line))
        .map(|e| e.raw.clone())
        .collect::<std::collections::HashSet<_>>()
        .len()
}

/// How many 503 responses reached `agent` this run.
fn rejections_to(fh: &FailoverHarness, agent: &Agent) -> usize {
    fh.sip_entries().iter().filter(|e| e.to == agent.addr() && e.opens_with("SIP/2.0 503")).count()
}

#[tokio::test(start_paused = true)]
async fn an_undecided_call_whose_node_departs_is_answered_503_by_its_backup() {
    let mut fh = FailoverHarness::new("undecided-call-departure", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let route = std::sync::Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let (decision, consults) = CountedDecision::counting(route, DECISION_DELAY);
    let (proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, decision).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;

    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let killed_at = fh.now_ms();
    let hydrated_before = kill_announced(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    let rejected = call.expect(503).await;
    assert!(
        fh.now_ms() - killed_at <= ANSWER_BOUND.as_millis() as i64,
        "the caller's final follows the departure at once",
    );
    assert!(!rejected.has(&HeaderName::RetryAfter), "no Retry-After: the proxy places a retry");
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    assert_eq!(
        survivor.metrics().repl_takeover_hydrated_total(),
        hydrated_before + 1,
        "the backup took the call over",
    );

    // The caller's ACK completes the transaction: no 503 is repeated.
    fh.advance(Duration::from_secs(5)).await;
    assert_eq!(rejections_to(&fh, &alice), 1, "the caller's ACK is absorbed, nothing repeats");
    assert!(!reached(&fh, &bob), "the callee is never dialled");
    assert_eq!(consults.load(Ordering::SeqCst), 1, "the decision is asked once");

    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    assert_eq!(consults.load(Ordering::SeqCst), 1, "the reclaim asks no decision");
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}

#[tokio::test(start_paused = true)]
async fn a_cancel_of_an_undecided_call_whose_node_died_is_answered_by_its_backup() {
    let mut fh = FailoverHarness::new("undecided-call-cancel-unannounced", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let route = std::sync::Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let (decision, consults) = CountedDecision::counting(route, DECISION_DELAY);
    let (mut proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, decision).await;
    // The reactive path: the proxy tells no dead verdict (ADR-0050 §3).
    proxy.tell_no_dead_verdict();

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;

    // The proxy reads the node dead; the peers are not told (ADR-0031 case 3a).
    let (primary, _survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    fh.mark(&pri_ord, None, "crash", "node down, departure unannounced");
    primary.crash();
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(300)).await;

    let mut cancel = call.cancel().await;
    cancel.expect(200).await;
    call.expect(487).await;
    fh.advance(Duration::from_secs(5)).await;
    assert!(!reached(&fh, &bob), "the callee is never dialled");
    assert_eq!(consults.load(Ordering::SeqCst), 1, "the decision is asked once");

    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}

#[tokio::test(start_paused = true)]
async fn an_undecided_call_reclaimed_by_its_restarted_node_is_answered_503() {
    let mut fh = FailoverHarness::new("undecided-call-restart-in-place", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let route = std::sync::Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let (decision, consults) = CountedDecision::counting(route, DECISION_DELAY);
    let (mut proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, decision).await;
    // The reactive path: the proxy tells no dead verdict (ADR-0050 §3).
    proxy.tell_no_dead_verdict();

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;

    // Restarted in place: the endpoint never leaves the membership (ADR-0031
    // case 4), so no departure is announced; the restart reclaims the call.
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    fh.mark(&pri_ord, None, "crash", "container restarts in place");
    primary.crash();
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    let hydrated_before = survivor.metrics().repl_takeover_hydrated_total();
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    // The harness reads the caller's queue only now, so the 503 has repeated on
    // the §17.2.1 ladder meanwhile; the caller's ACK ends the repeats.
    let rejected = call.expect(503).await;
    assert!(!rejected.has(&HeaderName::RetryAfter), "no Retry-After: the proxy places a retry");
    assert_eq!(
        survivor_of(&pri_ord, &w_b1, &w_b2).metrics().repl_takeover_hydrated_total(),
        hydrated_before,
        "the backup took nothing over",
    );
    let heard = rejections_to(&fh, &alice);
    fh.advance(Duration::from_secs(5)).await;
    assert_eq!(rejections_to(&fh, &alice), heard, "the caller's ACK is absorbed, nothing repeats");
    assert!(!reached(&fh, &bob), "the callee is never dialled");
    assert_eq!(consults.load(Ordering::SeqCst), 1, "the restarted node asks no decision");
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}

/// The caller's retransmission reaches the survivor through a front proxy that
/// lost its retransmission memo (restarted in place: same address, so the same
/// Via sent-by and, §16.11, the same branch): the survivor holds it as a call of
/// its own, on the very transaction the departed call's INVITE names. The
/// departure then announced, the sweep settles the departed call with nothing
/// on the wire: its 503 would answer the caller's live call.
#[tokio::test(start_paused = true)]
async fn a_retransmission_placed_on_the_survivor_is_not_answered_for_the_departed_call() {
    let mut fh = FailoverHarness::new("undecided-call-retransmission-replaced", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let route = std::sync::Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let (decision, consults) = CountedDecision::counting(route, DECISION_DELAY);
    let (mut proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, decision).await;
    // The reactive path: the proxy tells no dead verdict (ADR-0050 §3).
    proxy.tell_no_dead_verdict();

    // The node's 100 never reaches the caller, so it retransmits (§17.1.1.2).
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    fh.cut_signalling("alice", alice.addr());
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;
    let (primary, _survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    fh.mark(&pri_ord, None, "crash", "node down, departure not yet announced");
    primary.crash();
    fh.mark("proxy", None, "restart", "the front proxy restarts in place, its memo lost");
    drop(proxy);
    fh.advance(Duration::from_millis(10)).await;
    let mut proxy = spawn_front_proxy(&fh).await;
    proxy.tell_no_dead_verdict();
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    fh.restore_signalling("alice", alice.addr());
    fh.advance(Duration::from_millis(400)).await;

    call.retransmit().await;
    call.expect(100).await;
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    assert_eq!(survivor.active_calls(), 1, "the retransmission is a call of its own there");
    survivor.simulate_peer_removed(&pri_ord);
    fh.advance(Duration::from_millis(300)).await;

    // The caller's call completes on the survivor: nothing of the departed
    // call reaches the caller.
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(OFFER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    assert_eq!(rejections_to(&fh, &alice), 0, "no 503 left for the departed call");
    assert_eq!(consults.load(Ordering::SeqCst), 2, "each placement asked its own decision");
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;

    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}

/// The abrupt removal (ADR-0031 case 2): the endpoint leaves the membership
/// at once while the process lives two more seconds, and a replacement of the
/// same ordinal comes up beside it. The survivor's departure sweep answers the
/// caller and writes the ended call's one CDR; the old process dies before its
/// decision returns. One final, one CDR, one
/// consult.
#[tokio::test(start_paused = true)]
async fn an_undecided_call_of_a_node_replaced_at_once_is_answered_once() {
    let mut fh = FailoverHarness::new("undecided-call-abrupt-replacement", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let route = std::sync::Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let (decision, consults) = CountedDecision::counting(route, DECISION_DELAY);
    let (proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, decision).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;

    fh.withdraw(&pri_ord);
    let rejected = call.expect(503).await;
    assert!(!rejected.has(&HeaderName::RetryAfter), "no Retry-After: the proxy places a retry");
    fh.advance(Duration::from_millis(400)).await;
    let replacement = fh.spawn_replacement(&pri_ord).await;
    fh.readmit(&pri_ord, replacement.sip_addr());
    fh.advance(Duration::from_millis(1_600)).await;
    let (elder, _survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    fh.mark(&pri_ord, None, "crash", "the removed process is killed at its grace");
    elder.crash();
    for _ in 0..120 {
        fh.advance(Duration::from_millis(500)).await;
        if replacement.is_ready() {
            break;
        }
    }
    assert!(replacement.is_ready(), "the replacement re-hydrated and became ready");
    proxy.set_health(&pri_ord, WorkerHealth::Alive);
    fh.advance(Duration::from_secs(10)).await;

    assert_eq!(rejections_to(&fh, &alice), 1, "one final reaches the caller");
    assert!(!reached(&fh, &bob), "the callee is never dialled");
    assert_eq!(consults.load(Ordering::SeqCst), 1, "the decision is asked once");
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    let nodes = [&replacement, survivor];
    let drained = fh
        .settle_terminal(async || {
            nodes[0].memory_clean()
                && nodes[1].memory_clean()
                && !nodes[0].holds_any_trace(&call_ref).await
                && !nodes[1].holds_any_trace(&call_ref).await
        })
        .await;
    assert!(drained, "the cluster drains the call within the settle budget");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(total_cdrs_for(&nodes, &call_ref), 1, "exactly one CDR across the cluster");
    assert_call_fully_released(&nodes, &call_ref).await;
    drop((replacement, proxy));
}

/// The same removal with a decision that returns inside the process's last
/// seconds, after a replacement came up: the removed process saw its ordinal
/// bound to the replacement's worker identity and stopped authoring
/// (ADR-0055 §6), so it neither applies the decision nor answers the caller:
/// the callee is never dialled, the caller hears the backup's 503 alone, and
/// the backup, which ended the call of a departed primary, writes its one CDR.
#[tokio::test(start_paused = true)]
async fn a_decision_returning_to_a_removed_process_is_not_applied() {
    let mut fh = FailoverHarness::new("undecided-call-abrupt-replacement-decision", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let route = std::sync::Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let (decision, consults) = CountedDecision::counting(route, Duration::from_millis(1_000));
    let (proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, decision).await;
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;

    fh.withdraw(&pri_ord);
    call.expect(503).await;
    fh.advance(Duration::from_millis(400)).await;
    let replacement = fh.spawn_replacement(&pri_ord).await;
    fh.readmit(&pri_ord, replacement.sip_addr());
    fh.advance(Duration::from_millis(1_600)).await;
    let (elder, _survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let elder_superseded = elder.is_superseded();
    let elder_cdrs = elder.cdr_records().iter().filter(|r| r.call_ref == call_ref).count();
    fh.mark(&pri_ord, None, "crash", "the removed process is killed at its grace");
    elder.crash();
    for _ in 0..120 {
        fh.advance(Duration::from_millis(500)).await;
        if replacement.is_ready() {
            break;
        }
    }
    assert!(replacement.is_ready(), "the replacement re-hydrated and became ready");
    proxy.set_health(&pri_ord, WorkerHealth::Alive);
    fh.advance(Duration::from_secs(10)).await;

    assert!(!reached(&fh, &bob), "the callee is never dialled");
    assert!(elder_superseded, "the removed process read its ordinal bound to its replacement");
    assert_eq!(finals_to(&fh, &alice, b"SIP/2.0 503"), 1, "the backup's 503 alone");
    assert_eq!(consults.load(Ordering::SeqCst), 1, "the decision was asked once");
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    let nodes = [&replacement, survivor];
    let drained = fh
        .settle_terminal(async || {
            nodes[0].memory_clean()
                && nodes[1].memory_clean()
                && !nodes[0].holds_any_trace(&call_ref).await
                && !nodes[1].holds_any_trace(&call_ref).await
        })
        .await;
    assert!(drained, "the cluster drains the call within the settle budget");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(elder_cdrs, 0, "the superseded process wrote no CDR");
    assert_eq!(total_cdrs_for(&nodes, &call_ref), 1, "one CDR across the cluster");
    assert_call_fully_released(&nodes, &call_ref).await;
    drop((call, replacement, proxy));
}

/// A plain crash after the decision was applied, before the routed call's
/// write reached the backup: the backup holds the call as born, undecided, and
/// refuses it 503 on the departure, while the callee the dead node dialled is
/// CANCELled by no one (ADR-0047, residual). The callee gives up on its own;
/// its final reaches no node and runs to its Timer H. The node never returns;
/// the backup writes the ended call's one CDR (ADR-0047 D8).
#[tokio::test(start_paused = true)]
async fn a_route_applied_before_it_replicated_leaves_the_callee_ringing() {
    let mut fh = FailoverHarness::new("undecided-call-route-not-replicated", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let route = std::sync::Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let (decision, _consults) = CountedDecision::counting(route, Duration::from_millis(1_000));
    let (proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, decision).await;
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;
    let bak_ord = if pri_ord == "b1" { "b2" } else { "b1" };

    // Nothing the node writes from now on reaches its backup.
    fh.stall(&pri_ord, bak_ord);
    let mut uas = bob.receive("INVITE").await;
    // The callee's give-up reaches neither the dead node nor the backup.
    fh.strike(Transitional::UndecidedDeparture);
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    kill_announced(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    call.expect(503).await;
    fh.advance(Duration::from_secs(30)).await;
    assert!(
        bob.try_receive_tolerating("CANCEL", &["INVITE"]).await.is_none(),
        "the callee the dead node dialled is CANCELled by no one",
    );
    uas.respond(480, "Temporarily Unavailable").await;

    let budget = b2bua::ReplicaConfig::default().reboot_budget_sec as u64;
    fh.advance(Duration::from_secs(budget + 120)).await;
    assert_eq!(rejections_to(&fh, &alice), 1, "the caller's one final is the backup's");
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    assert_eq!(survivor.metrics().repl_terminal_lost_total(), 0, "no ended call is lost");
    assert_eq!(total_cdrs_for(&[survivor], &call_ref), 1, "the backup writes the one CDR");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_call_fully_released(&[&w_b1, &w_b2], &call_ref).await;
    drop((call, proxy));
}

/// The caller CANCELs while the decision is in flight (RFC 3261 §9.2: 200 and
/// 487 from the node's transaction layer), then the node departs. Its backup
/// holds the call as born, undecided, and refuses it 503: the caller hears that
/// final after its 487, on a transaction already completed (ADR-0047,
/// residual), and writes the ended call's one CDR; the node restarts, and the
/// call is fully released.
#[tokio::test(start_paused = true)]
async fn a_setup_cancelled_during_its_decision_draws_the_backups_503_after_its_487() {
    let mut fh = FailoverHarness::new("undecided-call-cancelled-then-departed", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let route = std::sync::Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let (decision, _consults) = CountedDecision::counting(route, DECISION_DELAY);
    let (proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, decision).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;
    let mut cancel = call.cancel().await;
    cancel.expect(200).await;
    call.expect(487).await;
    fh.advance(Duration::from_millis(200)).await;

    // The backup's 503 reaches a caller whose transaction completed on the
    // 487.
    fh.strike(Transitional::UndecidedDeparture);
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    kill_announced(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    fh.advance(Duration::from_secs(5)).await;
    assert_eq!(finals_to(&fh, &alice, b"SIP/2.0 503"), 1, "the backup's 503 follows the 487");
    assert!(!reached(&fh, &bob), "the callee is never dialled");

    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}

/// Scale-down: the departed node never returns. Its backup answered the
/// undecided call; with no primary to defer to, it writes the call's one CDR
/// and deletes it (ADR-0047 D8), well inside the replica TTL.
#[tokio::test(start_paused = true)]
async fn an_undecided_call_of_a_node_that_never_returns_is_discharged_by_its_backup() {
    let mut fh = FailoverHarness::new("undecided-call-scale-down", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let route = std::sync::Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let (decision, _consults) = CountedDecision::counting(route, DECISION_DELAY);
    let (proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, decision).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    kill_announced(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    call.expect(503).await;

    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    let released = fh
        .settle_terminal(async || {
            survivor.memory_clean() && !survivor.holds_any_trace(&call_ref).await
        })
        .await;
    assert!(released, "nothing of the call is left live or stored");
    assert!(
        fh.now_ms() < b2bua::ReplicaConfig::default().reboot_budget_sec * 1000,
        "released inside the replica TTL, not by its reap"
    );
    assert_eq!(survivor.metrics().repl_terminal_lost_total(), 0, "no ended call is lost");
    assert_eq!(total_cdrs_for(&[survivor], &call_ref), 1, "the backup writes the one CDR");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    drop(proxy);
}

/// A removed process whose backup never received the call — its creation
/// write held on the stream — refuses the caller 503 when its decision
/// returns: the departure sweep found nothing to answer, and the caller has
/// no other final coming.
#[tokio::test(start_paused = true)]
async fn a_removed_process_whose_backup_holds_no_replica_answers_the_caller() {
    let mut fh = FailoverHarness::new("undecided-call-removed-no-replica", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let route = std::sync::Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let (decision, consults) = CountedDecision::counting(route, Duration::from_millis(1_000));
    let (proxy, mut w_b1, w_b2) = bring_up_deciding(&mut fh, decision).await;
    fh.stall("b1", "b2");
    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .identity(call_id_placed_on("b1", "unreplicated"), "unreplicated-tag")
        .through(proxy.addr())
        .send()
        .await;
    call.expect(100).await;
    fh.advance(Duration::from_millis(100)).await;
    assert!(w_b2.scan_one_backed_up("b1").await.is_none(), "the backup holds no replica");

    fh.withdraw("b1");
    let rejected = call.expect(503).await;
    assert!(!rejected.has(&HeaderName::RetryAfter), "no Retry-After");
    fh.advance(Duration::from_secs(5)).await;
    assert!(!reached(&fh, &bob), "the callee is never dialled");
    assert_eq!(consults.load(Ordering::SeqCst), 1, "the decision was asked once");
    let call_ref =
        w_b1.cdr_records().first().expect("the removed process wrote the CDR").call_ref.clone();
    assert_eq!(total_cdrs_for(&[&w_b1, &w_b2], &call_ref), 1, "exactly one CDR");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_call_fully_released(&[&w_b1, &w_b2], &call_ref).await;
    fh.mark("b1", None, "crash", "the removed process exits");
    w_b1.crash();
    drop(proxy);
}
