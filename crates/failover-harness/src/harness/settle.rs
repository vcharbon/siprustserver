//! The waits of [`FailoverHarness`] past a fixed advance: pumping the paused
//! clock toward a deadline nobody can compute, and settling a call's end.

use std::time::Duration;

use scenario_harness::Agent;

use super::FailoverHarness;

impl FailoverHarness {
    /// **Fine-grained pump toward an unknown timer deadline.** Advances the
    /// paused clock in `step` increments (each an [`advance`](Self::advance)
    /// across both planes), running the async `ready` probe *after every step*
    /// and returning `true` the instant it is satisfied — `false` if `max` total
    /// elapses first.
    ///
    /// Use this instead of a fixed [`advance`](Self::advance) whenever the
    /// deadline you need to react at is **not computable in advance** — e.g. a
    /// keepalive the reclaim re-armed a fresh interval out from an unknown
    /// reclaim instant. A fixed advance there either undershoots (the message has
    /// not been emitted yet, so a `receive` would block/auto-advance) or
    /// overshoots (it sails past the deadline *and* the deadline's own reap, e.g.
    /// the 5 s dead-peer timeout, tearing the call down before the test can
    /// answer — the CLAUDE.md keepalive hazard). The pump lets the test stop the
    /// instant the awaited message is queued and answer it inside its window. Pick
    /// `step` smaller than the tightest reaction window (e.g. ≤ 2 s for the 5 s
    /// reap). `ready` is run once *before* the first advance so an already-pending
    /// message costs no extra time; it typically drains the UA endpoints with
    /// [`Agent::try_receive_tolerating`](scenario_harness::Agent::try_receive_tolerating).
    pub async fn pump_until(
        &self,
        step: Duration,
        max: Duration,
        mut ready: impl AsyncFnMut() -> bool,
    ) -> bool {
        if ready().await {
            return true;
        }
        let mut elapsed = Duration::ZERO;
        while elapsed < max {
            self.advance(step).await;
            elapsed += step;
            if ready().await {
                return true;
            }
        }
        false
    }

    /// **The long-wait teardown settle.** After a call's terminal request,
    /// [`advance`](Self::advance) the paused clock until `drained` is satisfied —
    /// first in fine 200 ms steps for the immediate flush (CDR write, soft limiter
    /// release, reverse-delete drain and an acting-backup takeover copy's
    /// self-release on Timer H/J ~32 s), then in coarse 5 s steps **past one full
    /// `keepalive_interval` (300 s) + `keepalive_timeout` (45 s)** so that any
    /// zombie a buggy teardown left resurrectable *would* have armed its keepalive,
    /// probed, and surfaced (or self-healed) before we assert. Returns `true` if
    /// `drained` was satisfied within the budget, `false` if it timed out (the
    /// caller's `assert_call_fully_over` then reports precisely which invariant is
    /// still broken).
    pub async fn settle_terminal(&self, mut drained: impl AsyncFnMut() -> bool) -> bool {
        if drained().await {
            return true;
        }
        // Phase 1: fine steps for the fast path (flush + soft release, ~tens of s).
        let mut elapsed = Duration::ZERO;
        let fine = Duration::from_millis(200);
        while elapsed < Duration::from_secs(60) {
            self.advance(fine).await;
            elapsed += fine;
            if drained().await {
                return true;
            }
        }
        // Phase 2: coarse steps past keepalive_interval (300 s) + keepalive_timeout
        // (45 s) + margin, so a resurrected/stranded copy surfaces or self-heals.
        let coarse = Duration::from_secs(5);
        while elapsed < Duration::from_secs(400) {
            self.advance(coarse).await;
            elapsed += coarse;
            if drained().await {
                return true;
            }
        }
        false
    }

    /// Like [`settle_terminal`](Self::settle_terminal) but pumps past the deferral's
    /// replica TTL (`reboot_budget`, 600 s) so the backup's auto-cleanup reap
    /// has fired: a never-reclaimed deferred terminal has its limiter hold released
    /// and its body evicted only once that TTL expires. A cell whose backup defers
    /// to a primary that never returns needs this because NOTHING drains until then. Coarse 30 s steps (the reap cadence is
    /// 30 s) to `reboot_budget` + margin; stops as soon as `drained` holds.
    pub async fn settle_lossy_cleanup(&self, mut drained: impl AsyncFnMut() -> bool) -> bool {
        let step = Duration::from_secs(30);
        let mut elapsed = Duration::ZERO;
        while elapsed < Duration::from_secs(720) {
            if drained().await {
                return true;
            }
            self.advance(step).await;
            elapsed += step;
        }
        drained().await
    }

    /// **Post-terminal peer linger** — keep the named peer UAs' sockets open and
    /// *reading* for `window` of virtual time after the scenario's logical end, so
    /// any SIP still in flight at teardown is delivered AND consumed instead of
    /// dropped into an about-to-be-dropped endpoint (reported as "lost in transit")
    /// or left unread in the queue (a `queueLeak` at bind close).
    ///
    /// Two things go wrong without it, both because the synchronous test `drop`
    /// stops the world the instant the cell's body returns:
    /// 1. A datagram a peer *sent* just before the end (a redundant in-dialog BYE
    ///    the owner must answer `481`) never completes its transit hop — nothing
    ///    pumps the paused clock, so the delivery task's `sleep` never fires.
    /// 2. A datagram *delivered to* a peer's queue that the scenario never
    ///    explicitly `receive`d (a relayed final response, a retransmit toward a
    ///    deliberately-silent peer) is dropped unread when the endpoint closes.
    ///
    /// `linger_peers` pumps the clock in fine steps across `window` and drains each
    /// peer after every step (and once up front), modelling a real always-on UA
    /// that keeps its socket open and reading after the call. It asserts nothing —
    /// a cell that wants to *check* a specific late response (e.g. C10's `481` to
    /// bob) still does so explicitly; this only guarantees the trace is free of
    /// teardown-race losses. Call it after [`settle_terminal`](Self::settle_terminal),
    /// before the final invariant assertions.
    pub async fn linger_peers(&self, peers: &[&Agent], window: Duration) {
        let step = Duration::from_millis(200);
        for p in peers {
            p.drain().await;
        }
        let mut elapsed = Duration::ZERO;
        while elapsed < window {
            self.advance(step).await;
            elapsed += step;
            for p in peers {
                p.drain().await;
            }
        }
    }
}
