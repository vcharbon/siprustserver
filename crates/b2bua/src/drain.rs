//! Quiesce-aware drain: wait for a node's live work to clear or for its peers to
//! hold it, bounded by a grace deadline, then flush the node's limiter release
//! queue. A node with no live calls exits at once; a withdrawn node whose
//! backups hold every live call exits as soon as a floor has passed; anything
//! else is capped at `grace` and its residual is reported, never silently cut
//! without a number.
//!
//! The inputs are four closures ([`DrainInputs`]) and three durations
//! ([`DrainBounds`]); the interface is one async function over them — the
//! poll loop, the deadline arithmetic, the release flush and the
//! immediate-return shortcut all live behind it, so the runner's shutdown path
//! stays a single call. A clean exit (quiescent, caught up) is taken at a
//! moment verified after the flush, since the node serves its calls while it
//! flushes. The exit is named ([`DrainExit`]) so "the peers were behind" is
//! visible and never read as a clean drain (ADR-0031 D2). Rides `tokio::time`
//! directly, so `#[tokio::test(start_paused = true)]` drives it exactly like
//! every other behavioural timer in the tree.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::limiter::release_queue::ReleaseFlush;

/// How often the drain re-reads its inputs while waiting. Small enough that a
/// node which clears its last call mid-grace exits promptly, not at the next
/// big tick.
const DRAIN_POLL: Duration = Duration::from_millis(100);

/// An owned, clone-cheap probe the drain reads on each poll tick. Owned so the
/// wait can outlive the caller's borrow of whatever it reads.
pub type Probe<T> = Arc<dyn Fn() -> T + Send + Sync>;

/// The worker's limiter release flush, bounded by the duration it is given.
pub type ReleaseFlusher =
    Arc<dyn Fn(Duration) -> Pin<Box<dyn Future<Output = ReleaseFlush> + Send>> + Send + Sync>;

/// Why the drain returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainExit {
    /// Every live call cleared — the node holds nothing.
    Quiescent,
    /// The node is withdrawn from routing and, for every live call, the peer that
    /// holds the other copy has applied this node's changelog head (ADR-0031 D2):
    /// a *replicated* crash, deliberately, with calls still live.
    CaughtUp,
    /// The grace elapsed with calls still live on a node that is **not**
    /// withdrawn: quiescence-or-grace, as for any non-orchestrated shutdown.
    Grace,
    /// The grace elapsed on a **withdrawn** node whose peers never reported
    /// holding its live calls — a flush window was lost, and must be visible.
    GracePeersBehind,
}

impl DrainExit {
    /// Quiescent or caught up: nothing is abandoned unheld.
    pub fn is_clean(self) -> bool {
        matches!(self, DrainExit::Quiescent | DrainExit::CaughtUp)
    }

    /// Every exit, in declaration order.
    pub const ALL: [DrainExit; 4] =
        [DrainExit::Quiescent, DrainExit::CaughtUp, DrainExit::Grace, DrainExit::GracePeersBehind];

    /// The snake_case reason label — the metric's `reason` value and the log field.
    pub const fn label(self) -> &'static str {
        match self {
            DrainExit::Quiescent => "quiescent",
            DrainExit::CaughtUp => "caught_up",
            DrainExit::Grace => "grace",
            DrainExit::GracePeersBehind => "grace_peers_behind",
        }
    }
}

/// What the drain returned with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrainOutcome {
    /// Why it returned.
    pub exit: DrainExit,
    /// Live calls at the exit — `0` for [`DrainExit::Quiescent`], the count the
    /// node is about to abandon otherwise.
    pub residual: usize,
    /// How long the drain waited, its release flushes included.
    pub elapsed: Duration,
    /// What its release flushes did, summed.
    pub release_flush: ReleaseFlush,
}

/// The three things the drain reads, each on every poll tick.
pub struct DrainInputs {
    /// Live calls this node is serving.
    pub active: Probe<usize>,
    /// Whether every live call's other copy is held by a peer that has applied
    /// this node's changelog head (ADR-0031 D2).
    pub backups_caught_up: Probe<bool>,
    /// Whether this node has observed its own withdrawal from routing
    /// (ADR-0031 D6) — the precondition that nothing new can arrive.
    pub withdrawn: Probe<bool>,
    /// Flush the worker's limiter release queue before an exit.
    pub flush_releases: ReleaseFlusher,
    /// Releases the worker's queue still has to send (none once it is
    /// stopped): an exit is taken only with none.
    pub releases_waiting: Probe<usize>,
}

impl DrainInputs {
    /// Build the inputs from three closures.
    pub fn new(
        active: impl Fn() -> usize + Send + Sync + 'static,
        backups_caught_up: impl Fn() -> bool + Send + Sync + 'static,
        withdrawn: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            active: Arc::new(active),
            backups_caught_up: Arc::new(backups_caught_up),
            withdrawn: Arc::new(withdrawn),
            flush_releases: Arc::new(|_| Box::pin(async { ReleaseFlush::default() })),
            releases_waiting: Arc::new(|| 0),
        }
    }

    /// These inputs with `waiting` as the release queue's depth (the default
    /// reads none).
    pub fn with_releases_waiting(
        mut self,
        waiting: impl Fn() -> usize + Send + Sync + 'static,
    ) -> Self {
        self.releases_waiting = Arc::new(waiting);
        self
    }

    /// These inputs with `flush` as the release flush (the default flushes
    /// nothing).
    pub fn with_release_flush(
        mut self,
        flush: impl Fn(Duration) -> Pin<Box<dyn Future<Output = ReleaseFlush> + Send>>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        self.flush_releases = Arc::new(flush);
        self
    }
}

