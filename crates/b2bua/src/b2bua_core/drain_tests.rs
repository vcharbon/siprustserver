//! A worker that observes its own withdrawal with no SIGTERM learns of it as
//! an event its host drains on (ADR-0031 D5): the withdrawal resolves, and the
//! drain that follows returns as a SIGTERM's would.

use std::time::Duration;

use topology::Peer;

use crate::drain::{DrainBounds, DrainExit};
use crate::router::test_support::node;

const BOUNDS: DrainBounds = DrainBounds {
    grace: Duration::from_secs(5),
    floor: Duration::from_secs(1),
    release_flush: Duration::from_secs(3),
    cdr_flush: Duration::ZERO,
};

#[tokio::test(start_paused = true)]
async fn a_withdrawal_no_sigterm_follows_resolves_and_the_drain_returns() {
    let n = node("w0").await;
    n.membership.add(Peer::new("w0", "w0"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let early = tokio::time::timeout(Duration::from_secs(60), n.core.withdrawn()).await;
    assert!(early.is_err(), "a routable worker is not withdrawn");

    n.membership.set_conditions("w0", false, true);
    let withdrawn = tokio::time::timeout(Duration::from_secs(1), n.core.withdrawn()).await;
    assert!(withdrawn.is_ok(), "the terminating own endpoint is a withdrawal");
    assert!(n.core.is_withdrawn());

    let out = n.core.drain(BOUNDS).await;
    assert_eq!(out.exit, DrainExit::Quiescent, "no live call: the drain returns at once");
    assert!(n.core.readiness().is_draining());
}

#[tokio::test(start_paused = true)]
async fn a_worker_its_membership_never_shows_is_never_withdrawn() {
    let n = node("w0").await;
    let never = tokio::time::timeout(Duration::from_secs(60), n.core.withdrawn()).await;
    assert!(never.is_err());
}
