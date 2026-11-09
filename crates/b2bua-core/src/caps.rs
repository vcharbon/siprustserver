//! The per-call caps on a turn (ADR-0057 §2–§4): the lengths of the call
//! record's capped collections read as the turn starts and as it ends, the
//! actions it returns counted against its fuel, and the [`Tripped`] result
//! that replaces the turn's when either crosses its cap.
//!
//! A counted collection trips when the turn grew it past its cap: a call
//! already past one, as a union merge may leave it, trips on its next growth.
//! A service's ext value is measured once, as the turn ends, and trips past
//! its cap whatever it was before. The collection caps are off on a reaper
//! verdict's turn and on a call already going away, whose teardown would
//! otherwise trip on its own BYE record, CANCEL record, timer or CDR event,
//! and whose wait `TerminatingTimeout` bounds; the fuel is never off.

use std::io;
use std::ops::ControlFlow;

use b2bua_sdk::config::{AgeCap, CallCaps, Cap};
use b2bua_sdk::event::CallEvent;
use call::features::StatedHeaders;
use call::{Call, CallModelState, ExtMap, Leg};

use crate::effects::HandlerResult;
use crate::turn_scope::TurnScope;

/// A turn crossed `cap`: its result is discarded and the call fails from the
/// record it had before the event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tripped {
    pub cap: Cap,
    /// The turn was a reaper verdict's: its trip is the call's second strike.
    pub verdict: bool,
}

/// Which caps a turn is held to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Checked {
    /// The collections and the fuel.
    All,
    /// The fuel alone: a reaper verdict's turn, or a turn on a call already
    /// going away.
    FuelOnly { verdict: bool },
}

impl Checked {
    /// The caps a turn on `event` over `call`, as the turn finds it, is held to.
    pub fn of(call: &Call, event: &CallEvent) -> Self {
        let verdict = matches!(event, CallEvent::InternalEvent { topic, .. }
            if topic == crate::reaper_verdict::REAPER_TOPIC);
        let going_away =
            matches!(call.state, CallModelState::Terminating | CallModelState::Terminated);
        if verdict || going_away {
            Checked::FuelOnly { verdict }
        } else {
            Checked::All
        }
    }

    fn verdict(self) -> bool {
        matches!(self, Checked::FuelOnly { verdict: true })
    }
}

/// The lengths of a call's counted collections.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Gauge {
    legs: u64,
    attempts: u64,
    timers: u64,
    requests: u64,
    cdr_events: u64,
    dialogs: u64,
    header_updates: u64,
}

/// The counted collections' caps, in the order a turn's result is checked
/// against them.
const COUNTED: [Cap; 7] = [
    Cap::Legs,
    Cap::Attempts,
    Cap::Timers,
    Cap::Requests,
    Cap::CdrEvents,
    Cap::Dialogs,
    Cap::HeaderUpdates,
];

impl Gauge {
    /// The lengths `call` holds.
    pub fn of(call: &Call) -> Self {
        let legs = || std::iter::once(&call.a_leg).chain(call.b_legs.iter());
        Gauge {
            legs: len(call.b_legs.len()),
            attempts: len(call.attempts.len()),
            timers: len(call.timers.len()),
            requests: legs().map(requests_of).sum::<u64>() + len(call.awaited.len()),
            cdr_events: len(call.cdr_events.len()),
            dialogs: legs().map(|l| len(l.dialogs.len())).max().unwrap_or(0),
            header_updates: header_updates(call),
        }
    }

    /// The length the gauge reads for the collection `cap` bounds; `0` for
    /// the ext value and the fuel, which it does not count.
    pub fn get(&self, cap: Cap) -> u64 {
        match cap {
            Cap::Legs => self.legs,
            Cap::Attempts => self.attempts,
            Cap::Timers => self.timers,
            Cap::Requests => self.requests,
            Cap::CdrEvents => self.cdr_events,
            Cap::Dialogs => self.dialogs,
            Cap::HeaderUpdates => self.header_updates,
            Cap::ExtBytes | Cap::ActionsPerTurn => 0,
        }
    }
}

/// The caps on what grows with a call's age.
const AGED: [Cap; 3] = [Cap::Legs, Cap::Attempts, Cap::CdrEvents];

