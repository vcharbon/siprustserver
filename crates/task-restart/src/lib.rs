//! Supervised long-lived tasks (ADR-0051 §5): a detached task whose handle
//! nobody joins dies silently on a panic, so a process names its long-lived
//! tasks once, in a [`Restarts`] table, and spawns each through
//! [`spawn_restarting`]: a run that panics is logged, counted against the
//! task's name and made again after [`RESTART_DELAY`]. A task whose state a
//! new run cannot rebuild is not restarted: the process ends on it instead.

// The panic set at deny outside test code (ADR-0051 §3).
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::string_slice,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::task::JoinHandle;

/// How long a task whose run panicked waits before its next run: a run that
/// panics at once costs one log line and one count per delay, not a spin.
pub const RESTART_DELAY: Duration = Duration::from_secs(1);

/// A process's named long-lived tasks and the restarts of each, in the order
/// of [`names`](Self::names): the values of a `task` label.
pub struct Restarts<const N: usize> {
    names: [&'static str; N],
    counts: [AtomicU64; N],
}

impl<const N: usize> Restarts<N> {
    /// The table of the tasks `names`, none restarted.
    pub const fn new(names: [&'static str; N]) -> Self {
        Self { names, counts: [const { AtomicU64::new(0) }; N] }
    }

    /// The task names, in counter order.
    pub const fn names(&self) -> &[&'static str; N] {
        &self.names
    }

    /// Restarts of the task named `name`; a name outside the table has none.
    pub fn count(&self, name: &str) -> u64 {
        self.slot(name).map_or(0, |c| c.load(Ordering::Relaxed))
    }

    /// Restarts of the task at `position` in [`names`](Self::names).
    pub fn count_at(&self, position: usize) -> u64 {
        self.counts.get(position).map_or(0, |c| c.load(Ordering::Relaxed))
    }

    fn slot(&self, name: &str) -> Option<&AtomicU64> {
        self.names.iter().position(|n| *n == name).and_then(|i| self.counts.get(i))
    }

    /// Count and log one panicked run of the task `name`, about to run again.
    pub fn restarted(&self, name: &'static str) {
        let total = match self.slot(name) {
            Some(count) => count.fetch_add(1, Ordering::Relaxed).saturating_add(1),
            None => {
                debug_assert!(false, "{name} is no task of this table");
                0
            }
        };
        tracing::error!(task = name, total, "a supervised task panicked; it runs again");
    }
}

/// Aborts the run it holds when dropped, so aborting the supervisor aborts
/// the run in flight.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Spawn the long-lived task `name` of `restarts`, each run made by `make`,
/// serving the node of the code that spawns it. A run that panics is counted
/// and made again [`RESTART_DELAY`] later; a run that returns ends the task.
/// Aborting the handle aborts the run in flight.
pub fn spawn_restarting<const N: usize, F, Fut>(
    restarts: &'static Restarts<N>,
    name: &'static str,
    mut make: F,
) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    observe::spawn_keeping_node(async move {
        loop {
            let mut run = AbortOnDrop(observe::spawn_keeping_node(make()));
            match (&mut run.0).await {
                Err(e) if e.is_panic() => {
                    restarts.restarted(name);
                    tokio::time::sleep(RESTART_DELAY).await;
                }
                Ok(()) | Err(_) => return,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    use super::*;

    static TABLE: Restarts<2> = Restarts::new(["sampler", "other"]);

    #[tokio::test(start_paused = true)]
    async fn a_run_that_panics_is_counted_and_made_again() {
        let runs = Arc::new(AtomicU32::new(0));
        let seen = runs.clone();
        let task = spawn_restarting(&TABLE, "sampler", move || {
            let n = seen.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 2 {
                    panic!("expected: run {n} panics");
                }
            }
        });
        tokio::time::sleep(RESTART_DELAY * 3).await;
        assert!(task.is_finished(), "the third run returned, so the task ended");
        assert_eq!(runs.load(Ordering::SeqCst), 3);
        assert_eq!(TABLE.count("sampler"), 2, "one restart per panicked run");
        assert_eq!(TABLE.count("other"), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn aborting_the_task_aborts_its_run() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);
        let task = spawn_restarting(&TABLE, "other", move || {
            let tx = tx.clone();
            async move {
                let _held = tx;
                std::future::pending::<()>().await;
            }
        });
        tokio::task::yield_now().await;
        task.abort();
        assert_eq!(rx.recv().await, None, "the run's sender dropped with it");
    }
}
