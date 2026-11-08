use std::sync::Arc;
use std::time::Duration;

use super::*;

/// A panic under the store's lock stops no later admit: the state is taken as
/// it stood, and the recovery counted once.
#[test]
fn a_poisoned_store_recovers_and_counts_the_recovery() {
    let store = Arc::new(CallStore::new(LimiterConfig { lease_sec: 10 }, Clock::test_at(0)));
    let entries = [AdmitEntry { id: "trunk".into(), limit: 2 }];
    assert_eq!(store.admit("first", 1, &entries, false), AdmitResult::Admitted);
    let before = crate::poison::RECOVERIES.count("store");
    let held = store.clone();
    let poisoner = std::thread::spawn(move || {
        let _guard = held.inner.lock();
        panic!("a panic under the store's lock");
    });
    assert!(poisoner.join().is_err());
    assert!(store.inner.is_poisoned());

    assert_eq!(store.admit("second", 1, &entries, false), AdmitResult::Admitted);
    assert_eq!(store.held("trunk"), 2, "the state as it stood, the new admit on it");
    assert!(!store.inner.is_poisoned(), "the poison is cleared");
    assert_eq!(crate::poison::RECOVERIES.count("store"), before + 1, "counted once");
}

#[tokio::test(start_paused = true)]
async fn a_set_refreshed_many_times_keeps_one_pending_deadline() {
    let store = CallStore::new(LimiterConfig { lease_sec: 10 }, Clock::test_at(0));
    let entries = [AdmitEntry { id: "trunk".into(), limit: 1 }];
    assert_eq!(store.admit("call", 1, &entries, false), AdmitResult::Admitted);
    for _ in 0..100 {
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(store.refresh("call", 1, &entries).result, RefreshResult::Extended);
    }
    assert_eq!(store.inner.lock().unwrap().set_deadlines.len(), 1);

    // The set still lapses one lease after its last refresh.
    tokio::time::advance(Duration::from_millis(9_999)).await;
    assert_eq!(store.sweep_now(), 0);
    tokio::time::advance(Duration::from_millis(1)).await;
    assert_eq!(store.sweep_now(), 1);
    assert_eq!(store.inner.lock().unwrap().set_deadlines.len(), 0);
}
