//! The shared **observed multi-endpoint state** — the single source of truth
//! that replaces "who is blocked on which receive". Every reactor WRITES its
//! observations here; the controller's barriers READ predicates over it.
//!
//! Two properties make the N-reactor fold safe (see also [`super::ledger`]):
//! - **Monotone** — a leg phase only ever advances (`Absent → Early →
//!   Confirmed → Terminated`); an [`Observation`] that would downgrade is a
//!   no-op (`LegEarly` never demotes a `Confirmed` leg).
//! - **Idempotent & commutative** — re-applying a fact, or applying two facts
//!   in either order, yields the same state (phase folds by `max`; the ledger
//!   is grow-only). So a double-observation is harmless and the fold-order
//!   determinism gate holds by construction.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;
use tokio::time::Instant;

use super::ledger::ObligationLedger;

pub use super::facts::{RequestFact, ResponseFact};
pub use super::observation::Observation;
pub use super::replay_record::{RecordedFinal, ReplayEntry};
use crate::realcall::LostMessage;

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
/// conjunction of two parallel sub-flows (defined here so
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
    /// `(status, reason)` — a grow-only set (commutative fold), read by the
    /// accepted-ending verdict ([`super::Ending`]). Almost always 0 or 1
    /// entries.
    finals: std::collections::BTreeSet<(u16, String)>,
    /// The RFC method names of the inbound in-dialog requests this leg has
    /// received (`INFO`, `MESSAGE`, `NOTIFY`, …) — a grow-only set (commutative
    /// fold), read by [`Barrier::received`](super::goals::Barrier::received) so an
    /// origination can gate on an *observed* inbound request rather than a timed
    /// dwell (the MRF `INFO(EOF)` that must follow the worker's `INFO(play)`).
    received_methods: std::collections::BTreeSet<String>,
    /// The in-dialog requests received, a retransmission once (`(Call-ID, CSeq, method)`).
    requests: std::collections::BTreeMap<(String, u32, String), RequestFact>,
    /// The renegotiations (re-INVITEs, UPDATEs) this leg ORIGINATED and
    /// COMPLETED, as `((local tag, remote tag), CSeq)` — a grow-only set (commutative
    /// fold) whose *cardinality* serializes an N-cycle re-INVITE script: cycle
    /// `i` (0-based) waits until `reneg_count() >= i`, so no two re-INVITEs are
    /// ever in flight (which would glare into a 491). The tags keep apart the
    /// equal CSeqs of distinct dialogs (an early fork's and the confirmed
    /// one's). Distinct from the monotone SUBFLOW_RENEG latch, which cannot
    /// count.
    reneg_cseqs: std::collections::BTreeSet<((String, String), u32)>,
    /// Whether this leg sent a CANCEL for its establishing INVITE.
    cancel_sent: bool,
    /// Every inbound response this leg's reactor observed, in arrival order — an
    /// append-only per-leg log with ONE writer (the leg's own reactor), so its
    /// order is deterministic; reception goals consume it through a per-actor
    /// cursor.
    responses: Vec<ResponseFact>,
    /// The datagrams to or from this leg the call's loss model dropped,
    /// folded in once the call ran ([`Observation::Lost`]).
    lost: Vec<LostMessage>,
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

    /// The lowest non-2xx final this leg's establishing INVITE drew, with its
    /// reason phrase.
    pub fn first_final(&self) -> Option<(u16, &str)> {
        self.finals.iter().next().map(|(s, r)| (*s, r.as_str()))
    }

    /// Whether this leg sent a CANCEL for its establishing INVITE.
    pub fn cancel_sent(&self) -> bool {
        self.cancel_sent
    }

    /// Whether this leg received a provisional above 100 to an INVITE — the
    /// callee is ringing (or playing early media). Unlike [`LegPhase::Early`],
    /// which a caller reaches the moment she originates, it needs the response.
    pub fn rang(&self) -> bool {
        self.responses.iter().any(|f| (101..200).contains(&f.status) && f.cseq_method == "INVITE")
    }

    /// Whether this leg's INVITE drew a provisional, `100 Trying` included:
    /// what RFC 3261 §9.1 asks of a caller before she CANCELs.
    pub fn heard_provisional(&self) -> bool {
        self.responses.iter().any(|f| (100..200).contains(&f.status) && f.cseq_method == "INVITE")
    }

    /// Whether this leg received a provisional carrying SDP — early media
    /// (RFC 3261 §13.2.1, RFC 3262).
    pub fn saw_early_media(&self) -> bool {
        self.responses.iter().any(|f| (101..200).contains(&f.status) && f.body_is_sdp)
    }

    /// Whether this leg has received an inbound in-dialog request of `method`
    /// (the RFC name, e.g. `"INFO"`) — the observed fact `Barrier::received`
    /// gates an origination on.
    pub fn received_method(&self, method: &str) -> bool {
        self.received_methods.contains(method)
    }

    /// How many renegotiations (re-INVITEs, UPDATEs) this leg originated and
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

    /// The datagrams to or from this leg the call's loss model dropped.
    pub fn lost(&self) -> &[LostMessage] {
        &self.lost
    }

    /// Whether the loss model dropped a 2xx to this leg's INVITE on its way
    /// to the leg: the sender had answered before any later copy arrived.
    pub fn lost_answer(&self) -> bool {
        self.lost.iter().any(|m| m.inbound && m.is_invite_response_in(200..300))
    }

    /// Whether the loss model dropped a provisional with a session
    /// description to this leg's INVITE on its way to the leg: early media
    /// the leg was sent and never received.
    pub fn lost_early_media(&self) -> bool {
        self.lost.iter().any(|m| m.inbound && m.is_invite_response_in(101..200) && m.sdp)
    }

    /// Record an observed inbound in-dialog method name (idempotent — a set, so
    /// the fold stays commutative).
    fn record_method(&mut self, method: String) {
        self.received_methods.insert(method);
    }

    /// Monotone phase advance — never retreats (the no-downgrade invariant).
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
    /// The caller's establishing INVITE ended on a final the call accepts
    /// ([`Observation::AcceptedEnding`]): the remaining phase barriers are moot.
    accepted_ending: bool,
    /// The system under test ended the caller cleanly ([`Observation::EndedCleanly`]).
    caller_ended_cleanly: bool,
    /// A party the system under test ended cleanly dropped a goal.
    goal_dropped: bool,
    /// The replay record: observed-vs-expected finals + reactor-serviced strays
    /// (append-only, diagnostic — never gates the verdict).
    replay: Vec<ReplayEntry>,
}

