//! [`CapacityGate`]: the configured ceilings, the last sampled RSS and level,
//! and the reject tallies.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use crate::config::CapacityConfig;

use super::probe::{ProcSelfProbe, SystemProbe};

/// The quantity whose ceiling refused a new call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    /// Live calls, takeover copies included.
    Calls,
    /// Live SIP transactions.
    Transactions,
    /// Process resident set size.
    Rss,
}

impl Bound {
    /// Every bound, in the order the gate checks them.
    pub const ALL: [Bound; 3] = [Bound::Calls, Bound::Transactions, Bound::Rss];

    /// Short stable tag, keyed by logs and the `bound` metric label.
    pub fn as_str(self) -> &'static str {
        match self {
            Bound::Calls => "calls",
            Bound::Transactions => "transactions",
            Bound::Rss => "rss",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// The quantity whose ceiling kept a backup replica out of the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupBound {
    /// Backup replicas held.
    Calls,
    /// Process resident set size.
    Rss,
}

impl BackupBound {
    /// Every backup bound, in the order the gate checks them.
    pub const ALL: [BackupBound; 2] = [BackupBound::Calls, BackupBound::Rss];

    /// Short stable tag, keyed by the `bound` metric label.
    pub fn as_str(self) -> &'static str {
        match self {
            BackupBound::Calls => "calls",
            BackupBound::Rss => "rss",
        }
    }
}

/// What the gate shed at its last sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Every new call is admitted.
    Open = 0,
    /// New non-emergency calls are refused.
    ShedNormal = 1,
    /// Every new call is refused.
    ShedAll = 2,
}

/// The exactly counted quantities a decision reads; RSS comes from the
/// gate's own sample.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Occupancy {
    pub calls: u64,
    pub transactions: u64,
}

/// `u64::MAX` in the RSS slot stands for "no reading yet".
const NO_READING: u64 = u64::MAX;
/// `0` in a level slot stands for "no bound reached".
const NONE_REACHED: u8 = 0;

struct Inner {
    probe: Arc<dyn SystemProbe>,
    limits: Mutex<CapacityConfig>,
    rss: AtomicU64,
    /// The bound a non-emergency call met at the last sample (index + 1).
    shed_normal: AtomicU8,
    /// The bound an emergency call met at the last sample (index + 1).
    shed_all: AtomicU8,
    /// Rejects by `[bound][class]`, class 0 = normal, 1 = emergency.
    rejected: [[AtomicU64; 2]; 3],
    backup_shed: [AtomicU64; 2],
}

/// The worker's memory admission gate (ADR-0037). Clone-cheap (one `Arc`).
///
/// Decisions read exact live-call and transaction counts passed by the caller
/// and the RSS of the last [`sample`](Self::sample). The sample also records
/// the [`Level`] the metrics publish. Every bound is off until
/// [`configure`](Self::configure) sets one.
#[derive(Clone)]
pub struct CapacityGate {
    inner: Arc<Inner>,
}

impl CapacityGate {
    /// A gate over `probe`, every bound off.
    pub fn new(probe: Arc<dyn SystemProbe>) -> Self {
        Self {
            inner: Arc::new(Inner {
                probe,
                limits: Mutex::new(CapacityConfig::default()),
                rss: AtomicU64::new(NO_READING),
                shed_normal: AtomicU8::new(NONE_REACHED),
                shed_all: AtomicU8::new(NONE_REACHED),
                rejected: Default::default(),
                backup_shed: Default::default(),
            }),
        }
    }

    /// A gate over the process's own `/proc/self/status`.
    pub fn live() -> Self {
        Self::new(Arc::new(ProcSelfProbe))
    }

    /// Install the operator's ceilings. The level is recomputed at the next
    /// [`sample`](Self::sample).
    pub fn configure(&self, limits: &CapacityConfig) {
        *self.inner.limits.lock().unwrap() = *limits;
    }

    /// The installed ceilings.
    pub fn limits(&self) -> CapacityConfig {
        *self.inner.limits.lock().unwrap()
    }

