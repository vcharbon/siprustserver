//! [`WorkerReach`] — whether any worker is reachable from this proxy: the
//! instant of the last answer the [`HealthProbe`](super::HealthProbe) heard
//! from any worker. A proxy that heard none for its isolation window is
//! isolated, whatever each worker's own health still reads: the verdict is
//! about the proxy's own reach and comes before the per-worker dead verdicts,
//! which stay the routing's.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

/// The last worker answer this proxy heard, on its own monotonic timeline.
#[derive(Debug)]
pub struct WorkerReach {
    /// The origin of the timeline.
    origin: Instant,
    /// Milliseconds from `origin` to the last answer, plus one; `0` while no
    /// worker has answered.
    last_answer: AtomicU64,
}

impl WorkerReach {
    /// A proxy that has heard no worker yet.
    pub fn unheard() -> Arc<Self> {
        Arc::new(Self { origin: Instant::now(), last_answer: AtomicU64::new(0) })
    }

    /// A worker answered now.
    pub fn answered(&self) {
        self.last_answer.store(self.elapsed_ms().saturating_add(1), Ordering::Relaxed);
    }

    /// Whether no worker answered within the last `window`: true before the
    /// first answer.
    pub fn isolated(&self, window: Duration) -> bool {
        match self.last_answer.load(Ordering::Relaxed) {
            0 => true,
            stamp => {
                let window_ms = u64::try_from(window.as_millis()).unwrap_or(u64::MAX);
                self.elapsed_ms().saturating_add(1).saturating_sub(stamp) > window_ms
            }
        }
    }

    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_millis(2_000);

    #[tokio::test(start_paused = true)]
    async fn a_proxy_that_heard_no_worker_yet_is_isolated() {
        let reach = WorkerReach::unheard();
        assert!(reach.isolated(WINDOW));
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(reach.isolated(WINDOW));
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_holds_reach_for_the_window_its_last_instant_included() {
        let reach = WorkerReach::unheard();
        tokio::time::sleep(Duration::from_millis(300)).await;
        reach.answered();
        tokio::time::sleep(WINDOW).await;
        assert!(!reach.isolated(WINDOW), "the window's last instant still holds reach");
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert!(reach.isolated(WINDOW));
        reach.answered();
        assert!(!reach.isolated(WINDOW), "a new answer restores reach at once");
    }
}