/// The turn that started at `start` and left `result`, held to `checked`
/// under `caps`: `Err` when its fuel ran out or it grew a collection past its
/// cap. A turn that lands has each collection it grew across half its cap
/// noted on `turn`, and what it left the call holding read against each cap
/// it is held to (ADR-0057 §9).
pub fn hold(
    caps: &CallCaps,
    start: &Gauge,
    checked: Checked,
    turn: &mut TurnScope,
    result: HandlerResult,
) -> Result<HandlerResult, Tripped> {
    let verdict = checked.verdict();
    if let Some(cap) = turn.tripped() {
        return Err(Tripped { cap, verdict });
    }
    if checked != Checked::All {
        turn.notes.cap_reading(Cap::ActionsPerTurn, turn.actions());
        return Ok(result);
    }
    let end = Gauge::of(&result.call);
    let granted = granted_ms(&result.call);
    let mut near = Vec::new();
    for cap in COUNTED {
        let (before, after, limit) = (start.get(cap), end.get(cap), caps.limit(cap, granted));
        if after <= before {
            continue;
        }
        if after > limit {
            return Err(Tripped { cap, verdict });
        }
        if before <= limit / 2 && after > limit / 2 {
            near.push(cap);
        }
    }
    let ext = ext_bytes(&result.call);
    if ext > caps.ext_bytes {
        return Err(Tripped { cap: Cap::ExtBytes, verdict });
    }
    for cap in near {
        turn.notes.cap_near(cap);
    }
    let actions = turn.actions();
    read(&mut turn.notes, &end, ext, actions, granted);
    Ok(result)
}

/// What a landed turn left its call holding, `end` and `ext` bytes, and the
/// `actions` it returned, read against each cap; an aged collection also per
/// started hour of the `granted` duration, once one is granted.
fn read(
    notes: &mut crate::turn_notes::TurnNotes,
    end: &Gauge,
    ext: u64,
    actions: u64,
    granted: i64,
) {
    notes.cap_reading(Cap::ActionsPerTurn, actions);
    for cap in COUNTED {
        notes.cap_reading(cap, end.get(cap));
    }
    notes.cap_reading(Cap::ExtBytes, ext);
    let hours = AgeCap::started_hours(granted);
    if hours > 0 {
        for cap in AGED {
            notes.cap_reading_per_hour(cap, end.get(cap).div_ceil(hours));
        }
    }
}

/// The call's granted maximum duration, in ms; `0` before a decision granted
/// one.
fn granted_ms(call: &Call) -> i64 {
    call.features.as_ref().map_or(0, |f| f.platform.max_duration_ms)
}

fn len(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// The requests `leg` holds open: the records of its dialogs, retired ones
/// included, and the requests queued behind them.
fn requests_of(leg: &Leg) -> u64 {
    leg.dialogs.iter().map(|d| len(d.requests.len()) + len(d.queued.len())).sum()
}

/// The header lines the call states, a removal counting one: every stated set
/// it holds, each leg's own and each one a set reverts to, however nested,
/// and the policy header overrides.
fn header_updates(call: &Call) -> u64 {
    let policy = call.policy_update_headers.as_ref().map_or(0, |h| len(h.len()));
    let Some(root) = call.features.as_ref().and_then(|f| f.stated_headers.as_ref()) else {
        return policy;
    };
    // A worklist over the record's own tree of sets, each visited once, then
    // the run that finds it empty; a tree past STATED_SETS_MAX reads as past
    // every cap.
    let (mut sets, mut total) = (vec![root], policy);
    let walked = bounds::bounded(STATED_SETS_MAX + 1, || {
        let Some(set) = sets.pop() else {
            return ControlFlow::Break(());
        };
        total = total.saturating_add(stated_lines(set));
        sets.extend(set.legs.values());
        sets.extend(set.reverts_to.as_deref());
        ControlFlow::Continue(())
    });
    walked.map_or(u64::MAX, |()| total)
}

/// The stated sets the header-updates gauge walks at most: far past a call
/// at its leg cap, each leg holding a set and the one it reverts to.
const STATED_SETS_MAX: usize = 1 << 16;

/// The lines one stated set states over its three scopes.
fn stated_lines(set: &StatedHeaders) -> u64 {
    [&set.launched_invite, &set.originator_finals, &set.every_message]
        .into_iter()
        .flat_map(|scope| scope.values())
        .map(|update| len(update.lines().len().max(1)))
        .sum()
}

/// The longest JSON encoding of one service's value in the call's ext or a
/// leg's.
pub fn ext_bytes(call: &Call) -> u64 {
    let legs = std::iter::once(&call.a_leg).chain(call.b_legs.iter());
    legs.map(|l| longest(l.ext.as_ref()))
        .chain(std::iter::once(longest(call.ext.as_ref())))
        .max()
        .unwrap_or(0)
}

/// The longest JSON encoding of one value in `ext`.
fn longest(ext: Option<&ExtMap>) -> u64 {
    ext.map_or(0, |ext| ext.values().map(encoded_len).max().unwrap_or(0))
}

/// The length of `value`'s JSON encoding, counted without building it.
fn encoded_len(value: &serde_json::Value) -> u64 {
    let mut counter = Counter(0);
    match serde_json::to_writer(&mut counter, value) {
        Ok(()) => counter.0,
        Err(_) => u64::MAX,
    }
}

/// A writer that keeps only the count of the bytes written to it.
struct Counter(u64);

impl io::Write for Counter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(len(buf.len()));
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "caps_tests.rs"]
mod tests;
