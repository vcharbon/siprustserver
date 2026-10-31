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

#[tokio::test(start_paused = true)]
async fn a_watch_wakes_on_its_own_invites_mark_only() {
    let s = state();
    let watch = s.watch_setup_cancel("c", 1);
    s.mark_setup_cancelled("c", 2);
    s.mark_setup_cancelled("d", 1);
    let early = tokio::time::timeout(std::time::Duration::from_millis(10), watch.marked()).await;
    assert!(early.is_err(), "another INVITE's mark wakes nothing");
    s.mark_setup_cancelled("c", 1);
    let woken = tokio::time::timeout(std::time::Duration::from_millis(10), watch.marked()).await;
    assert!(woken.is_ok(), "the INVITE's own mark wakes its watch");
}

#[tokio::test(start_paused = true)]
async fn a_watch_taken_after_the_mark_resolves_at_once() {
    let s = state();
    s.mark_setup_cancelled("c", 1);
    let watch = s.watch_setup_cancel("c", 1);
    let woken = tokio::time::timeout(std::time::Duration::from_millis(10), watch.marked()).await;
    assert!(woken.is_ok(), "a mark set before the watch resolves it");
}

#[test]
fn a_dropped_watch_leaves_no_entry() {
    let s = state();
    let first = s.watch_setup_cancel("c", 1);
    let second = s.watch_setup_cancel("c", 1);
    drop(first);
    assert_eq!(s.setup_cancel_watch_count(), 1, "a replaced watch keeps its successor's entry");
    drop(second);
    assert_eq!(s.setup_cancel_watch_count(), 0);
}
