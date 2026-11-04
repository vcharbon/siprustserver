use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Grace only, no withdrawal — the shape every non-orchestrated shutdown has.
fn bounds(grace: Duration) -> DrainBounds {
    DrainBounds {
        grace,
        floor: Duration::ZERO,
        release_flush: Duration::ZERO,
        cdr_flush: Duration::ZERO,
    }
}

#[tokio::test(start_paused = true)]
async fn returns_immediately_when_already_idle() {
    // No live calls ⇒ a clean node must not burn any of its grace.
    let start = tokio::time::Instant::now();
    let out = drain_until_quiescent(
        DrainInputs::new(|| 0, || false, || false),
        bounds(Duration::from_secs(30)),
    )
    .await;
    assert_eq!(out.exit, DrainExit::Quiescent);
    assert_eq!(out.residual, 0);
    assert_eq!(start.elapsed(), Duration::ZERO, "idle node should not wait");
}

#[tokio::test(start_paused = true)]
async fn exits_early_the_moment_calls_clear() {
    // Three calls that drop to zero a quarter-second in: the drain must
    // return ~then, well inside the 30 s grace — not wait the full grace.
    let n = Arc::new(AtomicUsize::new(3));
    let n2 = n.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        n2.store(0, Ordering::SeqCst);
    });
    let start = tokio::time::Instant::now();
    let out = drain_until_quiescent(
        DrainInputs::new(move || n.load(Ordering::SeqCst), || false, || false),
        bounds(Duration::from_secs(30)),
    )
    .await;
    assert_eq!(out.exit, DrainExit::Quiescent);
    assert_eq!(out.residual, 0);
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "should exit ~when calls cleared, got {:?}",
        start.elapsed()
    );
}

#[tokio::test(start_paused = true)]
async fn returns_residual_and_stops_at_the_grace_when_calls_never_clear() {
    // A wedged call that never clears: the drain is bounded by the grace
    // and hands back the residual count (the caller logs it).
    let grace = Duration::from_secs(5);
    let start = tokio::time::Instant::now();
    let out =
        drain_until_quiescent(DrainInputs::new(|| 2, || false, || false), bounds(grace)).await;
    assert_eq!(out.exit, DrainExit::Grace);
    assert_eq!(out.residual, 2);
    // Bounded *at* the grace — not a poll interval past it.
    assert_eq!(start.elapsed(), grace);
    assert_eq!(out.elapsed, grace);
}

#[tokio::test(start_paused = true)]
async fn a_withdrawn_node_whose_peers_are_current_waits_out_the_floor() {
    let floor = Duration::from_secs(1);
    let start = tokio::time::Instant::now();
    let out = drain_until_quiescent(
        DrainInputs::new(|| 1, || true, || true),
        DrainBounds {
            grace: Duration::from_secs(5),
            floor,
            release_flush: Duration::ZERO,
            cdr_flush: Duration::ZERO,
        },
    )
    .await;
    assert_eq!(out.exit, DrainExit::CaughtUp);
    assert_eq!(out.residual, 1, "the live call is abandoned to its backup, not lost");
    assert_eq!(start.elapsed(), floor, "the floor is waited out exactly");
}

#[tokio::test(start_paused = true)]
async fn a_withdrawn_node_whose_peers_stay_behind_runs_to_the_grace() {
    let grace = Duration::from_secs(5);
    let start = tokio::time::Instant::now();
    let out = drain_until_quiescent(
        DrainInputs::new(|| 1, || false, || true),
        DrainBounds {
            grace,
            floor: Duration::from_secs(1),
            release_flush: Duration::ZERO,
            cdr_flush: Duration::ZERO,
        },
    )
    .await;
    assert_eq!(out.exit, DrainExit::GracePeersBehind, "a flush window was lost");
    assert_eq!(out.residual, 1);
    assert_eq!(start.elapsed(), grace);
}

#[tokio::test(start_paused = true)]
async fn a_node_that_is_not_withdrawn_keeps_quiescence_or_grace() {
    // Current peers are not an exit for a worker still reachable from the
    // proxy (a direct-bound one would have its calls abandoned).
    let grace = Duration::from_secs(5);
    let start = tokio::time::Instant::now();
    let out = drain_until_quiescent(
        DrainInputs::new(|| 1, || true, || false),
        DrainBounds {
            grace,
            floor: Duration::from_secs(1),
            release_flush: Duration::ZERO,
            cdr_flush: Duration::ZERO,
        },
    )
    .await;
    assert_eq!(out.exit, DrainExit::Grace);
    assert_eq!(out.residual, 1);
    assert_eq!(start.elapsed(), grace);
}

#[tokio::test(start_paused = true)]
async fn the_grace_wins_over_a_floor_set_past_it() {
    // A floor above the grace leaves no caught-up window: the ceiling holds.
    let grace = Duration::from_secs(2);
    let start = tokio::time::Instant::now();
    let out = drain_until_quiescent(
        DrainInputs::new(|| 1, || true, || true),
        DrainBounds {
            grace,
            floor: Duration::from_secs(5),
            release_flush: Duration::ZERO,
            cdr_flush: Duration::ZERO,
        },
    )
    .await;
    assert_eq!(out.exit, DrainExit::GracePeersBehind);
    assert_eq!(start.elapsed(), grace);
}

