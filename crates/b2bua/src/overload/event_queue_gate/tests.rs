//! The gate's state machine: refuse at the depth bound at once and on the
//! delay once it has persisted for the interval; accept once both accept
//! bounds have held for the hold; a reading between the bounds restarts the
//! hold while refusing and changes nothing while accepting. Also the stored
//! form the router task writes.

use std::time::Duration;

use tokio::time::Instant;

use super::{GateState, Stored, Transition};
use crate::config::EventQueueGateConfig;

const MS: Duration = Duration::from_millis(1);

fn bounds() -> EventQueueGateConfig {
    EventQueueGateConfig {
        refuse_depth: 100,
        accept_depth: 10,
        refuse_delay_ms: 300,
        refuse_interval_ms: 150,
        accept_delay_ms: 50,
        accept_hold_ms: 3_000,
    }
}

#[test]
fn a_quiet_queue_accepts() {
    let (mut s, t0) = (GateState::default(), Instant::now());
    assert_eq!(s.observe(&bounds(), 99, 299 * MS, t0), None);
    assert!(!s.refusing(), "below both refusal bounds");
}

#[test]
fn the_depth_bound_refuses_at_once() {
    let (mut s, t0) = (GateState::default(), Instant::now());
    assert_eq!(s.observe(&bounds(), 100, Duration::ZERO, t0), Some(Transition::Refuse));
    assert!(s.refusing());
}

/// One reading of an event that has waited past the bound for less than
/// the interval refuses nothing; one that has waited past it for the whole
/// interval refuses at once.
#[test]
fn one_reading_refuses_once_its_oldest_event_waited_the_interval_past_the_bound() {
    let t0 = Instant::now();
    let mut spike = GateState::default();
    assert_eq!(spike.observe(&bounds(), 1, 449 * MS, t0), None, "149 ms past the bound");
    let mut held = GateState::default();
    assert_eq!(held.observe(&bounds(), 1, 450 * MS, t0), Some(Transition::Refuse));
}

/// A delay past the bound, read again and again with each oldest event
/// younger than bound plus interval, refuses once it has persisted for the
/// interval; a reading under the bound in between starts the count again.
#[test]
fn a_delay_sustained_past_the_bound_for_the_interval_refuses_and_a_dip_restarts_it() {
    let (mut s, t0) = (GateState::default(), Instant::now());
    assert_eq!(s.observe(&bounds(), 3, 350 * MS, t0), None, "past the bound since t0 − 50");
    assert_eq!(s.observe(&bounds(), 3, 320 * MS, t0 + 50 * MS), None, "100 ms past it");
    assert_eq!(s.observe(&bounds(), 3, 299 * MS, t0 + 60 * MS), None, "a dip under the bound");
    assert_eq!(s.observe(&bounds(), 3, 310 * MS, t0 + 100 * MS), None, "counted from t0 + 90");
    assert_eq!(s.observe(&bounds(), 3, 330 * MS, t0 + 200 * MS), None, "110 ms past it");
    assert_eq!(
        s.observe(&bounds(), 3, 340 * MS, t0 + 240 * MS),
        Some(Transition::Refuse),
        "150 ms past it"
    );
}

#[test]
fn it_accepts_once_both_accept_bounds_hold_through_the_hold() {
    let (mut s, t0) = (GateState::default(), Instant::now());
    s.observe(&bounds(), 0, 600 * MS, t0);
    assert!(s.refusing());
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
    assert_eq!(s.observe(&bounds(), 0, 51 * MS, t0 + 3_600 * MS), None, "delay above");
    assert_eq!(s.observe(&bounds(), 0, Duration::ZERO, t0 + 6_599 * MS), None);
    assert_eq!(s.observe(&bounds(), 0, Duration::ZERO, t0 + 6_600 * MS), Some(Transition::Accept));
}

#[test]
fn a_reading_between_the_bounds_changes_nothing_while_accepting() {
    let (mut s, t0) = (GateState::default(), Instant::now());
    assert_eq!(s.observe(&bounds(), 50, 200 * MS, t0), None);
    assert!(!s.refusing());
}

#[test]
fn the_stored_form_round_trips() {
    let base = Instant::now();
    let stored = Stored {
        base,
        refusing: Default::default(),
        delayed_since: Default::default(),
        last_hot: Default::default(),
    };
    assert_eq!(stored.load(), GateState::default(), "nothing stored");
    let state =
        GateState { refusing: true, delayed_since: Some(base), last_hot: Some(base + 1234 * MS) };
    stored.store(&state);
    assert_eq!(stored.load(), state);
}