impl StateInner {
    fn new() -> Self {
        Self {
            legs: HashMap::new(),
            ledger: ObligationLedger::default(),
            accepted_ending: false,
            caller_ended_cleanly: false,
            goal_dropped: false,
            replay: Vec::new(),
        }
    }

    /// A leg's observation (an `Absent` default view if it has never been seen).
    pub fn leg(&self, role: &str) -> LegObservation {
        self.legs.get(role).cloned().unwrap_or_default()
    }

    /// A leg's observation by reference (`None` before the leg appears) — the
    /// allocation-free read the call's verdict takes.
    pub fn leg_ref(&self, role: &str) -> Option<&LegObservation> {
        self.legs.get(role)
    }

    /// Whether `role` received a provisional above 100 to an INVITE
    /// ([`LegObservation::rang`]); borrows, so a barrier poll clones nothing.
    pub fn leg_rang(&self, role: &str) -> bool {
        self.legs.get(role).is_some_and(LegObservation::rang)
    }

    /// Whether `role` received a provisional above 100 to an INVITE, or the
    /// call's loss model dropped one on any leg, so her ringing may never
    /// come: the gate of a step timed from the caller's ringing.
    pub fn rang_or_ring_lost(&self, role: &str) -> bool {
        self.leg_rang(role)
            || self.legs.values().flat_map(|l| &l.lost).any(|m| m.is_invite_response_in(101..200))
    }

    /// Whether the caller's establishing INVITE ended on a final the call
    /// accepts — every remaining phase barrier is then moot.
    pub fn accepted_ending(&self) -> bool {
        self.accepted_ending
    }

