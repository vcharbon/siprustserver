//! The message budget of each Call-ID (ADR-0063 §3, §4): how many datagrams
//! the layer received and sent under it, the mute a Call-ID past its budget
//! is under, and what its trip reports. The map is bounded; a muted entry is
//! never evicted. Node-local: every ownership move counts from zero (§5).

use std::collections::HashMap;
use std::time::Duration;

use tokio::time::Instant;

use crate::event::CallIdBudgetTrip;
use crate::timers::{ms, TIMER_B};

/// How long an entry outlives the last message it counted once no
/// transaction of its Call-ID lives, and how long a mute outlives the last
/// message it dropped: 64·T1, past which a compliant peer retransmits
/// nothing.
const WINDOW: Duration = ms(TIMER_B);

/// The most caller tags and call references a trip reports.
pub const TRIP_REPORTS_AT_MOST: usize = 4;

/// The per-Call-ID budget a consumer sets on the layer
/// ([`TransactionConfig::call_id_budget`](crate::TransactionConfig)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallIdBudget {
    /// The datagrams one Call-ID may carry, received and sent together; the
    /// one past it trips the budget. At least 1.
    pub messages: u64,
    /// The Call-IDs counted at once. A new Call-ID evicts an unmuted entry
    /// when the map is full, and goes uncounted when every entry is muted.
    pub entries: usize,
}

impl CallIdBudget {
    /// The Call-IDs counted at once by default: every Call-ID of 100 000
    /// two-leg calls, and 64·T1 of ended ones at 800 calls a second.
    pub const ENTRIES: usize = 262_144;
}

/// What the budget decided for one datagram under a Call-ID.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Admission {
    /// Counted under its Call-ID, within the budget.
    Counted,
    /// Not counted: every entry of the full map is muted.
    Uncounted,
    /// Dropped: its Call-ID is muted.
    Muted,
    /// Dropped: it passed its Call-ID's budget, which is muted from now on.
    Tripped(CallIdBudgetTrip),
}

/// The count of one Call-ID.
struct Entry {
    messages: u64,
    last_message: Instant,
    /// The transactions of the Call-ID resident in the layer.
    txns: u32,
    /// The From-tags of the tagless INVITEs that opened a server
    /// transaction under the Call-ID, at most [`TRIP_REPORTS_AT_MOST`].
    caller_tags: Vec<String>,
    /// The call references of its transactions, at most
    /// [`TRIP_REPORTS_AT_MOST`].
    call_refs: Vec<String>,
    /// The instant of the last message the mute dropped; `None` while the
    /// Call-ID is not muted.
    last_drop: Option<Instant>,
}

impl Entry {
    fn new(now: Instant) -> Self {
        Self {
            messages: 0,
            last_message: now,
            txns: 0,
            caller_tags: Vec::new(),
            call_refs: Vec::new(),
            last_drop: None,
        }
    }

    /// Whether the entry is over at `now`: no transaction of its Call-ID is
    /// resident and 64·T1 passed since its last message, or since the last
    /// message its mute dropped.
    fn lapsed(&self, now: Instant) -> bool {
        let last = self.last_drop.unwrap_or(self.last_message);
        self.txns == 0 && now.saturating_duration_since(last) >= WINDOW
    }
}

/// Record `item` in `list` unless it holds it already or is full.
fn note(list: &mut Vec<String>, item: &str) {
    if list.len() < TRIP_REPORTS_AT_MOST && !list.iter().any(|known| known == item) {
        list.push(item.to_string());
    }
}

/// The counts of the Call-IDs the layer carries.
pub(super) struct Budgets {
    budget: CallIdBudget,
    entries: HashMap<String, Entry>,
    muted: usize,
}

impl Budgets {
    pub(super) fn new(budget: CallIdBudget) -> Self {
        Self { budget, entries: HashMap::new(), muted: 0 }
    }

