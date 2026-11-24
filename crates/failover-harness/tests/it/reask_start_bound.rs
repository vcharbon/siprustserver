//! **A re-ask and the caller's 100 (ADR-0070, ADR-0047 D4).** A new call's
//! primary dies with its routing decision in flight; its backup takes the
//! call over on the departure and asks the decision again.
//!
//! ```text
//!   alice ─INVITE─▶ proxy ─▶ primary ──(decision in flight)       backup: the born call
//!                            ✗ primary killed ✗
//!   departure announced → backup: inside the start bound, 100 to alice, re-ask, bob dialled
//!                                 past it, bare 503 to alice, nothing asked or dialled
//! ```
//!
//! The backup cannot tell whether the primary's 100 left. A re-ask that
//! starts inside the start bound sends the caller a 100 before it asks, so a
//! caller that heard none stops retransmitting and is not failed over while
//! the callee rings. The bound runs from the INVITE's receipt the record
//! states, never from the primary's turn: a primary whose router lagged
//! before the turn does not move the bound later.
//!
//! Beyond the failover sweep: the 100 and the start bound of a re-ask.

use std::sync::atomic::Ordering;
use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::Pace;
use failover_harness::{total_cdrs_for, FailoverHarness, ReplicatedB2buaSut, WorkerHealth};
use scenario_harness::agent::Agent;
use scenario_harness::callflow::OFFER_SDP;
use sip_message::header::HeaderName;

use crate::departure_support::{
    bring_up_deciding, call_id_placed_on, kill_announced, placed, CountedDecision,
};
use crate::inflight_resend::{split_mut, survivor_of, KILL_SETTLE};
use crate::undecided_call_settled_on_departure::{answer_and_hang_up, dialled, rejections_to};

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

/// The call's primary, the worker its Call-ID is placed on.
const PRIMARY: &str = "b1";
/// The fallback bound the primary arms as its decision request leaves.
const TRYING_BOUND: Duration = Duration::from_millis(200);
/// How long the routing decision takes to answer: past every step a cell
/// takes before the departure.
const DECISION_DELAY: Duration = Duration::from_secs(3);
/// The start bound at its default, counted from the INVITE's receipt.
const START_BOUND_MS: i64 = b2bua::config::B2buaConfig::DEFAULT_CALL_START_BOUND_MS;

/// Where in the recording the first datagram to `agent` opening with
/// `prefix` sits.
fn first_to(fh: &FailoverHarness, agent: &Agent, prefix: &str) -> Option<usize> {
    fh.sip_entries().iter().position(|e| e.to == agent.addr() && e.opens_with(prefix))
}

/// The call [`PRIMARY`] placed, once `backup` holds it, read at each
/// millisecond up to `within`; `None` past it.
async fn backed_up_within(backup: &ReplicatedB2buaSut, within: Duration) -> Option<String> {
    for _ in 0..within.as_millis() {
        if let Some(call_ref) = backup.scan_one_backed_up(PRIMARY).await {
            return Some(call_ref);
        }
        sip_clock::testkit::advance_settled(Duration::from_millis(1)).await;
    }
    None
}

