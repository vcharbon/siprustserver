//! The shared **observed multi-endpoint state** — the single source of truth
//! that replaces "who is blocked on which receive". Every reactor WRITES its
//! observations here; the controller's barriers READ predicates over it.
//!
//! Two properties make the N-reactor fold safe (see also [`super::ledger`]):
//! - **Monotone** — a leg phase only ever advances (`Absent → Early →
//!   Confirmed → Terminated`); an [`Observation`] that would downgrade is a
//!   no-op (`LegEarly` never demotes a `Confirmed` leg, the B2 invariant).
//! - **Idempotent & commutative** — re-applying a fact, or applying two facts
//!   in either order, yields the same state (phase folds by `max`; the ledger
//!   is grow-only). So a double-observation is harmless and the fold-order
//!   determinism gate holds by construction.
//!
//! The single wait primitive is [`await_pred`]: register on the tick BEFORE
//! reading the predicate (the B5 lost-wake fix), so a fact recorded in the
//! check→await gap is never lost.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;
use tokio::time::Instant;

use super::ledger::{ObligationKey, ObligationLedger};
use crate::StepError;

/// One endpoint leg's observed dialog lifecycle. Ordered so `max` gives the
/// monotone fold: a fact can advance the phase but never retreat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LegPhase {
    /// No dialog activity observed yet.
    Absent,
    /// An early dialog exists (our INVITE drew an 18x, or we answered a received
    /// INVITE and await the ACK).
    Early,
    /// The dialog is confirmed (our INVITE's 2xx was ACKed, or the peer ACKed
    /// our 2xx).
    Confirmed,
    /// The dialog reached a terminal state (BYE/200 observed, or a non-2xx final
    /// ended the establishing INVITE).
    Terminated,
}

/// A named sub-dialog's confirm progress — the a-realign / c-realign re-INVITEs
/// tracked as their own confirm points, so a `merged` barrier can be a
/// conjunction of two parallel sub-flows (the P1 refer payoff; defined here so
/// the state shape is stable across phases).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SubflowState {
    /// An offer was seen on this sub-flow.
    Offered,
    /// The sub-flow was answered (2xx sent/seen).
    Answered,
    /// Answered AND its ACK observed.
    Confirmed,
}

/// One leg's observed state — its phase plus any named sub-dialogs.
#[derive(Debug, Clone, Default)]
pub struct LegObservation {
    phase: Option<LegPhase>,
    subflows: HashMap<&'static str, SubflowState>,
}

impl LegObservation {
    /// The leg's phase, defaulting to `Absent` before any fact lands.
    pub fn phase(&self) -> LegPhase {
        self.phase.unwrap_or(LegPhase::Absent)
    }

    /// A named sub-flow's state, if it has been observed.
    pub fn subflow(&self, name: &str) -> Option<SubflowState> {
        self.subflows.get(name).copied()
    }

    /// Monotone phase advance — never retreats (the B2 no-downgrade invariant).
    fn advance(&mut self, to: LegPhase) {
        self.phase = Some(self.phase().max(to));
    }

    /// Monotone sub-flow advance.
    fn advance_subflow(&mut self, name: &'static str, to: SubflowState) {
        let entry = self.subflows.entry(name).or_insert(to);
        *entry = (*entry).max(to);
    }
}

/// The inner, lock-guarded state: per-leg observations + the acknowledgement
/// ledger. Barrier predicates read this via [`StateInner`]'s accessors.
pub struct StateInner {
    legs: HashMap<&'static str, LegObservation>,
    ledger: ObligationLedger,
}

impl StateInner {
    fn new() -> Self {
        Self { legs: HashMap::new(), ledger: ObligationLedger::default() }
    }

    /// A leg's observation (an `Absent` default view if it has never been seen).
    pub fn leg(&self, role: &str) -> LegObservation {
        self.legs.get(role).cloned().unwrap_or_default()
    }

    /// Whether a leg has reached (at least) a given phase.
    pub fn leg_at_least(&self, role: &str, phase: LegPhase) -> bool {
        self.leg(role).phase() >= phase
    }

    /// Whether EVERY leg that has appeared is `Terminated` — the `torn_down`
    /// predicate. Vacuously false before any leg appears (a call that never
    /// started has not "torn down").
    pub fn all_terminated(&self) -> bool {
        !self.legs.is_empty()
            && self.legs.values().all(|l| l.phase() == LegPhase::Terminated)
    }

    /// The ledger's verdict (every obligation acked, every dialog gap-free).
    pub fn ledger_closed(&self) -> bool {
        self.ledger.is_closed()
    }

    /// Human descriptions of the still-open obligations (the settle FAIL detail).
    pub fn describe_open(&self) -> Vec<String> {
        self.ledger.describe_open()
    }
}

