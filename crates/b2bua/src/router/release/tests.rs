//! An orphan release keeps the call's one mutex: a turn waiting on the lock
//! of a call the releasing turn never made resident takes the same mutex a
//! later taker queues on.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::{release_call, ReleaseKind};
use crate::router::test_support::node;
use crate::store::CallState;

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
async fn a_waiter_through_an_orphan_release_keeps_the_one_mutex() {
    let n = node("w0").await;
    let ctx = n.core.router_ctx();
    let r = "w0|orphan@10.0.0.9|alicetag";
    let held = ctx.state.lock(r).await;
    let (first, release_first, first_task) = taker(&ctx.state, r);
    sip_clock::testkit::settle().await;

    drop(held);
    release_call(ctx, r, ReleaseKind::Orphan).await;
    sip_clock::testkit::settle().await;
    assert!(first.load(Ordering::SeqCst), "the waiter took the lock");

    let (second, release_second, second_task) = taker(&ctx.state, r);
    sip_clock::testkit::settle().await;
    assert!(!second.load(Ordering::SeqCst), "a later taker queues on the waiter's mutex");

    release_first.send(()).unwrap();
    first_task.await.unwrap();
    sip_clock::testkit::settle().await;
    assert!(second.load(Ordering::SeqCst));
    release_second.send(()).unwrap();
    second_task.await.unwrap();
    assert_eq!(ctx.state.lock_count(), 0, "the last guard took the entry");
}
