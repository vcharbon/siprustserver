//! One call's message tally: every datagram its endpoints send or have
//! delivered, keyed by arrow `(from, to, label, nth)`, with the duplicates
//! received and the re-sends of each. A datagram whose identity (direction,
//! endpoint, Call-ID, CSeq, the request method, or the status with the RSeq
//! and the To-tag) was already seen on the call is a copy of that arrow and
//! opens no new occurrence. The state is per call, bounded by
//! [`MAX_ARROWS`] (past it a datagram counts on an "other" arrow), folded into
//! the reporter once when the call ends, and released then. The arrows of a
//! call share each lane name and label text they repeat.
//!
//! A request the mux re-answers after the call ended (its agent's held answer,
//! RFC 3261 §17.2.2) is reported through the tally's [`LateSink`] on the
//! arrow of the request, resolved when the agent answered it.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use sip_message::sniff::Identity;
use tokio::time::Instant;

use super::taps::Dir;

/// The SUT's lane: the far end of every arrow.
pub const SUT_LANE: &str = "SUT";

/// The distinct arrows one call keeps at most; a datagram opening an arrow
/// past it counts on the call's "other" arrow.
pub(crate) const MAX_ARROWS: usize = 256;

/// The arrows a call reserves room for at its first one: a two-leg call's
/// setup and teardown.
const USUAL_ARROWS: usize = 16;

/// One arrow of a ladder: who sent what to whom, the how-many-th distinct
/// occurrence of that `(from, to, label)` it is in its call (from 1).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArrowKey {
    pub from: Arc<str>,
    pub to: Arc<str>,
    pub label: Arc<str>,
    pub nth: u32,
}

/// One arrow's counters in one call (or folded over calls).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ArrowStat {
    /// Distinct occurrences.
    pub count: u64,
    /// Copies received after the first.
    pub duplicates: u64,
    /// Copies sent after the first: the retransmit engine's re-sends and the
    /// call's own.
    pub resent: u64,
    /// Sum over the occurrences of their first copy's offset from call
    /// start, in milliseconds.
    pub offset_ms_sum: f64,
}

/// What one call leaves for its scenario's ladder.
#[derive(Debug, Clone, Default)]
pub struct CallMessages {
    /// The caller's lane.
    pub caller: Arc<str>,
    /// Every arrow of the call, in first-seen order.
    pub arrows: Vec<(ArrowKey, ArrowStat)>,
    /// The datagrams past the call's arrow cap.
    pub other: ArrowStat,
    /// Per lane, the last arrow it received before the call's outcome was
    /// decided (`None`: an arrow past the bound, counted on "other").
    pub charges: Vec<(Arc<str>, Option<ArrowKey>)>,
}

impl CallMessages {
    /// Where a failure of the agent on `lane` is charged: the last arrow the
    /// lane received before the outcome. `None`: the lane had received
    /// nothing; `Some(None)`: the "other" arrow.
    pub fn charge_of(&self, lane: &str) -> Option<Option<&ArrowKey>> {
        self.charges.iter().find(|(l, _)| **l == *lane).map(|(_, arrow)| arrow.as_ref())
    }
}

/// An arrow's slot in the call: one of its entries, or "other".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    Entry(usize),
    Other,
}

struct Entry {
    key: u64,
    arrow: ArrowKey,
    stat: ArrowStat,
}

#[derive(Default)]
struct TallyState {
    entries: Vec<Entry>,
    other: ArrowStat,
    /// Per receiving lane, the last new arrow it received.
    last_inbound: Vec<(Arc<str>, Slot)>,
    /// Set once, when the outcome is decided.
    charges: Option<Vec<(Arc<str>, Slot)>>,
    /// The tally was handed over: later datagrams are not tallied.
    finished: bool,
}

/// Where a call's late re-answers go: the arrow of each request the mux
/// re-answered after the call ended.
pub type LateSink = Arc<dyn Fn(&ArrowKey) + Send + Sync>;

/// The report of one held answer's re-sends: its request's arrow, and where
/// it goes.
#[derive(Clone)]
pub(in crate::mux) struct LateRepair {
    sink: LateSink,
    arrow: ArrowKey,
}

impl LateRepair {
    /// One retransmission of the request re-answered after the call.
    pub(in crate::mux) fn record(&self) {
        (self.sink)(&self.arrow)
    }
}

/// One call's tally, shared by its endpoints.
pub struct CallTally {
    start: Instant,
    caller: Arc<str>,
    state: Mutex<TallyState>,
    late: Option<LateSink>,
}

impl CallTally {
    /// A tally starting now; `caller` names the caller's lane.
    pub fn new(caller: &str) -> Self {
        Self { start: Instant::now(), caller: caller.into(), state: Mutex::default(), late: None }
    }

    /// Report the call's late re-answers into `sink`.
    pub fn with_late_sink(mut self, sink: LateSink) -> Self {
        self.late = Some(sink);
        self
    }

    /// The report of a late re-send of `answer`, the final the endpoint `leg`
    /// sent: the arrow of the request it answers (as that endpoint received
    /// it). `None` without a sink, or when the request has no arrow of its own.
    pub(in crate::mux) fn late_repair(
        &self,
        leg: Option<&str>,
        answer: &Identity<'_>,
    ) -> Option<LateRepair> {
        let sink = self.late.clone()?;
        let request = Identity {
            call_id: answer.call_id,
            cseq: answer.cseq,
            cseq_method: answer.cseq_method,
            method: answer.cseq_method,
            ..Identity::default()
        };
        let key = dup_key(leg, Dir::In, &request);
        let g = self.state.lock().unwrap();
        let arrow = g.entries.iter().find(|e| e.key == key)?.arrow.clone();
        Some(LateRepair { sink, arrow })
    }

