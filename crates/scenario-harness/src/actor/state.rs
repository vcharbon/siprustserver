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

/// One observed inbound response on a leg — status, body presence, and the
/// fork identity (`To`-tag) it carried. `typed` retains the full response only
/// while a matcher-carrying reception goal is pending on the leg, so a content
/// matcher can compare headers+body without a second receive.
#[derive(Debug, Clone)]
pub struct ResponseFact {
    pub status: u16,
    pub reason: String,
    pub body_len: usize,
    pub body_is_sdp: bool,
    /// The `To`-tag — the early-dialog/fork identity a reception goal's
    /// `early` binding matches (RFC 3261 §12.1.2).
    pub early_tag: Option<String>,
    /// Boxed: retained only for a pending matcher, and kept off the
    /// `Observation` fold's common-variant size.
    pub typed: Option<Box<sip_message::SipResponse>>,
}

/// One `ObserveFinal`/`ExpectFinal` outcome: the capture-declared expectation
/// beside what actually arrived. Divergence is data, never a verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedFinal {
    pub key: u32,
    pub expected: Option<u16>,
    pub observed: u16,
}

/// One entry of the replay record — an observed final beside its expectation,
/// or a request the reactor (not the script) serviced on a `Scripted` actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayEntry {
    Final(RecordedFinal),
    ServicedStray { leg: &'static str, method: String, action: &'static str },
}

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
    /// Every non-2xx final observed on this leg's establishing INVITE, as
    /// `(status, reason)` — a grow-only set (commutative fold), read by
    /// `Expect::Reject`'s verdict mapping. Almost always 0 or 1 entries.
    finals: std::collections::BTreeSet<(u16, String)>,
    /// The RFC method names of the inbound in-dialog requests this leg has
    /// received (`INFO`, `MESSAGE`, `NOTIFY`, …) — a grow-only set (commutative
    /// fold), read by [`Barrier::received`](super::goals::Barrier::received) so an
    /// origination can gate on an *observed* inbound request rather than a timed
    /// dwell (the MRF `INFO(EOF)` that must follow the worker's `INFO(play)`).
    received_methods: std::collections::BTreeSet<String>,
    /// The CSeq numbers of caller-originated renegotiations (re-INVITEs) this
    /// leg has COMPLETED (2xx received AND ACKed) — a grow-only set (commutative
    /// fold) whose *cardinality* serializes an N-cycle re-INVITE script: cycle
    /// `i` (0-based) waits until `reneg_count() >= i`, so no two re-INVITEs are
    /// ever in flight (which would glare into a 491). Distinct from the monotone
    /// SUBFLOW_RENEG latch, which cannot count.
    reneg_cseqs: std::collections::BTreeSet<u32>,
    /// Every inbound response this leg's reactor observed, in arrival order — an
    /// append-only per-leg log with ONE writer (the leg's own reactor), so its
    /// order is deterministic; reception goals consume it through a per-actor
    /// cursor.
    responses: Vec<ResponseFact>,
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

    /// Whether this leg's establishing INVITE drew the given non-2xx final.
    pub fn saw_final(&self, status: u16) -> bool {
        self.finals.iter().any(|(s, _)| *s == status)
    }

    /// The reason phrase of the given observed non-2xx final, if any.
    pub fn final_reason(&self, status: u16) -> Option<String> {
        self.finals.iter().find(|(s, _)| *s == status).map(|(_, r)| r.clone())
    }

    /// Whether this leg has received an inbound in-dialog request of `method`
    /// (the RFC name, e.g. `"INFO"`) — the observed fact `Barrier::received`
    /// gates an origination on.
    pub fn received_method(&self, method: &str) -> bool {
        self.received_methods.contains(method)
    }

    /// How many caller-originated renegotiation cycles (re-INVITEs) this leg has
    /// COMPLETED — the count an N-cycle re-INVITE script's per-cycle barrier
    /// compares against to serialize the chain.
    pub fn reneg_count(&self) -> u32 {
        self.reneg_cseqs.len() as u32
    }

    /// Every response this leg observed, in arrival order.
    pub fn responses(&self) -> &[ResponseFact] {
        &self.responses
    }

    /// Whether this leg has observed a response of the given status.
    pub fn saw_status(&self, status: u16) -> bool {
        self.responses.iter().any(|f| f.status == status)
    }

    /// Record an observed inbound in-dialog method name (idempotent — a set, so
    /// the fold stays commutative).
    fn record_method(&mut self, method: String) {
        self.received_methods.insert(method);
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
    /// The replay record: observed-vs-expected finals + reactor-serviced strays
    /// (append-only, diagnostic — never gates the verdict).
    replay: Vec<ReplayEntry>,
}

impl StateInner {
    fn new() -> Self {
        Self { legs: HashMap::new(), ledger: ObligationLedger::default(), replay: Vec::new() }
    }

    /// A leg's observation (an `Absent` default view if it has never been seen).
    pub fn leg(&self, role: &str) -> LegObservation {
        self.legs.get(role).cloned().unwrap_or_default()
    }

    /// Whether a leg has reached (at least) a given phase.
    pub fn leg_at_least(&self, role: &str, phase: LegPhase) -> bool {
        self.leg(role).phase() >= phase
    }

    /// Whether `leg` has received an inbound in-dialog request of `method` — the
    /// observed-fact predicate [`Barrier::received`](super::goals::Barrier::received)
    /// gates on (borrows, so a poll never clones the leg's method set).
    pub fn leg_received_method(&self, role: &str, method: &str) -> bool {
        self.legs.get(role).is_some_and(|l| l.received_method(method))
    }

