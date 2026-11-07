//! How a landed turn persists and ends, by its result and the call's
//! residency, and which buffered observability it takes in itself.

use call::CallModelState::{self, Active, Terminated, Terminating};

use super::{classify, takes, CommitKind, Landed, Residency};
use crate::effects::{BufferedObservabilityEffect as Buffered, QuietTurn};

fn landed(state: CallModelState, removes: bool, sends_a_rejection: bool) -> Landed {
    Landed { quiet: None, state, removes, sends_a_rejection }
}

/// A residency the classification must not read.
fn unread() -> Residency {
    panic!("the residency of a call that is not terminated is read")
}

#[test]
fn a_quiet_turn_writes_nothing() {
    for kind in [QuietTurn::OwnRung, QuietTurn::ReAck, QuietTurn::AbsorbedAck] {
        let quiet = Landed { quiet: Some(kind), ..landed(Active, false, false) };
        assert_eq!(classify(quiet, unread), CommitKind::Quiet(kind));
    }
}

#[test]
fn a_live_call_turn_is_written_wherever_the_call_lives() {
    for state in [Active, Terminating] {
        for rejection in [false, true] {
            let kind = classify(landed(state, false, rejection), unread);
            assert_eq!(kind, CommitKind::Write, "{state:?}, rejection {rejection}");
        }
    }
}

#[test]
fn a_live_call_ending_ends_here_wherever_the_call_lives() {
    for state in [Active, Terminating] {
        assert_eq!(classify(landed(state, true, false), unread), CommitKind::DeleteBeforeSend);
        assert_eq!(classify(landed(state, true, true), unread), CommitKind::DeleteAfterDrain);
    }
}

#[test]
fn a_terminated_record_the_turn_does_not_remove_replaces_the_live_copy() {
    for residency in [Residency::Primary, Residency::OrphanedCopy] {
        for rejection in [false, true] {
            let kind = classify(landed(Terminated, false, rejection), || residency);
            assert_eq!(kind, CommitKind::Update, "rejection {rejection}");
        }
    }
}

#[test]
fn a_terminal_turn_ends_the_call_on_a_primary_or_an_orphaned_copy() {
    for residency in [Residency::Primary, Residency::OrphanedCopy] {
        let removing = |rejection| classify(landed(Terminated, true, rejection), || residency);
        assert_eq!(removing(false), CommitKind::DeleteBeforeSend);
        assert_eq!(removing(true), CommitKind::DeleteAfterDrain);
    }
}

#[test]
fn a_takeover_copy_defers_its_terminal_to_a_live_primary() {
    for removes in [false, true] {
        for rejection in [false, true] {
            let kind = classify(landed(Terminated, removes, rejection), || Residency::Copy);
            assert_eq!(
                kind,
                CommitKind::DeferToPrimary,
                "removes {removes}, rejection {rejection}"
            );
        }
    }
}

fn counters() -> [Buffered; 3] {
    [
        Buffered::SecondFinalRefused { status: 200, carried: 486 },
        Buffered::ProvisionalAfterFinalRefused { status: 180, carried: 486 },
        Buffered::GoingAwayAbsorbed { event: "event", rule: "rule" },
    ]
}

#[test]
fn every_commit_counts_its_refusals_and_absorptions() {
    let kinds = [
        CommitKind::Write,
        CommitKind::Update,
        CommitKind::DeleteBeforeSend,
        CommitKind::DeleteAfterDrain,
        CommitKind::DeferToPrimary,
    ];
    for kind in kinds {
        for eff in counters() {
            assert!(takes(kind, &eff), "{kind:?} counts {eff:?}");
        }
    }
}

#[test]
fn a_cdr_is_written_here_by_a_turn_that_keeps_the_call() {
    assert!(takes(CommitKind::Write, &Buffered::WriteCdr));
    assert!(takes(CommitKind::Update, &Buffered::WriteCdr));
    for persisted in [CommitKind::DeleteBeforeSend, CommitKind::DeleteAfterDrain] {
        assert!(!takes(persisted, &Buffered::WriteCdr), "{persisted:?} enqueued it at its persist");
    }
}

#[test]
fn a_deferring_copy_leaves_the_cdr_and_the_unrecorded_termination_to_its_primary() {
    assert!(!takes(CommitKind::DeferToPrimary, &Buffered::WriteCdr));
    assert!(!takes(CommitKind::DeferToPrimary, &Buffered::TerminationUnrecorded));
    let kinds = [
        CommitKind::Write,
        CommitKind::Update,
        CommitKind::DeleteBeforeSend,
        CommitKind::DeleteAfterDrain,
    ];
    for kind in kinds {
        assert!(takes(kind, &Buffered::TerminationUnrecorded), "{kind:?} counts it");
    }
}
