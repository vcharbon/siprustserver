//! [`TurnScope`]: what one turn on a call carries from its first gate to its
//! landing, beside the call itself: the identifiers it draws (ADR-0052 §3),
//! what it reports (ADR-0052 §4), the wire faults armed as it started
//! (ADR-0052 §5) and the fuel it spent (ADR-0057 §3).

use b2bua_sdk::config::Cap;
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
    /// The rule whose actions run now, by its id; `None` outside them.
    pub rule: Option<&'static str>,
    /// The rule that handled the turn's event, by its id; `None` while no
    /// rule has, and on a turn the framework takes before the rules.
    pub handled_by: Option<&'static str>,
    /// The actions the turn returned so far, rule and lifecycle actions
    /// together.
    actions: u64,
    /// The cap the turn crossed, once it crossed one.
    tripped: Option<Cap>,
}

impl TurnScope {
    /// The scope of the turn `ids` draws for, nothing reported yet, no wire
    /// fault armed.
    pub fn of(ids: TurnIds) -> Self {
        Self {
            ids,
            notes: TurnNotes::default(),
            faults: WireFaultSet::none(),
            rule: None,
            handled_by: None,
            actions: 0,
            tripped: None,
        }
    }

    /// Charge `n` actions to the turn's fuel of `fuel`: `false`, the turn
    /// tripped, once it holds more than that. A tripped turn charges nothing
    /// more and runs no action.
    pub fn charge(&mut self, n: usize, fuel: u64) -> bool {
        if self.tripped.is_some() {
            return false;
        }
        self.actions = self.actions.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
        if self.actions > fuel {
            self.tripped = Some(Cap::ActionsPerTurn);
            return false;
        }
        true
    }

    /// The cap the turn crossed, if it crossed one.
    pub fn tripped(&self) -> Option<Cap> {
        self.tripped
    }

    /// `self`, emitting the wire faults `faults`.
    pub fn with_faults(mut self, faults: WireFaultSet) -> Self {
        self.faults = faults;
        self
    }
}
