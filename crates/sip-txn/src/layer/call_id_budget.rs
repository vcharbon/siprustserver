//! The message budget of each Call-ID (ADR-0063 §3, §4): how many datagrams
//! the layer received and sent under it, the mute a Call-ID past its budget
//! is under, and what its trip reports. The map is bounded: a Call-ID new to
//! a full map takes the place of an idle entry, and an entry that is muted or
//! counts a resident transaction is never evicted. Node-local: every
//! ownership move counts from zero (§5).

use std::collections::{HashMap, VecDeque};
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
    /// The Call-IDs counted at once. A datagram of a Call-ID new to a full
    /// map takes the place of an idle entry (no resident transaction, not
    /// muted), the one whose last message is oldest among those idle before
    /// it, and goes uncounted when none is idle. A
    /// Call-ID with a resident transaction is always counted, past the bound.
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
    /// Not counted: no entry of the full map is idle (each is muted or
    /// holds a live transaction).
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
    /// The ticket of the entry's place in [`IdleQueue`]: `Some` exactly while
    /// no transaction of the Call-ID is resident and it is not muted.
    queued: Option<u64>,
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
            queued: None,
        }
    }

    /// Whether the entry is over at `now`: no transaction of its Call-ID is
    /// resident and 64·T1 passed since its last message, or since the last
    /// message its mute dropped.
    fn lapsed(&self, now: Instant) -> bool {
        let last = self.last_drop.unwrap_or(self.last_message);
        self.txns == 0 && now.saturating_duration_since(last) >= WINDOW
    }

    /// Whether the entry may give its place to another Call-ID.
    fn idle(&self) -> bool {
        self.txns == 0 && self.last_drop.is_none()
    }
}

/// Record `item` in `list` unless it holds it already or is full.
fn note(list: &mut Vec<String>, item: &str) {
    if list.len() < TRIP_REPORTS_AT_MOST && !list.iter().any(|known| known == item) {
        list.push(item.to_string());
    }
}

/// One place in [`IdleQueue`]: the Call-ID, the ticket its entry held when
/// queued, and the entry's last message then.
struct IdleItem {
    call_id: String,
    ticket: u64,
    last_message: Instant,
}

/// The idle entries, each queued with the instant of its last message. An
/// entry that counted a message since it was queued goes back to the end
/// when it reaches the front, so the front evicted is the entry whose last
/// message is the oldest of those queued before it. An item whose entry is
/// gone or holds another ticket is stale: skipped when popped, dropped by
/// the sweep.
#[derive(Default)]
struct IdleQueue {
    items: VecDeque<IdleItem>,
    next_ticket: u64,
}

impl IdleQueue {
    /// Queue `entry` of `call_id` if it went idle and is not queued yet.
    fn enqueue(&mut self, call_id: &str, entry: &mut Entry) {
        if entry.idle() && entry.queued.is_none() {
            self.push(call_id.to_string(), entry);
        }
    }

    /// Queue `entry` of `call_id` at the end under a new ticket.
    fn push(&mut self, call_id: String, entry: &mut Entry) {
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        entry.queued = Some(ticket);
        self.items.push_back(IdleItem { call_id, ticket, last_message: entry.last_message });
    }

    /// Evict the front entry of `entries` whose last message is the one it
    /// was queued with, re-queueing each one that counted a message since.
    /// `false` when no entry is queued.
    fn evict(&mut self, entries: &mut HashMap<String, Entry>) -> bool {
        while let Some(item) = self.items.pop_front() {
            let Some(entry) = entries.get_mut(&item.call_id) else { continue };
            if entry.queued != Some(item.ticket) {
                continue;
            }
            if entry.last_message > item.last_message {
                self.push(item.call_id, entry);
                continue;
            }
            entries.remove(&item.call_id);
            return true;
        }
        false
    }
}

/// Count one datagram on `entry` of `call_id` at `now` against `budget`, a
/// lapsed entry counting afresh.
fn count(
    idle: &mut IdleQueue,
    call_id: &str,
    entry: &mut Entry,
    budget: u64,
    now: Instant,
) -> Admission {
    renew_lapsed(idle, call_id, entry, now);
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
    entry.queued = None;
    Admission::Tripped(CallIdBudgetTrip {
        call_id: call_id.to_string(),
        caller_tags: entry.caller_tags.clone(),
        call_refs: entry.call_refs.clone(),
    })
}

/// Start `entry` of `call_id` afresh if it lapsed at `now`, lifting its mute.
fn renew_lapsed(idle: &mut IdleQueue, call_id: &str, entry: &mut Entry, now: Instant) {
    if entry.lapsed(now) {
        *entry = Entry { queued: entry.queued, ..Entry::new(now) };
        idle.enqueue(call_id, entry);
    }
}

