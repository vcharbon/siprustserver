//! The capacity gate's families: its level, its last RSS sample, its
//! ceilings, and the backup replicas it kept out.

use metric_catalogue::{assert_exposition_order, label_values, Dim, Family, Labels};

use crate::capacity::BackupBound;

/// A live-call ceiling's bound.
pub const CEILING_BOUND: Dim = Dim::new("bound", &["calls", "transactions", "rss"]);

/// The class a live-call ceiling applies to.
pub const CEILING_CLASS: Dim = Dim::new("class", &["normal", "emergency"]);

const BACKUP_BOUND_VALUES: [&str; 2] = label_values!(BackupBound::ALL, BackupBound::as_str);
/// A backup ceiling's bound, indexed like [`BackupBound::ALL`].
pub const BACKUP_BOUND: Dim = Dim::new("bound", &BACKUP_BOUND_VALUES);

assert_exposition_order!(BackupBound: Calls, Rss);

pub const CAPACITY_LEVEL: Family = Family::gauge(
    "b2bua_capacity_level",
    Labels::None,
    "0 open, 1 non-emergency calls refused, 2 every new call refused.",
);

pub const CAPACITY_RSS_BYTES: Family = Family::gauge(
    "b2bua_capacity_rss_bytes",
    Labels::None,
    "Process RSS the capacity gate last sampled; NaN before the first sample.",
);

pub const CAPACITY_CEILING: Family = Family::gauge(
    "b2bua_capacity_ceiling",
    Labels::Union(&[
        &[CEILING_BOUND, CEILING_CLASS],
        &[BACKUP_BOUND, Dim::new("class", &["backup"])],
    ]),
    "Configured capacity ceilings; +Inf where none is configured.",
);

pub const REPL_BACKUP_SHED: Family = Family::counter(
    "b2bua_repl_backup_shed_total",
    Labels::Product(&[BACKUP_BOUND]),
    "Backup replicas not stored because a backup ceiling was reached.",
);
