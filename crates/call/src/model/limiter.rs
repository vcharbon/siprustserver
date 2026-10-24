//! The call's admission state on the call limiter (ADR-0040): the key every
//! hold of the call is kept under, the set the limiter last stated it holds
//! for the key (**held**), the set the call's latest admit asked for
//! (**target**), and the change numbers that order the admits of the key.
//!
//! Every admit carries the call's next change number; the limiter refuses one
//! not above the number of the set it holds, and every answer states the set
//! it holds under its number. The call applies a statement only when it is
//! not older than the one its held set came from, so a late or lost answer
//! never rolls the call back, and the next answer repairs what a lost one
//! missed. Every change is computed from held, never from an unconfirmed
//! target. A call a service moved onto a set runs on that set whatever the
//! limiter answered; it runs uncounted while held lacks an id of it.

use serde::{Deserialize, Serialize};

/// One limiter entry: an id and the concurrent-call cap it is admitted under.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimiterEntry {
    /// Arbitrary limiter id (per-trunk / per-DID / global).
    pub id: String,
    /// Concurrent-call cap for this id.
    pub limit: i64,
}

/// The set the limiter states it holds for a key: its entries (empty when it
/// holds none) under the change number of the last admit it answered for the
/// key (0 when it knows none).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimiterHeld {
    /// The change number the set is stated under.
    pub change: u64,
    /// The entries held; empty when the limiter holds none for the key.
    pub entries: Vec<LimiterEntry>,
}

mod admit;
pub use admit::{AdmitOutcome, AdmitReport};

/// The call's admission state on the call limiter. The key is minted once at
/// the call's creation and is unique over time (`call_ref` alone is not: a
/// retried INVITE reuses it), so a release names this call and no later one;
/// it is replicated with the call, so a takeover, a reclaim and the lossy
/// reap release with the same key.
///
/// `held` is what the limiter last stated it holds for the key, under
/// `held_change`; `target` is what the call's latest admit asked for, under
/// `target_change`, from the turn that sends it when that turn knows the set
/// ([`replace_set`](Self::replace_set)), else from its report, and after a
/// refused or superseded change the held set; `change` is the last change
/// number the call gave an admit (or learnt the limiter knows), so the next
/// admit is numbered above it. A call materialised on another node moves
/// `change` to its next epoch ([`enter_epoch`](Self::enter_epoch)). `runs_on`
/// is the set the call runs on when a service moved it onto one on the turn
/// that sent its admit ([`replace_set`](Self::replace_set)), whatever the
/// limiter answers, or onto the set of a route a failover or a reroute runs
/// ([`apply_fold`](Self::apply_fold)); `None` when it runs on its target:
/// from its creation, and from a resolution after a refused route.
/// `release_owed` is set on the turn that sends an admit request for the key
/// (and by its report), and on the turn that dispatches a consult whose
/// answer may send one, whatever its answer, and is never cleared: the
/// request may have landed, so the call releases its key once at its end
/// ([`owed_release`](Self::owed_release)). `admit_sent` is set only where a
/// request left (the sending turn, or its report); `consults_open` counts the
/// consults dispatched whose route fold the call has not applied — each may
/// have sent an admit its lost fold would have reported. The call is counted
/// (refreshes its lease) iff held is not empty, which implies both
/// `release_owed` and `admit_sent`.
///
/// Every write is an operation of this type; the fields keep their order,
/// which is the replicated (positional) encoding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallLimiterState {
    key: String,
    release_owed: bool,
    held: Vec<LimiterEntry>,
    held_change: u64,
    target: Vec<LimiterEntry>,
    target_change: u64,
    change: u64,
    runs_on: Option<Vec<LimiterEntry>>,
    admit_sent: bool,
    consults_open: u32,
}

/// The change numbers of one epoch: a call's counter moves to the next
/// multiple on each materialisation on another node, so the new holder never
/// reuses a number the previous one reserved in its own epoch and sent
/// without replicating it. The bound that matters: a holder reserves far
/// fewer numbers over its whole lifetime on the call (a few per turn) than
/// one epoch holds.
pub const CHANGE_EPOCH: u64 = 1 << 32;