/// The bounds of a planned exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrainBounds {
    /// The ceiling of the wait for the live calls: it never returns later
    /// than this.
    pub grace: Duration,
    /// The floor a [`DrainExit::CaughtUp`] exit waits out, so a request routed
    /// before the withdrawal reached the proxy is still served (ADR-0031 D2).
    pub floor: Duration,
    /// The most one release flush waits for the limiter release queue to
    /// empty; the drain never returns later than `grace + release_flush`.
    pub release_flush: Duration,
}

/// Latch nothing here — the caller has already moved the node to `Draining` (so
/// the proxy is steering new calls away). This waits for the first of: the
/// live calls clearing, a withdrawn node's backups holding them past the floor,
/// or the grace; then flushes the release queue within `release_flush`. An exit
/// is taken only if it still holds once the flush is done (a clean exit's
/// condition, and an empty release queue); otherwise the wait goes on, and the
/// drain never returns later than the grace plus the flush bound, where what
/// is still queued is given up.
pub async fn drain_until_quiescent(inputs: DrainInputs, bounds: DrainBounds) -> DrainOutcome {
    let start = tokio::time::Instant::now();
    let deadline = start + bounds.grace;
    let floor_at = start + bounds.floor;
    let ceiling = deadline + bounds.release_flush;
    let mut flushed = ReleaseFlush::default();
    let flush = async |flushed: &mut ReleaseFlush, within: Duration| {
        let flush = (inputs.flush_releases)(within).await;
        flushed.queued += flush.queued;
        flushed.given_up += flush.given_up;
        flushed.elapsed += flush.elapsed;
    };
    loop {
        let now = tokio::time::Instant::now();
        if exit_at(&inputs, now, floor_at, deadline).0.is_some() {
            flush(&mut flushed, bounds.release_flush.min(ceiling.saturating_duration_since(now)))
                .await;
            // The node served its calls while it flushed: a clean exit must
            // still hold; past the deadline an exit always stands.
            let now = tokio::time::Instant::now();
            let (exit, live) = exit_at(&inputs, now, floor_at, deadline);
            if let Some(exit) = exit.filter(|e| e.is_clean() || now >= deadline) {
                if (inputs.releases_waiting)() > 0 {
                    if now < ceiling {
                        // A release queued after the flush emptied the queue:
                        // flush again, one poll later at most.
                        tokio::time::sleep(DRAIN_POLL.min(ceiling - now)).await;
                        continue;
                    }
                    flush(&mut flushed, Duration::ZERO).await;
                }
                return DrainOutcome {
                    exit,
                    residual: live,
                    elapsed: start.elapsed(),
                    release_flush: flushed,
                };
            }
        }
        // Never overshoot the deadline, and land exactly on the floor: a node
        // that won't quiesce reaches its grace *at* the grace, and one whose
        // peers are already current exits *at* the floor, not a tick late.
        let now = tokio::time::Instant::now();
        let mut wait = DRAIN_POLL.min(deadline.saturating_duration_since(now));
        if floor_at > now {
            wait = wait.min(floor_at - now);
        }
        tokio::time::sleep(wait).await;
    }
}

/// The exit the inputs name at `now`, if any, and the live calls read.
fn exit_at(
    inputs: &DrainInputs,
    now: tokio::time::Instant,
    floor_at: tokio::time::Instant,
    deadline: tokio::time::Instant,
) -> (Option<DrainExit>, usize) {
    let live = (inputs.active)();
    if live == 0 {
        return (Some(DrainExit::Quiescent), 0);
    }
    let withdrawn = (inputs.withdrawn)();
    // The caught-up exit needs all three preconditions: withdrawn, the floor
    // passed, and every live call's peer flow at this node's head.
    if withdrawn && now >= floor_at && (inputs.backups_caught_up)() {
        return (Some(DrainExit::CaughtUp), live);
    }
    // A withdrawn node reaching the grace lost a flush window; a node that is
    // not withdrawn simply still has calls (quiescence-or-grace).
    let grace = if withdrawn { DrainExit::GracePeersBehind } else { DrainExit::Grace };
    ((now >= deadline).then_some(grace), live)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// Grace only, no withdrawal — the shape every non-orchestrated shutdown has.
    fn bounds(grace: Duration) -> DrainBounds {
        DrainBounds { grace, floor: Duration::ZERO, release_flush: Duration::ZERO }
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
            DrainBounds { grace: Duration::from_secs(5), floor, release_flush: Duration::ZERO },
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
            DrainBounds { grace, floor: Duration::from_secs(1), release_flush: Duration::ZERO },
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
            DrainBounds { grace, floor: Duration::from_secs(1), release_flush: Duration::ZERO },
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
            DrainBounds { grace, floor: Duration::from_secs(5), release_flush: Duration::ZERO },
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
            },
        )
        .await;
        assert_eq!(out.exit, DrainExit::Quiescent);
        assert_eq!(
            waiting.load(Ordering::SeqCst),
            0,
            "the late release was flushed before the exit"
        );
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
        };
        let out = drain_until_quiescent(inputs, bounds).await;
        assert_eq!(out.exit, DrainExit::Grace);
        assert_eq!(out.residual, 1);
        assert_eq!(out.elapsed, bounds.grace + bounds.release_flush);
    }
}
