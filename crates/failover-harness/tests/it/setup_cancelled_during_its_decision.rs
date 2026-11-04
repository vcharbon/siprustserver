//! **A setup its caller CANCELs while its routing decision is in flight, then
//! a kill of its primary.** The decision takes longer than the CANCEL's
//! answers may be held, so the turn awaiting it writes the CANCEL before they
//! leave (ADR-0055 §3): whatever sub-point after the CANCEL the kill strikes,
//! the backup holds a call it can tell was cancelled, or the caller's CANCEL
//! is still unanswered and she repeats it to the backup.
//!
//! ```text
//!   alice ─INVITE─▶ proxy ─▶ primary ──(decision in flight, 3 s)    backup: the born call
//!   alice ─CANCEL─▶ proxy ─▶ primary: the cancel written, then 200 + 487
//!                            ✗ primary killed at turn k, sub-point p ✗
//!   the backup ends the call: the callee is never dialled
//! ```
//!
//! Turn 1 is that write, turn 2 the decision recorded and never applied, turn
//! 3 the CANCEL's own turn, which ends the call. Every cut is Guaranteed: the
//! answers never leave at their bound, the call ends cleanly with the callee
//! never dialled, and every CDR is the cancelled call's.

use std::sync::atomic::Ordering;
use std::time::Duration;

use b2bua::repl::SyncLevel;
use call::{CdrEventType, TerminationCause};
use failover_harness::cluster::{limited_route_to, Cluster, ClusterRecipe};
use failover_harness::sweep::{ALICE, BOB};
use failover_harness::{Incident, Tier, TurnAt, TurnPoint, WorkerHealth};
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser};

use crate::call_terminate_on_backup::OFFER;
use crate::departure_support::CountedDecision;
use crate::inflight_resend::last_request_sent;

/// How long the routing decision takes: far above the CANCEL's bound.
const DECISION_DELAY: Duration = Duration::from_secs(3);

/// The CDR events of a setup the caller CANCELed during its decision, whoever
/// ends it.
const CANCELLED_EVENTS: &[CdrEventType] = &[CdrEventType::InviteReceived, CdrEventType::Cancel];

/// How the primary dies.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kill {
    /// Its departure announced: the backup takes the call over.
    Announced,
    /// Unannounced, then restarted empty: it reclaims the call from the backup.
    UnannouncedThenRebooted,
}

