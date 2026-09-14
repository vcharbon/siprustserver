//! Quiesce-aware drain: wait for a node's live work to clear or for its peers to
//! hold it, bounded by a grace deadline. A node with no live calls exits at once;
//! a withdrawn node whose backups hold every live call exits as soon as a floor
//! has passed; anything else is capped at `grace` and its residual is reported,
//! never silently cut without a number.
//!
//! The inputs are three closures ([`DrainInputs`]) and two durations
//! ([`DrainBounds`]); the interface is one async function over them — the poll
//! loop, the deadline arithmetic and the immediate-return shortcut all live
//! behind it, so the runner's shutdown path stays a single call. The exit is
//! named ([`DrainExit`]) so "the peers were behind" is visible and never read as
//! a clean drain (ADR-0031 D2). Rides `tokio::time` directly, so
//! `#[tokio::test(start_paused = true)]` drives it exactly like every other
//! behavioural timer in the tree.

use std::sync::Arc;
use std::time::Duration;

/// How often the drain re-reads its inputs while waiting. Small enough that a
/// node which clears its last call mid-grace exits promptly, not at the next
/// big tick.
const DRAIN_POLL: Duration = Duration::from_millis(100);

/// An owned, clone-cheap probe the drain reads on each poll tick. Owned so the
/// wait can outlive the caller's borrow of whatever it reads.
pub type Probe<T> = Arc<dyn Fn() -> T + Send + Sync>;

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
    /// The snake_case reason label — the metric's `reason` value and the log field.
    pub fn label(self) -> &'static str {
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
    /// How long the drain waited.
    pub elapsed: Duration,
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
        }
    }
}

/// The drain's two durations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrainBounds {
    /// The ceiling: the drain never returns later than this.
    pub grace: Duration,
    /// The floor a [`DrainExit::CaughtUp`] exit waits out, so a request routed
    /// before the withdrawal reached the proxy is still served (ADR-0031 D2).
    pub floor: Duration,
}

/// Latch nothing here — the caller has already moved the node to `Draining` (so
/// the proxy is steering new calls away). This only *waits*, and returns on the
/// first of: the live calls clearing, a withdrawn node's backups holding them
/// past the floor, or the grace.
pub async fn drain_until_quiescent(inputs: DrainInputs, bounds: DrainBounds) -> DrainOutcome {
    let start = tokio::time::Instant::now();
    let deadline = start + bounds.grace;
    let floor_at = start + bounds.floor;
    loop {
        let n = (inputs.active)();
        if n == 0 {
            return DrainOutcome {
                exit: DrainExit::Quiescent,
                residual: 0,
                elapsed: start.elapsed(),
            };
        }
        let now = tokio::time::Instant::now();
        let withdrawn = (inputs.withdrawn)();
        // The caught-up exit needs all three preconditions: withdrawn, the floor
        // passed, and every live call's peer flow at this node's head.
        if withdrawn && now >= floor_at && (inputs.backups_caught_up)() {
            return DrainOutcome {
                exit: DrainExit::CaughtUp,
                residual: n,
                elapsed: start.elapsed(),
            };
        }
        if now >= deadline {
            // A withdrawn node reaching the grace lost a flush window; a node
            // that is not withdrawn simply still has calls (quiescence-or-grace).
            let exit = if withdrawn { DrainExit::GracePeersBehind } else { DrainExit::Grace };
            return DrainOutcome { exit, residual: n, elapsed: start.elapsed() };
        }
        // Never overshoot the deadline, and land exactly on the floor: a node
        // that won't quiesce returns its residual *at* the grace, and one whose
        // peers are already current exits *at* the floor, not a tick late.
        let mut wait = DRAIN_POLL.min(deadline - now);
        if floor_at > now {
            wait = wait.min(floor_at - now);
        }
        tokio::time::sleep(wait).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Grace only, no withdrawal — the shape every non-orchestrated shutdown has.
    fn bounds(grace: Duration) -> DrainBounds {
        DrainBounds { grace, floor: Duration::ZERO }
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
            DrainBounds { grace: Duration::from_secs(5), floor },
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
            DrainBounds { grace, floor: Duration::from_secs(1) },
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
            DrainBounds { grace, floor: Duration::from_secs(1) },
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
            DrainBounds { grace, floor: Duration::from_secs(5) },
        )
        .await;
        assert_eq!(out.exit, DrainExit::GracePeersBehind);
        assert_eq!(start.elapsed(), grace);
    }
}
