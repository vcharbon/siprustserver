//! One tick of the stale-payload sweep over the workers the registry reads
//! alive (ADR-0050 §6): a worker not alive takes no new call, and every
//! `200` it answers once back carries a payload.

use crate::registry::WorkerRegistry;

use super::WorkerLoadObserver;

/// Run [`sweep_stale`](WorkerLoadObserver::sweep_stale) at `now_ms` over the
/// workers `registry` reads alive. Returns the number floored.
pub fn sweep_alive(
    observer: &WorkerLoadObserver,
    registry: &dyn WorkerRegistry,
    now_ms: i64,
) -> u64 {
    let alive: Vec<String> =
        registry.snapshot().into_iter().filter(|w| w.is_alive()).map(|w| w.id).collect();
    observer.sweep_stale(now_ms, |id| alive.iter().any(|a| a == id))
}
