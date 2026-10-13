//! [`WorkerLoops`] — the two loops a worker serves through: the transaction
//! layer's owner (recv, timer wheel, sweep) and the router. For each, whether
//! it is turning ([`LoopPulse`]: a loop idle on its input is alive, one parked
//! on a single piece of work is not) and whether its task still runs. The
//! host judges the pulses against a stall threshold for its probes and exits
//! when a task ends.

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

/// The worker's loops. Clone-cheap; holding it keeps no task alive.
#[derive(Clone)]
pub struct WorkerLoops {
    loops: [Loop; 2],
}

impl WorkerLoops {
    pub(crate) fn new(
        owner: (Arc<LoopPulse>, AbortHandle),
        router: (Arc<LoopPulse>, AbortHandle),
    ) -> Self {
        let [owner_name, router_name] = LOOP_NAMES;
        Self {
            loops: [
                Loop { name: owner_name, pulse: owner.0, task: owner.1 },
                Loop { name: router_name, pulse: router.0, task: router.1 },
            ],
        }
    }

    /// The loops on one piece of work for longer than `threshold`.
    pub fn stalled(&self, threshold: Duration) -> Vec<&'static str> {
        self.loops.iter().filter(|l| l.pulse.stalled(threshold)).map(|l| l.name).collect()
    }

    /// The first loop whose task has ended: returned, aborted or panicked.
    pub fn ended(&self) -> Option<&'static str> {
        self.loops.iter().find(|l| l.task.is_finished()).map(|l| l.name)
    }

    /// Resolves with the first loop whose task has ended, looked at every
    /// [`ENDED_POLL`].
    pub async fn wait_ended(&self) -> &'static str {
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
            u64::from(self.loops[series.index(&LOOP)].pulse.stalled(threshold))
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
}
