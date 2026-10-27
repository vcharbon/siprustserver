//! The write path's settings (ADR-0055 §3): the replication sync level, how
//! long an outbox entry waits for its backup at most, and the backlog marks
//! past which new calls get no backup. The cap and the marks derive from the
//! SIP timers.

use std::time::Duration;

use sip_txn::timers::{T1, TRYING_BOUND};

use super::BacklogMarks;

/// When a turn's effects leave the node, relative to the write that records
/// them (ADR-0055 §3). Every effect leaves through the call's ordered outbox
/// at either level; the level decides only when an entry is released.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SyncLevel {
    /// The write is logged, then the effects leave at once.
    Off,
    /// Each entry leaves once the backup applied the write that records it,
    /// or at the cap.
    #[default]
    Applied,
}

impl std::str::FromStr for SyncLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "off" => Ok(SyncLevel::Off),
            "applied" => Ok(SyncLevel::Applied),
            other => Err(format!("replication sync level {other:?} is not off or applied")),
        }
    }
}

/// The write path's settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WritePath {
    /// When a turn's effects leave.
    pub sync: SyncLevel,
    /// The longest an outbox entry waits for its backup to apply the call's
    /// write, counted from its own enqueue. Under T1, so a held answer leaves
    /// before its peer's first retransmission, and under the §17.2.1 bound,
    /// so a released 100 leaves before the transaction layer's own. Checked
    /// at both levels, so a level change never makes a refused setting valid.
    pub sync_cap: Duration,
    /// When the backlog toward a backup gives new calls no backup.
    pub backlog: BacklogMarks,
}

impl Default for WritePath {
    /// `applied`, a cap of half the §17.2.1 bound (100 ms), and the default
    /// marks.
    fn default() -> Self {
        Self {
            sync: SyncLevel::default(),
            sync_cap: Duration::from_millis(TRYING_BOUND / 2),
            backlog: BacklogMarks::default(),
        }
    }
}

impl WritePath {
    /// `Err` naming the setting out of range: a cap not above zero, not below
    /// T1 or not below the §17.2.1 bound, or backlog marks out of order.
    pub fn validate(&self) -> Result<(), String> {
        let cap = self.sync_cap;
        let below = Duration::from_millis(T1.min(TRYING_BOUND));
        if cap.is_zero() || cap >= below {
            return Err(format!(
                "replication sync cap {cap:?} is not above zero and below T1 and the \
                 RFC 3261 §17.2.1 bound ({below:?})"
            ));
        }
        self.backlog.validate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_applied_with_a_cap_below_t1_and_the_trying_bound() {
        assert_eq!(WritePath::default().validate(), Ok(()));
        assert_eq!(WritePath::default().sync, SyncLevel::Applied);
        assert_eq!(WritePath::default().sync_cap, Duration::from_millis(100));
    }

    #[test]
    fn a_level_is_off_or_applied() {
        assert_eq!("off".parse(), Ok(SyncLevel::Off));
        assert_eq!("applied".parse(), Ok(SyncLevel::Applied));
        assert!("sent".parse::<SyncLevel>().is_err());
    }

    #[test]
    fn a_cap_not_below_the_trying_bound_is_refused_at_either_level() {
        for sync in [SyncLevel::Off, SyncLevel::Applied] {
            for ms in [0, TRYING_BOUND, T1, T1 + 1] {
                let cap = Duration::from_millis(ms);
                let path = WritePath { sync, sync_cap: cap, ..WritePath::default() };
                assert!(path.validate().is_err(), "a cap of {ms} ms is refused at {sync:?}");
            }
        }
        let cap = Duration::from_millis(TRYING_BOUND - 1);
        let path = WritePath { sync_cap: cap, ..WritePath::default() };
        assert_eq!(path.validate(), Ok(()));
    }

    #[test]
    fn backlog_marks_out_of_order_are_refused() {
        let backlog = BacklogMarks { low_entries: 10, high_entries: 10, ..BacklogMarks::default() };
        assert!(WritePath { backlog, ..WritePath::default() }.validate().is_err());
        let backlog = BacklogMarks {
            low_age: Duration::from_secs(5),
            high_age: Duration::from_secs(5),
            ..BacklogMarks::default()
        };
        assert!(WritePath { backlog, ..WritePath::default() }.validate().is_err());
    }
}
