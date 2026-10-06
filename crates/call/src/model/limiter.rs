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
    /// The entries held.
    pub entries: Vec<LimiterEntry>,
}

/// The honest outcome of one admit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmitOutcome {
    /// The call's set is now the entries sent, under the admit's number.
    Admitted,
    /// An id the call adds is at its cap: the call's set is what it was, or
    /// nothing when the admit asked for release on refusal, as `held` states.
    Rejected {
        /// The first id found at its cap.
        limiter_id: String,
        /// The set the limiter holds for the key after the refusal.
        held: LimiterHeld,
    },
    /// The admit's number is not above the one of the set the limiter holds
    /// for the key (a newer admit of the key landed first): nothing moved.
    Superseded {
        /// The set the limiter holds for the key.
        held: LimiterHeld,
    },
    /// The call was released within the last lease: the limiter holds nothing
    /// for it and re-creates nothing.
    Released,
    /// The request left and no usable answer came back (unreachable, slow,
    /// errored, a bad body): it may have landed.
    Unavailable,
    /// No request left (no limiter configured, or a local guard such as an
    /// open circuit breaker refused to send one): nothing can have landed.
    NotSent,
}

impl AdmitOutcome {
    /// The id a cap refusal names; `None` for any other outcome.
    pub fn refused_on(&self) -> Option<&str> {
        match self {
            AdmitOutcome::Rejected { limiter_id, .. } => Some(limiter_id),
            _ => None,
        }
    }

    /// The outcome's label on the wire of an internal event and in metrics.
    pub fn label(&self) -> &'static str {
        match self {
            AdmitOutcome::Admitted => "admitted",
            AdmitOutcome::Rejected { .. } => "rejected",
            AdmitOutcome::Superseded { .. } => "superseded",
            AdmitOutcome::Released => "released",
            AdmitOutcome::Unavailable => "unavailable",
            AdmitOutcome::NotSent => "not_sent",
        }
    }
}

/// One admit of a call's key as the call applies it: the key, the change
/// number the admit carried, the entries it asked for, and its outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmitReport {
    /// The limiter key the admit named.
    pub key: String,
    /// The change number the admit carried.
    pub change: u64,
    /// The entries the admit asked for.
    pub entries: Vec<LimiterEntry>,
    /// What came back.
    pub outcome: AdmitOutcome,
}

impl AdmitReport {
    /// The admit of `entries` for `key` under `change` whose answer never
    /// came: [`AdmitOutcome::Unavailable`].
    fn lost(key: String, change: u64, entries: Vec<LimiterEntry>) -> Self {
        Self { key, change, entries, outcome: AdmitOutcome::Unavailable }
    }

    /// The key whose release this admit owes: a request left, and it may
    /// have landed. `None` for an admit that sent no request.
    pub fn owed_release(&self) -> Option<&str> {
        (self.outcome != AdmitOutcome::NotSent).then_some(self.key.as_str())
    }