/// What a service's replacement of the call's admission set sends
/// ([`CallLimiterState::replace_set`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Replacement {
    /// An admit numbered `change` leaves, carrying the call's `held` set; the
    /// call owes its release from this turn.
    Send {
        /// The admit's change number.
        change: u64,
        /// The set the call holds as of this turn.
        held: LimiterHeld,
    },
    /// Nothing can have landed for the key and nothing is asked: no admit
    /// leaves, and the replacement numbered `change` reads as not sent.
    NothingToSend {
        /// The replacement's change number.
        change: u64,
    },
}

/// The admits a consult dispatched on a call's turn may send
/// ([`CallLimiterState::owe_consult`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConsultAdmits {
    /// The change number of the consult's first admit; the next ones follow.
    pub first_change: u64,
    /// Whether the limiter may hold a set for the key as of the dispatching
    /// turn, before the consult ([`CallLimiterState::may_hold`]).
    pub may_hold: bool,
}

/// How a refresh answer met the call's state
/// ([`CallLimiterState::apply_refresh`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshApplied {
    /// The answer names another key, or states a set older than held's.
    Stale,
    /// The answer left the state as it was.
    Unchanged,
    /// The stated set became held.
    Changed,
}

impl CallLimiterState {
    /// The state of a call under `key` that sent no admit request.
    pub fn uncounted(key: String) -> Self {
        Self {
            key,
            release_owed: false,
            held: Vec::new(),
            held_change: 0,
            target: Vec::new(),
            target_change: 0,
            change: 0,
            runs_on: None,
            admit_sent: false,
            consults_open: 0,
        }
    }

    /// The state under `key` of a call whose admit numbered `change` was
    /// admitted with `entries`.
    pub fn admitted(key: String, change: u64, entries: Vec<LimiterEntry>) -> Self {
        let mut state = Self::uncounted(key);
        state.apply_admit(&AdmitReport {
            key: state.key.clone(),
            change,
            entries,
            outcome: AdmitOutcome::Admitted,
        });
        state
    }

    /// A state with every field as given: `held` and `target` each with the
    /// change number it is stated under. For tests that set an exact state.
    #[cfg(any(test, feature = "testkit"))]
    pub fn from_parts(
        key: String,
        release_owed: bool,
        (held, held_change): (Vec<LimiterEntry>, u64),
        (target, target_change): (Vec<LimiterEntry>, u64),
        change: u64,
        runs_on: Option<Vec<LimiterEntry>>,
    ) -> Self {
        Self {
            key,
            release_owed,
            held,
            held_change,
            target,
            target_change,
            change,
            runs_on,
            admit_sent: release_owed,
            consults_open: 0,
        }
    }

    /// The limiter key every hold of the call is kept under.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The key whose release the call owes at its end: an admit request left
    /// for it. `None` for a call that sent none.
    pub fn owed_release(&self) -> Option<&str> {
        self.release_owed.then_some(self.key.as_str())
    }

    /// Whether the limiter may hold a set for the key, whatever held states:
    /// an admit request left for it, its answer lost or not yet applied, or a
    /// consult whose fold the call has not applied may have sent one. `false`
    /// only when nothing can have landed for the key.
    pub fn may_hold(&self) -> bool {
        self.admit_sent || self.consults_open > 0 || self.counted()
    }

    /// The set the limiter last stated it holds for the call.
    pub fn held(&self) -> &[LimiterEntry] {
        &self.held
    }

    /// The set the call's latest admit asked for; after a refused or
    /// superseded change, the held set.
    pub fn target(&self) -> &[LimiterEntry] {
        &self.target
    }

    /// The set a service or a route moved the call onto, which it runs on
    /// whatever the limiter answered; `None` when it runs on its target.
    pub fn runs_on(&self) -> Option<&[LimiterEntry]> {
        self.runs_on.as_deref()
    }

    /// Whether the limiter confirmed a non-empty set for the call: the call
    /// refreshes its lease.
    pub fn counted(&self) -> bool {
        !self.held.is_empty()
    }

