//! The write path's settings (ADR-0055 §3): how long a turn holds the ACK and
//! the first provisional of an INVITE for its backup, and the backlog marks
//! past which new calls get no backup. Both derive from the SIP timers.

use std::time::Duration;

use sip_txn::timers::{T1, TRYING_BOUND};

use super::BacklogMarks;

/// The write path's settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WritePath {
    /// The longest a turn holds the ACK of a 2xx or the first provisional of an
    /// INVITE for its backup to apply the call's write. Under T1, so a held
    /// message leaves before its peer's first retransmission, and under the
    /// §17.2.1 bound, so a released 100 leaves before the transaction layer's
    /// own.
    pub hold_cap: Duration,
    /// When the backlog toward a backup gives new calls no backup.
    pub backlog: BacklogMarks,
}

impl Default for WritePath {
    /// A cap of half the §17.2.1 bound (100 ms), and the default marks.
    fn default() -> Self {
        Self { hold_cap: Duration::from_millis(TRYING_BOUND / 2), backlog: BacklogMarks::default() }
    }
}

impl WritePath {
    /// `Err` naming the setting out of range: a cap not above zero, not below
    /// T1 or not below the §17.2.1 bound, or backlog marks out of order.
    pub fn validate(&self) -> Result<(), String> {
        let cap = self.hold_cap;
        let below = Duration::from_millis(T1.min(TRYING_BOUND));
        if cap.is_zero() || cap >= below {
            return Err(format!(
                "replication hold cap {cap:?} is not above zero and below T1 and the \
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
    fn the_default_cap_is_below_t1_and_the_trying_bound() {
        assert_eq!(WritePath::default().validate(), Ok(()));
        assert_eq!(WritePath::default().hold_cap, Duration::from_millis(100));
    }

    #[test]
    fn a_cap_not_below_the_trying_bound_is_refused() {
        for ms in [0, TRYING_BOUND, T1, T1 + 1] {
            let path = WritePath { hold_cap: Duration::from_millis(ms), ..WritePath::default() };
            assert!(path.validate().is_err(), "a cap of {ms} ms is refused");
        }
        let path =
            WritePath { hold_cap: Duration::from_millis(TRYING_BOUND - 1), ..WritePath::default() };
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
