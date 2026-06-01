//! `WindowStore` core invariants: cap enforcement, transactional all-or-none,
//! release flooring, refresh migration (never undercounts), sum-of-N-windows,
//! and TTL sweep. Fast config (1 s windows, 3 active, 4 s TTL) under a paused
//! clock so `tokio::time::advance` crosses window/TTL boundaries.

use std::time::Duration;

use call_limiter::wire::{AdmitEntry, Hold};
use call_limiter::{AdmitResult, LimiterConfig, WindowStore};
use sip_clock::Clock;

fn fast_cfg() -> LimiterConfig {
    LimiterConfig {
        window_sec: 1,
        active_windows: 3,
        ttl_sec: 4,
    }
}

fn store() -> WindowStore {
    WindowStore::new(fast_cfg(), Clock::test_at(0))
}

fn entry(id: &str, limit: i64) -> Vec<AdmitEntry> {
    vec![AdmitEntry {
        id: id.into(),
        limit,
    }]
}

async fn advance(ms: u64) {
    tokio::time::advance(Duration::from_millis(ms)).await;
}

#[tokio::test(start_paused = true)]
async fn admits_until_cap_then_rejects() {
    let s = store();
    assert!(matches!(s.admit(&entry("A", 2)), AdmitResult::Admitted { .. }));
    assert!(matches!(s.admit(&entry("A", 2)), AdmitResult::Admitted { .. }));
    match s.admit(&entry("A", 2)) {
        AdmitResult::Rejected { limiter_id } => assert_eq!(limiter_id, "A"),
        other => panic!("expected reject, got {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn transactional_all_or_none() {
    let s = store();
    // Fill B to its cap of 1.
    assert!(matches!(s.admit(&entry("B", 1)), AdmitResult::Admitted { .. }));
    // A batch {A: room, B: full} must reject AND leave A un-incremented.
    let batch = vec![
        AdmitEntry { id: "A".into(), limit: 5 },
        AdmitEntry { id: "B".into(), limit: 1 },
    ];
    match s.admit(&batch) {
        AdmitResult::Rejected { limiter_id } => assert_eq!(limiter_id, "B"),
        other => panic!("expected reject, got {other:?}"),
    }
    // A was never incremented: it now admits 5 in a row.
    for _ in 0..5 {
        assert!(matches!(s.admit(&entry("A", 5)), AdmitResult::Admitted { .. }));
    }
}

#[tokio::test(start_paused = true)]
async fn release_floors_at_zero() {
    let s = store();
    let AdmitResult::Admitted { window } = s.admit(&entry("A", 5)) else {
        panic!("admit");
    };
    let hold = vec![Hold { id: "A".into(), window }];
    // Two releases against one increment: count floors at 0, never negative.
    s.release(&hold);
    s.release(&hold);
    assert_eq!(s.stats().current_total, 0);
    // Capacity fully restored.
    for _ in 0..5 {
        assert!(matches!(s.admit(&entry("A", 5)), AdmitResult::Admitted { .. }));
    }
}

#[tokio::test(start_paused = true)]
async fn sum_spans_active_windows_then_ages_out() {
    let s = store(); // 3 active 1 s windows
    let AdmitResult::Admitted { window: w0 } = s.admit(&entry("A", 3)) else {
        panic!()
    };
    advance(1000).await;
    assert!(matches!(s.admit(&entry("A", 3)), AdmitResult::Admitted { .. })); // w1
    advance(1000).await;
    assert!(matches!(s.admit(&entry("A", 3)), AdmitResult::Admitted { .. })); // w2; sum now 3
    // At t=2 the sum {w0,w1,w2} == 3 -> next admit rejects.
    assert!(matches!(s.admit(&entry("A", 3)), AdmitResult::Rejected { .. }));
    // At t=3, w0 ages out of the 3-window lookback -> headroom returns.
    advance(1000).await;
    assert!(matches!(s.admit(&entry("A", 3)), AdmitResult::Admitted { .. }));
    // w0 still physically present (TTL 4 s), just no longer summed.
    assert_eq!(s.current_window(), w0 + 3);
}

#[tokio::test(start_paused = true)]
async fn refresh_migrates_and_never_undercounts() {
    let s = store();
    let AdmitResult::Admitted { window: w0 } = s.admit(&entry("A", 1)) else {
        panic!()
    };
    let mut holds = vec![Hold { id: "A".into(), window: w0 }];
    // Advance one window and refresh: the count moves to the current window, so
    // the call keeps occupying a slot (cap of 1 still blocks a newcomer).
    advance(1000).await;
    holds = s.refresh(&holds);
    assert_eq!(holds[0].window, s.current_window());
    assert!(matches!(s.admit(&entry("A", 1)), AdmitResult::Rejected { .. }));
    // Total is exactly 1 the whole time (incr-before-decr never doubles or drops).
    assert_eq!(s.stats().current_total, 1);
    // Releasing the refreshed hold frees the slot.
    s.release(&holds);
    assert_eq!(s.stats().current_total, 0);
}

#[tokio::test(start_paused = true)]
async fn ttl_sweep_auto_clears_idle_keys() {
    let s = store(); // ttl 4 s
    s.admit(&entry("A", 5));
    assert_eq!(s.stats().live_keys, 1);
    // Idle past the TTL, then poke the store: the expired key is swept.
    advance(5000).await;
    let swept = s.sweep_now();
    assert_eq!(swept, 1);
    let st = s.stats();
    assert_eq!(st.live_keys, 0);
    assert_eq!(st.auto_cleared, 1);
}

#[tokio::test(start_paused = true)]
async fn seeded_fuzz_keeps_counts_nonnegative_and_consistent() {
    // Deterministic LCG; no wall-clock randomness.
    let mut seed: u64 = 0x1234_5678;
    let mut next = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) as u32
    };
    let s = store();
    let mut outstanding: Vec<Hold> = Vec::new();
    let mut net: i64 = 0; // admits - releases, all within window 0 (no time advance)
    for _ in 0..2000 {
        if next() % 2 == 0 {
            // High cap so admits (almost) always succeed within one window.
            if let AdmitResult::Admitted { window } = s.admit(&entry("A", 1_000_000)) {
                outstanding.push(Hold { id: "A".into(), window });
                net += 1;
            }
        } else if let Some(h) = outstanding.pop() {
            s.release(&[h]);
            net -= 1;
        }
        // Invariant: the live total equals outstanding admits, never negative.
        let total = s.stats().current_total;
        assert!(total >= 0, "count went negative");
        assert_eq!(total, net, "live total tracks admits-minus-releases");
    }
}