    /// Whether EVERY leg that has appeared is `Terminated` — the `torn_down`
    /// predicate. Vacuously false before any leg appears (a call that never
    /// started has not "torn down").
    pub fn all_terminated(&self) -> bool {
        !self.legs.is_empty()
            && self.legs.values().all(|l| l.phase() == LegPhase::Terminated)
    }

    /// Whether a leg has an unconsumed response fact at or beyond `from` —
    /// `need_final` restricts to finals (>= 200); otherwise any non-100 fact
    /// counts (100 is transaction plumbing). Borrow-based: the goal-arm gate
    /// polls this every loop iteration.
    pub fn leg_response_ready(&self, role: &str, from: usize, need_final: bool) -> bool {
        self.legs.get(role).is_some_and(|l| {
            l.responses.get(from..).unwrap_or(&[]).iter().any(|f| {
                if need_final {
                    f.status >= 200
                } else {
                    f.status != 100
                }
            })
        })
    }

    /// The replay record (observed-vs-expected finals + serviced strays).
    pub fn replay_record(&self) -> &[ReplayEntry] {
        &self.replay
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
    /// `leg`'s DIALOG is being torn down (a BYE in either direction) — discharge
    /// its still-open in-dialog acknowledgement obligations (a pending re-INVITE
    /// ACK / PRACK-200 / … whose peer transaction dies with the call can never
    /// arrive), so the settle barrier does not hold the verdict its 32 s ceiling
    /// for an impossible ack. See [`ObligationLedger::discharge_leg`].
    DialogTornDown { leg: &'static str },
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
    /// detector (any method) AND into the receiving leg's received-method set
    /// (so an origination can gate on "leg X received an inbound `<method>`").
    InDialogRequest { leg: &'static str, call_id: String, cseq: u32, method: String },
    /// `leg`'s establishing INVITE drew a non-2xx final (the reject path) —
    /// grow-only, read by the `Expect::Reject` verdict mapping.
    LegFinal { leg: &'static str, status: u16, reason: String },
    /// A named sub-flow advanced (re-INVITE realign tracking; P1 realign use).
    Subflow { leg: &'static str, name: &'static str, to: SubflowState },
    /// A caller-originated renegotiation (re-INVITE) this leg ORIGINATED
    /// completed — its 2xx was received and ACKed. Folds into the leg's
    /// grow-only completed-reneg set (keyed by CSeq, so a re-emitted 2xx cannot
    /// double-count), whose cardinality serializes an N-cycle re-INVITE script.
    RenegCompleted { leg: &'static str, cseq: u32 },
    /// An inbound response observed on `leg` — appended to the leg's ordered
    /// response log (one writer: the leg's own reactor), which reception goals
    /// consume through a per-actor cursor.
    LegResponse { leg: &'static str, fact: ResponseFact },
    /// An `ObserveFinal`/`ExpectFinal` outcome — appended to the replay record.
    ReplayFinal { key: u32, expected: Option<u16>, observed: u16 },
    /// A request the reactor (not the script) serviced on a `Scripted` actor —
    /// appended to the replay record so divergence is never silent.
    ServicedStray { leg: &'static str, method: String, action: &'static str },
}

impl StateInner {
    /// Fold one observation. Monotone + idempotent + commutative.
    fn apply(&mut self, o: Observation, now: Instant) {
        match o {
            Observation::LegEarly { leg } => self.leg_mut(leg).advance(LegPhase::Early),
            Observation::LegConfirmed { leg } => self.leg_mut(leg).advance(LegPhase::Confirmed),
            Observation::LegTerminated { leg } => self.leg_mut(leg).advance(LegPhase::Terminated),
            Observation::DialogTornDown { leg } => self.ledger.discharge_leg(leg),
            Observation::SeedDialog { leg, call_id, cseq } => {
                self.leg_mut(leg).advance(LegPhase::Early);
                self.ledger.seed_dialog(call_id, leg, cseq);
            }
            Observation::RequestSent { key, detail } => self.ledger.open(key, now, detail),
            Observation::ResponseObserved { key } => self.ledger.close(key),
            Observation::InDialogRequest { leg, call_id, cseq, method } => {
                self.ledger.record_in_dialog(call_id, leg, cseq);
                self.leg_mut(leg).record_method(method);
            }
            Observation::LegFinal { leg, status, reason } => {
                self.leg_mut(leg).finals.insert((status, reason));
            }
            Observation::Subflow { leg, name, to } => self.leg_mut(leg).advance_subflow(name, to),
            Observation::RenegCompleted { leg, cseq } => {
                self.leg_mut(leg).reneg_cseqs.insert(cseq);
            }
            Observation::LegResponse { leg, fact } => {
                self.leg_mut(leg).responses.push(fact);
            }
            Observation::ReplayFinal { key, expected, observed } => {
                self.replay.push(ReplayEntry::Final(RecordedFinal { key, expected, observed }));
            }
            Observation::ServicedStray { leg, method, action } => {
                self.replay.push(ReplayEntry::ServicedStray { leg, method, action });
            }
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

    /// The replay record after a run — observed-vs-expected finals plus the
    /// requests the reactor serviced on a `Scripted` actor.
    pub fn replay_record(&self) -> Vec<ReplayEntry> {
        self.inner.lock().unwrap().replay.clone()
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