/// The primary's outbox holds the caller's 100 and the primary dies inside
/// the fallback bound, its decision in flight: the caller heard nothing. The
/// backup's re-ask runs inside the start bound and sends her a 100 before the
/// callee is dialled; the call then completes, with one 100 and no refusal.
#[tokio::test(start_paused = true)]
async fn a_reask_inside_the_start_bound_sends_the_100_before_the_dial() {
    let name = "reask-sends-the-100";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let route = std::sync::Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let (decision, consults) = CountedDecision::counting(route, DECISION_DELAY);
    let (proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, decision).await;
    fh.turn_points().pace_riders(PRIMARY, Duration::from_secs(60));

    let sent_at = fh.now_ms();
    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER_SDP)
        .with_call_id(call_id_placed_on(PRIMARY, "reask-sends-the-100"))
        .through(proxy.addr())
        .send()
        .await;
    let call_ref = backed_up_within(&w_b2, TRYING_BOUND / 2).await.expect("the backup holds it");
    assert_eq!(first_to(&fh, &alice, "SIP/2.0 100 "), None, "the rider holds the 100");

    let pri_ord = PRIMARY.to_string();
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    kill_announced(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    assert!(fh.now_ms() - sent_at < START_BOUND_MS, "the re-ask runs inside the start bound");
    call.expect(100).await;
    answer_and_hang_up(&mut call, &bob).await;
    let trying = first_to(&fh, &alice, "SIP/2.0 100 ").expect("the caller heard a 100");
    let dial = first_to(&fh, &bob, "INVITE ").expect("the callee is dialled");
    assert!(trying < dial, "the 100 leaves before the callee is dialled");
    assert_eq!(consults.load(Ordering::SeqCst), 2, "the decision is asked again, once");
    assert_eq!(dialled(&fh, &bob), 1, "the callee is dialled once");
    assert_eq!(rejections_to(&fh, &alice), 0, "the caller is not refused");

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

/// How long the primary's router takes no turn after the INVITE arrives:
/// well inside the start bound.
const ROUTER_LAG: Duration = Duration::from_millis(2_500);
/// When the primary dies, after the INVITE's receipt: after its turn created
/// the call, its decision in flight.
const DIES_AT: Duration = Duration::from_millis(2_600);
/// When the backup learns of the departure, after the INVITE's receipt: past
/// the start bound from the receipt, inside it from the primary's turn.
const DEPARTS_AT: Duration = Duration::from_millis(4_200);
const _: () = assert!(DEPARTS_AT.as_millis() as i64 > START_BOUND_MS);
const _: () = assert!(((DEPARTS_AT.as_millis() - ROUTER_LAG.as_millis()) as i64) < START_BOUND_MS);

/// The primary's router lags before the INVITE's turn, which creates the call
/// and asks the decision; the primary then dies, and its departure reaches
/// the backup past the start bound counted from the INVITE's receipt, inside
/// it counted from the turn. The record states the receipt: the backup asks
/// nothing, refuses the caller the bare 503 through the transaction it
/// seeded, and dials no one.
#[tokio::test(start_paused = true)]
async fn a_reask_counts_its_start_bound_from_the_invites_receipt() {
    let name = "reask-start-bound-from-the-receipt";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let route = std::sync::Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let (decision, consults) = CountedDecision::counting(route, Duration::from_secs(60));
    let (proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, decision).await;
    let pace = fh.turn_points().router_pace().clone();

    pace.set(PRIMARY, Pace::Frozen);
    let received = fh.now_ms();
    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER_SDP)
        .with_call_id(call_id_placed_on(PRIMARY, "reask-from-the-receipt"))
        .through(proxy.addr())
        .send()
        .await;
    sip_clock::testkit::advance_settled(ROUTER_LAG).await;
    pace.set(PRIMARY, Pace::Free);
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;
    assert_eq!(pri_ord, PRIMARY);
    assert_eq!(consults.load(Ordering::SeqCst), 1, "the primary's turn asked the decision");

    let dies_in = received + DIES_AT.as_millis() as i64 - fh.now_ms();
    sip_clock::testkit::advance_settled(Duration::from_millis(dies_in.unsigned_abs())).await;
    fh.mark(PRIMARY, None, "crash", "its decision in flight, no one told yet");
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    primary.crash();
    let departs_in = received + DEPARTS_AT.as_millis() as i64 - fh.now_ms();
    sip_clock::testkit::advance_settled(Duration::from_millis(departs_in.unsigned_abs())).await;
    fh.mark(PRIMARY, None, "departure", "the proxy reads it dead, its endpoint leaves");
    proxy.set_health(PRIMARY, WorkerHealth::Dead);
    survivor.simulate_peer_removed(PRIMARY);
    fh.advance(KILL_SETTLE).await;

    let refused = call.expect(503).await;
    assert_eq!(refused.reason(), "Service Unavailable");
    assert!(!refused.has(&HeaderName::Reason), "no Reason");
    assert!(!refused.has(&HeaderName::RetryAfter), "no Retry-After");
    assert_eq!(consults.load(Ordering::SeqCst), 1, "the backup asks nothing");
    assert_eq!(rejections_to(&fh, &alice), 1, "one final");

    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    let released = fh
        .settle_terminal(async || {
            survivor.memory_clean() && !survivor.holds_any_trace(&call_ref).await
        })
        .await;
    assert!(released, "nothing of the call is left live or stored");
    assert_eq!(total_cdrs_for(&[survivor], &call_ref), 1, "the backup writes the one CDR");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(first_to(&fh, &bob, "INVITE "), None, "the callee is never dialled");
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}
