//! How a landed turn persists and ends, by its result and the call's
//! residency.

use call::CallModelState::{self, Active, Terminated, Terminating};

use super::{classify, CommitKind, Landed, Residency};
use crate::effects::QuietTurn;

const PRIMARY: Residency = Residency::Primary;
const COPY: Residency = Residency::Copy;
const ORPHANED_COPY: Residency = Residency::OrphanedCopy;

fn landed(state: CallModelState, removes: bool, sends_a_rejection: bool) -> Landed {
    Landed { quiet: None, state, removes, sends_a_rejection }
}

#[test]
fn a_quiet_turn_writes_nothing_wherever_the_call_lives() {
    for residency in [PRIMARY, COPY, ORPHANED_COPY] {
        for kind in [QuietTurn::OwnRung, QuietTurn::ReAck, QuietTurn::AbsorbedAck] {
            let quiet = Landed { quiet: Some(kind), ..landed(Active, false, false) };
            assert_eq!(classify(quiet, residency), CommitKind::Quiet(kind));
        }
    }
}

#[test]
fn a_live_call_turn_is_written() {
    for residency in [PRIMARY, COPY, ORPHANED_COPY] {
        for state in [Active, Terminating] {
            for rejection in [false, true] {
                let kind = classify(landed(state, false, rejection), residency);
                assert_eq!(kind, CommitKind::Write, "{state:?}, rejection {rejection}");
            }
        }
    }
}

#[test]
fn a_terminated_record_the_turn_does_not_remove_replaces_the_live_copy() {
    assert_eq!(classify(landed(Terminated, false, false), PRIMARY), CommitKind::Update);
    assert_eq!(classify(landed(Terminated, false, true), ORPHANED_COPY), CommitKind::Update);
}

#[test]
fn a_removing_turn_logs_its_delete_before_its_effects_leave() {
    for residency in [PRIMARY, ORPHANED_COPY] {
        for state in [Active, Terminating, Terminated] {
            let kind = classify(landed(state, true, false), residency);
            assert_eq!(kind, CommitKind::DeleteBeforeSend, "{state:?}");
        }
    }
}

#[test]
fn a_removing_turn_owing_the_caller_her_final_logs_its_delete_once_drained() {
    for residency in [PRIMARY, ORPHANED_COPY] {
        for state in [Active, Terminating, Terminated] {
            let kind = classify(landed(state, true, true), residency);
            assert_eq!(kind, CommitKind::DeleteAfterDrain, "{state:?}");
        }
    }
}

#[test]
fn a_takeover_copy_defers_its_terminal_to_a_live_primary() {
    for removes in [false, true] {
        for rejection in [false, true] {
            let kind = classify(landed(Terminated, removes, rejection), COPY);
            assert_eq!(
                kind,
                CommitKind::DeferToPrimary,
                "removes {removes}, rejection {rejection}"
            );
        }
    }
}

#[test]
fn a_takeover_copy_ends_a_call_itself_before_it_is_terminated() {
    assert_eq!(classify(landed(Terminating, true, false), COPY), CommitKind::DeleteBeforeSend);
    assert_eq!(classify(landed(Active, true, true), COPY), CommitKind::DeleteAfterDrain);
}
