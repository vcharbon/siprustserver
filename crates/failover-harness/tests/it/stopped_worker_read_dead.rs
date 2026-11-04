//! **A worker that answers no probe is read dead.** The proxy's OPTIONS probe
//! reads a worker `Dead` after `threshold` missed answers (ADR-0050 §3)
//! whether its router loop is wedged, with its process and transaction layer
//! running, or it is cut off the signalling plane; once it answers again the
//! probe reads it `Alive`.
//!
//! ```text
//!   wedged       b1's router loop takes no turn: read Dead, then Alive once it turns
//!   partitioned  b1 is cut off the SIP plane: read Dead, then Alive once restored
//! ```
//!
//! No call is placed: the probe's view is the subject.
//!
//! Beyond the failover sweep: the probe's verdict on a worker that stops answering.

use std::time::Duration;

use b2bua_harness::Pace;
use failover_harness::{FailoverHarness, WorkerHealth};
use tokio::time::Instant;

use crate::new_call_acceptance::{bring_up, drain, Cluster, PROBE_ROUND};

/// Pump until the proxy reads `ordinal` as `health`, for at most `within`.
async fn until_read(
    fh: &FailoverHarness,
    c: &Cluster,
    ordinal: &str,
    health: WorkerHealth,
    within: Duration,
) {
    let give_up = Instant::now() + within;
    while Instant::now() < give_up && c.proxy.health(ordinal) != Some(health) {
        fh.advance(Duration::from_millis(500)).await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_worker_whose_router_loop_is_wedged_is_read_dead_and_alive_once_it_turns() {
    let name = "wedged-router-read-dead";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let c = bring_up(&mut fh).await;

    fh.mark("b1", None, "wedge", "b1's router loop takes no turn");
    let pace = fh.turn_points().router_pace().clone();
    pace.set("b1", Pace::Frozen);
    until_read(&fh, &c, "b1", WorkerHealth::Dead, 3 * PROBE_ROUND).await;
    assert_eq!(c.proxy.health("b1"), Some(WorkerHealth::Dead), "a wedged worker is read dead");
    assert_eq!(c.proxy.health("b2"), Some(WorkerHealth::Alive), "the other stays alive");

    pace.set("b1", Pace::Free);
    until_read(&fh, &c, "b1", WorkerHealth::Alive, 2 * PROBE_ROUND).await;
    assert_eq!(c.proxy.health("b1"), Some(WorkerHealth::Alive), "turning again: read alive");
    drain(&fh, &c).await;
    fh.assert_sip_rfc_clean(name);
}

#[tokio::test(start_paused = true)]
async fn a_worker_cut_off_the_signalling_plane_is_read_dead_and_alive_once_restored() {
    let name = "partitioned-worker-read-dead";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let c = bring_up(&mut fh).await;

    let addr = c.b1.sip_addr();
    fh.cut_signalling("b1", addr);
    until_read(&fh, &c, "b1", WorkerHealth::Dead, 3 * PROBE_ROUND).await;
    assert_eq!(c.proxy.health("b1"), Some(WorkerHealth::Dead), "a partitioned worker is read dead");
    assert_eq!(c.proxy.health("b2"), Some(WorkerHealth::Alive), "the other stays alive");

    fh.restore_signalling("b1", addr);
    until_read(&fh, &c, "b1", WorkerHealth::Alive, 2 * PROBE_ROUND).await;
    assert_eq!(c.proxy.health("b1"), Some(WorkerHealth::Alive), "restored: read alive");
    drain(&fh, &c).await;
    fh.assert_sip_rfc_clean(name);
}
