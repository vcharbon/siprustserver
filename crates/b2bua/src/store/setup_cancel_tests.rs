//! The setup-CANCEL marks: one per `callRef` and INVITE CSeq, cleared one at
//! a time or by the release of their call.

use std::sync::Arc;

use super::{CallState, CallStore, InMemoryCallStore};
use crate::metrics::B2buaMetrics;

fn state() -> CallState {
    let store = Arc::new(InMemoryCallStore::new());
    CallState::new(store as Arc<dyn CallStore>, "w0", B2buaMetrics::new())
}

#[test]
fn a_mark_is_keyed_by_call_and_cseq() {
    let s = state();
    s.mark_setup_cancelled("c", 1);
    s.mark_setup_cancelled("c", 1);
    s.mark_setup_cancelled("c", 2);
    s.mark_setup_cancelled("d", 1);
    assert_eq!(s.setup_cancelled_count(), 3, "marking twice is one mark");
    assert!(s.is_setup_cancelled("c", 1) && s.is_setup_cancelled("c", 2));
    assert!(!s.is_setup_cancelled("d", 2));

    s.clear_setup_cancelled("c", 1);
    assert!(!s.is_setup_cancelled("c", 1));
    assert!(s.is_setup_cancelled("c", 2), "the other CSeq keeps its mark");
    assert_eq!(s.setup_cancelled_count(), 2);
}

#[test]
fn a_release_keeps_only_the_marks_of_waiting_invites_of_its_call() {
    let s = state();
    s.mark_setup_cancelled("c", 1);
    s.mark_setup_cancelled("c", 2);
    s.mark_setup_cancelled("d", 1);
    s.retain_setup_cancelled("c", |cseq| cseq == 2);
    assert!(!s.is_setup_cancelled("c", 1));
    assert!(s.is_setup_cancelled("c", 2));
    assert!(s.is_setup_cancelled("d", 1), "another call's marks stay");
    s.retain_setup_cancelled("c", |_| false);
    s.clear_setup_cancelled("d", 1);
    assert_eq!(s.setup_cancelled_count(), 0);
}
