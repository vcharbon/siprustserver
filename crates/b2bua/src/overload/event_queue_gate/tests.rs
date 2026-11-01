//! The gate's state machine: refuse past either bound, accept once both
//! accept bounds have held for the hold, and a reading between the bounds
//! restarts the hold while refusing and changes nothing while accepting.

use std::time::Duration;

use tokio::time::Instant;

use super::{GateState, Transition};
use crate::config::EventQueueGateConfig;

const MS: Duration = Duration::from_millis(1);

fn bounds() -> EventQueueGateConfig {
    EventQueueGateConfig {
        refuse_depth: 100,
        accept_depth: 10,
        refuse_sojourn_ms: 500,
        accept_sojourn_ms: 50,
        accept_hold_ms: 3_000,
    }
}

#[test]
fn a_quiet_queue_accepts() {
    let (mut s, t0) = (GateState::default(), Instant::now());
    assert_eq!(s.observe(&bounds(), 99, 499 * MS, t0), None);
    assert!(!s.refusing(), "below both refusal bounds");
}

#[test]
fn either_refusal_bound_refuses() {
    let t0 = Instant::now();
    let mut by_depth = GateState::default();
    assert_eq!(by_depth.observe(&bounds(), 100, Duration::ZERO, t0), Some(Transition::Refuse));
    let mut by_sojourn = GateState::default();
    assert_eq!(by_sojourn.observe(&bounds(), 0, 500 * MS, t0), Some(Transition::Refuse));
    assert_eq!(by_sojourn.observe(&bounds(), 0, 900 * MS, t0), None, "told once");
    assert!(by_depth.refusing() && by_sojourn.refusing());
}

#[test]
fn it_accepts_once_both_accept_bounds_hold_through_the_hold() {
    let (mut s, t0) = (GateState::default(), Instant::now());
    s.observe(&bounds(), 0, 600 * MS, t0);
    assert_eq!(s.observe(&bounds(), 10, 50 * MS, t0 + 2_999 * MS), None, "inside the hold");
    assert_eq!(s.observe(&bounds(), 0, Duration::ZERO, t0 + 3_000 * MS), Some(Transition::Accept));
    assert!(!s.refusing());
}

#[test]
fn a_reading_between_the_bounds_restarts_the_hold_while_refusing() {
    let (mut s, t0) = (GateState::default(), Instant::now());
    s.observe(&bounds(), 200, Duration::ZERO, t0);
    assert_eq!(s.observe(&bounds(), 11, Duration::ZERO, t0 + 1_000 * MS), None, "depth above");
    assert_eq!(s.observe(&bounds(), 0, Duration::ZERO, t0 + 3_500 * MS), None, "hold restarted");
    assert_eq!(s.observe(&bounds(), 0, 51 * MS, t0 + 3_600 * MS), None, "sojourn above");
    assert_eq!(s.observe(&bounds(), 0, Duration::ZERO, t0 + 6_599 * MS), None);
    assert_eq!(s.observe(&bounds(), 0, Duration::ZERO, t0 + 6_600 * MS), Some(Transition::Accept));
}

#[test]
fn a_reading_between_the_bounds_changes_nothing_while_accepting() {
    let (mut s, t0) = (GateState::default(), Instant::now());
    assert_eq!(s.observe(&bounds(), 50, 200 * MS, t0), None);
    assert!(!s.refusing());
}
