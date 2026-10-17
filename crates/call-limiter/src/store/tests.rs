use std::time::Duration;

use super::*;

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
