//! The worker's restarted long-lived tasks (ADR-0051 §5): each owns no state a
//! new run cannot rebuild, so a run that panics is counted under its name
//! (`b2bua_task_restarts_total{task}`) and made again. A task whose state a
//! new run cannot rebuild (the transaction owner, the router, the timer
//! driver, the CDR writer, the replication server) joins the
//! worker's loops instead, whose end exits the process ([`crate::loops`]).

use std::future::Future;

use task_restart::Restarts;
use tokio::task::JoinHandle;

/// The runner's memory-attribution and loop-stall sampler.
pub const GAUGE_SAMPLER: &str = "gauge_sampler";
/// The worker-side load sampler feeding `X-Overload` and the capacity gate.
pub const OVERLOAD_SAMPLER: &str = "overload_sampler";
/// The wall-clock divergence sampler.
pub const CLOCK_DIVERGENCE: &str = "clock_divergence";
/// The latch of the worker's own withdrawal into readiness.
pub const WITHDRAWAL_LATCH: &str = "withdrawal_latch";
/// The replication backup-deferral gate.
pub const BACKUP_GATE: &str = "backup_gate";
/// The replication supervisor's membership reconcile.
pub const MEMBERSHIP_RECONCILE: &str = "membership_reconcile";
/// A replication puller, respawned by the supervisor's next reconcile.
pub const REPL_PULLER: &str = "repl_puller";
/// The EndpointSlice informer feeding the replication membership.
pub const K8S_MEMBERSHIP: &str = "k8s_membership";

/// Every task name, in counter order: the `task` label's values.
pub const TASKS: [&str; 8] = [
    GAUGE_SAMPLER,
    OVERLOAD_SAMPLER,
    CLOCK_DIVERGENCE,
    WITHDRAWAL_LATCH,
    BACKUP_GATE,
    MEMBERSHIP_RECONCILE,
    REPL_PULLER,
    K8S_MEMBERSHIP,
];

/// The restarts of the worker's tasks in this process.
pub static RESTARTS: Restarts<{ TASKS.len() }> = Restarts::new(TASKS);

/// Spawn the worker task `name`, each run made by `make`, restarted on a
/// panic and counted ([`task_restart::spawn_restarting`]).
pub fn spawn<F, Fut>(name: &'static str, make: F) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    task_restart::spawn_restarting(&RESTARTS, name, make)
}
