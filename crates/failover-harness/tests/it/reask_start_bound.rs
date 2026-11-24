//! **A re-ask's start bound runs from the INVITE's receipt (ADR-0070,
//! ADR-0047 D4).** A new call's primary dies with its routing decision in
//! flight; its backup takes the call over on the departure and would ask the
//! decision again.
//!
//! ```text
//!   alice ─INVITE─▶ proxy ─▶ primary (router lags 2.5 s) ─ turn: call born, decision asked
//!                            ✗ primary killed at 2.6 s ✗
//!   departure told at 4.2 s → backup: past the start bound from the receipt,
//!                             bare 503 to alice, nothing asked, no one dialled
//! ```
//!
//! The bound runs from the INVITE's receipt the record states, never from the
//! primary's turn: a primary whose router lagged before the turn does not
//! move the bound later.
//!
//! Beyond the failover sweep: the start bound of a re-ask.

use std::sync::atomic::Ordering;
use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::Pace;
use failover_harness::{total_cdrs_for, FailoverHarness, ReplicatedB2buaSut, WorkerHealth};
use scenario_harness::callflow::OFFER_SDP;
use sip_message::header::HeaderName;

use crate::departure_support::{bring_up_deciding, call_id_placed_on, CountedDecision};
use crate::inflight_resend::KILL_SETTLE;
use crate::undecided_call_settled_on_departure::rejections_to;

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

/// The call's primary, the worker its Call-ID is placed on.
const PRIMARY: &str = "b1";
/// The start bound at its default, counted from the INVITE's receipt.
const START_BOUND_MS: i64 = b2bua::config::B2buaConfig::DEFAULT_CALL_START_BOUND_MS;
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

/// Advance the paused clock to `at` after `origin`, epoch ms on `fh`'s clock.
async fn advance_to(fh: &FailoverHarness, origin: i64, at: Duration) {
    let left = origin + at.as_millis() as i64 - fh.now_ms();
    assert!(left > 0, "{at:?} after the INVITE is still ahead: {left} ms");
    sip_clock::testkit::advance_settled(Duration::from_millis(left.unsigned_abs())).await;
}

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
    let (proxy, mut w_b1, w_b2) = bring_up_deciding(&mut fh, decision).await;
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
    advance_to(&fh, received, ROUTER_LAG).await;
    pace.set(PRIMARY, Pace::Free);
    let lag_left = DIES_AT - ROUTER_LAG;
    let call_ref = backed_up_within(&w_b2, lag_left / 2).await.expect("the backup holds the call");
    assert_eq!(consults.load(Ordering::SeqCst), 1, "the primary's turn asked the decision");

    advance_to(&fh, received, DIES_AT).await;
    fh.mark(PRIMARY, None, "crash", "its decision in flight, no one told yet");
    w_b1.crash();
    advance_to(&fh, received, DEPARTS_AT).await;
    fh.mark(PRIMARY, None, "departure", "the proxy reads it dead, its endpoint leaves");
    proxy.set_health(PRIMARY, WorkerHealth::Dead);
    w_b2.simulate_peer_removed(PRIMARY);
    fh.advance(KILL_SETTLE).await;

    let refused = call.expect(503).await;
    assert_eq!(refused.reason(), "Service Unavailable");
    assert!(!refused.has(&HeaderName::Reason), "no Reason");
    assert!(!refused.has(&HeaderName::RetryAfter), "no Retry-After");
    assert_eq!(consults.load(Ordering::SeqCst), 1, "the backup asks nothing");
    assert_eq!(rejections_to(&fh, &alice), 1, "one final");

    let released = fh
        .settle_terminal(async || w_b2.memory_clean() && !w_b2.holds_any_trace(&call_ref).await)
        .await;
    assert!(released, "nothing of the call is left live or stored");
    assert_eq!(total_cdrs_for(&[&w_b2], &call_ref), 1, "the backup writes the one CDR");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    let dialled = fh.sip_entries().iter().any(|e| e.to == bob.addr());
    assert!(!dialled, "the callee is never dialled");
    fh.assert_sip_rfc_clean(name);
    drop((w_b1, proxy));
}
