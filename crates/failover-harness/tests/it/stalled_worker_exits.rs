//! **A worker whose router loop stays stalled.** Its process, transaction owner
//! and replication server run on; the router takes nothing more. The cells
//! measure the whole sequence on the production probe, the times from the stall:
//!
//! ```text
//!   about 3.6 s  OPTIONS unanswered: the proxy reads b1 dead, tells b2
//!                b2 hears b1's heartbeats: refuted, nothing taken over, spent
//!   10 s         /ready 503 (B2BUA_LOOP_STALL_MS); the endpoint flips a probe
//!                period or two later: the proxy already reads b1 dead, no verdict
//!   15 to 17 s   the process exits (B2BUA_LOOP_EXIT_MS): no verdict is left to
//!                tell, so the backup takes nothing over on the exit
//!   + restart    b1 returns and reclaims its calls: about 22 s from the stall
//!                on a 3 s restart, about 29 s behind a 10 s crash-loop backoff
//! ```
//!
//! The exit threshold against the readiness flip and the restart's length are
//! the cells' axes. A stall that ends under the exit threshold keeps the worker
//! and its calls.
//!
//! Beyond the failover sweep: a worker stalled past its exit threshold.

use std::time::Duration;

use b2bua_harness::Pace;
use failover_harness::{FailoverHarness, WorkerHealth};
use sip_proxy::health::HealthProbeConfig;
use tokio::time::Instant;

use crate::departure_support::{spawn_workers_deciding, B1, B2, PROXY};
use crate::not_ready_flip_tells_a_dead_verdict::{
    ends_at_the_deadline_on_the_primary, restarts_in_place, routable_once_ready,
};
use crate::ringing_call_taken_over_on_departure::{
    place_on, replicated_deadline, ring_deadline_decision, Callee, Cluster, Placed, OFFER,
};
use crate::swept_call_primary_returns::hang_up_quiet_and_settle;

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROBE: &str = "127.0.0.1:5099";

/// A ring deadline that outlasts the stall, the exit and the slowest restart.
const LONG_RING: Duration = Duration::from_secs(40);
/// `B2BUA_LOOP_STALL_MS`, as it defaults.
const LOOP_STALL: Duration = Duration::from_secs(10);
/// `B2BUA_LOOP_EXIT_MS`, as it defaults.
const LOOP_EXIT: Duration = Duration::from_secs(15);
/// How far past its threshold the host reads a stall: two polls.
const EXIT_WITHIN: Duration = Duration::from_secs(2);
/// When the readiness probe flips the endpoint past `/ready`'s 503: its period
/// (2 s) × its failure threshold (2).
const FLIP_AFTER_READY_FAILS: Duration = Duration::from_secs(4);
/// How long the orchestrator takes to flip a dead process's endpoint.
const FLIP_AFTER_THE_EXIT: Duration = Duration::from_millis(1_500);
/// A container's first restart: boot to listening.
const QUICK_RESTART: Duration = Duration::from_secs(3);
/// A restart behind the orchestrator's first crash-loop backoff.
const BACKOFF_RESTART: Duration = Duration::from_secs(10);
/// How long the backup takes to hear the stream that refutes a verdict: the
/// heartbeat interval, and its flight.
const REFUTED_WITHIN: Duration = Duration::from_secs(1);

