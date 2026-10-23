//! Pins `backup_held` to a scan of the meta map through every writer that
//! adds, re-roles or removes an entry.

use super::*;

fn scanned(store: &ReplicatingCallStore) -> u64 {
    let meta = store.meta.lock().unwrap();
    meta.values().filter(|m| m.role == PartitionRole::Backup).count() as u64
}

async fn put(store: &ReplicatingCallStore, role: PartitionRole, call_ref: &str, ttl_ms: i64) {
    store
        .put_call(role, "w0", call_ref, b"b".to_vec(), &[], ttl_ms, 1, 0, &PutOpts::default())
        .await
        .unwrap();
}

/// A refresh that lands between the reap's snapshot and its eviction (a
/// healed peer's flush while the sweep runs) keeps its fresh body and
/// meta, and the reap hands nothing back for it.
#[tokio::test(start_paused = true)]
async fn a_refresh_landing_after_the_reap_snapshot_survives_the_reap() {
    let clock = Clock::test_at(0);
    let store = ReplicatingCallStore::new(1, clock.clone());
    store
        .put_call(
            PartitionRole::Backup,
            "w0",
            "w0|r|t",
            b"old".to_vec(),
            &[],
            1_000,
            1,
            0,
            &PutOpts::default(),
        )
        .await
        .unwrap();
    tokio::time::advance(std::time::Duration::from_millis(2_000)).await;

    let expired = store.expired_snapshot(clock.now_ms());
    assert_eq!(expired.len(), 1, "the old body is expired");
    store
        .put_call(
            PartitionRole::Backup,
            "w0",
            "w0|r|t",
            b"new".to_vec(),
            &[],
            60_000,
            2,
            0,
            &PutOpts::default(),
        )
        .await
        .unwrap();
    let evicted = store.evict_snapshot(expired, clock.now_ms());

    assert!(evicted.is_empty(), "nothing is handed back for a refreshed ref");
    let body = store.get_call(PartitionRole::Backup, "w0", "w0|r|t").await.unwrap();
    assert_eq!(body.as_deref(), Some(&b"new"[..]), "the fresh body survives");
    assert_eq!(
        store.current_cv(PartitionRole::Backup, "w0", "w0|r|t"),
        Some((2, 0)),
        "and its meta"
    );
    assert_eq!(store.backup_held(), 1);
}

/// A panic under the metadata lock never takes the store down with it:
/// every later reader and the reap go on with the data as it stands.
#[tokio::test(start_paused = true)]
async fn a_poisoned_metadata_lock_still_reads_and_reaps() {
    let clock = Clock::test_at(0);
    let store = ReplicatingCallStore::new(1, clock.clone());
    put(&store, PartitionRole::Backup, "w0|p|t", 1_000).await;
    let poisoner = store.clone();
    let _ = std::thread::spawn(move || {
        let _held = poisoner.meta.lock();
        panic!("poison the metadata lock");
    })
    .join();
    assert!(store.meta.is_poisoned());

    tokio::time::advance(std::time::Duration::from_millis(2_000)).await;
    assert!(store.get_call(PartitionRole::Backup, "w0", "w0|p|t").await.unwrap().is_none());
    assert_eq!(store.reap(clock.now_ms()).await.len(), 1, "the reap still evicts");
    assert_eq!(store.backup_held(), 0);
}

#[tokio::test(start_paused = true)]
async fn the_backup_count_matches_a_scan_through_every_writer() {
    let clock = Clock::test_at(0);
    let store = ReplicatingCallStore::new(1, clock.clone()).with_default_ttl_ms(0);
    let check = |want: u64| {
        assert_eq!(store.backup_held(), want);
        assert_eq!(store.backup_held(), scanned(&store));
    };

    put(&store, PartitionRole::Backup, "w0|a|t", 0).await;
    put(&store, PartitionRole::Backup, "w0|b|t", 0).await;
    put(&store, PartitionRole::Primary, "w1|c|t", 0).await;
    check(2);

    put(&store, PartitionRole::Backup, "w0|a|t", 0).await;
    check(2);
    put(&store, PartitionRole::Primary, "w0|a|t", 0).await;
    check(1);
    put(&store, PartitionRole::Backup, "w0|a|t", 0).await;
    check(2);

    store
        .delete_call(PartitionRole::Backup, "w0", "w0|b|t", &[], false, &PutOpts::default())
        .await
        .unwrap();
    check(1);

    put(&store, PartitionRole::Backup, "w0|d|t", 1_000).await;
    put(&store, PartitionRole::Backup, "w0|e|t", 1_000).await;
    check(3);
    tokio::time::advance(std::time::Duration::from_millis(2_000)).await;
    assert!(store.get_call(PartitionRole::Backup, "w0", "w0|d|t").await.unwrap().is_none());
    check(3);
    assert_eq!(store.reap(clock.now_ms()).await.len(), 2, "the reap evicts both");
    check(1);
}