/// A release queue holding `queued` entries that one flush takes `takes`
/// to send (at most the bound it is given, the rest given up), running
/// `during` as it starts; every later flush finds the queue empty.
fn flush_taking(
    takes: Duration,
    queued: usize,
    during: impl Fn() + Send + Sync + 'static,
) -> impl Fn(Duration) -> Pin<Box<dyn Future<Output = ReleaseFlush> + Send>> + Send + Sync {
    let during = Arc::new(during);
    let flushed = Arc::new(AtomicBool::new(false));
    move |within| {
        if flushed.swap(true, Ordering::SeqCst) {
            return Box::pin(async { ReleaseFlush::default() });
        }
        during();
        let waited = takes.min(within);
        let given_up = if takes > within { queued } else { 0 };
        Box::pin(async move {
            tokio::time::sleep(waited).await;
            ReleaseFlush { queued, given_up, elapsed: waited }
        })
    }
}

#[tokio::test(start_paused = true)]
async fn a_quiescent_exit_returns_once_the_release_flush_is_done() {
    let inputs = DrainInputs::new(|| 0, || false, || false).with_release_flush(flush_taking(
        Duration::from_secs(2),
        1,
        || {},
    ));
    let out = drain_until_quiescent(
        inputs,
        DrainBounds {
            grace: Duration::from_secs(5),
            floor: Duration::ZERO,
            release_flush: Duration::from_secs(3),
            cdr_flush: Duration::ZERO,
        },
    )
    .await;
    assert_eq!(out.exit, DrainExit::Quiescent);
    assert_eq!(out.elapsed, Duration::from_secs(2), "the drain waited for the flush");
    assert_eq!(
        out.release_flush,
        ReleaseFlush { queued: 1, given_up: 0, elapsed: Duration::from_secs(2) }
    );
}

#[tokio::test(start_paused = true)]
async fn a_grace_exit_flushes_within_its_bound_and_no_longer() {
    let inputs = DrainInputs::new(|| 1, || false, || false).with_release_flush(flush_taking(
        Duration::from_secs(60),
        2,
        || {},
    ));
    let out = drain_until_quiescent(
        inputs,
        DrainBounds {
            grace: Duration::from_secs(5),
            floor: Duration::ZERO,
            release_flush: Duration::from_secs(3),
            cdr_flush: Duration::ZERO,
        },
    )
    .await;
    assert_eq!(out.exit, DrainExit::Grace);
    assert_eq!(out.residual, 1);
    assert_eq!(out.elapsed, Duration::from_secs(8), "grace, then the flush bound");
    assert_eq!(
        out.release_flush,
        ReleaseFlush { queued: 2, given_up: 2, elapsed: Duration::from_secs(3) }
    );
}

/// The worker keeps serving its calls while it flushes: a change it logs
/// then puts its backup behind, and the caught-up exit waits until the
/// backup has applied it again (ADR-0031 D2).
#[tokio::test(start_paused = true)]
async fn a_caught_up_exit_is_taken_at_a_moment_verified_after_the_flush() {
    let caught_up = Arc::new(AtomicBool::new(true));
    let (probe, written) = (caught_up.clone(), caught_up.clone());
    let applied_at = Duration::from_millis(1_800);
    let applier = caught_up.clone();
    tokio::spawn(async move {
        tokio::time::sleep(applied_at).await;
        applier.store(true, Ordering::SeqCst);
    });
    let inputs = DrainInputs::new(|| 1, move || probe.load(Ordering::SeqCst), || true)
        .with_release_flush(flush_taking(Duration::from_millis(500), 1, move || {
            written.store(false, Ordering::SeqCst)
        }));
    let out = drain_until_quiescent(
        inputs,
        DrainBounds {
            grace: Duration::from_secs(5),
            floor: Duration::from_secs(1),
            release_flush: Duration::from_secs(3),
            cdr_flush: Duration::ZERO,
        },
    )
    .await;
    assert_eq!(out.exit, DrainExit::CaughtUp);
    assert!(
        out.elapsed >= applied_at,
        "the exit waits for the backup to apply what was logged during the flush: {:?}",
        out.elapsed
    );
    assert!(out.elapsed < applied_at + DRAIN_POLL + Duration::from_millis(1));
    assert!(caught_up.load(Ordering::SeqCst), "the exit's predicate holds when it returns");
    assert_eq!(out.release_flush.queued, 1, "one flush, before the verified moment");
}

