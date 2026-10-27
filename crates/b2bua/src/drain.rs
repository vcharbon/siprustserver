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
    /// Entries the calls' outboxes still have to send, each within the sync
    /// cap (ADR-0055 §3): an exit is taken only with none.
    pub outboxes_waiting: Probe<usize>,
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
            outboxes_waiting: Arc::new(|| 0),
        }
    }

    /// These inputs with `waiting` as the outbox entries left to send (the
    /// default reads none).
    pub fn with_outboxes_waiting(
        mut self,
        waiting: impl Fn() -> usize + Send + Sync + 'static,
    ) -> Self {
        self.outboxes_waiting = Arc::new(waiting);
        self
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
    /// The most the CDR sink's close waits, after the drain, for the records
    /// it holds (`CdrWriter::close`); the drain itself does not read it.
    pub cdr_flush: Duration,
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
                if (inputs.releases_waiting)() > 0 || (inputs.outboxes_waiting)() > 0 {
                    if now < ceiling {
                        // A release queued after the flush emptied the queue,
                        // or an outbox entry not yet sent: flush again, one
                        // poll later at most.
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
mod tests;
