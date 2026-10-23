//! The proxy's restarted long-lived tasks (ADR-0051 §5): each one owns no
//! state a new run cannot rebuild, so a run that panics is counted under its
//! name (`sip_proxy_task_restarts_total{task}`) and made again. The recv
//! shards and the health probe are not here: the runner ends the process
//! when one of them ends.

use std::future::Future;

use metric_catalogue::{Dim, Family, Labels};
use task_restart::Restarts;
use tokio::task::JoinHandle;

/// The CANCEL / retransmission memo's expiry sweep.
pub const BRANCH_MEMO_SWEEP: &str = "branch_memo_sweep";
/// A recv shard's endpoint-stats publisher.
pub const SHARD_STATS: &str = "shard_stats";
/// The resolver's off-path lookups: a send's lookup, a refresh, a prewarm.
pub const RESOLVER_LOOKUP: &str = "resolver_lookup";
/// The runner's stale-payload sweep of the load observer.
pub const STALE_FLOOR_SWEEP: &str = "stale_floor_sweep";
/// The runner's self-gate load sampler.
pub const SELF_GATE_SAMPLER: &str = "self_gate_sampler";
/// The runner's health gauges sampler.
pub const HEALTH_SAMPLER: &str = "health_sampler";
/// The EndpointSlice informer feeding the worker pool.
pub const K8S_MEMBERSHIP: &str = "k8s_membership";
/// The worker registry's follower of membership changes.
pub const REGISTRY_MEMBERSHIP: &str = "registry_membership";

/// Every task name, in counter order: the `task` label's values.
pub const TASKS: [&str; 8] = [
    BRANCH_MEMO_SWEEP,
    SHARD_STATS,
    RESOLVER_LOOKUP,
    STALE_FLOOR_SWEEP,
    SELF_GATE_SAMPLER,
    HEALTH_SAMPLER,
    K8S_MEMBERSHIP,
    REGISTRY_MEMBERSHIP,
];

/// The restarts of the proxy's tasks in this process.
pub static RESTARTS: Restarts<{ TASKS.len() }> = Restarts::new(TASKS);

/// The restarts, by task.
pub const TASK_RESTARTS: Family = Family::counter(
    "sip_proxy_task_restarts_total",
    Labels::Product(&[Dim::new("task", &TASKS)]),
    "Runs of a long-lived proxy task that panicked and were made again, by task; each one is a defect, expected 0.",
);

/// The families [`prometheus_text`] renders, in exposition order.
pub const FAMILIES: &[Family] = &[TASK_RESTARTS];

/// Prometheus exposition of [`FAMILIES`], appended by the runner's `/metrics`.
pub fn prometheus_text() -> String {
    let mut s = String::new();
    TASK_RESTARTS.render(&mut s, |series| RESTARTS.count_at(series.at(0)));
    s
}

/// Spawn the proxy task `name`, each run made by `make`, restarted on a
/// panic and counted ([`task_restart::spawn_restarting`]).
pub fn spawn<F, Fut>(name: &'static str, make: F) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    task_restart::spawn_restarting(&RESTARTS, name, make)
}