    /// Whether the call runs on a target naming an id (with its multiplicity)
    /// the limiter does not hold for it: its admit got no usable answer or was
    /// not sent, a release fence refused it, the limiter dropped its set, or a
    /// change asked on a turn adds an id, for its round trip.
    pub fn fail_open(&self) -> bool {
        self.lacks_any(&self.target)
    }

    /// Whether the call runs on an id (with its multiplicity) the limiter
    /// does not hold for it: it runs [fail open](Self::fail_open), or on a
    /// set a service moved it onto ([`runs_on`](Self::runs_on)) that held
    /// lacks an id of, its admit refused, superseded, unanswered or in
    /// flight. Restated on every write of the call, so the next statement
    /// that holds the set ends it.
    pub fn runs_uncounted(&self) -> bool {
        self.fail_open() || self.runs_on.as_deref().is_some_and(|set| self.lacks_any(set))
    }

    /// Whether `set` names an id (with its multiplicity) held lacks.
    fn lacks_any(&self, set: &[LimiterEntry]) -> bool {
        let mut held: Vec<Option<&str>> = self.held.iter().map(|e| Some(e.id.as_str())).collect();
        set.iter().any(|e| match held.iter_mut().find(|id| **id == Some(e.id.as_str())) {
            Some(slot) => {
                *slot = None;
                false
            }
            None => true,
        })
    }

    /// The ids of the held set, in order: what the call is counted under.
    pub fn held_ids(&self) -> Vec<String> {
        self.held.iter().map(|e| e.id.clone()).collect()
    }

    /// The set held as a statement under its change number.
    pub fn held_set(&self) -> LimiterHeld {
        LimiterHeld { change: self.held_change, entries: self.held.clone() }
    }

    /// The report of the admit numbered `change` whose answer never came
    /// (`AdmitReport::lost`): its entries are the target while no later
    /// change replaced it, none otherwise.
    pub fn lost_admit(&self, change: u64) -> AdmitReport {
        let entries = if self.target_change == change { self.target.clone() } else { Vec::new() };
        AdmitReport::lost(self.key.clone(), change, entries)
    }

    /// This state with its change counter at zero: what it holds that a
    /// refresh's counter does not move.
    pub fn without_change(&self) -> Self {
        CallLimiterState { change: 0, ..self.clone() }
    }

    /// The call's turn dispatches a consult whose answer may admit a route:
    /// the call owes its release from this turn, and `n` change numbers are
    /// reserved, one per admit the consult may send (a failure consult's
    /// chain, a release consult's one). Returns the first one, and whether the
    /// limiter may hold a set for the key before the consult: a route stating
    /// no limiter still sends its empty replacement when it may.
    pub fn owe_consult(&mut self, n: u64) -> ConsultAdmits {
        let may_hold = self.may_hold();
        self.release_owed = true;
        self.consults_open = self.consults_open.saturating_add(1);
        ConsultAdmits { first_change: self.reserve_changes(n), may_hold }
    }

    /// A service replaces the call's admission set with `entries`, moving the
    /// call onto them when `moves_call` (it runs on them from this turn,
    /// whatever the answer). The replacement takes the next change number and
    /// is the call's target from this turn, so a change computed before its
    /// report comes back sees it. An admit leaves, and the call owes its
    /// release, unless nothing is asked and the limiter can hold nothing for
    /// the key ([`may_hold`](Self::may_hold)).
    pub fn replace_set(&mut self, entries: Vec<LimiterEntry>, moves_call: bool) -> Replacement {
        if moves_call {
            self.runs_on = Some(entries.clone());
        }
        let change = self.next_change();
        let nothing_asked = entries.is_empty();
        self.ask(change, entries);
        if nothing_asked && !self.may_hold() {
            return Replacement::NothingToSend { change };
        }
        self.release_owed = true;
        self.admit_sent = true;
        Replacement::Send { change, held: self.held_set() }
    }

    /// The change number and held set of the initial route's admit, sent on
    /// this turn. Owes nothing by itself: the admit's report does when a
    /// request left ([`apply_admit`](Self::apply_admit)).
    pub fn number_admit(&mut self) -> (u64, LimiterHeld) {
        (self.next_change(), self.held_set())
    }