/// The counts of the Call-IDs the layer carries.
pub(super) struct Budgets {
    budget: CallIdBudget,
    entries: HashMap<String, Entry>,
    idle: IdleQueue,
}

impl Budgets {
    pub(super) fn new(budget: CallIdBudget) -> Self {
        Self { budget, entries: HashMap::new(), idle: IdleQueue::default() }
    }

    /// Count one datagram under `call_id` at `now`. A muted Call-ID drops it
    /// and its mute runs 64·T1 from it; the datagram past the budget mutes
    /// its Call-ID and is dropped. A lapsed entry counts afresh.
    pub(super) fn admit(&mut self, call_id: &str, now: Instant) -> Admission {
        let budget = self.budget.messages;
        if let Some(entry) = self.entries.get_mut(call_id) {
            return count(&mut self.idle, call_id, entry, budget, now);
        }
        if !self.make_room() {
            return Admission::Uncounted;
        }
        let mut entry = Entry::new(now);
        self.idle.enqueue(call_id, &mut entry);
        let admission = count(&mut self.idle, call_id, &mut entry, budget, now);
        self.entries.insert(call_id.to_string(), entry);
        admission
    }

    /// Whether `call_id` is muted at `now`, for a datagram outside the
    /// count: one it drops restarts the mute's 64·T1.
    pub(super) fn drops(&mut self, call_id: &str, now: Instant) -> bool {
        match self.entries.get_mut(call_id) {
            Some(entry) if entry.last_drop.is_some() && !entry.lapsed(now) => {
                entry.last_drop = Some(now);
                true
            }
            _ => false,
        }
    }

    /// A transaction of `call_id` entered the layer at `now`: attributed to
    /// `call_ref`, and opened by a tagless INVITE from `caller_tag`, when
    /// they are known. The entry lives while it does, made past the bound
    /// when the map is full.
    pub(super) fn opened(
        &mut self,
        call_id: &str,
        call_ref: Option<&str>,
        caller_tag: Option<&str>,
        now: Instant,
    ) {
        if let Some(entry) = self.entries.get_mut(call_id) {
            renew_lapsed(&mut self.idle, call_id, entry, now);
            open(entry, call_ref, caller_tag);
            return;
        }
        let mut entry = Entry::new(now);
        open(&mut entry, call_ref, caller_tag);
        self.entries.insert(call_id.to_string(), entry);
    }

    /// A transaction of `call_id`, [`opened`](Self::opened) before, left the
    /// layer.
    pub(super) fn closed(&mut self, call_id: &str) {
        let entry = self.entries.get_mut(call_id);
        debug_assert!(
            entry.as_ref().is_some_and(|e| e.txns > 0),
            "a transaction closed under a Call-ID counting none"
        );
        let Some(entry) = entry else { return };
        entry.txns = entry.txns.saturating_sub(1);
        self.idle.enqueue(call_id, entry);
    }

    /// Drop every entry lapsed at `now`, lifting the mutes among them, and
    /// the stale items of the idle queue.
    pub(super) fn expire(&mut self, now: Instant) {
        let before = self.entries.len();
        self.entries.retain(|_, e| !e.lapsed(now));
        if self.entries.len() < before {
            super::shrink::shrink_idle(&mut self.entries);
        }
        let entries = &self.entries;
        let items = &mut self.idle.items;
        items.retain(|item| holds_ticket(entries, &item.call_id, item.ticket));
        super::shrink::shrink_idle_queue(items);
    }

    /// Room for one more entry: below the bound, or an idle entry evicted
    /// ([`IdleQueue::evict`]). `false` when no entry is idle; a lapsed mute
    /// frees its place at the next [`expire`](Self::expire).
    fn make_room(&mut self) -> bool {
        self.entries.len() < self.budget.entries || self.idle.evict(&mut self.entries)
    }
}

/// Count a transaction opened by a tagless INVITE from `caller_tag`, of
/// `call_ref`, on `entry`.
fn open(entry: &mut Entry, call_ref: Option<&str>, caller_tag: Option<&str>) {
    entry.txns = entry.txns.saturating_add(1);
    entry.queued = None;
    if let Some(call_ref) = call_ref {
        note(&mut entry.call_refs, call_ref);
    }
    if let Some(tag) = caller_tag {
        note(&mut entry.caller_tags, tag);
    }
}

/// Whether `call_id`'s entry still holds the idle queue's `ticket`.
fn holds_ticket(entries: &HashMap<String, Entry>, call_id: &str, ticket: u64) -> bool {
    entries.get(call_id).is_some_and(|e| e.queued == Some(ticket))
}

#[cfg(test)]
mod tests;
