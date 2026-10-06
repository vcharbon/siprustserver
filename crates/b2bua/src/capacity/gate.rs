//! [`CapacityGate`]: the configured ceilings, the last sampled RSS and level,
//! and the backup sheds. A new call is judged on the gate's
//! [`reading`](CapacityGate::reading) by the admission ladder
//! ([`crate::admission`]).

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use sip_txn::InviteClass;

use crate::config::CapacityConfig;
use crate::new_calls::Refusal;

use super::probe::{ProcSelfProbe, SystemProbe};

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
    pub const fn as_str(self) -> &'static str {
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

/// What the capacity rung of the admission ladder judges a new call on: the
/// ceilings, the exactly counted quantities, and the RSS of the last sample
/// (`None` without one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapacityReading {
    pub limits: CapacityConfig,
    pub occupancy: Occupancy,
    pub rss: Option<u64>,
}

impl CapacityReading {
    /// The first bound — calls, transactions, RSS — whose ceiling for a new
    /// call of `class` ([`crate::admission::Class`]) the reading reaches: the
    /// normal ceilings for a normal call, the emergency ones for an emergency
    /// call; an in-dialog INVITE meets none. No RSS reading reaches no
    /// ceiling.
    pub fn refusal(&self, class: InviteClass) -> Option<Refusal> {
        let emergency = match class {
            InviteClass::Normal => false,
            InviteClass::Emergency => true,
            InviteClass::InDialog => return None,
        };
        let limits = &self.limits;
        [
            (Refusal::CapacityCalls, limits.calls, Some(self.occupancy.calls)),
            (Refusal::CapacityTransactions, limits.transactions, Some(self.occupancy.transactions)),
            (Refusal::CapacityRss, limits.rss_bytes, self.rss),
        ]
        .into_iter()
        .find(|(_, ceilings, value)| reached(ceilings.for_class(emergency), *value))
        .map(|(refusal, _, _)| refusal)
    }
}

/// `u64::MAX` in the RSS slot stands for "no reading yet".
const NO_READING: u64 = u64::MAX;

struct Inner {
    probe: Arc<dyn SystemProbe>,
    limits: Mutex<CapacityConfig>,
    rss: AtomicU64,
    /// The [`Level`] of the last sample.
    level: AtomicU8,
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
                level: AtomicU8::new(Level::Open as u8),
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
        let reading = self.reading(occupancy);
        let level = if reading.refusal(InviteClass::Emergency).is_some() {
            Level::ShedAll
        } else if reading.refusal(InviteClass::Normal).is_some() {
            Level::ShedNormal
        } else {
            Level::Open
        };
        self.inner.level.store(level as u8, Ordering::Relaxed);
    }

    /// What a new call is judged on now: the ceilings, `occupancy`, and the
    /// RSS of the last sample.
    pub fn reading(&self, occupancy: Occupancy) -> CapacityReading {
        CapacityReading { limits: self.limits(), occupancy, rss: self.rss_bytes() }
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
        match self.inner.level.load(Ordering::Relaxed) {
            l if l == Level::ShedAll as u8 => Level::ShedAll,
            l if l == Level::ShedNormal as u8 => Level::ShedNormal,
            _ => Level::Open,
        }
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
