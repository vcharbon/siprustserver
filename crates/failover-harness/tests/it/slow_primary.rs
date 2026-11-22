//! **A primary that answers late is slow, not dead** (ADR-0069), on the
//! production probe. Every datagram `b1` sends lands 1.8 s late: past the
//! probe's reply window (1.6 s), inside its slow window (2 s).
//!
//! ```text
//!   PrimarySlow             b1 answers late: Slow within the dead bound
//!                           no verdict, no notice, nothing taken over,
//!                           b1 keeps its calls; on time again: Alive
//!                           after 3 answers, the calls end on b1
//!   PrimarySlowThenKilled   b1 Slow, then killed unannounced: Dead once its
//!                           last late answer is out of the slow window,
//!                           told to b2, which takes the ringing call over
//! ```
//!
//! Beyond the failover sweep: the sweep reads health from its cuts, these
//! cells from the probe's own answers.

use std::net::SocketAddr;
use std::time::Duration;

use failover_harness::WorkerHealth;
use sip_proxy::health::HealthProbeConfig;
use tokio::time::Instant;

use crate::departure_support::B1;
use crate::not_ready_flip_tells_a_dead_verdict::ends_at_the_deadline_on_the_primary;
use crate::ringing_call_taken_over_on_departure::{
    final_at, finish, replicated_deadline, ring_until, Placed,
};
use crate::stalled_worker_exits::place_probed;
use crate::swept_call_primary_returns::hang_up_quiet_and_settle;

/// How late each datagram of the slow primary lands.
const LATE: Duration = Duration::from_millis(1_800);
/// How long the primary stays slow: well under `HEALTH_SLOW_MAX_MS`.
const SLOW_SPELL: Duration = Duration::from_secs(10);
/// How long the backup takes to confirm a told verdict on the heartbeats it
/// no longer hears and take the ringing call over.
const TAKEN_OVER_WITHIN: Duration = Duration::from_secs(5);

fn b1() -> SocketAddr {
    B1.parse().unwrap()
}

/// Pump the clock until the proxy reads `b1` as `health`: whether it did
/// within `bound` on the clock.
async fn reads(p: &Placed, health: WorkerHealth, bound: Duration) -> bool {
    let from = Instant::now();
    let read =
        p.fh.pump_until(Duration::from_millis(100), bound, async || {
            p.proxy.health("b1") == Some(health)
        })
        .await;
    read && from.elapsed() <= bound
}

/// `b1` turns slow; the proxy reads it `Slow` within the bound it reads a
/// silent worker dead, and tells nobody. Returns the instant it turned slow.
async fn turns_slow(p: &mut Placed, hydrated: u64) -> Instant {
    p.fh.slow_signalling("b1", b1(), LATE);
    let slowed_at = Instant::now();
    let cadence = HealthProbeConfig::default();
    let bound = cadence.verdict_within() + Duration::from_millis(cadence.interval_ms);
    assert!(reads(p, WorkerHealth::Slow, bound).await, "read slow within {bound:?}");
    let metrics = p.w_b2.metrics();
    assert_eq!(metrics.dead_verdicts_taken_total(), 0, "a slow worker is no verdict");
    assert_eq!(metrics.repl_takeover_hydrated_total(), hydrated, "nothing is taken over");
    slowed_at
}

#[tokio::test(start_paused = true)]
async fn a_primary_answering_late_reads_slow_keeps_its_calls_and_reads_alive_again() {
    let mut p = place_probed("slow-primary").await;
    let hydrated = p.w_b2.metrics().repl_takeover_hydrated_total();
    let deadline = replicated_deadline(&p).await;
    let slowed_at = turns_slow(&mut p, hydrated).await;

    p.fh.pump_until(Duration::from_millis(100), SLOW_SPELL, async || {
        slowed_at.elapsed() >= SLOW_SPELL || p.proxy.health("b1") != Some(WorkerHealth::Slow)
    })
    .await;
    assert_eq!(p.proxy.health("b1"), Some(WorkerHealth::Slow), "{:?} on", slowed_at.elapsed());
    assert_eq!(p.w_b2.metrics().dead_verdicts_taken_total(), 0, "no verdict told");
    assert!(p.w_b1.serves(&p.ringing_ref), "b1 keeps its ringing call");
    assert!(p.w_b1.serves(&p.quiet_ref), "and its established call");

    p.fh.restore_signalling("b1", b1());
    let cadence = HealthProbeConfig::default();
    let recover = Duration::from_millis(
        u64::from(cadence.recover_probes + 1) * cadence.interval_ms + cadence.timeout_ms,
    );
    assert!(reads(&p, WorkerHealth::Alive, recover).await, "Alive within {recover:?}");
    assert_eq!(p.w_b2.metrics().repl_takeover_hydrated_total(), hydrated);
    ends_at_the_deadline_on_the_primary(&mut p, deadline).await;
    hang_up_quiet_and_settle(p, &[]).await;
}

#[tokio::test(start_paused = true)]
async fn a_slow_primary_killed_unannounced_reads_dead_and_its_backup_takes_over() {
    let mut p = place_probed("slow-primary-then-killed").await;
    let hydrated = p.w_b2.metrics().repl_takeover_hydrated_total();
    let deadline = replicated_deadline(&p).await;
    turns_slow(&mut p, hydrated).await;

    p.fh.mark("b1", None, "crash", "kill (abrupt), nobody told");
    p.w_b1.crash();
    let killed_at = Instant::now();
    let cadence = HealthProbeConfig::default();
    let bound = LATE + cadence.slow_window() + Duration::from_millis(cadence.interval_ms);
    assert!(reads(&p, WorkerHealth::Dead, bound).await, "read dead within {bound:?}");
    assert!(killed_at.elapsed() > LATE, "not before its last late answer");
    let taken =
        p.fh.pump_until(Duration::from_millis(100), TAKEN_OVER_WITHIN, async || {
            p.w_b2.serves(&p.ringing_ref)
        })
        .await;
    assert!(taken, "the backup takes the ringing call over");
    let metrics = p.w_b2.metrics();
    assert_eq!(metrics.dead_verdicts_taken_total(), 1, "the verdict is told once");
    assert_eq!(metrics.dead_verdicts_unconfirmed_total(), 0, "and confirmed");
    assert_eq!(metrics.repl_takeover_hydrated_total(), hydrated + 1);

    ring_until(&p, deadline).await;
    p.fh.advance(Duration::from_millis(600)).await;
    p.bob.receive("CANCEL").await.respond(200, "OK").await;
    p.at_bob.respond(487, "Request Terminated").await;
    p.bob.receive("ACK").await;
    final_at(&mut p, deadline).await;
    finish(p).await;
}
