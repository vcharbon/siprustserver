//! The replication gauges a node publishes: its last bootstrap and reclaim
//! passes, the replicating store's sizes, the outbox depth, and its standing
//! toward the membership.

use std::sync::atomic::Ordering;

use super::B2buaMetrics;

impl B2buaMetrics {
    /// Record how many bodies the most recent bootstrap pass imported (gauge).
    pub fn set_repl_bootstrap_last_applied(&self, n: u64) {
        self.inner.repl_bootstrap_last_applied.store(n, Ordering::Relaxed);
    }
    pub fn repl_bootstrap_last_applied(&self) -> u64 {
        self.inner.repl_bootstrap_last_applied.load(Ordering::Relaxed)
    }

    /// Record the most recent bulk-reclaim pass's `(scanned, materialized)` — the
    /// reboot-reclaim completeness denominator/numerator (gauges). `scanned` is the
    /// `pri:{self}` partition size the pass swept; `materialized` is how many it
    /// freshly re-served. Overwritten each pass; the cumulative materialised total
    /// is `repl_reclaimed_total`.
    pub fn set_repl_reclaim_pass(&self, scanned: u64, materialized: u64) {
        self.inner.repl_reclaim_scanned.store(scanned, Ordering::Relaxed);
        self.inner.repl_reclaim_materialized.store(materialized, Ordering::Relaxed);
    }
    pub fn repl_reclaim_scanned(&self) -> u64 {
        self.inner.repl_reclaim_scanned.load(Ordering::Relaxed)
    }
    pub fn repl_reclaim_materialized(&self) -> u64 {
        self.inner.repl_reclaim_materialized.load(Ordering::Relaxed)
    }

    /// Push the replicating-store sizes (memory-attribution gauges): total +
    /// backup-partition replica metadata entries, and the outbound changelog
    /// depth (entries across peers + peer count). See the field docs.
    pub fn set_repl_store_gauges(
        &self,
        meta_total: u64,
        meta_backup: u64,
        calls_unbacked: u64,
        changelog_entries: u64,
        changelog_peers: u64,
    ) {
        self.inner.repl_meta_total.store(meta_total, Ordering::Relaxed);
        self.inner.repl_meta_backup.store(meta_backup, Ordering::Relaxed);
        self.inner.repl_calls_unbacked.store(calls_unbacked, Ordering::Relaxed);
        self.inner.repl_changelog_entries.store(changelog_entries, Ordering::Relaxed);
        self.inner.repl_changelog_peers.store(changelog_peers, Ordering::Relaxed);
    }

    /// Publish the outbox entries queued across every call (ADR-0055 §3).
    pub fn set_repl_outbox_depth(&self, depth: u64) {
        self.inner.repl_outbox_depth.store(depth, Ordering::Relaxed);
    }

    /// Set whether this worker is withdrawn from routing while still running
    /// (ADR-0031 D6). Written by the replication supervisor on the observation.
    pub fn set_withdrawn_running(&self, withdrawn: bool) {
        self.inner.withdrawn_running.store(u64::from(withdrawn), Ordering::Relaxed);
    }
    pub fn withdrawn_running(&self) -> bool {
        self.inner.withdrawn_running.load(Ordering::Relaxed) == 1
    }

    /// Set the number of replication peers currently pulled while not `ready`
    /// (present in membership only, ADR-0031 D1). Written by the supervisor at
    /// every reconcile.
    pub fn set_repl_peers_pulled_not_ready(&self, n: u64) {
        self.inner.repl_peers_pulled_not_ready.store(n, Ordering::Relaxed);
    }
    pub fn repl_peers_pulled_not_ready(&self) -> u64 {
        self.inner.repl_peers_pulled_not_ready.load(Ordering::Relaxed)
    }
}