    /// Move the change counter to the start of its next epoch
    /// ([`CHANGE_EPOCH`]): the call was materialised on another node, whose
    /// previous holder may have reserved and sent numbers its last replicated
    /// write does not show.
    pub fn enter_epoch(&mut self) {
        self.change = (self.change / CHANGE_EPOCH + 1) * CHANGE_EPOCH;
    }

    /// Apply an admit of the call's key (`report.key` is this call's): the
    /// release obligation only grows; the limiter's statement becomes held
    /// when not older than held's; a release fence empties held; the target
    /// becomes what the admit asked for, unless a newer admit already set it.
    /// A refused or superseded change is not taken: the target is the held
    /// set, the newest statement of all.
    pub fn apply_admit(&mut self, report: &AdmitReport) {
        let sent = report.owed_release().is_some();
        self.release_owed |= sent;
        self.admit_sent |= sent;
        self.change = self.change.max(report.change);
        if let Some(held) = report.held() {
            self.apply_held(&held);
        }
        if report.outcome == AdmitOutcome::Released {
            self.held.clear();
        }
        if report.change < self.target_change {
            return;
        }
        self.target = match &report.outcome {
            AdmitOutcome::Rejected { .. } | AdmitOutcome::Superseded { .. } => self.held.clone(),
            _ => report.entries.clone(),
        };
        self.target_change = report.change;
    }

    /// Apply a route fold's admit ([`apply_admit`](Self::apply_admit)), which
    /// closes the consult it answers: a fold that `runs_route` (a failover
    /// route, a release reroute) moves the call onto the route's set whatever
    /// its outcome, in any report order; a resolution after a refused route
    /// runs the call on its target.
    pub fn apply_fold(&mut self, report: &AdmitReport, runs_route: bool) {
        self.apply_unawaited_fold(report);
        self.runs_on = runs_route.then(|| report.entries.clone());
    }

    /// Apply a route fold no consult awaits any more: its admit applies and it
    /// closes its consult, moving no route onto the call.
    pub fn apply_unawaited_fold(&mut self, report: &AdmitReport) {
        self.consults_open = self.consults_open.saturating_sub(1);
        self.apply_admit(report);
    }

    /// Apply a refresh answer for `key` stating `held`, if it states one. An
    /// answer for another key (an earlier call under the same `call_ref`), or
    /// stating a set older than held's (an admit restated the set since), is
    /// stale and changes nothing; otherwise its statement becomes held.
    pub fn apply_refresh(&mut self, key: &str, held: Option<&LimiterHeld>) -> RefreshApplied {
        if key != self.key || held.is_some_and(|held| held.change < self.held_change) {
            return RefreshApplied::Stale;
        }
        let Some(held) = held else {
            return RefreshApplied::Unchanged;
        };
        let changed = self.change < held.change
            || self.held_change != held.change
            || self.held != held.entries;
        self.apply_held(held);
        if changed {
            RefreshApplied::Changed
        } else {
            RefreshApplied::Unchanged
        }
    }

    /// Reserve `n` change numbers for admits sent in order; the first one.
    fn reserve_changes(&mut self, n: u64) -> u64 {
        let first = self.change + 1;
        self.change += n.max(1);
        first
    }

    fn next_change(&mut self) -> u64 {
        self.reserve_changes(1)
    }

    /// The call's turn sends a change numbered `change` asking for `entries`:
    /// they are its target from this turn on.
    fn ask(&mut self, change: u64, entries: Vec<LimiterEntry>) {
        if change < self.target_change {
            return;
        }
        self.target = entries;
        self.target_change = change;
    }

    /// Apply the limiter's statement of the set it holds: taken when not older
    /// than the statement held came from; `true` when taken. The next admit is
    /// numbered above it either way.
    fn apply_held(&mut self, held: &LimiterHeld) -> bool {
        self.change = self.change.max(held.change);
        if held.change < self.held_change {
            return false;
        }
        self.held_change = held.change;
        self.held = held.entries.clone();
        true
    }
}

#[cfg(test)]
mod tests;
