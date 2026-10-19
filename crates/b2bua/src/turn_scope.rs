//! [`TurnScope`]: what one turn on a call carries from its first gate to its
//! landing, beside the call itself: the identifiers it draws (ADR-0052 §3),
//! what it reports (ADR-0052 §4) and the wire faults armed as it started
//! (ADR-0052 §5).

use sip_ids::TurnIds;

use crate::turn_notes::TurnNotes;
use crate::wire_fault_set::WireFaultSet;

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
    /// The wire faults the turn emits.
    pub faults: WireFaultSet,
}

impl TurnScope {
    /// The scope of the turn `ids` draws for, nothing reported yet, no wire
    /// fault armed.
    pub fn of(ids: TurnIds) -> Self {
        Self { ids, notes: TurnNotes::default(), faults: WireFaultSet::none() }
    }

    /// `self`, emitting the wire faults `faults`.
    pub fn with_faults(mut self, faults: WireFaultSet) -> Self {
        self.faults = faults;
        self
    }
}