/// A caught-up exit whose flush outlasts the grace and whose backup falls
/// behind meanwhile ends as a grace exit, within the grace plus the flush
/// bound.
#[tokio::test(start_paused = true)]
async fn a_caught_up_exit_lost_during_a_long_flush_is_bounded_by_the_grace_and_the_flush() {
    let caught_up = Arc::new(AtomicBool::new(true));
    let (probe, written) = (caught_up.clone(), caught_up.clone());
    let inputs = DrainInputs::new(|| 1, move || probe.load(Ordering::SeqCst), || true)
        .with_release_flush(flush_taking(Duration::from_secs(60), 1, move || {
            written.store(false, Ordering::SeqCst)
        }));
    let bounds = DrainBounds {
        grace: Duration::from_secs(2),
        floor: Duration::from_secs(1),
        release_flush: Duration::from_secs(3),
        cdr_flush: Duration::ZERO,
    };
    let out = drain_until_quiescent(inputs, bounds).await;
    assert_eq!(out.exit, DrainExit::GracePeersBehind, "the caught-up moment was not verified");
    assert_eq!(out.elapsed, bounds.floor + bounds.release_flush, "no second wait");
    assert!(out.elapsed <= bounds.grace + bounds.release_flush);
    assert_eq!(out.release_flush.given_up, 1);
}

/// A release pushed after the flush emptied the queue (a gone call's
/// fold, a reclaimed terminal) keeps a clean exit waiting: it is flushed
/// before the exit, not lost with the process.
#[tokio::test(start_paused = true)]
async fn a_release_pushed_after_the_flush_emptied_the_queue_is_flushed_before_the_exit() {
    let waiting = Arc::new(AtomicUsize::new(1));
    let flushes = Arc::new(AtomicUsize::new(0));
    let (probe, queue, count) = (waiting.clone(), waiting.clone(), flushes.clone());
    let inputs = DrainInputs::new(|| 0, || false, || false)
        .with_release_flush(move |_| {
            let queue = queue.clone();
            let first = count.fetch_add(1, Ordering::SeqCst) == 0;
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                // The first flush empties the queue, then one more
                // release lands before the drain looks again.
                queue.store(if first { 1 } else { 0 }, Ordering::SeqCst);
                ReleaseFlush { queued: 1, given_up: 0, elapsed: Duration::from_millis(500) }
            })
        })
        .with_releases_waiting(move || probe.load(Ordering::SeqCst));
    let out = drain_until_quiescent(
        inputs,
        DrainBounds {
            grace: Duration::from_secs(5),
            floor: Duration::ZERO,
            release_flush: Duration::from_secs(3),
            cdr_flush: Duration::ZERO,
        },
    )
    .await;
    assert_eq!(out.exit, DrainExit::Quiescent);
    assert_eq!(waiting.load(Ordering::SeqCst), 0, "the late release was flushed before the exit");
    assert_eq!(flushes.load(Ordering::SeqCst), 2);
    assert_eq!(out.release_flush.queued, 2);
}

/// A queue that never reads empty (a push racing every flush) keeps the
/// drain flushing at most until the grace plus the flush bound, then it
/// exits: it never spins on a flush that returns at once.
#[tokio::test(start_paused = true)]
async fn a_queue_that_never_settles_ends_the_drain_at_its_ceiling() {
    let flushes = Arc::new(AtomicUsize::new(0));
    let count = flushes.clone();
    let inputs = DrainInputs::new(|| 1, || false, || false)
        .with_release_flush(move |_| {
            let n = count.fetch_add(1, Ordering::SeqCst);
            assert!(n < 1_000, "the drain spins on its release flush");
            Box::pin(async { ReleaseFlush::default() })
        })
        .with_releases_waiting(|| 1);
    let bounds = DrainBounds {
        grace: Duration::from_secs(2),
        floor: Duration::ZERO,
        release_flush: Duration::from_secs(1),
        cdr_flush: Duration::ZERO,
    };
    let out = drain_until_quiescent(inputs, bounds).await;
    assert_eq!(out.exit, DrainExit::Grace);
    assert_eq!(out.residual, 1);
    assert_eq!(out.elapsed, bounds.grace + bounds.release_flush);
}

/// A withdrawn node whose backups caught up still exits only once every
/// outbox entry left (ADR-0055 §3): an entry waits at most the sync cap.
#[tokio::test(start_paused = true)]
async fn a_caught_up_exit_waits_for_the_outboxes_to_empty() {
    let queued = Arc::new(AtomicUsize::new(2));
    let probe = queued.clone();
    let inputs = DrainInputs::new(|| 1, || true, || true)
        .with_outboxes_waiting(move || probe.load(Ordering::SeqCst));
    let emptied = queued.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        emptied.store(0, Ordering::SeqCst);
    });
    let out = drain_until_quiescent(
        inputs,
        DrainBounds {
            grace: Duration::from_secs(5),
            floor: Duration::ZERO,
            release_flush: Duration::from_secs(1),
            cdr_flush: Duration::ZERO,
        },
    )
    .await;
    assert_eq!(out.exit, DrainExit::CaughtUp);
    assert_eq!(queued.load(Ordering::SeqCst), 0, "no outbox entry was left behind");
    assert!(out.elapsed >= Duration::from_millis(250), "it waited for the outboxes");
    assert!(out.elapsed <= Duration::from_millis(400), "and no longer: {:?}", out.elapsed);
}
