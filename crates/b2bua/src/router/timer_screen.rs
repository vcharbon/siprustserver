//! The timer screen, before anything reads a timer fire: a fire reaches the
//! call only while the call's ledger holds its key under its generation, and
//! a delivered fire takes that entry off the ledger — a timer that fired is no
//! longer armed. A fire the ledger no longer holds (its key re-armed under a
//! later generation, disarmed, or retired with its scope) is stale and reaches
//! no rule.

use call::Call;

use b2bua_sdk::event::CallEvent;

/// Why a fire reached no rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dropped {
    /// The ledger holds the key under a later generation.
    Superseded,
    /// The ledger holds no entry under the key.
    Disarmed,
}

impl Dropped {
    /// The reason as the trace states it.
    pub(crate) const fn reason(self) -> &'static str {
        match self {
            Dropped::Superseded => "superseded",
            Dropped::Disarmed => "disarmed",
        }
    }
}

/// Screen `event` against `call`'s ledger: `Ok` for any event but a timer
/// fire, and for a fire the ledger holds, whose entry leaves the ledger;
/// `Err` for a stale fire, the call untouched.
pub(crate) fn screen(call: &mut Call, event: &CallEvent, now_ms: i64) -> Result<(), Dropped> {
    let CallEvent::Timer { key, generation, .. } = event else {
        return Ok(());
    };
    let Some(at) = call.timers.iter().position(|t| &t.key == key) else {
        trace_drop(call, now_ms, event, Dropped::Disarmed);
        return Err(Dropped::Disarmed);
    };
    if call.timers[at].generation != *generation {
        trace_drop(call, now_ms, event, Dropped::Superseded);
        return Err(Dropped::Superseded);
    }
    call.timers.remove(at);
    if crate::trace::sampled(call) {
        crate::trace::emit::timer_fire(call, now_ms, key, *generation);
    }
    Ok(())
}

fn trace_drop(call: &Call, now_ms: i64, event: &CallEvent, dropped: Dropped) {
    let CallEvent::Timer { key, generation, .. } = event else { return };
    if crate::trace::sampled(call) {
        crate::trace::emit::timer_drop(call, now_ms, key, *generation, dropped.reason());
    }
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
