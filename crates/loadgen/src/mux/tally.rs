//! One call's message tally: every datagram its endpoints send or have
//! delivered, keyed by arrow `(from, to, label, nth)`, with the duplicates
//! received and the re-sends of each. A datagram whose identity (direction,
//! endpoint, Call-ID, CSeq, method or status, RSeq) was already seen on the
//! call is a retransmission of that arrow and opens no new occurrence. The
//! state is per call, bounded by [`MAX_ARROWS`], and folded into the reporter
//! once when the call ends.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::time::Instant;

use super::sniffed::Sniffed;
use super::taps::Dir;

/// The SUT's lane: the far end of every arrow.
pub const SUT_LANE: &str = "SUT";

/// The distinct arrows one call keeps at most; a datagram opening an arrow
/// past it is not tallied.
pub(crate) const MAX_ARROWS: usize = 256;

/// One arrow of a ladder: who sent what to whom, the how-many-th distinct
/// occurrence of that `(from, to, label)` it is in its call (from 1).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArrowKey {
    pub from: String,
    pub to: String,
    pub label: String,
    pub nth: u32,
}

/// One arrow's counters in one call (or folded over calls).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ArrowStat {
    /// Distinct occurrences.
    pub count: u64,
    /// Copies received after the first.
    pub duplicates: u64,
    /// Copies sent after the first: the retransmit engine's re-sends and any
    /// re-send by the call itself.
    pub resent: u64,
    /// Sum over the occurrences of their first copy's offset from call
    /// start, in milliseconds.
    pub offset_ms_sum: f64,
}

/// What one call leaves for its scenario's ladder.
#[derive(Debug, Clone, Default)]
pub struct CallMessages {
    /// Every arrow of the call, in first-seen order.
    pub arrows: Vec<(ArrowKey, ArrowStat)>,
    /// The last inbound arrow before the call's outcome was decided; `None`
    /// when nothing had come in (or the outcome was never marked).
    pub charge: Option<ArrowKey>,
    /// The call ran the retransmit engine.
    pub engine: bool,
}

/// A datagram's identity, hashed: one call's handful of messages never
/// collide in 64 bits, and no field is copied per datagram.
type DupKey = u64;

struct TallyState {
    seen: HashMap<DupKey, usize>,
    nth: HashMap<(String, String, String), u32>,
    arrows: Vec<(ArrowKey, ArrowStat)>,
    last_inbound: Option<usize>,
    /// Set once, when the outcome is decided: the inbound arrow it is charged to.
    charge: Option<Option<usize>>,
    /// The tally was handed over: later datagrams are not tallied.
    finished: bool,
}

/// One call's tally, shared by its endpoints.
pub struct CallTally {
    start: Instant,
    caller: String,
    engine: bool,
    state: Mutex<TallyState>,
}

impl CallTally {
    /// A tally starting now; `caller` names the caller's lane, `engine` says
    /// whether the call runs the retransmit engine.
    pub fn new(caller: &str, engine: bool) -> Self {
        Self {
            start: Instant::now(),
            caller: caller.to_string(),
            engine,
            state: Mutex::new(TallyState {
                seen: HashMap::new(),
                nth: HashMap::new(),
                arrows: Vec::new(),
                last_inbound: None,
                charge: None,
                finished: false,
            }),
        }
    }

    /// A datagram the endpoint `leg` (`None`: the caller) sent (`Out`, its
    /// first transmission by the call) or had delivered (`In`) at `now`.
    pub(super) fn on_datagram(&self, leg: Option<&str>, dir: Dir, m: &Sniffed, now: Instant) {
        let mut g = self.state.lock().unwrap();
        if g.finished {
            return;
        }
        let key = dup_key(leg, dir, m);
        let idx = match g.seen.get(&key).copied() {
            Some(idx) => {
                if let Some((_, stat)) = g.arrows.get_mut(idx) {
                    match dir {
                        Dir::In => stat.duplicates += 1,
                        Dir::Out => stat.resent += 1,
                    }
                }
                Some(idx)
            }
            None => self.open(&mut g, key, leg, dir, m, now),
        };
        if dir == Dir::In && idx.is_some() {
            g.last_inbound = idx;
        }
    }

    /// A re-send the retransmit engine of endpoint `leg` made of a datagram
    /// it sent before.
    pub(super) fn on_resend(&self, leg: Option<&str>, m: &Sniffed) {
        let mut g = self.state.lock().unwrap();
        if g.finished {
            return;
        }
        let key = dup_key(leg, Dir::Out, m);
        let idx = match g.seen.get(&key).copied() {
            Some(idx) => Some(idx),
            None => self.open(&mut g, key, leg, Dir::Out, m, Instant::now()),
        };
        if let Some((_, stat)) = idx.and_then(|idx| g.arrows.get_mut(idx)) {
            stat.resent += 1;
        }
    }

    /// The call's outcome is decided: its error, if any, is charged to the
    /// last inbound arrow seen so far. Only the first mark counts.
    pub fn mark_outcome(&self) {
        let mut g = self.state.lock().unwrap();
        if g.charge.is_none() {
            g.charge = Some(g.last_inbound);
        }
    }

    /// The call's tally, for its scenario's ladder. The tally is then closed:
    /// a datagram after it (a late copy, a resender still running) is not
    /// tallied, and a second call returns nothing.
    pub fn finish(&self) -> CallMessages {
        let mut g = self.state.lock().unwrap();
        g.finished = true;
        g.seen.clear();
        let arrows = std::mem::take(&mut g.arrows);
        let charge = g.charge.flatten().and_then(|i| arrows.get(i)).map(|(k, _)| k.clone());
        CallMessages { arrows, charge, engine: self.engine }
    }

    fn open(
        &self,
        g: &mut TallyState,
        key: DupKey,
        leg: Option<&str>,
        dir: Dir,
        m: &Sniffed,
        now: Instant,
    ) -> Option<usize> {
        if g.arrows.len() >= MAX_ARROWS {
            return None;
        }
        let side = leg.unwrap_or(&self.caller).to_string();
        let (from, to) = match dir {
            Dir::Out => (side, SUT_LANE.to_string()),
            Dir::In => (SUT_LANE.to_string(), side),
        };
        let label = m.label();
        let nth = g.nth.entry((from.clone(), to.clone(), label.clone())).or_default();
        *nth += 1;
        let arrow = ArrowKey { from, to, label, nth: *nth };
        let offset_ms = now.saturating_duration_since(self.start).as_secs_f64() * 1000.0;
        let stat = ArrowStat { count: 1, offset_ms_sum: offset_ms, ..ArrowStat::default() };
        g.arrows.push((arrow, stat));
        let idx = g.arrows.len() - 1;
        g.seen.insert(key, idx);
        Some(idx)
    }
}

fn dup_key(leg: Option<&str>, dir: Dir, m: &Sniffed) -> DupKey {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (dir, leg, &m.call_id, m.cseq, &m.cseq_method, m.status, &m.method, m.rseq).hash(&mut h);
    h.finish()
}

#[cfg(test)]
#[path = "tally_tests.rs"]
mod tests;
