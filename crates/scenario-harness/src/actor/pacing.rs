//! The call's deliberate dwells.
//!
//! A party that schedules a dwell (a ring before its answer, a talk before its
//! BYE, a keepalive cadence, a held ACK, a retry back-off) states when it will
//! next act. Every controller barrier and goal-guard wait is bounded from the
//! later of its own start and the end of the last dwell still owed
//! ([`super::barrier::await_paced`]): a dwell of any length is never charged to
//! the bound, while a message that never comes still fails the call one bound
//! after the act that should have drawn it. A dwell its party will no longer
//! finish ends where it was withdrawn: a goal's when the goal is abandoned,
//! every one of a party's when its leg ends.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

use super::runner::ActorState;
use super::state::Observation;

/// What a party waits out by its own choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Dwell {
    /// A goal's post-guard delay (a talk before the BYE, a gap before a
    /// re-INVITE).
    Goal,
    /// A callee's ring before its timed answer.
    Ring,
    /// A keepalive hold's cadence.
    Keepalive,
    /// A declared ACK-to-2xx hold.
    HeldAck,
    /// A request's retry back-off (491 glare, 500 with `Retry-After`).
    Retry,
}

/// The end of each dwell a party of the call still owes, by role and kind.
/// Every change ticks the observed state's waiters, which read their deadline
/// again.
#[derive(Clone, Default)]
pub struct Pacing {
    owed: Arc<Mutex<HashMap<(&'static str, Dwell), Instant>>>,
    tick: Arc<Notify>,
}

impl Pacing {
    /// The notifier the observed state ticks its waiters with.
    pub(super) fn tick(&self) -> Arc<Notify> {
        self.tick.clone()
    }

    /// Withdraw what an observation ends: every dwell of a leg that ended.
    pub(super) fn observe(&self, o: &Observation) {
        if let Observation::LegTerminated { leg } = o {
            self.withdraw_all(leg);
        }
    }

    /// Party `role` will next act at `at`, by its own choice.
    pub fn dwell_until(&self, role: &'static str, dwell: Dwell, at: Instant) {
        {
            let mut owed = self.owed.lock().unwrap();
            let end = owed.entry((role, dwell)).or_insert(at);
            *end = (*end).max(at);
        }
        self.tick.notify_waiters();
    }

    /// Party `role` will not finish its `dwell`: it ends now, the time
    /// already dwelt still uncharged.
    pub fn withdraw(&self, role: &str, dwell: Dwell) {
        self.cut(|r, d| r == role && d == dwell);
    }

    /// Party `role` will finish none of its dwells: its leg ended.
    pub fn withdraw_all(&self, role: &str) {
        self.cut(|r, _| r == role);
    }

    /// End every dwell `which` selects now, if it ends later.
    fn cut(&self, which: impl Fn(&str, Dwell) -> bool) {
        let now = Instant::now();
        for ((role, dwell), end) in self.owed.lock().unwrap().iter_mut() {
            if which(role, *dwell) {
                *end = (*end).min(now);
            }
        }
        self.tick.notify_waiters();
    }

    /// When the latest dwell still owed ends, if any is.
    pub fn until(&self) -> Option<Instant> {
        self.owed.lock().unwrap().values().max().copied()
    }

    /// The deadline of a wait started at `from` and bounded by `bound`.
    pub(super) fn deadline(&self, from: Instant, bound: Duration) -> Instant {
        self.until().map_or(from, |u| u.max(from)) + bound
    }
}

impl ActorState<'_> {
    /// This party will next act `length` from now, by its own choice: when.
    pub(super) fn dwell(&self, dwell: Dwell, length: Duration) -> Instant {
        let at = Instant::now() + length;
        self.obs.pacing.dwell_until(self.role, dwell, at);
        at
    }
}