/// A single fact a reactor folds into the observed state. Each variant is a
/// monotone, idempotent, commutative update (see the module doc), so a
/// double-observation is harmless and the fold is order-independent.
#[derive(Debug, Clone)]
pub enum Observation {
    /// An early dialog exists on `leg` — our INVITE drew an 18x, or we sent a
    /// 180 for a received INVITE. Never demotes a `Confirmed`/`Terminated` leg.
    LegEarly { leg: &'static str },
    /// `leg`'s dialog is confirmed — our INVITE's 2xx was ACKed, or the peer
    /// ACKed our 2xx.
    LegConfirmed { leg: &'static str },
    /// `leg` reached a terminal state — a BYE/200, a non-2xx final, a CANCEL.
    LegTerminated { leg: &'static str },
    /// We answered a received dialog-creating INVITE — seed the dialog's
    /// received-CSeq baseline with the INVITE's CSeq (so the first in-dialog
    /// request is not a phantom hole, §12.2.1.1).
    SeedDialog { leg: &'static str, call_id: String, cseq: u32 },
    /// We sent an in-dialog request awaiting its final — opens a ledger
    /// obligation.
    RequestSent { key: ObligationKey, detail: String },
    /// A final response to one of our sent in-dialog requests — closes the
    /// obligation.
    ResponseObserved { key: ObligationKey },
    /// An in-dialog request arrived on a dialog — folds into the CSeq gap
    /// detector (any method).
    InDialogRequest { leg: &'static str, call_id: String, cseq: u32 },
    /// A named sub-flow advanced (re-INVITE realign tracking; P1 realign use).
    Subflow { leg: &'static str, name: &'static str, to: SubflowState },
}

impl StateInner {
    /// Fold one observation. Monotone + idempotent + commutative.
    fn apply(&mut self, o: Observation, now: Instant) {
        match o {
            Observation::LegEarly { leg } => self.leg_mut(leg).advance(LegPhase::Early),
            Observation::LegConfirmed { leg } => self.leg_mut(leg).advance(LegPhase::Confirmed),
            Observation::LegTerminated { leg } => self.leg_mut(leg).advance(LegPhase::Terminated),
            Observation::SeedDialog { leg, call_id, cseq } => {
                self.leg_mut(leg).advance(LegPhase::Early);
                self.ledger.seed_dialog(call_id, leg, cseq);
            }
            Observation::RequestSent { key, detail } => self.ledger.open(key, now, detail),
            Observation::ResponseObserved { key } => self.ledger.close(key),
            Observation::InDialogRequest { leg, call_id, cseq } => {
                self.ledger.record_in_dialog(call_id, leg, cseq)
            }
            Observation::Subflow { leg, name, to } => self.leg_mut(leg).advance_subflow(name, to),
        }
    }

    fn leg_mut(&mut self, role: &'static str) -> &mut LegObservation {
        self.legs.entry(role).or_default()
    }
}

/// Shared, cheaply-cloned observed state. Every mutation fires `tick` so
/// barrier waiters re-check.
#[derive(Clone)]
pub struct ObservedState {
    inner: Arc<Mutex<StateInner>>,
    tick: Arc<Notify>,
}

impl ObservedState {
    pub fn new() -> Self {
        Self { inner: Arc::new(Mutex::new(StateInner::new())), tick: Arc::new(Notify::new()) }
    }

    /// Fold one reactor observation into the state and wake every barrier
    /// waiter. `now` is a `tokio::time::Instant` (rides the paused clock).
    pub fn record(&self, o: Observation, now: Instant) {
        self.inner.lock().unwrap().apply(o, now);
        self.tick.notify_waiters();
    }

    /// Run a predicate against a consistent snapshot of the state.
    pub fn with_snapshot<R>(&self, f: impl FnOnce(&StateInner) -> R) -> R {
        f(&self.inner.lock().unwrap())
    }

    /// Whether the ledger is closed (the settle barrier's poll).
    pub fn ledger_closed(&self) -> bool {
        self.inner.lock().unwrap().ledger_closed()
    }

    /// The still-open obligations (the settle FAIL detail).
    pub fn describe_open(&self) -> Vec<String> {
        self.inner.lock().unwrap().describe_open()
    }

    /// Whether every appeared leg is terminated (the `torn_down` predicate).
    pub fn all_terminated(&self) -> bool {
        self.inner.lock().unwrap().all_terminated()
    }
}

impl Default for ObservedState {
    fn default() -> Self {
        Self::new()
    }
}

/// Await a predicate over the observed state, re-checked on every `tick`,
/// bounded by `deadline`. `who` names the barrier for the timeout `StepError`
/// (a bounded label the case-keyer accepts — never free-form gap text).
///
/// **B5 lost-wake fix**: `Notify::notified()` only registers the waiter when it
/// is first polled, not at creation, so a naive `if !pred { notified().await }`
/// loses a `notify_waiters` fired in the check→await gap. We pin the future and
/// `enable()` it — registering NOW — *before* reading the predicate, so no tick
/// between the read and the park is missed. (Under tokio auto-advance a missed
/// tick would silently leap the clock to `deadline` → a spurious timeout.)
pub async fn await_pred<F>(
    obs: &ObservedState,
    who: &'static str,
    pred: F,
    deadline: Instant,
) -> Result<(), StepError>
where
    F: Fn(&StateInner) -> bool,
{
    loop {
        let notified = obs.tick.notified();
        tokio::pin!(notified);
        notified.as_mut().enable(); // register BEFORE the predicate read
        if obs.with_snapshot(&pred) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(StepError::Timeout { who: who.to_string() });
        }
        tokio::select! {
            _ = &mut notified => {}
            _ = tokio::time::sleep_until(deadline) => {}
        }
    }
}