    /// The set the limiter stated it holds for the key: the entries sent when
    /// admitted, the stated set on a refusal or a supersession, none
    /// otherwise.
    pub fn held(&self) -> Option<LimiterHeld> {
        match &self.outcome {
            AdmitOutcome::Admitted => {
                Some(LimiterHeld { change: self.change, entries: self.entries.clone() })
            }
            AdmitOutcome::Rejected { held, .. } | AdmitOutcome::Superseded { held } => {
                Some(held.clone())
            }
            AdmitOutcome::Released | AdmitOutcome::Unavailable | AdmitOutcome::NotSent => None,
        }
    }
}

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
/// (and by its report), whatever its answer, and is never cleared: the
/// request may have landed, so the call releases its key once at its end
/// ([`owed_release`](Self::owed_release)). The call is counted (refreshes its
/// lease) iff held is not empty, which implies `release_owed`.
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
        Self { key, release_owed, held, held_change, target, target_change, change, runs_on }
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
        let mut held: Vec<&str> = self.held.iter().map(|e| e.id.as_str()).collect();
        set.iter().any(|e| match held.iter().position(|id| *id == e.id) {
            Some(i) => {
                held.swap_remove(i);
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

    /// The call's turn dispatches a consult whose answer may admit a route:
    /// the call owes its release from this turn, and `n` change numbers are
    /// reserved, one per admit the consult may send (a failure consult's
    /// chain, a release consult's one); the first one.
    pub fn owe_consult(&mut self, n: u64) -> u64 {
        self.release_owed = true;
        self.reserve_changes(n)
    }

    /// A service replaces the call's admission set with `entries`, moving the
    /// call onto them when `moves_call` (it runs on them from this turn,
    /// whatever the answer). The replacement takes the next change number and
    /// is the call's target from this turn, so a change computed before its
    /// report comes back sees it. An admit leaves, and the call owes its
    /// release, unless nothing can have landed for the key (not counted, no
    /// release owed) and nothing is asked.
    pub fn replace_set(&mut self, entries: Vec<LimiterEntry>, moves_call: bool) -> Replacement {
        if moves_call {
            self.runs_on = Some(entries.clone());
        }
        let change = self.next_change();
        let nothing_asked = entries.is_empty();
        self.ask(change, entries);
        if nothing_asked && !self.counted() && !self.release_owed {
            return Replacement::NothingToSend { change };
        }
        self.release_owed = true;
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
        self.release_owed |= report.owed_release().is_some();
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

    /// Apply a route fold's admit ([`apply_admit`](Self::apply_admit)): a
    /// fold that `runs_route` (a failover route, a release reroute) moves the
    /// call onto the route's set whatever its outcome, in any report order; a
    /// resolution after a refused route runs the call on its target.
    pub fn apply_fold(&mut self, report: &AdmitReport, runs_route: bool) {
        self.apply_admit(report);
        self.runs_on = runs_route.then(|| report.entries.clone());
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

    /// Reserve the next change number.
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
mod tests {
    use super::*;

    fn e(id: &str) -> LimiterEntry {
        LimiterEntry { id: id.into(), limit: 10 }
    }

    fn report(change: u64, entries: Vec<LimiterEntry>, outcome: AdmitOutcome) -> AdmitReport {
        AdmitReport { key: "c#k".into(), change, entries, outcome }
    }

    fn held(change: u64, entries: Vec<LimiterEntry>) -> LimiterHeld {
        LimiterHeld { change, entries }
    }

    fn refused(n: u64, id: &str, entries: Vec<LimiterEntry>) -> AdmitOutcome {
        AdmitOutcome::Rejected { limiter_id: id.into(), held: held(n, entries) }
    }

    /// The change number of a replacement that leaves.
    fn sent(replacement: Replacement) -> u64 {
        match replacement {
            Replacement::Send { change, .. } => change,
            Replacement::NothingToSend { .. } => panic!("an admit leaves"),
        }
    }

    #[test]
    fn an_admitted_set_is_held_and_targeted_and_owes_the_release() {
        let state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x"), e("y")]);
        assert!(state.counted() && !state.fail_open());
        assert_eq!(state.owed_release(), Some("c#k"));
        assert_eq!((state.held_change, state.target_change, state.change), (1, 1, 1));
    }

    #[test]
    fn a_lost_answer_keeps_held_and_runs_open_on_what_it_asked_for() {
        let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
        state.apply_admit(&report(2, vec![e("x"), e("y")], AdmitOutcome::Unavailable));
        assert_eq!(state.held(), [e("x")], "held is what the limiter last stated");
        assert!(state.counted() && state.fail_open(), "counted, y uncounted");
        let mut fresh = CallLimiterState::uncounted("c#k".into());
        fresh.apply_admit(&report(1, vec![e("x")], AdmitOutcome::NotSent));
        assert_eq!(fresh.owed_release(), None, "an unsent admit owes nothing");
        assert!(!fresh.counted() && fresh.fail_open());
        fresh.apply_admit(&report(2, vec![], AdmitOutcome::NotSent));
        assert!(!fresh.fail_open(), "a target naming no id asks for nothing");
    }

    #[test]
    fn a_cap_refusal_leaves_the_call_on_what_the_limiter_holds() {
        let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
        state.apply_admit(&report(2, vec![e("x"), e("y")], refused(2, "y", vec![e("x")])));
        assert_eq!((state.held(), state.target()), (&[e("x")][..], &[e("x")][..]));
        assert!(state.counted() && !state.fail_open(), "a refused change is not taken");
        state.apply_admit(&report(3, vec![e("y")], refused(3, "y", vec![])));
        assert!(!state.counted(), "a refusal that dropped the set");
        assert_eq!(state.owed_release(), Some("c#k"));
    }

    #[test]
    fn an_older_statement_never_rolls_held_back() {
        let mut state = CallLimiterState::admitted("c#k".into(), 3, vec![e("z")]);
        let older = held(2, vec![e("x")]);
        assert_eq!(state.apply_refresh("c#k", Some(&older)), RefreshApplied::Stale);
        assert_eq!(state.held(), [e("z")]);
        let again = held(3, vec![e("z")]);
        assert_eq!(state.apply_refresh("c#k", Some(&again)), RefreshApplied::Unchanged);
        state.apply_admit(&report(2, vec![e("x")], AdmitOutcome::Admitted));
        assert_eq!((state.held(), state.target()), (&[e("z")][..], &[e("z")][..]));
        let newer = held(7, vec![e("y")]);
        assert_eq!(state.apply_refresh("c#k", Some(&newer)), RefreshApplied::Changed);
        assert_eq!(state.number_admit().0, 8, "the next admit is numbered above it");
    }

    #[test]
    fn a_refresh_for_another_key_is_stale_and_one_stating_no_set_changes_nothing() {
        let mut state = CallLimiterState::admitted("c#k".into(), 3, vec![e("z")]);
        let before = state.clone();
        let newer = held(9, vec![]);
        assert_eq!(state.apply_refresh("c#earlier", Some(&newer)), RefreshApplied::Stale);
        assert_eq!(state.apply_refresh("c#k", None), RefreshApplied::Unchanged);
        assert_eq!(state, before);
    }

    #[test]
    fn a_superseded_admit_restates_held_and_moves_the_numbers_on() {
        let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
        let superseded = AdmitOutcome::Superseded { held: held(4, vec![e("y")]) };
        state.apply_admit(&report(2, vec![e("z")], superseded));
        assert_eq!(state.held(), [e("y")], "what the limiter holds");
        assert_eq!(state.number_admit().0, 5);
    }

    /// A superseded change is not taken, like a refused one: the call runs on
    /// what the limiter holds, not open on a set nobody holds.
    #[test]
    fn a_superseded_change_is_not_taken() {
        let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
        let n = sent(state.replace_set(vec![e("x"), e("z")], false));
        let superseded = AdmitOutcome::Superseded { held: held(n, vec![e("y")]) };
        state.apply_admit(&report(n, vec![e("x"), e("z")], superseded));
        assert_eq!(state.target(), [e("y")]);
        assert!(!state.fail_open(), "no set nobody holds");
    }

    /// A refresh stated a newer set before a refusal's report arrives: the
    /// target is the held set, the newest statement, not the refusal's own.
    #[test]
    fn a_refusal_after_a_newer_statement_leaves_the_target_on_the_newest() {
        let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
        let n = sent(state.replace_set(vec![e("x"), e("y")], false));
        let newer = held(n + 3, vec![e("z")]);
        assert_eq!(state.apply_refresh("c#k", Some(&newer)), RefreshApplied::Changed);
        state.apply_admit(&report(n, vec![e("x"), e("y")], refused(n, "y", vec![e("x")])));
        assert_eq!((state.held(), state.target()), (&[e("z")][..], &[e("z")][..]));
        assert!(!state.fail_open());
    }

    #[test]
    fn a_release_fence_empties_held() {
        let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
        state.apply_admit(&report(2, vec![e("y")], AdmitOutcome::Released));
        assert!(!state.counted() && state.fail_open());
        assert_eq!(state.owed_release(), Some("c#k"));
    }

    #[test]
    fn fail_open_counts_multiplicity() {
        let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
        state.apply_admit(&report(2, vec![e("x"), e("x")], AdmitOutcome::Unavailable));
        assert!(state.fail_open(), "the second slot of x is not held");
    }

    #[test]
    fn a_new_epoch_numbers_above_every_number_of_the_previous_one() {
        let mut state = CallLimiterState::uncounted("c#k".into());
        state.owe_consult(4);
        state.enter_epoch();
        assert_eq!(state.number_admit().0, CHANGE_EPOCH + 1);
        state.enter_epoch();
        assert_eq!(state.number_admit().0, 2 * CHANGE_EPOCH + 1);
    }

    #[test]
    fn a_consult_owes_the_release_and_reserves_consecutive_numbers() {
        let mut state = CallLimiterState::uncounted("c#k".into());
        assert_eq!(state.owe_consult(6), 1);
        assert_eq!(state.owed_release(), Some("c#k"), "owed from the dispatching turn");
        assert_eq!(state.number_admit().0, 7);
    }

    #[test]
    fn an_initial_admit_s_number_owes_nothing_by_itself() {
        let mut state = CallLimiterState::uncounted("c#k".into());
        assert_eq!(state.number_admit(), (1, LimiterHeld::default()));
        assert_eq!(state.owed_release(), None);
    }

    #[test]
    fn a_change_is_the_target_from_its_sending_turn_and_a_refusal_takes_what_is_held() {
        let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
        let n = sent(state.replace_set(vec![e("x"), e("y")], false));
        assert_eq!(state.target(), [e("x"), e("y")], "the target while the change is in flight");
        state.apply_admit(&report(1, vec![e("x")], AdmitOutcome::Unavailable));
        assert_eq!(state.target(), [e("x"), e("y")], "an older report does not take it back");
        state.apply_admit(&report(n, vec![e("x"), e("y")], refused(n, "y", vec![e("x")])));
        assert_eq!(state.target(), [e("x")], "a refused change is not taken: what is held");
        assert!(!state.fail_open());
    }

    /// A consult's admit lands before a newer asked change is refused: the
    /// refusal states what the limiter holds, the consult's set, which is the
    /// target from then on; the consult's older report changes nothing.
    #[test]
    fn a_refused_change_takes_the_set_an_older_change_in_flight_made() {
        let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
        let consult = state.owe_consult(6);
        let n = sent(state.replace_set(vec![e("x"), e("y")], false));
        state.apply_admit(&report(n, vec![e("x"), e("y")], refused(n, "y", vec![e("z")])));
        state.apply_admit(&report(consult, vec![e("z")], AdmitOutcome::Admitted));
        assert_eq!((state.held(), state.target()), (&[e("z")][..], &[e("z")][..]));
        assert!(!state.fail_open(), "nothing in flight, nothing uncounted");
    }

    /// Two asked changes in flight both refused, the older first: the target
    /// is what the limiter holds, never the older refused set.
    #[test]
    fn two_refused_changes_leave_the_target_on_what_is_held() {
        let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
        let a = sent(state.replace_set(vec![e("x"), e("y")], false));
        let b = sent(state.replace_set(vec![e("x"), e("w")], false));
        state.apply_admit(&report(a, vec![e("x"), e("y")], refused(a, "y", vec![e("x")])));
        state.apply_admit(&report(b, vec![e("x"), e("w")], refused(b, "w", vec![e("x")])));
        assert_eq!(state.target(), [e("x")]);
    }

    /// A replacement asking for nothing, by a call that holds and owes
    /// nothing, sends no admit; once the call owes its release, or holds a
    /// set, every replacement leaves and carries the held set.
    #[test]
    fn a_replacement_leaves_unless_nothing_can_have_landed_and_nothing_is_asked() {
        let mut state = CallLimiterState::uncounted("c#k".into());
        assert_eq!(state.replace_set(vec![], true), Replacement::NothingToSend { change: 1 });
        assert_eq!(state.owed_release(), None);
        assert_eq!(state.runs_on(), Some(&[][..]), "moved onto nothing");
        let asked = state.replace_set(vec![e("x")], false);
        assert_eq!(asked, Replacement::Send { change: 2, held: LimiterHeld::default() });
        assert_eq!(state.owed_release(), Some("c#k"), "owed from the sending turn");
        assert_eq!(
            state.replace_set(vec![], false),
            Replacement::Send { change: 3, held: LimiterHeld::default() }
        );
        let mut counted = CallLimiterState::admitted("c#k".into(), 4, vec![e("x")]);
        assert_eq!(
            counted.replace_set(vec![], false),
            Replacement::Send { change: 5, held: held(4, vec![e("x")]) }
        );
    }

    /// Moved onto `[x, y]`, refused on `y`: counted on `[x]`, not fail open,
    /// yet running on `y` uncounted; admitted changes that still lack `y`
    /// leave it so, and the one that holds `y` ends it.
    #[test]
    fn a_call_moved_onto_a_refused_set_runs_uncounted_until_held_names_it() {
        let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
        let n = sent(state.replace_set(vec![e("x"), e("y")], true));
        assert!(state.runs_uncounted(), "in flight, y is not held");
        state.apply_admit(&report(n, vec![e("x"), e("y")], refused(n, "y", vec![e("x")])));
        assert_eq!(state.target(), [e("x")], "a refused change is not taken");
        assert!(state.counted() && !state.fail_open() && state.runs_uncounted());

        let a = state.owe_consult(1);
        state.apply_admit(&report(a, vec![e("x"), e("z")], AdmitOutcome::Admitted));
        assert!(state.runs_uncounted(), "an admitted change that does not hold y");
        let b = state.owe_consult(1);
        state.apply_admit(&report(b, vec![e("x")], AdmitOutcome::Admitted));
        assert!(state.runs_uncounted(), "an admitted return that does not hold y");
        let c = sent(state.replace_set(vec![e("z")], true));
        state.apply_admit(&report(c, vec![e("z")], AdmitOutcome::Admitted));
        assert!(!state.runs_uncounted(), "the call runs on z, which is held");
    }

    /// Moved onto `[x, y]` whose refusal lands after a newer change's
    /// statement: the refusal is not taken, and the call still runs on `y`.
    #[test]
    fn a_refusal_not_taken_still_leaves_the_moved_call_uncounted() {
        let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
        let moved = sent(state.replace_set(vec![e("x"), e("y")], true));
        let newer = state.owe_consult(1);
        state.apply_admit(&report(newer, vec![e("x"), e("z")], AdmitOutcome::Admitted));
        state.apply_admit(&report(moved, vec![e("x"), e("y")], refused(moved, "y", vec![e("x")])));
        assert_eq!(state.held(), [e("x"), e("z")], "the refusal's statement is older");
        assert!(state.runs_uncounted());
    }

    /// Moved onto `[x, y]` admitted with its answer lost: uncounted until a
    /// refresh states the set it holds, which ends it.
    #[test]
    fn a_lost_answer_of_a_moved_call_is_repaired_by_the_next_statement() {
        let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
        let n = sent(state.replace_set(vec![e("x"), e("y")], true));
        state.apply_admit(&report(n, vec![e("x"), e("y")], AdmitOutcome::Unavailable));
        assert!(state.runs_uncounted());
        let stated = held(n, vec![e("x"), e("y")]);
        assert_eq!(state.apply_refresh("c#k", Some(&stated)), RefreshApplied::Changed);
        assert!(!state.runs_uncounted());
    }

    /// A fold that runs its route moves the call onto the route's set
    /// whatever the outcome; a resolution after a refused route runs the call
    /// on its target again.
    #[test]
    fn a_resolution_after_a_refused_route_runs_the_call_on_its_target_again() {
        let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
        let failover = state.owe_consult(1);
        let route = vec![e("x"), e("y")];
        state.apply_fold(&report(failover, route.clone(), AdmitOutcome::Unavailable), true);
        assert_eq!(state.runs_on(), Some(&route[..]), "the call runs on its route");
        assert!(state.runs_uncounted());
        let release = state.owe_consult(1);
        state.apply_fold(&report(release, route, refused(release, "y", vec![e("x")])), false);
        assert_eq!(state.runs_on(), None);
        assert!(!state.runs_uncounted(), "the call runs on its target, which is held");
    }

    #[test]
    fn runs_on_counts_multiplicity() {
        let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
        let n = state.owe_consult(1);
        let superseded = AdmitOutcome::Superseded { held: held(n, vec![e("x")]) };
        state.apply_fold(&report(n, vec![e("x"), e("x")], superseded), true);
        assert!(!state.fail_open() && state.runs_uncounted(), "the second slot of x is not held");
    }

    #[test]
    fn a_lost_admit_reports_the_target_it_asked_for_while_no_later_change_replaced_it() {
        let mut state = CallLimiterState::admitted("c#k".into(), 1, vec![e("x")]);
        let n = sent(state.replace_set(vec![e("x"), e("y")], false));
        assert_eq!(state.lost_admit(n), AdmitReport::lost("c#k".into(), n, vec![e("x"), e("y")]));
        sent(state.replace_set(vec![e("z")], false));
        assert_eq!(state.lost_admit(n), AdmitReport::lost("c#k".into(), n, vec![]));
    }
}
