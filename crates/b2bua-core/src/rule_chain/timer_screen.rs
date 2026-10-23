//! The timer screen, before anything reads a timer fire: a fire reaches the
//! call only while the call's ledger holds its key under its generation and
//! the scope it guards has not ended, and a delivered fire takes that entry off
//! the ledger — a timer that fired is no longer armed. A fire the ledger no
//! longer holds (its key re-armed under a later generation, disarmed, or
//! retired with its scope), or one whose scope ended whatever the ledger holds
//! (ADR-0055 §10), reaches no rule; a rule never re-checks for either.

use call::Call;

use crate::turn_notes::TurnNotes;
use b2bua_sdk::event::CallEvent;

/// Why a fire reached no rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dropped {
    /// The ledger holds the key under a later generation.
    Superseded,
    /// The ledger holds no entry under the key.
    Disarmed,
    /// The scope the timer guards ended ([`call::helpers::scope_ended`]).
    ScopeEnded,
}

impl Dropped {
    /// The reason as the trace states it.
    pub const fn reason(self) -> &'static str {
        match self {
            Dropped::Superseded => "superseded",
            Dropped::Disarmed => "disarmed",
            Dropped::ScopeEnded => "scope ended",
        }
    }
}

/// Screen `event` against `call`'s ledger: `Ok` for any event but a timer
/// fire, and for a fire the ledger holds under a live scope, whose entry
/// leaves the ledger; `Err` for a stale fire, the call untouched. The fire's trace fact goes to
/// `notes`.
pub fn screen(
    call: &mut Call,
    event: &CallEvent,
    now_ms: i64,
    notes: &mut TurnNotes,
) -> Result<(), Dropped> {
    let CallEvent::Timer { key, generation, .. } = event else {
        return Ok(());
    };
    let Some(at) = call.timers.iter().position(|t| &t.key == key) else {
        trace_drop(call, now_ms, event, Dropped::Disarmed, notes);
        return Err(Dropped::Disarmed);
    };
    if call.timers.get(at).is_some_and(|t| t.generation != *generation) {
        trace_drop(call, now_ms, event, Dropped::Superseded, notes);
        return Err(Dropped::Superseded);
    }
    if call::helpers::scope_ended(call, key) {
        trace_drop(call, now_ms, event, Dropped::ScopeEnded, notes);
        return Err(Dropped::ScopeEnded);
    }
    #[expect(clippy::disallowed_methods, reason = "at is the position just found")]
    call.timers.remove(at);
    notes.trace(call, "timer.fire", now_ms, || format!("{key:?} gen {generation}"));
    Ok(())
}

fn trace_drop(
    call: &Call,
    now_ms: i64,
    event: &CallEvent,
    dropped: Dropped,
    notes: &mut TurnNotes,
) {
    let CallEvent::Timer { key, generation, .. } = event else { return };
    let reason = dropped.reason();
    notes.trace(call, "timer.drop", now_ms, || format!("{key:?} gen {generation}: {reason}"));
    tracing::debug!(
        call_ref = %call.call_ref,
        timer = ?key,
        generation,
        reason = dropped.reason(),
        "stale timer fire dropped"
    );
}

#[cfg(test)]
#[path = "timer_screen_tests.rs"]
mod tests;
