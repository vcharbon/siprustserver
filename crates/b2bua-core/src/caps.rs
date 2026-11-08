//! The per-call caps on a turn (ADR-0057 §2–§4): the lengths of the call
//! record's capped collections read as the turn starts and as it ends, the
//! actions it returns counted against its fuel, and the [`Tripped`] result
//! that replaces the turn's when either crosses its cap.
//!
//! A collection trips when the turn grew it past its cap: a call already past
//! one, as a union merge may leave it, trips on its next growth. The
//! collection caps are off on a reaper verdict's turn and on a call already
//! going away, whose teardown would otherwise trip on its own BYE record,
//! CANCEL record, timer or CDR event, and whose wait `TerminatingTimeout`
//! bounds; the fuel is never off.

use std::io;

use b2bua_sdk::config::{CallCaps, Cap};
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

/// The lengths of a call's capped collections.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Gauge {
    legs: u64,
    attempts: u64,
    timers: u64,
    requests: u64,
    cdr_events: u64,
    dialogs: u64,
    header_updates: u64,
    ext_bytes: u64,
}

/// The collection caps, in the order a turn's result is checked against them.
const COLLECTIONS: [Cap; 8] = [
    Cap::Legs,
    Cap::Attempts,
    Cap::Timers,
    Cap::Requests,
    Cap::CdrEvents,
    Cap::Dialogs,
    Cap::HeaderUpdates,
    Cap::ExtBytes,
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
            ext_bytes: legs()
                .map(|l| ext_bytes(l.ext.as_ref()))
                .chain(std::iter::once(ext_bytes(call.ext.as_ref())))
                .max()
                .unwrap_or(0),
        }
    }

    /// The length the gauge reads for the collection `cap` bounds; `0` for
    /// the fuel.
    pub fn get(&self, cap: Cap) -> u64 {
        match cap {
            Cap::Legs => self.legs,
            Cap::Attempts => self.attempts,
            Cap::Timers => self.timers,
            Cap::Requests => self.requests,
            Cap::CdrEvents => self.cdr_events,
            Cap::Dialogs => self.dialogs,
            Cap::HeaderUpdates => self.header_updates,
            Cap::ExtBytes => self.ext_bytes,
            Cap::ActionsPerTurn => 0,
        }
    }
}

/// The turn that started at `start` and left `result`, held to `checked`
/// under `caps`: `Err` when its fuel ran out or it grew a collection past its
/// cap. Each collection the turn grew across half its cap is noted on
/// `turn`.
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
        return Ok(result);
    }
    let end = Gauge::of(&result.call);
    let granted = granted_ms(&result.call);
    let mut tripped = None;
    for cap in COLLECTIONS {
        let (before, after, limit) = (start.get(cap), end.get(cap), caps.limit(cap, granted));
        if after <= before {
            continue;
        }
        if before <= limit / 2 && after > limit / 2 {
            turn.notes.cap_near(cap);
        }
        if after > limit && tripped.is_none() {
            tripped = Some(cap);
        }
    }
    match tripped {
        Some(cap) => Err(Tripped { cap, verdict }),
        None => Ok(result),
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

/// The header lines the call states, a removal counting one: the call's
/// stated set and the one it reverts to, each with its legs' own sets, and
/// the policy header overrides.
fn header_updates(call: &Call) -> u64 {
    let policy = call.policy_update_headers.as_ref().map_or(0, |h| len(h.len()));
    let Some(stated) = call.features.as_ref().and_then(|f| f.stated_headers.as_ref()) else {
        return policy;
    };
    let mut total = policy + stated_with_legs(stated);
    if let Some(reverts) = stated.reverts_to.as_deref() {
        total += stated_with_legs(reverts);
    }
    total
}

/// The lines `set` states, and those of each leg's own set.
fn stated_with_legs(set: &StatedHeaders) -> u64 {
    stated_lines(set) + set.legs.values().map(stated_lines).sum::<u64>()
}

/// The lines one stated set states over its three scopes.
fn stated_lines(set: &StatedHeaders) -> u64 {
    [&set.launched_invite, &set.originator_finals, &set.every_message]
        .into_iter()
        .flat_map(|scope| scope.values())
        .map(|update| len(update.lines().len().max(1)))
        .sum()
}

/// The longest JSON encoding of one service's value in `ext`.
fn ext_bytes(ext: Option<&ExtMap>) -> u64 {
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