    /// Read the probe's RSS and recompute the level from `occupancy`.
    pub fn sample(&self, occupancy: Occupancy) {
        let rss = self.inner.probe.rss_bytes();
        self.inner.rss.store(rss.unwrap_or(NO_READING), Ordering::Relaxed);
        let limits = self.limits();
        let slot = |b: Option<Bound>| b.map_or(NONE_REACHED, |b| b.index() as u8 + 1);
        let normal = first_reached(&limits, false, occupancy, rss);
        let all = first_reached(&limits, true, occupancy, rss);
        self.inner.shed_normal.store(slot(normal), Ordering::Relaxed);
        self.inner.shed_all.store(slot(all), Ordering::Relaxed);
    }

    /// The RSS of the last sample; `None` before the first or when the probe
    /// has no reading.
    pub fn rss_bytes(&self) -> Option<u64> {
        match self.inner.rss.load(Ordering::Relaxed) {
            NO_READING => None,
            v => Some(v),
        }
    }

    /// The level of the last sample.
    pub fn level(&self) -> Level {
        if self.inner.shed_all.load(Ordering::Relaxed) != NONE_REACHED {
            Level::ShedAll
        } else if self.inner.shed_normal.load(Ordering::Relaxed) != NONE_REACHED {
            Level::ShedNormal
        } else {
            Level::Open
        }
    }

    /// The bound a new call of this class met at the last sample.
    pub fn refused_at_sample(&self, is_emergency: bool) -> Option<Bound> {
        let slot = if is_emergency { &self.inner.shed_all } else { &self.inner.shed_normal };
        match slot.load(Ordering::Relaxed) {
            NONE_REACHED => None,
            n => Some(Bound::ALL[usize::from(n) - 1]),
        }
    }

    /// The bound a new call of this class meets now, from exact counts and
    /// the last sampled RSS.
    pub fn refuses(&self, is_emergency: bool, occupancy: Occupancy) -> Option<Bound> {
        first_reached(&self.limits(), is_emergency, occupancy, self.rss_bytes())
    }

    /// Count one reject sent for `bound`.
    pub fn record_reject(&self, bound: Bound, is_emergency: bool) {
        self.inner.rejected[bound.index()][usize::from(is_emergency)]
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Rejects sent for `bound` to calls of this class.
    pub fn rejected_total(&self, bound: Bound, is_emergency: bool) -> u64 {
        self.inner.rejected[bound.index()][usize::from(is_emergency)].load(Ordering::Relaxed)
    }

    /// Every capacity reject sent, all bounds and classes.
    pub fn rejected_sum(&self) -> u64 {
        self.inner.rejected.iter().flatten().map(|c| c.load(Ordering::Relaxed)).sum()
    }

    /// Whether a backup replica of a call this node does not hold yet may be
    /// stored while it holds `held` of them. A refusal is counted. Pullers of
    /// different peers decide concurrently, so the count ceiling may be passed
    /// by one replica per other peer.
    pub fn refuses_backup(&self, held: u64) -> Option<BackupBound> {
        let limits = self.limits();
        let bound = if limits.backup_calls.is_some_and(|max| held >= max) {
            Some(BackupBound::Calls)
        } else if reached(limits.backup_rss_bytes, self.rss_bytes()) {
            Some(BackupBound::Rss)
        } else {
            None
        };
        if let Some(b) = bound {
            self.inner.backup_shed[b as usize].fetch_add(1, Ordering::Relaxed);
        }
        bound
    }

    /// Backup replicas not stored because of `bound`.
    pub fn backup_shed_total(&self, bound: BackupBound) -> u64 {
        self.inner.backup_shed[bound as usize].load(Ordering::Relaxed)
    }
}

/// `value` at or above `ceiling`, both present.
fn reached(ceiling: Option<u64>, value: Option<u64>) -> bool {
    matches!((ceiling, value), (Some(c), Some(v)) if v >= c)
}

/// The first bound, in [`Bound::ALL`] order, a new call of this class meets.
fn first_reached(
    limits: &CapacityConfig,
    is_emergency: bool,
    occupancy: Occupancy,
    rss: Option<u64>,
) -> Option<Bound> {
    Bound::ALL.into_iter().find(|b| match b {
        Bound::Calls => reached(limits.calls.for_class(is_emergency), Some(occupancy.calls)),
        Bound::Transactions => {
            reached(limits.transactions.for_class(is_emergency), Some(occupancy.transactions))
        }
        Bound::Rss => reached(limits.rss_bytes.for_class(is_emergency), rss),
    })
}