#[tokio::test(start_paused = true)]
async fn a_kill_as_the_cancel_is_logged_dials_nobody() {
    cancelled_during_its_decision(
        Kill::Announced,
        "cancel-in-decision-t1-logged",
        1,
        TurnPoint::Logged,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_kill_as_the_cancel_is_applied_dials_nobody() {
    cancelled_during_its_decision(
        Kill::Announced,
        "cancel-in-decision-t1-applied",
        1,
        TurnPoint::Applied,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_kill_as_the_cancels_answers_left_dials_nobody() {
    cancelled_during_its_decision(
        Kill::Announced,
        "cancel-in-decision-t1-left",
        1,
        TurnPoint::Left,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_kill_as_the_recorded_decision_is_logged_dials_nobody() {
    cancelled_during_its_decision(
        Kill::Announced,
        "cancel-in-decision-t2-logged",
        2,
        TurnPoint::Logged,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_kill_as_the_recorded_decision_is_applied_dials_nobody() {
    cancelled_during_its_decision(
        Kill::Announced,
        "cancel-in-decision-t2-applied",
        2,
        TurnPoint::Applied,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_kill_as_the_recorded_decision_left_dials_nobody() {
    cancelled_during_its_decision(
        Kill::Announced,
        "cancel-in-decision-t2-left",
        2,
        TurnPoint::Left,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_kill_as_the_end_is_logged_dials_nobody() {
    cancelled_during_its_decision(
        Kill::Announced,
        "cancel-in-decision-t3-logged",
        3,
        TurnPoint::Logged,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_kill_as_the_end_is_applied_dials_nobody() {
    cancelled_during_its_decision(
        Kill::Announced,
        "cancel-in-decision-t3-applied",
        3,
        TurnPoint::Applied,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_kill_as_the_end_left_dials_nobody() {
    cancelled_during_its_decision(
        Kill::Announced,
        "cancel-in-decision-t3-left",
        3,
        TurnPoint::Left,
    )
    .await;
}

/// Killed unannounced as the CANCEL is applied, the primary restarts empty
/// and reclaims the marked call from its backup; the caller's repeated CANCEL
/// reaches it.
#[tokio::test(start_paused = true)]
async fn a_primary_rebooted_after_the_cancel_is_applied_reclaims_it_and_dials_nobody() {
    let name = "cancel-in-decision-rebooted-t1-applied";
    cancelled_during_its_decision(Kill::UnannouncedThenRebooted, name, 1, TurnPoint::Applied).await;
}

/// With no kill, the CANCEL's answers leave once the backup applied its
/// write, well before their bound; the decision that returns is recorded,
/// never applied, and the call's one CDR is the cancelled call's, built from
/// that decision.
#[tokio::test(start_paused = true)]
async fn with_no_kill_the_answers_leave_before_their_bound_and_the_cdr_records_the_decision() {
    let name = "cancel-in-decision-no-kill";
    let (decision, consults) = CountedDecision::counting(
        limited_route_to(BOB.parse().expect("bob's address")),
        DECISION_DELAY,
    );
    let recipe = ClusterRecipe::deciding(move |_| decision.clone());
    let cluster = Cluster::start_at(name, &recipe, SyncLevel::Applied).await;
    let alice = cluster.fh.agent("alice", ALICE).await;
    let bob = cluster.fh.agent("bob", BOB).await;
    let placement = cluster.place_next_call(&alice, "b1");

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(cluster.proxy.addr()).send().await;
    call.expect(100).await;
    cluster.fh.advance(Duration::from_millis(100)).await;
    let sent_at = cluster.fh.now_ms();
    let mut cancel = call.cancel().await;
    cancel.expect(200).await;
    call.expect(487).await;
    let bound = b2bua::repl::WritePath::default().cancel_bound.as_millis() as i64;
    let waited = cluster.fh.now_ms() - sent_at;
    assert!(waited < bound, "the answers left after {waited} ms, not at their {bound} ms bound");

    cluster.fh.advance(DECISION_DELAY + Duration::from_secs(5)).await;
    assert!(!cluster.fh.sip_entries().iter().any(|e| e.to == bob.addr()), "nobody is dialled");
    assert_eq!(consults.load(Ordering::SeqCst), 1, "the decision is asked once");
    let call_ref = cluster.fh.turn_points().first_call().expect("a call").0;
    let drained = cluster.fh.settle_terminal(async || cluster.is_clear_of(&call_ref).await).await;
    assert!(drained, "the cluster forgets the call, its limiter set released");
    cluster.fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    let cdrs: Vec<_> = cluster
        .nodes()
        .iter()
        .flat_map(|n| n.cdr_records())
        .filter(|r| r.call_ref == call_ref)
        .collect();
    assert_eq!(cdrs.len(), 1, "one CDR");
    let cdr = &cdrs[0];
    let cause = cdr.termination.as_ref().map(|t| t.cause);
    assert_eq!(cause, Some(TerminationCause::RemoteCancel), "a cancelled call");
    assert_eq!(cdr.decision_log.len(), 1, "the decision recorded, never applied");
    assert_eq!(event_kinds(cdr), CANCELLED_EVENTS, "the cancelled call's events");
    assert!(cdr.b_legs.is_empty(), "the CDR dials nobody");
    assert_eq!(cluster.node(&placement.backup).cdr_records().len(), 0, "the primary wrote it");
    cluster.fh.assert_sip_rfc_clean(name);
}

/// The caller CANCELs 100 ms into a 3 s decision; the primary dies at
/// sub-point `point` of turn `turn` as `kill` says.
async fn cancelled_during_its_decision(kill: Kill, name: &str, turn: usize, point: TurnPoint) {
    let (decision, consults) = CountedDecision::counting(
        limited_route_to(BOB.parse().expect("bob's address")),
        DECISION_DELAY,
    );
    let recipe = ClusterRecipe::deciding(move |_| decision.clone());
    let mut cluster = Cluster::start_at(name, &recipe, SyncLevel::Applied).await;
    let alice = cluster.fh.agent("alice", ALICE).await;
    let bob = cluster.fh.agent("bob", BOB).await;
    let placement = cluster.place_next_call(&alice, "b1");
    let reached = cluster.fh.turn_points().arm(TurnAt { turn, point });

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(cluster.proxy.addr()).send().await;
    call.expect(100).await;
    cluster.fh.advance(Duration::from_millis(100)).await;
    let mut cancel = call.cancel().await;
    let held = tokio::time::timeout(Duration::from_secs(10), reached)
        .await
        .unwrap_or_else(|_| panic!("[{name}] the call never reached t{turn}-{}", point.label()))
        .expect("the armed turn is handed over");
    assert_eq!(consults.load(Ordering::SeqCst), 1, "the primary asked the decision once");
    // The primary wrote the end's CDR and its delete is not applied: the
    // survivor writes one too (ADR-0020 X2).
    let (_, turns) = cluster.fh.turn_points().first_call().expect("a call");
    let most = if turns.get(2).is_some_and(|end| !end.applied) { 2 } else { 1 };
    match kill {
        Kill::Announced => {
            cluster.fh.strike(Incident::PrimaryKilled);
            cluster.kill(&placement.primary, &placement.backup);
            held.release();
        }
        Kill::UnannouncedThenRebooted => {
            cluster.fh.strike(Incident::PrimaryRebooted);
            cluster.kill_unannounced(&placement.primary);
            held.release();
            cluster.fh.advance(Duration::from_secs(1)).await;
            reboot_and_reclaim(&mut cluster, &placement.primary).await;
        }
    }

    // A CANCEL whose answers died with the primary goes again on Timer E
    // (RFC 3261 §17.1.2.2) and reaches the node now serving the call.
    cluster.fh.advance(Duration::from_millis(500)).await;
    if !answered(&cluster, &alice) {
        let (wire, dst) = last_request_sent(&cluster.fh, alice.addr(), &Method::Cancel);
        alice.try_send_datagram(&wire, dst).await.expect("the caller's repeat leaves");
    }
    cancel.expect(200).await;
    call.expect(487).await;

    cluster.fh.advance(DECISION_DELAY + Duration::from_secs(5)).await;
    assert!(
        !cluster.fh.sip_entries().iter().any(|e| e.to == bob.addr()),
        "[{name}] the callee is never dialled"
    );
    // Only a backup holding the call as born asks the decision again.
    let asked = consults.load(Ordering::SeqCst);
    let unmarked = (turn, point) == (1, TurnPoint::Logged);
    let asks = if unmarked { 1..=2 } else { 1..=1 };
    assert!(asks.contains(&asked), "[{name}] the decision was asked {asked} times");
    let call_ref = cluster.fh.turn_points().first_call().expect("a call").0;
    let drained = cluster.fh.settle_terminal(async || cluster.is_clear_of(&call_ref).await).await;
    assert!(drained, "[{name}] the cluster forgets the call, its limiter set released");
    cluster.fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(cluster.fh.tier(), Some(Tier::Guaranteed), "[{name}] no answer left at its bound");

    let cdrs: Vec<_> = cluster
        .nodes()
        .iter()
        .flat_map(|n| n.cdr_records())
        .filter(|r| r.call_ref == call_ref)
        .collect();
    assert!((1..=most).contains(&cdrs.len()), "[{name}] {} CDRs, at most {most}", cdrs.len());
    for cdr in &cdrs {
        let cause = cdr.termination.as_ref().map(|t| t.cause);
        assert_eq!(cause, Some(TerminationCause::RemoteCancel), "[{name}] a cancelled call");
        assert_eq!(event_kinds(cdr), CANCELLED_EVENTS, "[{name}] the cancelled call's events");
        assert!(cdr.b_legs.is_empty(), "[{name}] the CDR dials nobody");
    }
    let only_the_487 = finals_to(&cluster, &alice).iter().all(|s| *s == 487);
    assert!(only_the_487, "[{name}] nothing but the 487 ends the caller's INVITE");
    cluster.fh.assert_sip_rfc_clean(name);
}

/// The primary, killed unannounced, restarts empty at a new address,
/// becomes ready and is published to the proxy again.
async fn reboot_and_reclaim(cluster: &mut Cluster, primary: &str) {
    let addr = cluster.reboot(primary).await;
    for _ in 0..40 {
        cluster.fh.advance(Duration::from_millis(500)).await;
        if cluster.node(primary).is_ready() {
            break;
        }
    }
    assert!(cluster.node(primary).is_ready(), "the restarted primary became ready");
    cluster.proxy.set_address(primary, addr);
    cluster.fh.readmit(primary, addr);
    cluster.proxy.set_health(primary, WorkerHealth::Alive);
}

/// The kinds of `cdr`'s events, in order.
fn event_kinds(cdr: &b2bua::cdr::CdrRecord) -> Vec<CdrEventType> {
    cdr.events.iter().map(|e| e.event_type).collect()
}

/// Whether the caller's CANCEL drew its 200 so far.
fn answered(cluster: &Cluster, alice: &scenario_harness::Agent) -> bool {
    responses_to(cluster, alice).iter().any(|(s, m)| *s == 200 && *m == Method::Cancel)
}

/// The finals to the caller's INVITE delivered so far.
fn finals_to(cluster: &Cluster, alice: &scenario_harness::Agent) -> Vec<u16> {
    let finals = responses_to(cluster, alice).into_iter();
    finals.filter(|(s, m)| *s >= 200 && *m == Method::Invite).map(|(s, _)| s).collect()
}

/// Every response delivered to `alice`, by status and CSeq method.
fn responses_to(cluster: &Cluster, alice: &scenario_harness::Agent) -> Vec<(u16, Method)> {
    let parser = CustomParser::new();
    cluster
        .fh
        .sip_entries()
        .into_iter()
        .filter(|e| e.to == alice.addr() && e.delivered)
        .filter_map(|e| match parser.parse(&e.raw) {
            Ok(SipMessage::Response(r)) => Some((r.status(), r.cseq().method().clone())),
            _ => None,
        })
        .collect()
}
