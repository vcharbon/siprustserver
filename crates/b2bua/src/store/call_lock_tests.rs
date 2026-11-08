//! The per-call lock keeps one mutex per call while any task holds it or
//! waits on it: a release under a held guard leaves a later taker waiting
//! for that holder, and the last guard of a call that is not resident takes
//! the entry with it, a waiter aborted in the queue included.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::{CallState, InMemoryCallStore};
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::metrics::B2buaMetrics;
use crate::test_call::{invite, src};

fn state() -> CallState {
    CallState::new(Arc::new(InMemoryCallStore::new()), "w0", B2buaMetrics::new())
}

/// A call of `w0`'s own, resident in `state`.
fn resident(state: &CallState, cid: &str) -> String {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let mut ids = sip_ids::TurnIds::fixed(1);
    state.create(build_initial_call(&invite("w0", "w1", cid), src(), &config, &mut ids, 0), None)
}

/// A task taking `call_ref`'s lock and holding it until `release` fires:
/// whether it took the lock, the trigger, and the task.
fn taker(
    state: &CallState,
    call_ref: &str,
) -> (Arc<AtomicBool>, oneshot::Sender<()>, JoinHandle<()>) {
    let (state, call_ref) = (state.clone(), call_ref.to_string());
    let taken = Arc::new(AtomicBool::new(false));
    let (release, released) = oneshot::channel();
    let flag = taken.clone();
    let task = tokio::spawn(async move {
        let _guard = state.lock(&call_ref).await;
        flag.store(true, Ordering::SeqCst);
        let _ = released.await;
    });
    (taken, release, task)
}

#[tokio::test(start_paused = true)]
async fn a_lock_after_a_release_under_a_guard_waits_for_the_releasing_holder() {
    let state = state();
    let r = resident(&state, "released");
    let held = state.lock(&r).await;
    assert!(state.drop_local(&r), "released under the guard");

    let (taken, release, task) = taker(&state, &r);
    sip_clock::testkit::settle().await;
    assert!(!taken.load(Ordering::SeqCst), "the taker waits for the releasing holder");

    drop(held);
    sip_clock::testkit::settle().await;
    assert!(taken.load(Ordering::SeqCst), "the taker holds the call's one mutex");
    release.send(()).unwrap();
    task.await.unwrap();
    assert_eq!(state.lock_count(), 0, "the last guard took the entry");
}

/// A `try_lock` that finds the lock held takes nothing and leaves the entry
/// count as it was; its guard is dropped with the map lock released, so the
/// attempt never deadlocks on it.
#[tokio::test(start_paused = true)]
async fn a_failed_try_lock_takes_nothing_and_leaves_the_count_balanced() {
    let state = state();
    let held = state.lock("held").await;
    assert!(state.try_lock("held").is_none(), "another task holds it");
    assert_eq!(state.lock_count(), 1, "the failed attempt left the one entry");
    drop(held);
    assert_eq!(state.lock_count(), 0);
    drop(state.try_lock("held").expect("free"));
    assert_eq!(state.lock_count(), 0, "a call never resident keeps no entry");

    let r = resident(&state, "resident");
    drop(state.try_lock(&r).expect("free"));
    assert_eq!(state.lock_count(), 1, "a resident call keeps its entry");
    let releasing = state.lock(&r).await;
    state.drop_local(&r);
    drop(releasing);
    assert_eq!(state.lock_count(), 0, "its release's guard took it");
}

#[tokio::test(start_paused = true)]
async fn an_unbound_lock_and_an_aborted_waiter_leave_no_entry() {
    let state = state();
    drop(state.lock("never-resident").await);
    assert_eq!(state.lock_count(), 0, "a lock of a call never resident leaves no entry");

    let held = state.lock("aborted").await;
    let (taken, _release, task) = taker(&state, "aborted");
    sip_clock::testkit::settle().await;
    task.abort();
    let _ = task.await;
    assert!(!taken.load(Ordering::SeqCst), "the waiter was aborted in the queue");
    drop(held);
    assert_eq!(state.lock_count(), 0, "neither the holder nor the aborted waiter left one");
}

/// Guards of one call dropped at once on several threads: the takers of a
/// call never resident, and the failed attempts beside the release of a
/// resident one, leave no entry once all are gone. A smoke test of the
/// drop's cross-thread check under real scheduling, not a proof of its
/// fence: interleavings it never meets it does not rule out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guards_dropped_at_once_on_several_threads_leave_no_entry() {
    let state = state();
    for round in 0..200 {
        let takers: Vec<JoinHandle<()>> = (0..8)
            .map(|_| {
                let state = state.clone();
                tokio::spawn(async move { drop(state.lock("never-resident").await) })
            })
            .collect();
        for taker in takers {
            taker.await.unwrap();
        }
        assert_eq!(state.lock_count(), 0, "round {round}: a call never resident");

        let r = resident(&state, &format!("released-{round}"));
        let attempts: Vec<JoinHandle<()>> = (0..8)
            .map(|_| {
                let (state, r) = (state.clone(), r.clone());
                tokio::spawn(async move {
                    for _ in 0..32 {
                        drop(state.try_lock(&r));
                        tokio::task::yield_now().await;
                    }
                })
            })
            .collect();
        let releasing = state.lock(&r).await;
        state.drop_local(&r);
        drop(releasing);
        for attempt in attempts {
            attempt.await.unwrap();
        }
        assert_eq!(state.lock_count(), 0, "round {round}: a call released beside failed attempts");
    }
}
