//! The incidents that befall a call's two owners (ADR-0055 §11): what the
//! failover sweep cuts each turn point by, and what a hand-written failover
//! test strikes ([`crate::FailoverHarness::strike`]).

/// What befalls the call's owners at the armed turn point.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Incident {
    /// The primary dies; the proxy and the backup see it gone.
    PrimaryKilled,
    /// The primary dies and nobody is told (ADR-0031 case 3): the proxy's
    /// probe reads it dead 2 s later, its endpoint reads not ready but stays
    /// in the slice 40 s later, and it leaves the slice only when the evicted
    /// pod is force-deleted, 5 min later. Until then only a message reaching
    /// the backup takes the call over.
    PrimaryKilledUnannounced,
    /// The primary's pod is deleted (ADR-0031 case 2): it dies, the proxy and
    /// the backup see it gone, and it restarts one second later, empty, at a
    /// new address. Once ready it is published again, the proxy reads it
    /// alive, and it reclaims its calls from the backup.
    PrimaryRebooted,
    /// The backup dies; the proxy and the primary see it gone.
    BackupKilled,
    /// The replication between the two owners is cut and the proxy reads the
    /// primary dead while it runs on; nothing heals.
    Split,
    /// A [`Split`](Self::Split) healed one second later.
    SplitHealed,
    /// The replication from the primary to the backup stalls for longer than
    /// the hold's cap, then the primary dies (ADR-0055 §3).
    StalledKilled,
    /// The call's backup refused its replica at its memory ceiling, so the
    /// call has no backup; the primary dies (ADR-0055 §3).
    UnbackedKilled,
}

impl Incident {
    /// Every incident, in lane order.
    pub const ALL: [Incident; 8] = [
        Incident::PrimaryKilled,
        Incident::PrimaryKilledUnannounced,
        Incident::PrimaryRebooted,
        Incident::BackupKilled,
        Incident::Split,
        Incident::SplitHealed,
        Incident::StalledKilled,
        Incident::UnbackedKilled,
    ];

    /// The incident's name in a cell's name.
    pub fn label(self) -> &'static str {
        match self {
            Incident::PrimaryKilled => "primary-killed",
            Incident::PrimaryKilledUnannounced => "primary-killed-unannounced",
            Incident::PrimaryRebooted => "primary-rebooted",
            Incident::BackupKilled => "backup-killed",
            Incident::Split => "split",
            Incident::SplitHealed => "split-healed",
            Incident::StalledKilled => "stalled-killed",
            Incident::UnbackedKilled => "unbacked-killed",
        }
    }

    /// Whether the incident may lose the call as a non-HA B2BUA loses it
    /// (ADR-0055 §11): every dialog, the caller's included, may be left to its
    /// endpoint's own timers, and every request that reaches a living worker
    /// still draws its final there. A stalled call leaves its primary's
    /// CDR, the one of the node that ends it after, or both, never none; an
    /// unbacked one at most one. Nothing is left on any node or in the limiter
    /// either way.
    pub fn loses_the_call(self) -> bool {
        matches!(self, Incident::StalledKilled | Incident::UnbackedKilled)
    }

    /// Whether the incident kills a node of a backed call outright: the only
    /// incidents whose cells may stand in the Guaranteed tier.
    pub fn kills(self) -> bool {
        self.kills_the_primary() || self == Incident::BackupKilled
    }

    /// Whether the incident kills the primary of a backed call outright: the
    /// incidents that open the rows of a primary's kill.
    pub fn kills_the_primary(self) -> bool {
        matches!(
            self,
            Incident::PrimaryKilled
                | Incident::PrimaryKilledUnannounced
                | Incident::PrimaryRebooted
        )
    }

    /// The CDRs the incident leaves a call before any window widens the count
    /// (ADR-0055 §5): one, or one or two when the primary dies, the dead
    /// primary's and the survivor's.
    pub fn cdrs(self) -> std::ops::RangeInclusive<usize> {
        if self.kills_the_primary() {
            1..=2
        } else {
            1..=1
        }
    }

    /// Whether the incident splits the call's two owners, healed or not: its
    /// cells judge leaks, cleanup and the CDR count, never the parties'
    /// verdict (ADR-0055 §11).
    pub fn splits(self) -> bool {
        matches!(self, Incident::Split | Incident::SplitHealed)
    }

    /// Whether the incident is cut at a turn's applied sub-point: a call with
    /// no backup has none, and a stall at it is a kill after the write.
    pub(crate) fn cuts_applied(self) -> bool {
        !self.loses_the_call()
    }
}
