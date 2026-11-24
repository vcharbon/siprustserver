//! The incident sets the rows of the window table open on.

use crate::incident::Incident;
use crate::incident::Incident::{
    BackupKilled, PrimaryKilled, PrimaryKilledUnannounced, PrimaryRebooted, Split, SplitHealed,
    StalledKilled, UnbackedKilled,
};

/// The rows of a split, healed or not, and of a stall past the cap then a
/// kill: two acting copies.
pub(super) const TWO_COPIES: &[Incident] = &[Split, SplitHealed, StalledKilled];
/// The rows of a split, healed or not.
pub(super) const SPLITS: &[Incident] = &[Split, SplitHealed];
/// The rows of a copy behind the wire or of two acting copies.
pub(super) const STALE_OR_TWO_COPIES: &[Incident] =
    &[PrimaryKilled, PrimaryKilledUnannounced, PrimaryRebooted, StalledKilled, Split, SplitHealed];
/// The rows of a kill of the primary, announced or not, rebooted or not.
pub(super) const PRIMARY_KILLS: &[Incident] =
    &[PrimaryKilled, PrimaryKilledUnannounced, PrimaryRebooted];
/// The rows of a copy whose record may be behind the wire, the requests it
/// awaited answers to sent again: a kill of the primary under `off` or past
/// the cap, and a stall past the cap then a kill.
pub(super) const STALE_COPY: &[Incident] =
    &[PrimaryKilled, PrimaryKilledUnannounced, PrimaryRebooted, StalledKilled];
/// The rows of every incident but a split.
pub(super) const NO_SPLIT: &[Incident] = &[
    PrimaryKilled,
    PrimaryKilledUnannounced,
    PrimaryRebooted,
    BackupKilled,
    StalledKilled,
    UnbackedKilled,
];
