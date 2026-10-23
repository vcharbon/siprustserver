//! [`ReplicaConfig`]: what a replicating worker reads beside its
//! [`B2buaConfig`]: how long the replicas it writes for its peers live, the
//! ceilings of the replicas it holds for them, and the pace at which a worker
//! back from a restart catches up on its calls' overdue keepalives.

use crate::capacity::BackupCeilings;
use crate::config::B2buaConfig;

/// The replication tunables of one worker (ADR-0011, ADR-0014, ADR-0037).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplicaConfig {
    /// **Reboot budget**, seconds: the TTL stamped on every replica this node
    /// writes for its peers (ADR-0011 X11), how long a primary may be down
    /// before its backups drop its calls. A replica's TTL is refreshed only by
    /// a write of its call, and a quiet established call writes only at its
    /// keepalive, so the budget outlasts one keepalive interval
    /// ([`validate`](Self::validate)). Default 600.
    pub reboot_budget_sec: i64,
    /// The ceilings of the backup replicas this node holds for its peers
    /// (ADR-0037). Off by default.
    pub backup: BackupCeilings,
    /// **Keepalive catch-up speed-up**, load management only: a worker back
    /// from a restart takes its calls back with many keepalives overdue, and
    /// fires them over `L_max / speedup` (bounded to `speedup`× the normal
    /// cadence), the most overdue first, where `L_max` is the largest overdue
    /// gap. `<= 1` fires every overdue keepalive at once. Default 10.
    pub keepalive_catchup_speedup: i64,
    /// A cap, seconds, on that catch-up window; `None` (the default) drains
    /// over the whole `L_max / speedup`.
    pub max_catchup_window_sec: Option<i64>,
}

impl Default for ReplicaConfig {
    fn default() -> Self {
        Self {
            reboot_budget_sec: 600,
            backup: BackupCeilings::default(),
            keepalive_catchup_speedup: 10,
            max_catchup_window_sec: None,
        }
    }
}

impl ReplicaConfig {
    /// The least reboot budget, seconds: a backup outlives a primary's
    /// restart.
    pub const MIN_REBOOT_BUDGET_SEC: i64 = 60;

    /// Validate the tunables at boot against the worker's `config`; the first
    /// violation as a message.
    pub fn validate(&self, config: &B2buaConfig) -> Result<(), String> {
        if self.reboot_budget_sec < Self::MIN_REBOOT_BUDGET_SEC {
            return Err(format!(
                "reboot_budget_sec={} < min {} s (1 min): a replicated backup must survive a \
                 primary reboot",
                self.reboot_budget_sec,
                Self::MIN_REBOOT_BUDGET_SEC
            ));
        }
        if self.reboot_budget_sec < config.keepalive_interval_sec {
            return Err(format!(
                "reboot_budget_sec={} < keepalive_interval_sec={}: a quiet call refreshes its \
                 replica only at its keepalive, so the budget must outlast one keepalive gap",
                self.reboot_budget_sec, config.keepalive_interval_sec
            ));
        }
        self.backup.validate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_validate_against_the_default_worker() {
        assert_eq!(ReplicaConfig::default().validate(&B2buaConfig::default()), Ok(()));
    }

    #[test]
    fn a_budget_under_its_floor_or_one_keepalive_gap_is_refused() {
        let config = B2buaConfig { keepalive_interval_sec: 300, ..Default::default() };
        let short = ReplicaConfig { reboot_budget_sec: 30, ..Default::default() };
        assert!(short.validate(&config).is_err_and(|e| e.contains("min 60")));
        let gap = ReplicaConfig { reboot_budget_sec: 200, ..Default::default() };
        assert!(gap.validate(&config).is_err_and(|e| e.contains("keepalive_interval_sec=300")));
    }

    #[test]
    fn a_zero_backup_ceiling_is_refused() {
        let zero = ReplicaConfig {
            backup: BackupCeilings { calls: Some(0), rss_bytes: None },
            ..Default::default()
        };
        assert!(zero.validate(&B2buaConfig::default()).is_err());
    }
}
