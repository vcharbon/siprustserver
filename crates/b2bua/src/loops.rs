//! [`WorkerLoops`] — the two loops a worker serves through: the transaction
//! layer's owner (recv, timer wheel, sweep) and the router. For each, whether
//! it is turning ([`LoopPulse`]: a loop idle on its input is alive, one parked
//! on a single piece of work is not) and whether its task still runs. Beside
//! them, the vital tasks the loops depend on (the timer driver, the store and
//! CDR drainers, the changelog server): whether each still runs. The host
//! judges the pulses against a stall threshold for its probes and exits when
//! a task ends.

use std::sync::Arc;
use std::time::Duration;

use sip_txn::LoopPulse;
use tokio::task::AbortHandle;

use crate::metrics::catalogue::worker::{LOOP, LOOP_STALLED};

/// How often [`WorkerLoops::wait_ended`] looks at the tasks.
pub const ENDED_POLL: Duration = Duration::from_secs(1);

/// The loops' names, as the `loop` label.
pub const LOOP_NAMES: [&str; 2] = ["txn_owner", "router"];

/// One loop: its pulse and its task.
#[derive(Clone)]
struct Loop {
    name: &'static str,
    pulse: Arc<LoopPulse>,
    task: AbortHandle,
}

/// The worker's loops and vital tasks. Holding it keeps no task alive.
#[derive(Clone)]
pub struct WorkerLoops {
    loops: [Loop; 2],
    vital: Vec<(&'static str, AbortHandle)>,
}

impl WorkerLoops {
    /// The loops of the owner's and the router's pulse and task.
    pub fn new(
        owner: (Arc<LoopPulse>, AbortHandle),
        router: (Arc<LoopPulse>, AbortHandle),
    ) -> Self {
        let [owner_name, router_name] = LOOP_NAMES;
        Self {
            loops: [
                Loop { name: owner_name, pulse: owner.0, task: owner.1 },
                Loop { name: router_name, pulse: router.0, task: router.1 },
            ],
            vital: Vec::new(),
        }
    }

    /// These loops and the vital task `task`, named `name`: the worker ends
    /// when it ends.
    pub fn with_vital(mut self, name: &'static str, task: AbortHandle) -> Self {
        self.vital.push((name, task));
        self
    }

    /// The loops on one piece of work for longer than `threshold`.
    pub fn stalled(&self, threshold: Duration) -> Vec<&'static str> {
        self.loops.iter().filter(|l| l.pulse.stalled(threshold)).map(|l| l.name).collect()
    }

    /// The first loop, then vital task, whose task has ended: returned,
    /// aborted or panicked.
    pub fn ended(&self) -> Option<&'static str> {
        let loops = self.loops.iter().map(|l| (l.name, &l.task));
        let vital = self.vital.iter().map(|(name, task)| (*name, task));
        loops.chain(vital).find(|(_, task)| task.is_finished()).map(|(name, _)| name)
    }

    /// Resolves with the first loop or vital task whose task has ended,
    /// looked at every [`ENDED_POLL`].
    pub async fn wait_ended(&self) -> &'static str {
        #[expect(clippy::disallowed_methods, reason = "ENDED_POLL is not zero")]
        let mut tick = tokio::time::interval(ENDED_POLL);
        loop {
            tick.tick().await;
            if let Some(name) = self.ended() {
                return name;
            }
        }
    }

    /// `b2bua_loop_stalled{loop}`: 1 for a loop stalled past `threshold`.
    pub fn prometheus_text(&self, threshold: Duration) -> String {
        let mut s = String::new();
        LOOP_STALLED.render(&mut s, |series| {
            let loop_ = self.loops.get(series.index(&LOOP));
            u64::from(loop_.is_some_and(|l| l.pulse.stalled(threshold)))
        });
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const THRESHOLD: Duration = Duration::from_secs(5);

    fn parked() -> AbortHandle {
        tokio::spawn(std::future::pending::<()>()).abort_handle()
    }

    fn loops() -> (WorkerLoops, Arc<LoopPulse>, AbortHandle) {
        let router = (Arc::new(LoopPulse::new()), parked());
        let loops = WorkerLoops::new((Arc::new(LoopPulse::new()), parked()), router.clone());
        (loops, router.0, router.1)
    }

    #[tokio::test(start_paused = true)]
    async fn a_loop_past_the_threshold_is_named_stalled_and_rendered() {
        let (loops, router, _) = loops();
        router.turning();
        tokio::time::advance(THRESHOLD + Duration::from_millis(1)).await;
        assert_eq!(loops.stalled(THRESHOLD), ["router"]);
        let text = loops.prometheus_text(THRESHOLD);
        assert!(text.contains("b2bua_loop_stalled{loop=\"txn_owner\"} 0"), "{text}");
        assert!(text.contains("b2bua_loop_stalled{loop=\"router\"} 1"), "{text}");
        router.waiting();
        assert!(loops.stalled(THRESHOLD).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn an_ended_task_is_named_within_one_poll() {
        let (loops, _, router_task) = loops();
        assert_eq!(loops.ended(), None);
        router_task.abort();
        let ended = tokio::time::timeout(ENDED_POLL * 2, loops.wait_ended()).await;
        assert_eq!(ended, Ok("router"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_vital_task_that_panics_is_named_within_one_poll() {
        let (loops, _, _) = loops();
        let vital = tokio::spawn(async { panic!("the vital task panics") });
        let loops = loops.with_vital("timer_driver", vital.abort_handle());
        let ended = tokio::time::timeout(ENDED_POLL * 2, loops.wait_ended()).await;
        assert_eq!(ended, Ok("timer_driver"));
        assert!(vital.await.is_err_and(|e| e.is_panic()));
    }
}