    /// Count one datagram under `call_id` at `now`. A muted Call-ID drops it
    /// and its mute runs 64·T1 from it; the datagram past the budget mutes
    /// its Call-ID and is dropped. A lapsed entry counts afresh.
    pub(super) fn admit(&mut self, call_id: &str, now: Instant) -> Admission {
        let budget = self.budget.messages;
        let Some(entry) = self.entry(call_id, now) else { return Admission::Uncounted };
        if entry.last_drop.is_some() {
            entry.last_drop = Some(now);
            return Admission::Muted;
        }
        entry.messages += 1;
        entry.last_message = now;
        if entry.messages <= budget {
            return Admission::Counted;
        }
        entry.last_drop = Some(now);
        let trip = CallIdBudgetTrip {
            call_id: call_id.to_string(),
            caller_tags: entry.caller_tags.clone(),
            call_refs: entry.call_refs.clone(),
        };
        self.muted += 1;
        Admission::Tripped(trip)
    }

    /// Whether `call_id` is muted at `now`, for a datagram outside the
    /// count: one it drops restarts the mute's 64·T1.
    pub(super) fn drops(&mut self, call_id: &str, now: Instant) -> bool {
        self.forget_lapsed(call_id, now);
        match self.entries.get_mut(call_id) {
            Some(entry) if entry.last_drop.is_some() => {
                entry.last_drop = Some(now);
                true
            }
            _ => false,
        }
    }

    /// `call_id`'s entry at `now`: a lapsed one replaced by a fresh one, one
    /// made where there is room; `None` when every entry of the full map is
    /// muted.
    fn entry(&mut self, call_id: &str, now: Instant) -> Option<&mut Entry> {
        self.forget_lapsed(call_id, now);
        if !self.entries.contains_key(call_id) {
            if !self.make_room() {
                return None;
            }
            self.entries.insert(call_id.to_string(), Entry::new(now));
        }
        self.entries.get_mut(call_id)
    }

    /// Drop `call_id`'s entry if it lapsed at `now`, lifting its mute.
    fn forget_lapsed(&mut self, call_id: &str, now: Instant) {
        if self.entries.get(call_id).is_some_and(|e| e.lapsed(now)) {
            self.remove(call_id);
        }
    }

    /// A transaction of `call_id` entered the layer at `now`: attributed to
    /// `call_ref`, and opened by a tagless INVITE from `caller_tag`, when
    /// they are known. The entry lives while it does; with no room for an
    /// entry, the Call-ID goes untracked.
    pub(super) fn opened(
        &mut self,
        call_id: &str,
        call_ref: Option<&str>,
        caller_tag: Option<&str>,
        now: Instant,
    ) {
        let Some(entry) = self.entry(call_id, now) else { return };
        entry.txns = entry.txns.saturating_add(1);
        if let Some(call_ref) = call_ref {
            note(&mut entry.call_refs, call_ref);
        }
        if let Some(tag) = caller_tag {
            note(&mut entry.caller_tags, tag);
        }
    }

    /// A transaction of `call_id` left the layer.
    pub(super) fn closed(&mut self, call_id: &str) {
        if let Some(entry) = self.entries.get_mut(call_id) {
            entry.txns = entry.txns.saturating_sub(1);
        }
    }

    /// Drop every entry lapsed at `now`, lifting the mutes among them.
    pub(super) fn expire(&mut self, now: Instant) {
        let before = self.entries.len();
        let mut lifted = 0;
        self.entries.retain(|_, e| {
            let lapsed = e.lapsed(now);
            if lapsed && e.last_drop.is_some() {
                lifted += 1;
            }
            !lapsed
        });
        self.muted -= lifted;
        if self.entries.len() < before {
            super::shrink::shrink_idle(&mut self.entries);
        }
    }

    fn remove(&mut self, call_id: &str) {
        if let Some(entry) = self.entries.remove(call_id) {
            if entry.last_drop.is_some() {
                self.muted -= 1;
            }
        }
    }

    /// Room for one more entry: below the bound, or an unmuted entry
    /// evicted. `false` when every entry is muted.
    fn make_room(&mut self) -> bool {
        if self.entries.len() < self.budget.entries {
            return true;
        }
        if self.muted >= self.entries.len() {
            return false;
        }
        let evicted =
            self.entries.iter().find(|(_, e)| e.last_drop.is_none()).map(|(k, _)| k.clone());
        evicted.is_some_and(|call_id| self.entries.remove(&call_id).is_some())
    }
}

#[cfg(test)]
mod tests;