    /// Whether the system under test ended the caller cleanly while the call
    /// accepts clean ends: every remaining phase barrier is then moot.
    pub fn caller_ended_cleanly(&self) -> bool {
        self.caller_ended_cleanly
    }

    /// Whether a party the system under test ended cleanly, the caller or
    /// another, dropped a goal the ending did not itself meet
    /// ([`Observation::GoalDropped`]).
    pub fn goal_dropped(&self) -> bool {
        self.goal_dropped
    }

    /// Whether `role` holds no live dialog: none ever (no early or confirmed
    /// dialog observed) or every one ended.
    pub fn leg_idle_or_ended(&self, role: &str) -> bool {
        matches!(self.leg(role).phase(), LegPhase::Absent | LegPhase::Terminated)
    }

    /// Every leg observed, by role, with its phase.
    pub fn leg_phases(&self) -> impl Iterator<Item = (&'static str, LegPhase)> + '_ {
        self.legs.iter().map(|(role, l)| (*role, l.phase()))
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

    /// Whether `role` received an in-dialog request `pred` holds for — a barrier
    /// on what a request said, not only on its method (borrows).
    pub fn leg_received_where(&self, role: &str, pred: impl Fn(&RequestFact) -> bool) -> bool {
        self.legs.get(role).is_some_and(|l| l.requests.values().any(pred))
    }

    /// Whether EVERY leg that has appeared is `Terminated` — the `torn_down`
    /// predicate. Vacuously false before any leg appears (a call that never
    /// started has not "torn down").
    pub fn all_terminated(&self) -> bool {
        !self.legs.is_empty() && self.legs.values().all(|l| l.phase() == LegPhase::Terminated)
    }

    /// Whether a leg has an unconsumed response fact at or beyond `from` —
    /// `need_final` restricts to finals (>= 200); otherwise any non-100 fact
    /// counts (100 is transaction plumbing). Borrow-based: the goal-arm gate
    /// polls this every loop iteration.
    pub fn leg_response_ready(
        &self,
        role: &str,
        from: usize,
        need_final: bool,
        cseq_method: Option<&str>,
    ) -> bool {
        self.legs.get(role).is_some_and(|l| {
            l.responses.get(from..).unwrap_or(&[]).iter().any(|f| {
                if cseq_method.is_some_and(|m| !f.cseq_method.eq_ignore_ascii_case(m)) {
                    // Another transaction's response never readies a pinned
                    // expectation, or the goal fires with nothing to consume.
                    return false;
                }
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

impl StateInner {
    /// Fold one observation. Monotone + idempotent + commutative.
    fn apply(&mut self, o: Observation, now: Instant) {
        match o {
            Observation::LegEarly { leg } => self.leg_mut(leg).advance(LegPhase::Early),
            Observation::LegConfirmed { leg } => self.leg_mut(leg).advance(LegPhase::Confirmed),
            Observation::LegTerminated { leg } => self.leg_mut(leg).advance(LegPhase::Terminated),
            Observation::DialogTornDown { leg } => self.ledger.discharge_leg(leg),
            Observation::SeedDialog { stream, cseq } => {
                self.leg_mut(stream.leg).advance(LegPhase::Early);
                self.ledger.record_in_dialog(stream, cseq);
            }
            Observation::RequestSent { key, detail } => self.ledger.open(key, now, detail),
            Observation::ResponseObserved { key } => self.ledger.close(key),
            Observation::InDialogRequest { stream, cseq, method } => {
                self.leg_mut(stream.leg).record_method(method);
                self.ledger.record_in_dialog(stream, cseq);
            }
            Observation::RefusedInDialog { stream, cseq } => {
                self.ledger.record_in_dialog(stream, cseq)
            }
            Observation::LegRequest { leg, call_id, cseq, fact } => {
                self.leg_mut(leg)
                    .requests
                    .entry((call_id, cseq, fact.method.clone()))
                    .or_insert(fact);
            }
            Observation::LegFinal { leg, status, reason } => {
                self.leg_mut(leg).finals.insert((status, reason));
            }
            Observation::AcceptedEnding => self.accepted_ending = true,
            Observation::EndedCleanly { caller } => self.caller_ended_cleanly |= caller,
            Observation::GoalDropped => self.goal_dropped = true,
            Observation::CancelSent { leg } => self.leg_mut(leg).cancel_sent = true,
            Observation::Subflow { leg, name, to } => self.leg_mut(leg).advance_subflow(name, to),
            Observation::RenegCompleted { leg, dialog, cseq } => {
                self.leg_mut(leg).reneg_cseqs.insert((dialog, cseq));
            }
            Observation::LegResponse { leg, fact } => {
                self.leg_mut(leg).responses.push(fact);
            }
            Observation::Lost { leg, message } => self.leg_mut(leg).lost.push(message),
            Observation::ReplayFinal { key, expected, observed } => {
                self.replay.push(ReplayEntry::Final(RecordedFinal { key, expected, observed }));
            }
            Observation::ServicedStray { leg, method, action } => {
                self.replay.push(ReplayEntry::ServicedStray { leg, method, action });
            }
            Observation::AcceptedDelta { leg, step, expected, observed, rule } => {
                self.replay.push(ReplayEntry::AcceptedDelta {
                    leg,
                    step,
                    expected,
                    observed,
                    rule,
                });
            }
            Observation::UnclaimedInbound { endpoint, detail } => {
                self.replay.push(ReplayEntry::UnclaimedInbound { endpoint, detail });
            }
            Observation::HeldFinalRetransmitted { leg, status, cseq_method, cseq } => {
                self.replay.push(ReplayEntry::HeldFinalRetransmitted {
                    leg,
                    status,
                    cseq_method,
                    cseq,
                });
            }
            Observation::BodyExpectMiss {
                leg,
                step,
                expected,
                body_len,
                body_is_sdp,
                status,
                method,
                initial,
            } => {
                self.replay.push(ReplayEntry::BodyExpectMiss {
                    leg,
                    step,
                    expected,
                    body_len,
                    body_is_sdp,
                    status,
                    method,
                    initial,
                });
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
    /// Fired on every mutation: what [`await_pred`](super::barrier::await_pred)
    /// parks on.
    pub(super) tick: Arc<Notify>,
    /// The call's scheduled dwells, which every wait on it is bounded past.
    pub(super) pacing: super::pacing::Pacing,
}

impl ObservedState {
    pub fn new() -> Self {
        let pacing = super::pacing::Pacing::default();
        Self { inner: Arc::new(Mutex::new(StateInner::new())), tick: pacing.tick(), pacing }
    }

    /// Fold one reactor observation into the state and wake every barrier
    /// waiter. `now` is a `tokio::time::Instant` (rides the paused clock).
    pub fn record(&self, o: Observation, now: Instant) {
        self.pacing.observe(&o);
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

    /// The run's accepted deltas (ADR-0024 §6) — the replay record filtered to
    /// its [`ReplayEntry::AcceptedDelta`] entries, the list a report consumer
    /// classifies on (e.g. an `ok-with-accepted-delta` bucket).
    pub fn accepted_deltas(&self) -> Vec<ReplayEntry> {
        self.inner
            .lock()
            .unwrap()
            .replay
            .iter()
            .filter(|e| matches!(e, ReplayEntry::AcceptedDelta { .. }))
            .cloned()
            .collect()
    }

    /// The run's undelivered inbounds on shared endpoints — the replay record
    /// filtered to its [`ReplayEntry::UnclaimedInbound`] entries, the list a
    /// report consumer counts (an unclaimed leg is a scenario wiring defect).
    pub fn unclaimed_inbound(&self) -> Vec<ReplayEntry> {
        self.inner
            .lock()
            .unwrap()
            .replay
            .iter()
            .filter(|e| matches!(e, ReplayEntry::UnclaimedInbound { .. }))
            .cloned()
            .collect()
    }
}

impl Default for ObservedState {
    fn default() -> Self {
        Self::new()
    }
}
