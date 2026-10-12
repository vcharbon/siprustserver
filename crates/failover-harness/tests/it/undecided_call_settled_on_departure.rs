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
//!   primary restarts, reclaims the ended call: one CDR
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
use failover_harness::{assert_call_fully_released, total_cdrs_for, FailoverHarness, WorkerHealth};
use scenario_harness::agent::Agent;
use sip_message::header::HeaderName;

use crate::departure_support::{
    bring_up_deciding, kill_announced, placed, spawn_front_proxy, CountedDecision,
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

/// How many 503 responses reached `agent` this run.
fn rejections_to(fh: &FailoverHarness, agent: &Agent) -> usize {
    fh.sip_entries()
        .iter()
        .filter(|e| e.to == agent.addr() && e.raw.starts_with(b"SIP/2.0 503"))
        .count()
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
    let (proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, decision).await;

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
    let (proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, decision).await;

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
    let (proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, decision).await;

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
    let proxy = spawn_front_proxy(&fh).await;
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
/// caller; the replacement reclaims the ended call and writes its one CDR; the
/// old process dies before its decision returns. One final, one CDR, one
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
/// seconds, after a replacement came up: the removed process knows it left the
/// slice and does not apply the decision. The callee is never dialled, and the
/// call ends there with nothing on the wire: the backup's 503 is the caller's
/// one final. Both the removed process and the replacement, which reclaims the
/// ended call, write a CDR: the dual-owner window of ADR-0031 D3.
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
    assert_eq!(rejections_to(&fh, &alice), 1, "the caller's one final is the backup's");
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
    assert_eq!(elder_cdrs, 1, "the removed process wrote the CDR of the call it refused");
    assert_eq!(
        total_cdrs_for(&nodes, &call_ref),
        1,
        "and the replacement the CDR of the call it reclaimed ended: two in all",
    );
    assert_call_fully_released(&nodes, &call_ref).await;
    drop((call, replacement, proxy));
}

/// A plain crash after the decision was applied, before the routed call's
/// write reached the backup: the backup holds the call as born, undecided, and
/// refuses it 503 on the departure, while the callee the dead node dialled is
/// left ringing, CANCELled by no one (ADR-0046, residual). The caller has its
/// one final.
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
    let uas = bob.receive("INVITE").await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    kill_announced(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    call.expect(503).await;
    fh.advance(Duration::from_secs(40)).await;

    assert!(
        bob.try_receive_tolerating("CANCEL", &["INVITE"]).await.is_none(),
        "the callee the dead node dialled is CANCELled by no one",
    );
    assert_eq!(rejections_to(&fh, &alice), 1, "the caller's one final is the backup's");
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    assert!(survivor.memory_clean(), "the backup holds no live copy of the call");
    assert!(!survivor.serves(&call_ref), "and serves nothing for it");
    drop((call, uas, proxy));
}

/// The caller CANCELs while the decision is in flight (RFC 3261 §9.2: 200 and
/// 487 from the node's transaction layer), then the node departs: its backup
/// holds the call as answered and sends the caller nothing more. The restarted
/// node reclaims the call and ends it: one CDR, full release.
#[tokio::test(start_paused = true)]
async fn a_setup_cancelled_before_its_decision_draws_no_503_on_departure() {
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

    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    kill_announced(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    fh.advance(Duration::from_secs(5)).await;
    assert_eq!(rejections_to(&fh, &alice), 0, "no 503 after the 487");
    assert!(!reached(&fh, &bob), "the callee is never dialled");

    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}

/// Scale-down: the departed node never returns. Its backup answered the
/// undecided call and deferred the CDR to a primary that is gone, so the ended
/// call's Element is reaped past its TTL with its CDR lost (ADR-0020 X3).
#[tokio::test(start_paused = true)]
async fn an_undecided_call_of_a_node_that_never_returns_loses_its_cdr() {
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

    let budget = b2bua::B2buaConfig::default().reboot_budget_sec as u64;
    fh.advance(Duration::from_secs(budget + 120)).await;
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    assert_eq!(survivor.metrics().repl_terminal_lost_total(), 1, "the ended call's CDR is lost");
    assert_eq!(total_cdrs_for(&[survivor], &call_ref), 0, "the backup writes none");
    assert!(survivor.memory_clean(), "nothing of the call is left live");
    assert!(!survivor.holds_any_trace(&call_ref).await, "nor stored");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    drop(proxy);
}
