//! [`ReplicaConfig`]: what a replicating worker reads beside its
//! [`B2buaConfig`]: how long the replicas it writes for its peers live, the
//! ceilings of the replicas it holds for them, and the pace at which a worker
//! back from a restart catches up on its calls' overdue keepalives.

use crate::capacity::BackupCeilings;
use crate::config::B2buaConfig;
use sip_txn::timers::TIMER_F;

/// The replication tunables of one worker (ADR-0011, ADR-0014, ADR-0037).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplicaConfig {
    /// **Reboot budget**, seconds: the TTL stamped on every replica this node
    /// writes for its peers (ADR-0011 X11), how long a primary may be down
    /// before its backups drop its calls. A replica's TTL is refreshed only by
    /// a write of its call, and a quiet established call writes only at its
    /// keepalive, so the budget outlasts one keepalive round, and a ringing
    /// call writes none, so it outlasts the INVITE transaction bound
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

    /// What the reboot budget keeps above one keepalive round, seconds: the
    /// replication of the write the probe's answer makes.
    pub const KEEPALIVE_WRITE_MARGIN_SEC: i64 = 10;

    /// Validate the tunables against the worker's final `config`, in a
    /// homogeneous pool (every peer runs this node's ceilings). `Err` lists
    /// every violation, `; `-separated, naming the runner variables:
    ///
    /// - `B2BUA_REBOOT_BUDGET_SEC` ≥ [`MIN_REBOOT_BUDGET_SEC`](Self::MIN_REBOOT_BUDGET_SEC);
    /// - ≥ `B2BUA_KEEPALIVE_SEC` + Timer F + [`KEEPALIVE_WRITE_MARGIN_SEC`](Self::KEEPALIVE_WRITE_MARGIN_SEC):
    ///   a quiet call refreshes its replica only by the write its keepalive
    ///   probe's answer (or Timer F) makes;
    /// - \> `B2BUA_INVITE_TXN_TIMEOUT_SEC`: a ringing call outlives no replica;
    /// - `B2BUA_REPL_BACKUP_MAX_CALLS` ≥ `B2BUA_MAX_CALLS_EMERGENCY` and
    ///   `B2BUA_REPL_BACKUP_MAX_RSS` ≥ `B2BUA_MAX_RSS_EMERGENCY`, when both
    ///   are stated: every call a peer may admit can be backed here.
    pub fn validate(&self, config: &B2buaConfig) -> Result<(), String> {
        let mut out = Vec::new();
        let budget = self.reboot_budget_sec;
        if budget < Self::MIN_REBOOT_BUDGET_SEC {
            out.push(format!(
                "B2BUA_REBOOT_BUDGET_SEC={budget} < min {} s (1 min): a replicated backup must \
                 survive a primary reboot",
                Self::MIN_REBOOT_BUDGET_SEC
            ));
        }
        let timer_f_sec = (TIMER_F / 1000).cast_signed();
        let round = config
            .keepalive_interval_sec
            .saturating_add(timer_f_sec)
            .saturating_add(Self::KEEPALIVE_WRITE_MARGIN_SEC);
        if budget < round {
            out.push(format!(
                "B2BUA_REBOOT_BUDGET_SEC={budget} < B2BUA_KEEPALIVE_SEC={} + Timer F ({timer_f_sec} \
                 s) + {} s: a quiet call refreshes its replica only by the write its keepalive \
                 probe's outcome makes, so the budget must outlast one keepalive round",
                config.keepalive_interval_sec,
                Self::KEEPALIVE_WRITE_MARGIN_SEC
            ));
        }
        if budget <= config.invite_txn_timeout_sec {
            out.push(format!(
                "B2BUA_REBOOT_BUDGET_SEC={budget} <= B2BUA_INVITE_TXN_TIMEOUT_SEC={}: the replica \
                 of a call ringing up to the transaction bound would expire under it",
                config.invite_txn_timeout_sec
            ));
        }
        let cap = config.capacity;
        if let (Some(backups), Some(calls)) = (self.backup.calls, cap.calls.emergency) {
            if backups < calls {
                out.push(format!(
                    "B2BUA_REPL_BACKUP_MAX_CALLS={backups} < B2BUA_MAX_CALLS_EMERGENCY={calls}: \
                     a peer admits calls this node cannot back"
                ));
            }
        }
        if let (Some(backup_rss), Some(rss)) = (self.backup.rss_bytes, cap.rss_bytes.emergency) {
            if backup_rss < rss {
                out.push(format!(
                    "B2BUA_REPL_BACKUP_MAX_RSS={backup_rss} < B2BUA_MAX_RSS_EMERGENCY={rss} \
                     bytes: backups are shed while this node still admits calls"
                ));
            }
        }
        if let Err(e) = self.backup.validate() {
            out.push(e);
        }
        if out.is_empty() {
            Ok(())
        } else {
            Err(out.join("; "))
        }
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
        let gap = ReplicaConfig { reboot_budget_sec: 340, ..Default::default() };
        assert!(gap.validate(&config).is_err_and(|e| e.contains("B2BUA_KEEPALIVE_SEC=300")));
        let round = ReplicaConfig { reboot_budget_sec: 342, ..Default::default() };
        assert_eq!(round.validate(&config), Ok(()));
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
