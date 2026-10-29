//! **The proxy's dead verdict hands a dead primary's in-flight call to its
//! backup.** The caller heard the primary's 100 Trying through the stateless
//! front proxy (RFC 3261 §16.11), so she sends nothing more until a final
//! (§17.1.1.2). The routing decision's write reached the backup, and the node
//! dies with its INVITE to the callee lost. The peers are not told of the
//! death (ADR-0031 case 3a): no departure sweep runs, and nothing either party
//! sends reaches the backup. The proxy's probe reads the node dead and tells
//! the workers; the backup takes the call over at that verdict and dials the
//! callee from its record, long before the node's endpoint leaves the slice.
//!
//! ```text
//!   alice ─INVITE─▶ proxy ─▶ b1 ──100──▶ alice; b1 ─INVITE─✗ bob; the route applied at b2
//!                            ✗ b1 killed, unannounced ✗
//!   proxy reads b1 dead → tells b2 → b2 ─INVITE─▶ bob
//!   alice ◀─180/200── b2 ◀─180/200── bob; ACK, BYE
//!   b1's endpoint leaves the slice: b2 writes the one CDR
//! ```

use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use failover_harness::WorkerHealth;

use crate::departure_support::{bring_up_deciding, placed, CountedDecision};
use crate::inflight_resend::{settle_to_one_cdr, split_mut, survivor_of, REPLICATE};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

/// How long the routing decision takes to answer.
const DECISION_DELAY: Duration = Duration::from_millis(300);
/// How long the proxy's probe takes to read the killed node dead (ADR-0031
/// case 3a: two missed windows).
const PROBE_READS_DEAD_AFTER: Duration = Duration::from_secs(2);
/// How soon after the verdict the callee is dialled.
const TAKEOVER_BOUND: Duration = Duration::from_millis(500);

#[tokio::test(start_paused = true)]
async fn the_backup_dials_the_callee_of_a_primary_the_proxy_reads_dead() {
    let name = "read-dead-primary-taken-over";
    let mut fh = failover_harness::FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let route = std::sync::Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let (decision, _consults) = CountedDecision::counting(route, DECISION_DELAY);
    let (proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, decision).await;

    fh.cut_signalling("bob", bob.addr());
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;
    fh.advance(DECISION_DELAY + REPLICATE).await;

    // The node dies with its INVITE to the callee lost; nobody is told.
    let (primary, _) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    fh.mark(&pri_ord, None, "crash", "kill (unannounced)");
    primary.crash();
    fh.restore_signalling("bob", bob.addr());
    fh.advance(PROBE_READS_DEAD_AFTER).await;
    assert!(bob.take_queued().await.is_none(), "nothing reached the callee before the verdict");

    let hydrated = survivor_of(&pri_ord, &w_b1, &w_b2).metrics().repl_takeover_hydrated_total();
    fh.mark(&pri_ord, None, "health", "the proxy's probe reads it dead");
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    let verdict_at = fh.now_ms();
    let mut uas = bob.receive("INVITE").await;
    assert!(
        fh.now_ms() - verdict_at <= TAKEOVER_BOUND.as_millis() as i64,
        "the callee is dialled at the verdict",
    );
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    assert_eq!(
        survivor.metrics().repl_takeover_hydrated_total(),
        hydrated + 1,
        "the backup took the call over",
    );

    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;

    // The node's endpoint leaves the slice: the backup discharges the call
    // its copy ended and deferred (ADR-0047 D8).
    fh.mark(&pri_ord, None, "depart", "the evicted pod is force-deleted");
    survivor.simulate_peer_removed(&pri_ord);
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}

/// How long a silent replication stream stays open: the pullers' idle bound
/// (ADR-0045).
const IDLE_BOUND: Duration = Duration::from_secs(2);

/// **A host that goes silent with its sockets open.** The primary stops
/// answering on every plane at once, its replication stream still connected
/// but carrying nothing, as a host lost to the network does. The proxy's
/// probe reads it dead and tells the backup while that stream still reads
/// open; the stream is left at the idle bound with no frame since the
/// verdict, which confirms it, and the backup dials the callee from its
/// record (ADR-0047 D1).
#[tokio::test(start_paused = true)]
async fn a_silent_primary_is_confirmed_dead_at_the_idle_bound_and_taken_over() {
    let name = "silent-primary-taken-over";
    let mut fh = failover_harness::FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let route = std::sync::Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let (decision, _consults) = CountedDecision::counting(route, DECISION_DELAY);
    let (proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, decision).await;

    fh.cut_signalling("bob", bob.addr());
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;
    fh.advance(DECISION_DELAY + REPLICATE).await;

    // The primary's host goes silent: nothing it sends reaches anyone, and
    // its replication stream stays connected, carrying nothing.
    let bak_ord = if pri_ord == "b1" { "b2" } else { "b1" };
    let (primary, _) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let primary_sip = primary.sip_addr();
    fh.partition(&pri_ord, bak_ord);
    fh.cut_signalling(&pri_ord, primary_sip);
    fh.restore_signalling("bob", bob.addr());
    fh.advance(PROBE_READS_DEAD_AFTER).await;

    fh.mark(&pri_ord, None, "health", "the proxy's probe reads it dead");
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    let verdict_at = fh.now_ms();
    let mut uas = bob.receive("INVITE").await;
    let dialled_after = fh.now_ms() - verdict_at;
    assert!(
        dialled_after <= (IDLE_BOUND + TAKEOVER_BOUND).as_millis() as i64,
        "the callee is dialled within the idle bound of the verdict ({dialled_after} ms)",
    );
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    assert_eq!(survivor.metrics().dead_verdicts_unconfirmed_total(), 0, "the verdict confirmed");

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;

    // The silent host is gone for good: its process ends and its endpoint
    // leaves the slice, and the backup discharges the call (ADR-0047 D8).
    let (primary, _) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    primary.crash();
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    survivor.simulate_peer_removed(&pri_ord);
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}
