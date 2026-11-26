//! **A primary that answers late is slow, not dead** (ADR-0069), on the
//! production probe. Every datagram `b1` sends lands 1.8 s late: past the
//! probe's reply window (1.6 s), inside its slow window (2 s).
//!
//! ```text
//!   PrimarySlow             b1 answers late: Slow within the dead bound
//!                           no verdict, no notice, nothing taken over,
//!                           b1 keeps its calls; on time again: Alive
//!                           after 3 answers, the calls end on b1
//!   PrimarySlowThenKilled   b1 Slow; the callee refuses the ringing call,
//!                           b1 consults on it and is killed unannounced,
//!                           the consult in flight: Dead within slow window
//!                           + interval of its last late answer, told to b2,
//!                           which takes the call over and asks again before
//!                           the consult's own deadline
//! ```
//!
//! Timing axes: the hold starts with every probe answered in time; b1's last
//! answers are still in flight at the kill, held, so the last one lands up to
//! 1.8 s after it. The consult leaves b1 20 ms before the kill and is answered
//! 1 s after it is sent; nothing else is in flight but the ringing call's
//! INVITE transaction.
//!
//! Beyond the failover sweep: the sweep reads health from its cuts, these
//! cells from the probe's own answers.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, NewCallRequest, NewCallResponse,
};
use b2bua_sdk::config::B2buaConfig;
use failover_harness::WorkerHealth;
use sip_proxy::health::HealthProbeConfig;
use tokio::time::Instant;

use crate::departure_support::B1;
use crate::not_ready_flip_tells_a_dead_verdict::ends_at_the_deadline_on_the_primary;
use crate::ringing_call_taken_over_on_departure::{finish, replicated_deadline, Placed};
use crate::stalled_worker_exits::{place_probed, place_probed_deciding, LONG_RING};
use crate::swept_call_primary_returns::hang_up_quiet_and_settle;

/// How late each datagram of the slow primary lands.
const LATE: Duration = Duration::from_millis(1_800);
/// How long the primary stays slow.
const SLOW_SPELL: Duration = Duration::from_secs(10);
/// From the callee's refusal reaching `b1` to its kill: the consult is sent.
const CONSULT_SENT: Duration = Duration::from_millis(20);
/// How long the `/call/failure` consult takes to answer.
const FAILURE_DELAY: Duration = Duration::from_secs(1);

fn b1() -> SocketAddr {
    B1.parse().unwrap()
}

/// Routes every call to bob, ringing past the cell, with a callback context,
/// so a refusal draws a `/call/failure` consult, answered after
/// [`FAILURE_DELAY`] with a relay of the refusal.
struct RelayOnFailure {
    failures: Arc<AtomicUsize>,
}

#[async_trait]
impl CallDecisionEngine for RelayOnFailure {
    async fn new_call(&self, _req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        let mut route = route_to("127.0.0.1", 5070);
        route.no_answer_ms = Some(LONG_RING.as_millis() as i64);
        route.callback_context = Some("ctx".into());
        Ok(NewCallResponse::Route(route))
    }

    async fn call_failure(
        &self,
        _req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        self.failures.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(FAILURE_DELAY).await;
        Ok(CallFailureResponse::Relay { label: None })
    }

    async fn call_refer(
        &self,
        _req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        Err(CallDecisionError::Unavailable("no transfer here".into()))
    }
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
async fn a_slow_primary_killed_with_a_consult_in_flight_is_taken_over_before_its_deadline() {
    let failures = Arc::new(AtomicUsize::new(0));
    let engine = Arc::new(RelayOnFailure { failures: failures.clone() });
    let mut p = place_probed_deciding("slow-primary-then-killed", engine).await;
    let hydrated = p.w_b2.metrics().repl_takeover_hydrated_total();
    turns_slow(&mut p, hydrated).await;

    p.at_bob.respond(486, "Busy Here").await;
    p.fh.advance(CONSULT_SENT).await;
    assert_eq!(failures.load(Ordering::SeqCst), 1, "b1 consults on the refusal");
    let consulted_at = Instant::now() - CONSULT_SENT;
    p.fh.mark("b1", None, "crash", "kill (abrupt), nobody told, the consult in flight");
    p.w_b1.crash();
    let killed_at = Instant::now();

    let cadence = HealthProbeConfig::default();
    let last_answer_by = killed_at + LATE;
    let bound = LATE + cadence.slow_window() + cadence.period();
    assert!(reads(&p, WorkerHealth::Dead, bound).await, "read dead within {bound:?}");
    assert!(killed_at.elapsed() > LATE, "not before its last late answer");
    assert!(
        Instant::now() <= last_answer_by + cadence.slow_window() + cadence.period(),
        "within slow window + interval of the last answer"
    );
    let deadline = Duration::from_millis(B2buaConfig::DEFAULT_CALL_CONTROL_TIMEOUT_MS as u64);
    let left = deadline.saturating_sub(consulted_at.elapsed());
    let taken =
        p.fh.pump_until(Duration::from_millis(10), left, async || p.w_b2.serves(&p.ringing_ref))
            .await;
    assert!(taken, "the backup takes the call over before the consult's {deadline:?}");
    assert!(consulted_at.elapsed() < deadline, "{:?} after the consult", consulted_at.elapsed());
    let metrics = p.w_b2.metrics();
    assert_eq!(metrics.dead_verdicts_taken_total(), 1, "the verdict is told once");
    assert_eq!(metrics.dead_verdicts_unconfirmed_total(), 0, "and confirmed");
    assert_eq!(metrics.repl_takeover_hydrated_total(), hydrated + 1);

    p.ringing.expect(486).await;
    assert_eq!(failures.load(Ordering::SeqCst), 2, "the backup asks again");
    finish(p).await;
}
