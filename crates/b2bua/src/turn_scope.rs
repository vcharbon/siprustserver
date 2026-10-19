//! [`TurnScope`]: what one turn on a call carries from its first gate to its
//! landing, beside the call itself: the identifiers it draws (ADR-0052 §3)
//! and what it reports (ADR-0052 §4).

use sip_ids::TurnIds;

use crate::turn_notes::TurnNotes;

/// One turn's own state. Every identifier the turn mints is a draw of
/// [`ids`](Self::ids), so the turn's identifiers are a function of the
/// process's key and the turn's sequence number; what the turn has to report
/// collects in [`notes`](Self::notes) until the shell drains it.
#[derive(Debug)]
pub struct TurnScope {
    /// The turn's identifier source.
    pub ids: TurnIds,
    /// What the turn reports, not yet drained.
    pub notes: TurnNotes,
}

impl TurnScope {
    /// The scope of the turn `ids` draws for, nothing reported yet.
    pub fn of(ids: TurnIds) -> Self {
        Self { ids, notes: TurnNotes::default() }
    }
}
