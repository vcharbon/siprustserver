//! How a worker's process leaves, as its host runs it: the graceful drain,
//! waited out or left in flight, and the stuck exit once the poll watchdog
//! read a task stuck (ADR-0057 §6). The process exit that follows is the
//! test's `crash`.

use std::time::Duration;

use b2bua::drain::{DrainBounds, DrainExit, DrainOutcome};
use b2bua::StuckDischarge;

use super::ReplicatedB2buaSut;
use crate::views::Belief;

impl ReplicatedB2buaSut {
    /// Begin a graceful drain of this incarnation ([`b2bua::B2buaCore::drain`]):
    /// latch `Draining` — the proxy's OPTIONS probe then steers new calls away —
    /// and wait for the first of the live calls clearing, a withdrawn worker's
    /// backups holding them past the floor, or the grace. Returns the named
    /// exit + residual. The process keeps serving throughout: draining is not
    /// death.
    pub async fn begin_drain(&self, bounds: DrainBounds) -> DrainOutcome {
        self.views.record(
            &self.incarnation_key(),
            &self.ordinal,
            Belief::Draining { gen: self.gen },
            "drain",
        );
        match self.core.as_ref() {
            Some(core) => core.drain(bounds).await,
            None => DrainOutcome {
                exit: DrainExit::Quiescent,
                residual: 0,
                elapsed: Duration::ZERO,
                release_flush: Default::default(),
            },
        }
    }

    /// [`begin_drain`](Self::begin_drain) without the wait: latch `Draining`
    /// now and hand the wait back as a [`PendingDrain`] the test polls while it
    /// drives the timeline. The wait holds only the core's owned probes, its
    /// release flush included, so the process can still be killed mid-drain;
    /// a killed incarnation's flush returns at once.
    pub fn begin_drain_detached(&self, bounds: DrainBounds) -> PendingDrain {
        self.views.record(
            &self.incarnation_key(),
            &self.ordinal,
            Belief::Draining { gen: self.gen },
            "drain",
        );
        let probe = self.core.as_ref().map(|core| {
            core.begin_draining();
            core.drain_probe()
        });
        let metrics = self.core.as_ref().map(|core| core.metrics().clone());
        let limiter = self.core.as_ref().map(|core| core.limiter().clone());
        let mut pending = PendingDrain {
            fut: Box::pin(async move {
                match probe {
                    Some(p) => {
                        // The core's own drain, flush included, without the
                        // borrow of the core; it records the same exit, unless
                        // the incarnation was killed mid-drain.
                        let out = b2bua::drain::drain_until_quiescent(p, bounds).await;
                        let killed = limiter.is_some_and(|l| l.is_stopped());
                        if let (Some(m), false) = (metrics, killed) {
                            m.record_drain_exit(&out);
                        }
                        out
                    }
                    None => DrainOutcome {
                        exit: DrainExit::Quiescent,
                        residual: 0,
                        elapsed: Duration::ZERO,
                        release_flush: Default::default(),
                    },
                }
            }),
            outcome: None,
        };
        // One poll arms the first poll-interval sleep on the paused clock.
        pending.poll();
        pending
    }

    /// Latch `Draining` and discharge `call_ref`, the call the stuck task
    /// `task` serves, within `within`, as the host's stuck exit does. `None`
    /// while crashed.
    pub async fn discharge_stuck(
        &self,
        call_ref: &str,
        task: tokio::task::Id,
        within: Duration,
    ) -> Option<StuckDischarge> {
        let core = self.core.as_ref()?;
        core.begin_draining();
        Some(core.discharge_stuck(call_ref, task, within).await)
    }
}

/// A drain the test started and has not waited out: the node is latched
/// `Draining` and keeps serving its live calls while the timeline runs. Owns
/// its wait (an active-call probe, not the node), so the caller stays free to
/// kill the process mid-drain. Created by
/// [`FailoverHarness::begin_drain_pending`](crate::FailoverHarness::begin_drain_pending).
pub struct PendingDrain {
    fut: std::pin::Pin<Box<dyn std::future::Future<Output = DrainOutcome> + Send>>,
    outcome: Option<DrainOutcome>,
}

impl PendingDrain {
    /// The drain's outcome once it has returned — why it exited, the residual
    /// live-call count and how long it waited — `None` while it still waits.
    /// Re-polls the drain, so call it after every advance: nothing else drives
    /// this future.
    pub fn poll(&mut self) -> Option<DrainOutcome> {
        if self.outcome.is_none() {
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            if let std::task::Poll::Ready(out) = self.fut.as_mut().poll(&mut cx) {
                self.outcome = Some(out);
            }
        }
        self.outcome
    }
}
