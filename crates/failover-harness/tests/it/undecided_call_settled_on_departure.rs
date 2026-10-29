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
    assert_call_fully_released, total_cdrs_for, FailoverHarness, Incident, Transitional,
    WorkerHealth,
};
use scenario_harness::agent::Agent;
use sip_message::header::HeaderName;
use sip_message::SipParser;

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

/// How many INVITEs reached `agent` this run, a retransmission counted once.
fn dialled(fh: &FailoverHarness, agent: &Agent) -> usize {
    fh.sip_entries()
        .iter()
        .filter(|e| e.to == agent.addr() && e.opens_with("INVITE "))
        .filter_map(|e| sip_message::parser::custom::CustomParser::new().parse(&e.raw).ok())
        .map(|m| match m {
            sip_message::SipMessage::Request(r) => {
                r.top_via().branch().unwrap_or_default().to_string()
            }
            sip_message::SipMessage::Response(_) => String::new(),
        })
        .collect::<std::collections::HashSet<_>>()
        .len()
}

/// The callee answers the INVITE the backup dials, the caller takes the 2xx,
/// ACKs it and hangs up.
async fn answer_and_hang_up(call: &mut scenario_harness::ClientInvite, bob: &Agent) {
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(OFFER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    // A callee read late absorbs the INVITE's retransmissions first.
    bob.receive_absorbing("ACK", &["INVITE"]).await;
    scenario_harness::callflow::hangup(&mut dialog, bob).await;
}

/// How many 503 responses reached `agent` this run.
fn rejections_to(fh: &FailoverHarness, agent: &Agent) -> usize {
    fh.sip_entries().iter().filter(|e| e.to == agent.addr() && e.opens_with("SIP/2.0 503")).count()
}

#[tokio::test(start_paused = true)]
async fn an_undecided_call_whose_node_departs_is_asked_again_by_its_backup() {
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
    let mut uas = bob.receive("INVITE").await;
    assert!(
        fh.now_ms() - killed_at <= (DECISION_DELAY + ANSWER_BOUND).as_millis() as i64,
        "the callee is dialled once the decision asked again answers",
    );
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    assert_eq!(
        survivor.metrics().repl_takeover_hydrated_total(),
        hydrated_before + 1,
        "the backup took the call over",
    );
    assert_eq!(consults.load(Ordering::SeqCst), 2, "the decision is asked again, once");

    uas.respond(200, "OK").with_sdp(OFFER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    assert_eq!(rejections_to(&fh, &alice), 0, "the caller is not refused");
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;

    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    assert_eq!(consults.load(Ordering::SeqCst), 2, "the reclaim asks no decision");
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    fh.assert_sip_rfc_clean("undecided-call-departure");
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
async fn an_undecided_call_reclaimed_by_its_restarted_node_is_asked_again() {
    let mut fh = FailoverHarness::new("undecided-call-restart-in-place", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let route = std::sync::Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let (decision, consults) = CountedDecision::counting(route, DECISION_DELAY);
    let (mut proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, decision).await;
    // A restart in place is read by the tombstone, not the probe (ADR-0031
    // case 4): the proxy tells no dead verdict.
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
    fh.mark(&pri_ord, None, "reboot", "restart empty, higher gen, new pod IP");
    let new_addr = primary.reboot().await;
    proxy.set_address(&pri_ord, new_addr);
    fh.note_worker_rebound(&pri_ord, new_addr);
    survivor.simulate_peer_added(&pri_ord);
    for _ in 0..120 {
        fh.advance(Duration::from_millis(500)).await;
        if primary.is_ready() {
            break;
        }
    }
    assert!(primary.is_ready(), "the restarted node became ready");
    proxy.set_health(&pri_ord, WorkerHealth::Alive);
    let mut uas = bob.receive("INVITE").await;
    assert_eq!(
        survivor_of(&pri_ord, &w_b1, &w_b2).metrics().repl_takeover_hydrated_total(),
        hydrated_before,
        "the backup took nothing over",
    );
    assert_eq!(consults.load(Ordering::SeqCst), 2, "the restarted node asks the decision again");
    uas.respond(200, "OK").with_sdp(OFFER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    assert_eq!(rejections_to(&fh, &alice), 0, "the caller is not refused");
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    fh.assert_sip_rfc_clean("undecided-call-restart-in-place");
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
/// same ordinal comes up beside it. The survivor's departure sweep asks the
/// decision again; the replacement reclaims the call, writes it and asks too,
/// and the copy, written over, applies nothing and stands down (ADR-0047 D7).
/// The old process dies before its own decision returns. One final, one
/// dialled INVITE, one CDR.
#[tokio::test(start_paused = true)]
async fn an_undecided_call_of_a_node_replaced_at_once_is_asked_again_once() {
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
    answer_and_hang_up(&mut call, &bob).await;
    fh.advance(Duration::from_secs(10)).await;

    assert_eq!(rejections_to(&fh, &alice), 0, "the caller is not refused");
    assert_eq!(dialled(&fh, &bob), 1, "the callee is dialled once");
    assert_eq!(consults.load(Ordering::SeqCst), 3, "asked by the copy and the replacement");
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
/// (ADR-0055 §6), so it neither applies the decision nor answers the caller.
/// The backup asks the decision again on the departure; the replacement
/// reclaims the call undecided, writes it and asks again too, and the copy,
/// written over, applies nothing and stands down (ADR-0047 D7). The
/// replacement alone dials the callee and answers the caller. One CDR.
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
    answer_and_hang_up(&mut call, &bob).await;
    fh.advance(Duration::from_secs(10)).await;

    assert_eq!(dialled(&fh, &bob), 1, "the replacement's INVITE alone");
    assert!(elder_superseded, "the removed process read its ordinal bound to its replacement");
    assert_eq!(rejections_to(&fh, &alice), 0, "the caller is not refused");
    assert_eq!(consults.load(Ordering::SeqCst), 3, "asked by the copy and the replacement");
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
    drop((replacement, proxy));
}

/// A plain crash after the decision was applied, before the routed call's
/// write reached the backup: the backup holds the call as born, undecided,
/// asks the decision again on the departure and dials the callee a second
/// time, while the INVITE the dead node sent is CANCELled by no one (ADR-0047,
/// residual). The callee answers the backup's and gives up on the other on its
/// own; that final reaches no node and runs to its Timer H. The node never
/// returns; the backup writes the call's one CDR (ADR-0047 D8).
#[tokio::test(start_paused = true)]
async fn a_route_applied_before_it_replicated_is_dialled_again_by_the_backup() {
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
    answer_and_hang_up(&mut call, &bob).await;
    assert_eq!(dialled(&fh, &bob), 2, "the dead node's INVITE and the backup's");
    fh.advance(Duration::from_secs(30)).await;
    assert!(
        bob.try_receive_tolerating("CANCEL", &["INVITE"]).await.is_none(),
        "the callee the dead node dialled is CANCELled by no one",
    );
    uas.respond(480, "Temporarily Unavailable").await;

    let budget = b2bua::ReplicaConfig::default().reboot_budget_sec as u64;
    fh.advance(Duration::from_secs(budget + 120)).await;
    assert_eq!(rejections_to(&fh, &alice), 0, "the caller is not refused");
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    assert_eq!(survivor.metrics().repl_terminal_lost_total(), 0, "no ended call is lost");
    assert_eq!(total_cdrs_for(&[survivor], &call_ref), 1, "the backup writes the one CDR");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_call_fully_released(&[&w_b1, &w_b2], &call_ref).await;
    drop((call, proxy));
}

/// The caller CANCELs while the decision is in flight (RFC 3261 §9.2: 200 and
/// 487 from the node's transaction layer), and the node dies before the
/// decision returns: the turn that records the CANCEL never ran, so the
/// backup holds the call undecided, not cancelled. It asks the decision again
/// and dials the callee after the caller's 487 (ADR-0055 §11,
/// `W-kill-reask-after-cancel`); the callee's answer reaches a caller whose
/// transaction completed on the 487.
#[tokio::test(start_paused = true)]
async fn a_setup_cancelled_during_its_decision_is_asked_again_by_its_backup() {
    let name = "undecided-call-cancelled-then-departed";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let route = std::sync::Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let (decision, consults) = CountedDecision::counting(route, DECISION_DELAY);
    let (proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, decision).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;
    let mut cancel = call.cancel().await;
    cancel.expect(200).await;
    call.expect(487).await;
    fh.advance(Duration::from_millis(200)).await;

    fh.strike(Incident::PrimaryKilled);
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    kill_announced(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    let mut uas = bob.receive("INVITE").await;
    assert_eq!(consults.load(Ordering::SeqCst), 2, "the backup asks the decision again");
    uas.respond(200, "OK").with_sdp(OFFER).await;
    // The caller ACKs the 2xx her completed INVITE draws and ends the
    // dialog it confirms (RFC 3261 §13.2.2.4, §15).
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;

    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}

/// Scale-down: the departed node never returns. Its backup asked the
/// undecided call's decision again and served the call to its end; with no
/// primary to defer to, it writes the call's one CDR and deletes it
/// (ADR-0047 D8), well inside the replica TTL.
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
    answer_and_hang_up(&mut call, &bob).await;

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

/// The backup asks a departed node's undecided call's decision again and the
/// caller CANCELs during that round trip: her CANCEL draws 200 and 487 from
/// the INVITE transaction the takeover seeded, the decision is recorded and
/// never applied, and the callee is never dialled; the backup writes the
/// call's one CDR.
#[tokio::test(start_paused = true)]
async fn a_cancel_during_the_backups_asked_again_decision_dials_nobody() {
    let name = "undecided-call-cancel-during-the-reask";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let route = std::sync::Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let (decision, consults) = CountedDecision::counting(route, DECISION_DELAY);
    let (proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, decision).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    kill_announced(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    assert_eq!(consults.load(Ordering::SeqCst), 2, "the backup asks the decision again");

    let mut cancel = call.cancel().await;
    cancel.expect(200).await;
    call.expect(487).await;
    fh.advance(DECISION_DELAY + Duration::from_secs(5)).await;
    assert!(!reached(&fh, &bob), "the callee is never dialled");

    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    let released = fh
        .settle_terminal(async || {
            survivor.memory_clean() && !survivor.holds_any_trace(&call_ref).await
        })
        .await;
    assert!(released, "nothing of the call is left live or stored");
    assert_eq!(total_cdrs_for(&[survivor], &call_ref), 1, "the backup writes the one CDR");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    fh.assert_sip_rfc_clean(name);
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
