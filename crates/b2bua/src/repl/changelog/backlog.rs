//! The backlog toward a backup peer (ADR-0055 §3): the entries of its Backup
//! sub-log above the highest position it reported applying, and the age of the
//! oldest. Above a high-water mark on either a new call is admitted with no
//! backup; new calls get one again once both are under their low-water marks.
//! The marks read a peer whose open flow completed its bootstrap, slow to
//! answer or not: a peer no flow carries, or one still bootstrapping, is no
//! slow consumer. No call is refused for the backlog.

use std::ops::Bound;
use std::time::Duration;

use repl_net::frame::Partition;

use super::Changelog;
use crate::repl::puller::DEFAULT_BOOTSTRAP_HARD_TIMEOUT_MS;
use crate::store::{locked, REPLICA_CHANGELOG};

/// The high- and low-water marks of the backlog toward one backup peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BacklogMarks {
    /// Pending entries at or above which new calls get no backup.
    pub high_entries: usize,
    /// Pending entries under which new calls get a backup again.
    pub low_entries: usize,
    /// Age of the oldest pending entry at or above which new calls get no
    /// backup.
    pub high_age: Duration,
    /// Age of the oldest pending entry under which new calls get a backup
    /// again.
    pub low_age: Duration,
}

impl Default for BacklogMarks {
    /// Ten thousand entries or 5 s (a link the idle `Noop` interval found
    /// silent) rise over; a thousand entries and 1 s fall back under.
    fn default() -> Self {
        Self {
            high_entries: 10_000,
            low_entries: 1_000,
            high_age: Duration::from_secs(5),
            low_age: Duration::from_secs(1),
        }
    }
}

impl BacklogMarks {
    /// `Err` naming the mark out of order: each low mark is under its high one.
    pub fn validate(&self) -> Result<(), String> {
        if self.low_entries >= self.high_entries {
            return Err(format!(
                "backlog low-water entries {} not under the high-water {}",
                self.low_entries, self.high_entries
            ));
        }
        if self.low_age >= self.high_age {
            return Err(format!(
                "backlog low-water age {:?} not under the high-water {:?}",
                self.low_age, self.high_age
            ));
        }
        Ok(())
    }
}

/// The backlog toward one peer: pending entries (counted up to the high-water
/// mark) and the age of the oldest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Backlog {
    pub entries: usize,
    pub oldest_age: Duration,
}

impl Changelog {
    /// Set the backlog marks.
    pub fn set_backlog_marks(&self, marks: BacklogMarks) {
        locked(&self.inner, REPLICA_CHANGELOG).marks = marks;
    }

    /// Whether a new call admitted now gets `peer` as its backup. Toward a
    /// peer whose open flow completed its bootstrap (it claimed a position),
    /// or stalled in it past the bootstrap hard deadline (10 s, dated from the
    /// first flow open since its last claim, re-opens included), `false` from
    /// the moment the backlog reaches a high-water mark until it is under both
    /// low-water marks: the entries new calls add stop at the high-water mark.
    /// Toward a peer no flow carries, or one still bootstrapping, always
    /// `true`: its bootstrap carries the call. A bootstrap in flight holds the
    /// log for at most that deadline; with no flow open the log goes one
    /// dead-peer TTL (60 s, `B2BUA_REPL_DEAD_PEER_TTL_MS`) after the peer's
    /// last claim, writes or flows without a claim not counted.
    pub fn admits_backup(&self, peer: &str) -> bool {
        let now = self.clock.now_ms();
        let gen = self.gen;
        let mut inner = locked(&self.inner, REPLICA_CHANGELOG);
        let marks = inner.marks;
        let Some(log) = inner.peers.get_mut(peer) else { return true };
        let sub = log.sub(Partition::Bak);
        let deadline = Duration::from_millis(DEFAULT_BOOTSTRAP_HARD_TIMEOUT_MS);
        let bootstrapping =
            sub.applied.is_none() && sub.flow_started.is_some_and(|t| t.elapsed() < deadline);
        if sub.streams == 0 || bootstrapping {
            return true;
        }
        let backlog = measure(log.sub(Partition::Bak), gen, now, marks.high_entries);
        log.over_high_water = if log.over_high_water {
            backlog.entries >= marks.low_entries || backlog.oldest_age >= marks.low_age
        } else {
            backlog.entries >= marks.high_entries || backlog.oldest_age >= marks.high_age
        };
        !log.over_high_water
    }
}

/// The entries of `sub` above the position its peer reported applying under
/// incarnation `gen` (all of them under another), counted up to `cap`, and the
/// age at `now_ms` of the oldest.
fn measure(sub: &super::SubLog, gen: u64, now_ms: i64, cap: usize) -> Backlog {
    let acked = *sub.acked.borrow();
    let from = if acked.gen == gen { Bound::Excluded(acked.counter) } else { Bound::Unbounded };
    let mut pending = sub.entries.range((from, Bound::Unbounded));
    let oldest = pending.next().and_then(|(_, call_ref)| sub.by_ref.get(call_ref));
    let oldest_age = oldest.map_or(Duration::ZERO, |st| {
        Duration::from_millis(u64::try_from(now_ms.saturating_sub(st.written_ms)).unwrap_or(0))
    });
    let entries = usize::from(oldest.is_some()) + pending.take(cap.saturating_sub(1)).count();
    Backlog { entries, oldest_age }
}