    /// A datagram the endpoint `leg` (`None`: the caller) sent (`Out`, its
    /// first transmission by the call) or had delivered (`In`) at `now`. A
    /// copy counts on its arrow and moves nothing else.
    pub(super) fn on_datagram(&self, leg: Option<&str>, dir: Dir, id: &Identity<'_>, now: Instant) {
        let key = dup_key(leg, dir, id);
        let mut g = self.state.lock().unwrap();
        if g.finished {
            return;
        }
        if let Some(e) = g.entries.iter_mut().find(|e| e.key == key) {
            match dir {
                Dir::In => e.stat.duplicates += 1,
                Dir::Out => e.stat.resent += 1,
            }
            return;
        }
        let lane = match leg {
            Some(leg) => shared(&g.entries, leg),
            None => self.caller.clone(),
        };
        let slot = if g.entries.len() < MAX_ARROWS {
            let sut = shared(&g.entries, SUT_LANE);
            let (from, to) = match dir {
                Dir::Out => (lane.clone(), sut),
                Dir::In => (sut, lane.clone()),
            };
            let label = LABEL.with_borrow_mut(|text| {
                write_label(id, text);
                shared(&g.entries, text)
            });
            let nth = 1 + g
                .entries
                .iter()
                .filter(|e| e.arrow.from == from && e.arrow.to == to && e.arrow.label == label)
                .count() as u32;
            let arrow = ArrowKey { from, to, label, nth };
            let offset_ms = now.saturating_duration_since(self.start).as_secs_f64() * 1000.0;
            let stat = ArrowStat { count: 1, offset_ms_sum: offset_ms, ..ArrowStat::default() };
            if g.entries.capacity() == 0 {
                g.entries.reserve_exact(USUAL_ARROWS);
            }
            g.entries.push(Entry { key, arrow, stat });
            Slot::Entry(g.entries.len() - 1)
        } else {
            g.other.count += 1;
            Slot::Other
        };
        if dir == Dir::In {
            match g.last_inbound.iter_mut().find(|(l, _)| *l == lane) {
                Some((_, last)) => *last = slot,
                None => g.last_inbound.push((lane, slot)),
            }
        }
    }

    /// A re-send the retransmit engine of endpoint `leg` made of a datagram
    /// it sent before; a datagram the tally never saw opens nothing.
    pub(super) fn on_resend(&self, leg: Option<&str>, id: &Identity<'_>) {
        let key = dup_key(leg, Dir::Out, id);
        let mut g = self.state.lock().unwrap();
        if let Some(e) = g.entries.iter_mut().find(|e| e.key == key) {
            e.stat.resent += 1;
        }
    }

    /// The call's outcome is decided: each lane's failure is charged to the
    /// last arrow it received so far. Only the first mark counts.
    pub fn mark_outcome(&self) {
        let mut g = self.state.lock().unwrap();
        if g.charges.is_none() {
            g.charges = Some(g.last_inbound.clone());
        }
    }

    /// The call's tally, for its scenario's ladder. The tally is then closed
    /// and emptied: a later datagram (a late copy, a resender still running)
    /// is not tallied, and nothing of the call stays held.
    pub fn finish(&self) -> CallMessages {
        let mut g = self.state.lock().unwrap();
        let state =
            std::mem::replace(&mut *g, TallyState { finished: true, ..TallyState::default() });
        drop(g);
        let charges = state
            .charges
            .unwrap_or_default()
            .into_iter()
            .map(|(lane, slot)| {
                let arrow = match slot {
                    Slot::Entry(i) => state.entries.get(i).map(|e| e.arrow.clone()),
                    Slot::Other => None,
                };
                (lane, arrow)
            })
            .collect();
        CallMessages {
            caller: self.caller.clone(),
            arrows: state.entries.into_iter().map(|e| (e.arrow, e.stat)).collect(),
            other: state.other,
            charges,
        }
    }
}

/// `text` as an arrow of the call already holds it, else a new copy.
fn shared(entries: &[Entry], text: &str) -> Arc<str> {
    entries
        .iter()
        .flat_map(|e| [&e.arrow.from, &e.arrow.to, &e.arrow.label])
        .find(|held| ***held == *text)
        .cloned()
        .unwrap_or_else(|| text.into())
}

thread_local! {
    /// The label of the datagram being tallied, written before it is looked up.
    static LABEL: RefCell<String> = const { RefCell::new(String::new()) };
}

/// The message as a ladder labels it, into `out`: the request method
/// (`INVITE`), or the status and the CSeq method (`180 (INVITE)`); `?` when
/// unreadable.
fn write_label(id: &Identity<'_>, out: &mut String) {
    out.clear();
    // Writing into a String cannot fail.
    let _ = match (id.status, id.method, id.cseq_method) {
        (Some(status), _, Some(m)) => write!(out, "{status} ({m})"),
        (Some(status), _, None) => write!(out, "{status}"),
        (None, Some(method), _) => out.write_str(method),
        (None, None, _) => out.write_str("?"),
    };
}

/// A datagram's identity in one hash: one call's few hundred messages never
/// collide in 64 bits. A request is its method (its CSeq method is the same);
/// a response its status, CSeq method, RSeq and To-tag, so a forked final is a
/// new occurrence and a re-sent provisional a copy.
fn dup_key(leg: Option<&str>, dir: Dir, id: &Identity<'_>) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (dir, leg, id.call_id, id.cseq).hash(&mut h);
    match id.status {
        Some(status) => (status, id.cseq_method, id.rseq, id.to_tag).hash(&mut h),
        None => id.method.hash(&mut h),
    }
    h.finish()
}

#[cfg(test)]
#[path = "tally_tests.rs"]
mod tests;