/// The cluster on the production probe, read alive, one call established and
/// one ringing on `b1`, both backed up on `b2`.
async fn place_probed(name: &str) -> Placed {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let workers = [("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())];
    let cadence = HealthProbeConfig::default();
    let proxy = fh.spawn_proxy_with_probe_cadence(PROXY, PROBE, &workers, cadence).await;
    let (w_b1, w_b2) = spawn_workers_deciding(&mut fh, ring_deadline_decision(LONG_RING)).await;
    let read = Duration::from_millis(cadence.interval_ms + cadence.timeout_ms);
    fh.advance(read).await;
    assert_eq!(proxy.health("b1"), Some(WorkerHealth::Alive));
    assert_eq!(proxy.health("b2"), Some(WorkerHealth::Alive));
    let cluster = Cluster { fh, alice, bob, proxy, w_b1, w_b2 };
    place_on(cluster, Callee::Ringing, LONG_RING, Some(OFFER)).await
}

/// `b1`'s router stops. Returns the stall's first instant.
fn stall_the_router(p: &mut Placed) -> Instant {
    p.fh.mark("b1", None, "stall", "the router loop takes no more turns");
    p.fh.turn_points().router_pace().set("b1", Pace::Frozen);
    Instant::now()
}

/// The proxy reads the stalled `b1` dead at the probe's bound, and `b2` refutes
/// the verdict on the heartbeats it still hears: nothing is taken over.
async fn read_dead_and_refuted(p: &Placed, stalled_at: Instant, hydrated: u64) {
    let cadence = HealthProbeConfig::default();
    let bound = cadence.verdict_within() + Duration::from_millis(cadence.interval_ms);
    let read =
        p.fh.pump_until(Duration::from_millis(100), bound, async || {
            p.proxy.health("b1") == Some(WorkerHealth::Dead)
        })
        .await;
    assert!(read, "read dead within {bound:?} of the stall, {:?} on", stalled_at.elapsed());
    p.fh.advance(REFUTED_WITHIN).await;
    let metrics = p.w_b2.metrics();
    assert_eq!(metrics.dead_verdicts_taken_total(), 1, "the probe's verdict is told");
    assert_eq!(metrics.dead_verdicts_unconfirmed_total(), 1, "its heartbeats refute it");
    assert_eq!(metrics.repl_takeover_hydrated_total(), hydrated, "nothing is taken over");
}

/// The host's watch on `b1`'s loops, past `threshold`: the instant it asks the
/// exit, and the ask.
fn watch_the_exit(p: &Placed, threshold: Duration) -> tokio::task::JoinHandle<Instant> {
    let loops = p.w_b1.loops().expect("b1 runs");
    tokio::spawn(async move {
        loops.wait_out(threshold).await;
        Instant::now()
    })
}

/// Run the clock until the watch asks the exit; the instant it did.
async fn exit_asked(p: &Placed, watch: &mut tokio::task::JoinHandle<Instant>) -> Instant {
    let step = Duration::from_millis(100);
    let asked = p.fh.pump_until(step, Duration::from_secs(30), async || watch.is_finished()).await;
    assert!(asked, "the host asks the exit");
    (&mut *watch).await.expect("the watch ends")
}

/// `b1`'s process goes as its exit asks. A new incarnation's router runs free.
fn process_exits(p: &mut Placed) {
    p.fh.mark("b1", None, "crash", "the process exits on its stalled loop");
    p.w_b1.crash();
    p.fh.turn_points().router_pace().set("b1", Pace::Free);
}

/// The process is gone and no verdict is left to tell: `b2` takes nothing over.
fn exit_tells_nothing(p: &Placed, hydrated: u64) {
    let metrics = p.w_b2.metrics();
    assert_eq!(metrics.dead_verdicts_taken_total(), 1, "no verdict on the flip or the exit");
    assert_eq!(metrics.repl_takeover_hydrated_total(), hydrated, "nothing is taken over");
    assert!(!p.w_b2.serves(&p.ringing_ref), "the ringing call waits for its restart");
}

/// `b1` restarts `after` the exit and reclaims its ringing call. Asserts the
/// wait from the stall against `bound`; the ringing call then ends on `b1` alone
/// at its deadline, and everything settles to one CDR a call.
async fn restart_reclaims(mut p: Placed, stalled_at: Instant, after: Duration, bound: Duration) {
    let deadline = replicated_deadline(&p).await;
    p.fh.advance(after).await;
    let addr = restarts_in_place(&mut p).await;
    p.fh.flap_not_ready("b1");
    routable_once_ready(&mut p, addr).await;
    let served =
        p.fh.pump_until(Duration::from_millis(50), Duration::from_secs(5), async || {
            p.w_b1.serves(&p.ringing_ref)
        })
        .await;
    assert!(served, "the restart's reclaim serves the ringing call");
    let waited = stalled_at.elapsed();
    assert!(waited <= bound, "served again {waited:?} after the stall, past {bound:?}");
    assert!(p.fh.now_ms() < deadline, "before its ring deadline");
    ends_at_the_deadline_on_the_primary(&mut p, deadline).await;
    hang_up_quiet_and_settle(p, &[]).await;
}

/// The sequence at the defaults: the endpoint flips ahead of the exit, a read
/// dead worker's flip tells nothing, and the exit tells nothing. The ringing
/// call waits for the restart's reclaim: the threshold plus the restart.
async fn stalled_past_the_exit(name: &str, restart: Duration) {
    let mut p = place_probed(name).await;
    let hydrated = p.w_b2.metrics().repl_takeover_hydrated_total();
    let stalled_at = stall_the_router(&mut p);
    let mut watch = watch_the_exit(&p, LOOP_EXIT);

    read_dead_and_refuted(&p, stalled_at, hydrated).await;
    let until_flip = (LOOP_STALL + FLIP_AFTER_READY_FAILS).saturating_sub(stalled_at.elapsed());
    p.fh.advance(until_flip).await;
    assert!(!watch.is_finished(), "no exit before its threshold, {:?} on", stalled_at.elapsed());
    p.fh.flap_not_ready("b1");

    let asked = exit_asked(&p, &mut watch).await;
    let waited = asked - stalled_at;
    assert!(waited > LOOP_EXIT, "not before its threshold: {waited:?}");
    assert!(waited <= LOOP_EXIT + EXIT_WITHIN + Duration::from_secs(1), "at it: {waited:?}");
    process_exits(&mut p);
    p.fh.advance(REFUTED_WITHIN).await;
    exit_tells_nothing(&p, hydrated);

    let until_the_router_stops_and_the_refutation = Duration::from_secs(2);
    let ready_within = Duration::from_secs(1);
    let bound = LOOP_EXIT
        + EXIT_WITHIN
        + until_the_router_stops_and_the_refutation
        + restart
        + ready_within;
    restart_reclaims(p, stalled_at, restart, bound).await;
}

#[tokio::test(start_paused = true)]
async fn a_stalled_worker_exits_at_its_threshold_and_a_quick_restart_reclaims_its_calls() {
    stalled_past_the_exit("stalled-worker-quick-restart", QUICK_RESTART).await;
}

#[tokio::test(start_paused = true)]
async fn a_stalled_worker_behind_a_crash_loop_backoff_waits_for_its_restart() {
    stalled_past_the_exit("stalled-worker-backoff-restart", BACKOFF_RESTART).await;
}

/// An exit threshold under the stall threshold, which boot refuses, is measured
/// here to rule the order out: the process exits ahead of the readiness flip,
/// the flip follows the exit, and the proxy already reads the worker dead since
/// its probe, so that flip tells nothing either. Only an exit ahead of the
/// probe's verdict would change the outcome, well inside a starved node's turns.
#[tokio::test(start_paused = true)]
async fn an_exit_ahead_of_the_readiness_flip_tells_no_verdict_either() {
    let mut p = place_probed("stalled-worker-exit-before-the-flip").await;
    let hydrated = p.w_b2.metrics().repl_takeover_hydrated_total();
    let stalled_at = stall_the_router(&mut p);
    let under_the_stall = LOOP_STALL - Duration::from_secs(4);
    let mut watch = watch_the_exit(&p, under_the_stall);

    read_dead_and_refuted(&p, stalled_at, hydrated).await;
    let asked = exit_asked(&p, &mut watch).await;
    assert!(asked - stalled_at < LOOP_STALL, "ahead of /ready's 503");
    process_exits(&mut p);
    p.fh.advance(FLIP_AFTER_THE_EXIT).await;
    p.fh.flap_not_ready("b1");
    p.fh.advance(REFUTED_WITHIN).await;
    exit_tells_nothing(&p, hydrated);

    let bound = LOOP_STALL + QUICK_RESTART + Duration::from_secs(3);
    restart_reclaims(p, stalled_at, QUICK_RESTART, bound).await;
}

/// A stall that ends past `/ready`'s 503 and under the exit threshold, as a
/// starved node's long turn may: no exit, the endpoint reads ready again, the
/// proxy reads the worker alive, and it keeps its calls.
#[tokio::test(start_paused = true)]
async fn a_stall_that_ends_under_the_exit_threshold_keeps_the_worker_and_its_calls() {
    let mut p = place_probed("stalled-worker-recovers").await;
    let hydrated = p.w_b2.metrics().repl_takeover_hydrated_total();
    let deadline = replicated_deadline(&p).await;
    let stalled_at = stall_the_router(&mut p);
    let watch = watch_the_exit(&p, LOOP_EXIT);

    read_dead_and_refuted(&p, stalled_at, hydrated).await;
    let until_flip = (LOOP_STALL + FLIP_AFTER_READY_FAILS).saturating_sub(stalled_at.elapsed());
    p.fh.advance(until_flip).await;
    p.fh.flap_not_ready("b1");
    assert!(!watch.is_finished(), "under its threshold");

    p.fh.turn_points().router_pace().set("b1", Pace::Free);
    p.fh.advance(Duration::from_secs(3)).await;
    assert!(!watch.is_finished(), "the turn ended: no exit");
    watch.abort();
    p.fh.flap_ready("b1");
    let read_alive = Duration::from_secs(4);
    let alive =
        p.fh.pump_until(Duration::from_millis(100), read_alive, async || {
            p.proxy.health("b1") == Some(WorkerHealth::Alive)
        })
        .await;
    assert!(alive, "the proxy reads the worker alive again");
    assert_eq!(p.w_b2.metrics().repl_takeover_hydrated_total(), hydrated, "nothing taken over");
    assert!(p.w_b1.serves(&p.ringing_ref), "the worker keeps its call");
    ends_at_the_deadline_on_the_primary(&mut p, deadline).await;
    hang_up_quiet_and_settle(p, &[]).await;
}
