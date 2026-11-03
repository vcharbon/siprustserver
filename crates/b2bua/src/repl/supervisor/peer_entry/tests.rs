//! The readiness a peer's two flows grant (ADR-0011 X5, ADR-0014 §11), read
//! from their retained flags, one ordering per case.

use super::{FlowState, PeerEntry};

/// A flow whose hard timer read its peer unreachable: bootstrap-complete
/// best-effort, never connected.
fn timed_out_unreached() -> FlowState {
    FlowState { bootstrap_complete: true, connect_failed: true, ..FlowState::cold() }
}

fn entry(reclaim: FlowState, backup: FlowState) -> PeerEntry {
    PeerEntry { reclaim, backup, ..PeerEntry::cold() }
}

#[test]
fn a_backup_flow_not_yet_tried_holds_readiness_after_an_unreached_reclaim_flow() {
    assert!(!entry(timed_out_unreached(), FlowState::cold()).ready());
}

#[test]
fn a_failed_backup_connect_after_an_unreached_reclaim_flow_releases_readiness() {
    let backup = FlowState { connect_failed: true, ..FlowState::cold() };
    assert!(entry(timed_out_unreached(), backup).ready());
}

#[test]
fn a_backup_flow_that_reached_the_peer_holds_readiness_until_it_caught_up() {
    let reached = FlowState { ever_connected: true, connect_failed: true, ..FlowState::cold() };
    assert!(!entry(timed_out_unreached(), reached).ready(), "reached, not caught up");
    let caught_up = FlowState { ever_connected: true, current: true, ..FlowState::cold() };
    assert!(entry(timed_out_unreached(), caught_up).ready());
}

#[test]
fn a_backup_flows_own_hard_timer_releases_it_never_reached() {
    let reclaim = FlowState { ever_connected: true, current: true, ..FlowState::cold() };
    assert!(!entry(reclaim, FlowState { connect_failed: true, ..FlowState::cold() }).ready());
    let reclaim = FlowState { ever_connected: true, current: true, ..FlowState::cold() };
    assert!(entry(reclaim, FlowState { bootstrap_complete: true, ..FlowState::cold() }).ready());
}
