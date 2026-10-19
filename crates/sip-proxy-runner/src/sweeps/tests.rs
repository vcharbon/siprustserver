use std::sync::atomic::{AtomicU32, Ordering};

use sip_proxy::addr::ProxyAddr;
use sip_proxy::load_observer::LoadObserverConfig;
use sip_proxy::registry::WorkerEntry;
use sip_proxy::supervise::RESTARTS;
use task_restart::RESTART_DELAY;

use super::*;

/// A worker pool whose first listing panics, as a defect in the sweep's read
/// of it would.
#[derive(Default)]
struct PanicsOnce {
    listings: AtomicU32,
}

impl WorkerRegistry for PanicsOnce {
    fn snapshot(&self) -> Vec<WorkerEntry> {
        if self.listings.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("expected: the first listing panics");
        }
        Vec::new()
    }
    fn resolve(&self, _id: &str) -> Option<WorkerEntry> {
        None
    }
    fn lookup_by_address(&self, _addr: &ProxyAddr) -> Option<WorkerEntry> {
        None
    }
    fn rearm_departed(&self, _addr: &ProxyAddr) {}
    fn synced(&self) -> bool {
        true
    }
}

#[tokio::test(start_paused = true)]
async fn a_stale_sweep_that_panics_is_restarted_and_counted() {
    let pool = Arc::new(PanicsOnce::default());
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    let sweep = spawn_stale_floor_sweep(
        observer,
        pool.clone(),
        Clock::test_at(0),
        Arc::new(ProxyMetrics::new()),
    );
    // The first sweep, a period in, panics; the restarted run sweeps a delay
    // and a period later.
    tokio::time::sleep(STALE_SWEEP_PERIOD + RESTART_DELAY + STALE_SWEEP_PERIOD * 2).await;
    assert_eq!(RESTARTS.count(STALE_FLOOR_SWEEP), 1, "the panicked run is counted");
    assert!(pool.listings.load(Ordering::SeqCst) >= 2, "the sweep runs again after the panic");
    assert!(!sweep.is_finished(), "the sweep is still running");
    sweep.abort();
}
