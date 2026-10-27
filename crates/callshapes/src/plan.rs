//! The **pipeline algebra** — a [`ShapePlan`] is a chain of stages that
//! compiles to one `ActorCall` (spec §5 of the callshapes program).
//!
//! Compilation folds the chain left-to-right, threading the **gate**: the
//! barrier that says "the current dialog is quiescent enough for the next
//! deliberate action". An [`Establishment`] mints the first gate
//! (`established`); each [`Script`] appends caller goals guarded on the
//! incoming gate and (for renegotiations) replaces it; a [`Transfer`] moves
//! the current dialog to the transfer target and replaces the gate with the
//! post-transfer condition (`merged` / `declined`). The [`Teardown`] guards
//! the final BYE on whatever gate the chain ends with — so every shape
//! terminates its call, per the repo-wide test requirement.
//!
//! Barrier and phase names are a FIXED vocabulary (`established`, `reinvited`,
//! `updated`, `merged`, …): they key bounded `StepError::who` labels and the
//! downstream contract's phase trail, and must never be free-form.

use std::sync::Arc;
use std::time::Duration;

pub use scenario_harness::actor::ForkWinner;
use scenario_harness::actor::{
    phase, ActorCall, ActorScenario, ActorSpec, Barrier, BarrierPhase, Branch, CtxFeed,
    Disposition, Ending, Expect, Feed, Goal, GoalStep, LegPhase, MediaState, SettleBarrier,
    StateInner, SubflowState, SUBFLOW_EARLY, SUBFLOW_REALIGN, SUBFLOW_REFER,
};
use scenario_harness::realcall::{CallEnv, ScenarioId};
use scenario_harness::{StepError, ANSWER_SDP, OFFER_SDP};
use sip_message::generators::InDialogMethod;

use crate::binder::{RouteBinder, RouteIntent};

/// How the call is established — the first pipeline stage, which mints the
/// initial dialog (or a terminal outcome for the deliberately-failing shapes).
#[derive(Debug, Clone, Copy)]
pub enum Establishment {
    /// Plain INVITE/18x/200/ACK to the primary callee (`bob`).
    Transparent,
    /// RFC 3262 reliable establishment: the caller advertises `100rel`, the
    /// callee answers reliably (183+RSeq → PRACK → 200 → ACK).
    Reliable,
    /// The primary callee rejects with `reject` (no 18x) and the SUT fails
    /// over to `bob2`, which answers reliably (`winner_reliable`) or plainly.
    RerouteOnReject { reject: u16, winner_reliable: bool },
    /// NO-ANSWER-triggered failover: the primary callee rings
    /// (`180`) then NEVER answers ([`Disposition::RingThenSilent`]); the SUT's
    /// own no-answer timer fires, CANCELs the primary (whose `487` settles the
    /// leg cleanly) and fails over to `bob2`, which answers reliably
    /// (`winner_reliable`) or plainly. Routing rides the SAME
    /// [`RouteIntent::FailoverOnReject`](crate::binder::RouteIntent) seam as
    /// [`RerouteOnReject`](Self::RerouteOnReject), so a downstream binder maps
    /// it unchanged; `no_answer_ms` is the SUT ring-timer knob a binder MAY
    /// arm client-side ([`RouteBinder::invite_plan_no_answer`]) — a platform
    /// whose SUT owns its own ring timer ignores it. The primary's dwell is
    /// bounded by the SUT timer, never by the callee.
    RerouteOnNoAnswer { no_answer_ms: u32, winner_reliable: bool },
    /// TRUE forking (RFC 3261 §12.1.2): the callee `bob` emits one 18x per
    /// DISTINCT explicit To-tag in `tags` on its ONE INVITE server transaction
    /// (as if a downstream proxy had forked), then answers `200` under
    /// `winner`: a declared tag, or [`ForkWinner::Unrung`] for a fork that sent
    /// no provisional (a fresh tag, §13.2.2.4); `loser_late_200` optionally emits a late `200` under a losing
    /// tag (§13.2.2.4 — the caller ACKs then BYEs it). `reliable` makes each
    /// fork's 18x a `183` (`Require:100rel`) the caller PRACKs per early dialog.
    /// **Only valid under the SUT's transparent CORE relay**
    /// (`relay_first_18x_to_180 = None`, the default `B2buaSut`): any
    /// `relayFirst18x` masking mode collapses the forks to one To-tag and the
    /// distinct-early-dialog behavior cannot be exercised.
    Forked {
        tags: &'static [&'static str],
        winner: ForkWinner,
        reliable: bool,
        loser_late_200: Option<&'static str>,
    },
    /// A HELD answer: the callee rings at once (`reliable`: a `183` per early
    /// dialog that the caller PRACKs, RFC 3262) and answers only once the
    /// [`Stage::Early`] stages that follow completed, after the ring dwell.
    /// The first stage that is not early, or the teardown, marks the answer.
    /// [`Forks::Forked`] is valid only under the SUT's transparent CORE relay,
    /// like [`Forked`](Self::Forked).
    Held { forks: Forks, reliable: bool },
    /// TERMINAL: the callee rejects with `code` and that IS the call — no
    /// dialog, no teardown (the `rejected` branch).
    RejectTerminal { code: u16 },
    /// TERMINAL: the caller CANCELs once ringing (RFC 3261 §9.1) — no dialog,
    /// no teardown ([`Branch::ABANDONED`]); a 200 that crosses the CANCEL, or
    /// reaches the caller before it leaves, is ACKed and BYEd
    /// ([`Branch::ANSWERED_DESPITE_CANCEL`], [`Branch::ANSWERED_BEFORE_CANCEL`]).
    AbandonAfterRinging,
    /// CANCEL×200 CROSSING (C2/E5): the caller CANCELs while the callee answers,
    /// so the terminal outcome is EITHER confirmed (the 200 crossed the CANCEL,
    /// §9.2 — the call tears down) OR cancelled (the CANCEL won — 487). Each is
    /// an accepted ending ([`Branch::ANSWERED_DESPITE_CANCEL`] /
    /// [`Branch::ABANDONED`], and [`Branch::ANSWERED_BEFORE_CANCEL`] when the 200
    /// reached the caller before her CANCEL left); the verdict names whichever
    /// occurred. Terminal-style (no stages, no separate
    /// teardown): the branch-conditional teardown rides the callee.
    CancelAnswerCrossing,
}

/// The early dialogs a [`Establishment::Held`] callee presents.
#[derive(Debug, Clone, Copy)]
pub enum Forks {
    /// One early dialog, under the callee's own tag, which answers.
    Sole,
    /// One early dialog per tag (RFC 3261 §12.1.2), answered under `winner`.
    Forked { tags: &'static [&'static str], winner: ForkWinner },
}

impl Forks {
    /// The fork tag the callee's own early requests ride: the winner's when it
    /// rang, else the first fork's (`None`: the callee's own tag).
    pub(crate) fn early_fork(self) -> Option<&'static str> {
        match self {
            Forks::Sole => None,
            Forks::Forked { winner: ForkWinner::Tag(tag), .. } => Some(tag),
            Forks::Forked { tags, winner: ForkWinner::Unrung } => tags.first().copied(),
        }
    }

    /// How many early dialogs ring.
    pub(crate) fn count(self) -> usize {
        match self {
            Forks::Sole => 1,
            Forks::Forked { tags, .. } => tags.len(),
        }
    }
}

/// Which party of the call drives a stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Party {
    Caller,
    Callee,
}

/// The method of an early-dialog request ([`Stage::Early`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EarlyMethod {
    /// An UPDATE carrying the sender's session description as an offer
    /// (RFC 3311 §5.1).
    Update,
    /// A bodiless INFO.
    Info,
}

/// An in-dialog script or a dialog-changing transfer — the chainable stages
/// after establishment.
#[derive(Debug, Clone)]
pub enum Stage {
    /// A script the caller drives on the current dialog.
    Script(Script),
    /// A script the answering callee drives on the current dialog.
    Callee(Script),
    /// `count` requests `by` sends one after another on the early dialog of a
    /// [`Establishment::Held`] call, each after the previous one's 2xx. Early
    /// stages come before every other stage; the answer follows them.
    Early {
        by: Party,
        method: EarlyMethod,
        count: u32,
    },
    Transfer(Transfer),
}

/// A deliberate in-dialog sequence a party drives on the CURRENT dialog (the
/// caller's through [`Stage::Script`], the callee's through [`Stage::Callee`]).
#[derive(Debug, Clone, Copy)]
pub enum Script {
    /// `n` sequential delayed-offer re-INVITE renegotiations (each guarded on
    /// the previous one completing; each dwells `reinvite_gap`). Replaces the
    /// gate with `reinvited`.
    Reinvite { n: u32 },
    /// One in-dialog UPDATE (RFC 3311) renegotiation (its 200 completes it —
    /// no ACK). Replaces the gate with `updated`.
    UpdatePostConnect,
    /// EARLY UPDATE (C5, RFC 3311 §5.1): the caller renegotiates media on the
    /// reliable establishment's EARLY dialog — after the PRACK, before the final
    /// 200. REQUIRES a reliable early dialog (`Establishment::Reliable`); the
    /// callee is upgraded to hold its INVITE 200 until the UPDATE is answered.
    /// `validate()` rejects it on any other establishment. Gate unchanged (the
    /// call still confirms via the establishment's `established` gate).
    UpdateEarly,
    /// ONE in-dialog OPTIONS keepalive ping, its 200 read inline. Gate
    /// unchanged.
    KeepaliveOnce,
    /// OPTIONS keepalive pings every `options_cadence` until `options_hold`
    /// elapses (both env knobs). Gate unchanged; the goal cursor advances only
    /// after the hold, so a following teardown is naturally held back.
    KeepaliveLoop,
    /// ONE in-dialog OPTIONS whose 200 the reactor observes. Replaces the gate
    /// with `pinged`.
    Ping,
}

/// A blind transfer (REFER) — moves the current dialog to the transfer target.
#[derive(Debug, Clone)]
pub enum Transfer {
    /// The target answers; the B2BUA's post-transfer media merge re-INVITEs
    /// the caller and the target as parallel sub-flows. Replaces the gate with
    /// `merged` (the conjunction of both realigns).
    Blind { refer_key: String },
    /// The target DECLINES with `code` (603); the transfer fails, the original
    /// dialog stays up. Replaces the gate with `declined`; the call ends on its
    /// `transfer_declined` branch.
    BlindDeclined { refer_key: String, code: u16 },
}

/// A dwell knob resolved against the per-call env.
#[derive(Debug, Clone, Copy)]
pub enum DwellKnob {
    None,
    TalkTime,
    ReinviteGap,
    LongHold,
    /// A dwell the shape fixes itself, whatever the env.
    Fixed(Duration),
}

impl DwellKnob {
    fn resolve(self, env: &CallEnv<'_>) -> Duration {
        match self {
            DwellKnob::None => Duration::ZERO,
            DwellKnob::TalkTime => env.talk_time,
            DwellKnob::ReinviteGap => env.reinvite_gap,
            DwellKnob::LongHold => env.long_hold,
            DwellKnob::Fixed(d) => d,
        }
    }
}

/// What the caller's BYE feeds the per-call recorder (the contract table pins
/// this per shape: most stamp checkpoint+phase, `long_call` checkpoint only,
/// the refer family nothing).
#[derive(Debug, Clone, Copy)]
pub enum ByeFeed {
    CheckpointAndPhase,
    CheckpointOnly,
    NoFeed,
}

/// How the call ends. Terminal establishments take [`Teardown::None`]; every
/// other chain MUST hang up (the repo-wide "properly terminated" rule).
#[derive(Debug, Clone, Copy)]
pub enum Teardown {
    /// The caller BYEs once the final gate holds, after the dwell.
    CallerBye { after: DwellKnob, feed: ByeFeed },
    /// CROSSING BYE (C3/S3, RFC 3261 §15.1.2): the caller AND the winning callee
    /// both BYE on the final gate at the same instant, so each BYE is in flight
    /// when the peer's arrives. Each reactor 200s the inbound BYE while its own
    /// is outstanding; both legs terminate. The caller stamps the same
    /// checkpoint/phase feed as a plain `CallerBye`.
    CrossingBye { after: DwellKnob },
    /// The answering callee BYEs once the final gate holds, after the dwell.
    CalleeBye { after: DwellKnob },
    /// The caller CANCELs the [`Establishment::Held`] call at `at` (RFC 3261
    /// §9.1); an answer that crossed the CANCEL is ACKed and BYEd. Only early
    /// stages precede it.
    Cancel(CancelAt),
    /// No teardown goal — only legal for terminal establishments.
    None,
}

/// Where the caller's CANCEL meets the callee's responses. The callee rings
/// and answers on its own clock, so each value is a deterministic timing on
/// the harness fabric's one-hop transit delay ([`TRANSIT`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CancelAt {
    /// After the `100 Trying`, before the callee rang: it rings only later.
    BeforeProvisional,
    /// In flight toward the SUT while the SUT's relayed provisional is in
    /// flight toward the caller.
    CrossingProvisional,
    /// While the callee rings: the CANCEL reaches it well before its answer.
    AfterProvisional,
    /// As [`AfterProvisional`](Self::AfterProvisional), then the caller BYEs
    /// the early dialog (RFC 3261 §15) half a hop later, so the BYE crosses
    /// the SUT's 487 to her INVITE. That 487 ended the early dialog, so the
    /// SUT answers the BYE 481 (§12.3, §12.2.2).
    AfterProvisionalThenEarlyBye,
    /// Reaching the callee just after it sent its 2xx, which reaches the SUT
    /// after the CANCEL did.
    CrossingFinal,
    /// Reaching the SUT just after it relayed the 2xx toward the caller, who
    /// sent the CANCEL before that 2xx arrived: no effect, then BYE (§9.1).
    AfterFinal,
}

/// The one-hop transit delay of the harness fabric the [`CancelAt`] timings
/// are computed for.
pub const TRANSIT: Duration = Duration::from_millis(scenario_harness::SIMULATED_TRANSIT_DELAY_MS);

/// A composed call shape: establishment → stages → teardown, routed through a
/// [`RouteBinder`]. Implements [`ActorScenario`], so it plugs into the shape
/// registry, the load driver and the functional runner exactly like a
/// hand-written body.
pub struct ShapePlan {
    pub id: ScenarioId,
    pub binder: Arc<dyn RouteBinder>,
    pub establish: Establishment,
    pub stages: Vec<Stage>,
    pub teardown: Teardown,
    /// Feed the cross-call 18x delivery gate from this caller's provisionals
    /// (contract table §3: the shared-establishment shapes only — the
    /// hand-rolled refer/abandon contracts must NOT).
    pub ringing_gate: bool,
    /// Stamp `connected` on the answering callee's ACK receipt (the shared
    /// establishment's phase; the refer family's contract has no `connected`).
    pub stamp_connected: bool,
}

/// The compile fold's accumulator.
pub(crate) struct Build {
    pub(crate) caller_goals: Vec<Goal>,
    pub(crate) caller_feed: CtxFeed,
    pub(crate) callees: Vec<ActorSpec>,
    pub(crate) phases: Vec<BarrierPhase>,
    pub(crate) expect: Expect,
    /// The barrier gating the next deliberate action on the current dialog.
    pub(crate) gate: Barrier,
    /// The winning callee role (`"bob"`, or `"bob2"` after a reroute) — the leg
    /// a crossing-BYE teardown gives its own BYE goal to.
    pub(crate) winner: &'static str,
    /// A [`Establishment::Held`] answer not yet placed: the winner it answers
    /// under. Placed by [`crate::held::place_answer`].
    pub(crate) held_answer: Option<Option<ForkWinner>>,
    /// The renegotiations each role completed so far (its early UPDATEs
    /// included), which its next renegotiation gate counts on from.
    renegs: Vec<(&'static str, u32)>,
    /// The in-dialog OPTIONS each role had answered so far.
    pings: Vec<(&'static str, u32)>,
}

impl Build {
    pub(crate) fn callee_mut(&mut self, role: &'static str) -> &mut ActorSpec {
        self.callees
            .iter_mut()
            .find(|a| a.role == role)
            .expect("stage references an undeclared callee role")
    }

    /// The role a party names on the current dialog.
    pub(crate) fn role(&self, party: Party) -> &'static str {
        match party {
            Party::Caller => "alice",
            Party::Callee => self.winner,
        }
    }

    pub(crate) fn goals_mut(&mut self, role: &'static str) -> &mut Vec<Goal> {
        if role == "alice" {
            &mut self.caller_goals
        } else {
            &mut self.callee_mut(role).goals
        }
    }

    /// Count `n` more completed renegotiations for `role`; the new total.
    pub(crate) fn add_renegs(&mut self, role: &'static str, n: u32) -> u32 {
        bump(&mut self.renegs, role, n)
    }

    /// Count one more answered OPTIONS for `role`; the new total.
    fn add_ping(&mut self, role: &'static str) -> u32 {
        bump(&mut self.pings, role, 1)
    }
}

/// Add `n` to `role`'s counter in `counts`; the new value.
fn bump(counts: &mut Vec<(&'static str, u32)>, role: &'static str, n: u32) -> u32 {
    match counts.iter_mut().find(|(r, _)| *r == role) {
        Some((_, c)) => {
            *c += n;
            *c
        }
        None => {
            counts.push((role, n));
            n
        }
    }
}

/// The renegotiation count of `role` reached `n` — the `reinvited` /
/// `updated` gate.
fn renegs_reached(name: &'static str, role: &'static str, n: u32) -> Barrier {
    Barrier::pred(name, move |s| s.leg(role).reneg_count() >= n)
}

/// `role` received `n` 2xx to its `method` requests.
pub(crate) fn answered(
    name: &'static str,
    role: &'static str,
    method: &'static str,
    n: u32,
) -> Barrier {
    Barrier::pred(name, move |s| {
        let ok = |f: &&scenario_harness::actor::ResponseFact| {
            (200..300).contains(&f.status) && f.cseq_method == method
        };
        s.leg(role).responses().iter().filter(ok).count() >= n as usize
    })
}

/// `alice` + `winner` both confirmed — the `established` gate/phase, with the
/// winning callee role captured (bob, or bob2 after a reroute).
pub(crate) fn established_pred(
    winner: &'static str,
) -> impl Fn(&StateInner) -> bool + Send + Sync + Clone {
    move |s: &StateInner| {
        s.leg_at_least("alice", LegPhase::Confirmed) && s.leg_at_least(winner, LegPhase::Confirmed)
    }
}

/// The post-transfer media merge completed on BOTH realign legs.
fn merged(s: &StateInner) -> bool {
    let confirmed = |leg: &str| {
        s.leg(leg).subflow(SUBFLOW_REALIGN).is_some_and(|f| f >= SubflowState::Confirmed)
    };
    confirmed("alice") && confirmed("charlie")
}

/// The transfer target declined (its leg terminated without confirming).
fn declined(s: &StateInner) -> bool {
    s.leg_at_least("charlie", LegPhase::Terminated)
}

/// The declined transfer's accepted ending: the target refused, and the
/// original call tore down.
const TRANSFER_DECLINED: Branch = Branch::new("transfer_declined", Ending::Observed(declined));

/// The fork wiring every forked establishment shares: at least two tags, a
/// tagged winner among them.
fn validate_forks(tags: &[&str], winner: ForkWinner) -> Result<(), &'static str> {
    if tags.len() < 2 {
        return Err("Forked needs >= 2 distinct fork tags");
    }
    if let ForkWinner::Tag(w) = winner {
        if !tags.contains(&w) {
            return Err("Forked winner must be one of the declared tags");
        }
    }
    Ok(())
}

/// An early stage of a Held establishment (`held_reliable`: whether its
/// provisionals are reliable; `None`: not Held), before any answered stage.
fn validate_early(
    held_reliable: Option<bool>,
    answered: bool,
    by: Party,
    method: EarlyMethod,
    count: u32,
) -> Result<(), &'static str> {
    let Some(reliable) = held_reliable else {
        return Err("an Early stage needs a Held establishment");
    };
    if answered {
        return Err("an Early stage must precede every answered stage");
    }
    if count == 0 {
        return Err("Early count must be >= 1");
    }
    // RFC 3311 §5.1: an UPDATE offer needs the INVITE's answer first, which
    // only a reliable provisional carries.
    if method == EarlyMethod::Update && !reliable {
        return Err("an early UPDATE needs reliable provisionals");
    }
    // The callee learns that the caller holds its early dialog only from the
    // PRACK (RFC 3262).
    if by == Party::Callee && !reliable {
        return Err("a callee's early request needs reliable provisionals");
    }
    Ok(())
}

impl ShapePlan {
    /// Whether the establishment terminates the call by itself (no dialog to
    /// script or tear down).
    fn is_terminal(&self) -> bool {
        matches!(
            self.establish,
            Establishment::RejectTerminal { .. }
                | Establishment::AbandonAfterRinging
                | Establishment::CancelAnswerCrossing
        )
    }

    /// Structural validity, independent of any call env: a terminal
    /// establishment admits no stages and no teardown; every other chain MUST
    /// tear down (the repo-wide "properly terminated" rule).
    pub fn validate(&self) -> Result<(), StepError> {
        let err = |detail: &str| StepError::UnexpectedKind {
            who: self.id.to_string(),
            detail: detail.to_string(),
        };
        if self.is_terminal() {
            if !self.stages.is_empty() {
                return Err(err("stage chained after a terminal establishment"));
            }
            if !matches!(self.teardown, Teardown::None) {
                return Err(err("teardown declared on a terminal establishment"));
            }
        } else if matches!(self.teardown, Teardown::None) {
            return Err(err("non-terminal shape with no teardown (calls must terminate)"));
        }
        if let Establishment::Forked { tags, winner, loser_late_200, .. } = self.establish {
            validate_forks(tags, winner).map_err(&err)?;
            if let Some(loser) = loser_late_200 {
                if winner == ForkWinner::Tag(loser) || !tags.contains(&loser) {
                    return Err(err("Forked loser_late_200 must be a declared tag != winner"));
                }
            }
        }
        if let Establishment::Held { forks: Forks::Forked { tags, winner }, .. } = self.establish {
            validate_forks(tags, winner).map_err(&err)?;
        }
        let held_reliable = match self.establish {
            Establishment::Held { reliable, .. } => Some(reliable),
            _ => None,
        };
        let mut answered = false;
        for stage in &self.stages {
            if let Stage::Script(Script::Reinvite { n }) | Stage::Callee(Script::Reinvite { n }) =
                stage
            {
                if *n == 0 {
                    return Err(err("Reinvite n must be >= 1"));
                }
            }
            // C5: an early UPDATE needs a reliable early dialog to attach to
            // (RFC 3311 §5.1 requires a PRACKed reliable provisional first).
            if matches!(stage, Stage::Script(Script::UpdateEarly))
                && !matches!(self.establish, Establishment::Reliable)
            {
                return Err(err("UpdateEarly requires a reliable establishment (early dialog)"));
            }
            if matches!(stage, Stage::Callee(Script::UpdateEarly)) {
                return Err(err("UpdateEarly is the caller's; a callee's is Stage::Early"));
            }
            if let Stage::Early { by, method, count } = stage {
                validate_early(held_reliable, answered, *by, *method, *count).map_err(&err)?;
            } else {
                answered = true;
            }
        }
        if let Teardown::Cancel(at) = self.teardown {
            if held_reliable.is_none() {
                return Err(err("a Cancel teardown needs a Held establishment"));
            }
            if answered {
                return Err(err("a Cancel teardown follows early stages only"));
            }
            let early = !self.stages.is_empty();
            if early && matches!(at, CancelAt::BeforeProvisional | CancelAt::CrossingProvisional) {
                return Err(err("a CANCEL before the provisional leaves no early dialog to use"));
            }
        }
        Ok(())
    }

    fn compile(&self, env: &CallEnv<'_>) -> Result<ActorCall, StepError> {
        self.validate()?;
        let mut b = Build {
            caller_goals: Vec::new(),
            caller_feed: CtxFeed { ringing_gate: self.ringing_gate, ..CtxFeed::default() },
            callees: Vec::new(),
            phases: Vec::new(),
            expect: Expect::answered(),
            gate: Barrier::None,
            winner: "bob",
            held_answer: None,
            renegs: Vec::new(),
            pings: Vec::new(),
        };

        self.compile_establishment(env, &mut b)?;

        for stage in &self.stages {
            if let Stage::Early { by, method, count } = stage {
                crate::held::compile_early(self.establish, &mut b, *by, *method, *count);
                continue;
            }
            crate::held::place_answer(env, &mut b);
            match stage {
                Stage::Script(s) => self.compile_script(env, &mut b, s, Party::Caller),
                Stage::Callee(s) => self.compile_script(env, &mut b, s, Party::Callee),
                Stage::Transfer(t) => self.compile_transfer(env, &mut b, t)?,
                Stage::Early { .. } => unreachable!("compiled above"),
            }
        }

        if let Teardown::Cancel(at) = self.teardown {
            crate::held::compile_cancel(env, &mut b, at);
        } else {
            crate::held::place_answer(env, &mut b);
        }
        match self.teardown {
            Teardown::CallerBye { after, feed } => {
                b.caller_goals
                    .push(Goal::new(b.gate.clone(), GoalStep::Bye).after(after.resolve(env)));
                b.caller_feed.on_bye_ok = match feed {
                    ByeFeed::CheckpointAndPhase => {
                        Feed::new(Some("time_to_bye_200"), Some("bye_200"))
                    }
                    ByeFeed::CheckpointOnly => Feed::new(Some("time_to_bye_200"), None),
                    ByeFeed::NoFeed => Feed::default(),
                };
            }
            Teardown::CrossingBye { after } => {
                let dwell = after.resolve(env);
                // The caller AND the winning callee both BYE on the SAME final
                // gate, dwelling identically, so the two BYEs cross (RFC 3261
                // §15.1.2). Each reactor 200s the peer's inbound BYE while its
                // own is in flight — verified order-independent in the actor
                // machinery test `two_actor_crossing_bye_both_terminate`.
                b.caller_goals.push(Goal::new(b.gate.clone(), GoalStep::Bye).after(dwell));
                b.caller_feed.on_bye_ok = Feed::new(Some("time_to_bye_200"), Some("bye_200"));
                let gate = b.gate.clone();
                b.callee_mut(b.winner).goals.push(Goal::new(gate, GoalStep::Bye).after(dwell));
            }
            Teardown::CalleeBye { after } => {
                let gate = b.gate.clone();
                b.callee_mut(b.winner)
                    .goals
                    .push(Goal::new(gate, GoalStep::Bye).after(after.resolve(env)));
            }
            // Compiled with the answer above.
            Teardown::Cancel(_) => {}
            Teardown::None => {} // legal only for terminals — validate() gates
        }

        // The caller answers every renegotiation with its own session (RFC
        // 3264 §8): the description its INVITE offered.
        let mut actors = vec![ActorSpec {
            role: "alice",
            agent: env.alice.clone(),
            disposition: Disposition::Caller,
            media: MediaState::offer(OFFER_SDP),
            goals: b.caller_goals,
            invite_targets: vec![("bob", env.bob.clone())],
            via: None,
            feed: b.caller_feed,

            cseq: None,
            delayed: vec![],
            claim: None,
        }];
        actors.extend(b.callees);

        Ok(ActorCall {
            actors,
            plan: b.phases,
            settle: SettleBarrier::default_ceiling(),
            expect: b.expect,

            waivers: Vec::new(),
            automatics: Default::default(),
            ceiling: None,
            delta_policy: None,
            reception_observer: None,
        })
    }

    fn compile_establishment(&self, env: &CallEnv<'_>, b: &mut Build) -> Result<(), StepError> {
        let connected_feed = |stamp: bool| CtxFeed {
            on_ack_rx: if stamp { Feed::new(None, Some("connected")) } else { Feed::default() },
            ..CtxFeed::default()
        };
        match self.establish {
            Establishment::Transparent | Establishment::Reliable => {
                let reliable = matches!(self.establish, Establishment::Reliable);
                let mut plan = self.binder.invite_plan(env, RouteIntent::Direct { target: "bob" });
                if reliable {
                    plan = plan.with_supported_100rel();
                }
                b.caller_goals.push(Goal::new(
                    Barrier::None,
                    GoalStep::Invite { callee: "bob", plan: Some(plan) },
                ));
                b.caller_feed.on_answer_rx = Feed::new(Some("time_to_200"), None);
                if reliable {
                    b.caller_feed.on_prack_ok =
                        Feed::new(Some("time_to_prack_200"), Some("pracked"));
                }
                b.callees.push(ActorSpec {
                    role: "bob",
                    agent: env.bob.clone(),
                    disposition: if reliable {
                        Disposition::ReliableAnswer
                    } else {
                        Disposition::RingThenAnswer { ring: env.ring_delay }
                    },
                    media: MediaState::answer(ANSWER_SDP),
                    goals: vec![],
                    invite_targets: vec![],
                    via: None,
                    feed: connected_feed(self.stamp_connected),

                    cseq: None,
                    delayed: vec![],
                    claim: None,
                });
                let est = established_pred("bob");
                b.phases.push(phase("established", est.clone()));
                b.gate = Barrier::pred("established", est);
            }
            Establishment::RerouteOnReject { .. } | Establishment::RerouteOnNoAnswer { .. } => {
                // The two failover triggers share the whole scaffolding — the
                // [bob, bob2] intent, the bob2 winner, the `established` gate —
                // and differ ONLY in the primary's stimulus (an immediate reject
                // final vs ring-then-silence bounded by the SUT's no-answer
                // timer) and in whether the plan arms that timer client-side.
                let (winner_reliable, primary, plan) = match self.establish {
                    Establishment::RerouteOnReject { reject, winner_reliable } => (
                        winner_reliable,
                        // The primary callee rejects (its reject-ACK absorbed
                        // without confirming), triggering the SUT's failover.
                        Disposition::Reject(reject),
                        self.binder.invite_plan(
                            env,
                            RouteIntent::FailoverOnReject { targets: &["bob", "bob2"] },
                        ),
                    ),
                    Establishment::RerouteOnNoAnswer { no_answer_ms, winner_reliable } => (
                        winner_reliable,
                        // The primary rings then stays silent; the SUT's
                        // no-answer timer CANCELs it (→ 487, a clean settle) and
                        // walks the plan to bob2.
                        Disposition::RingThenSilent,
                        self.binder.invite_plan_no_answer(
                            env,
                            RouteIntent::FailoverOnReject { targets: &["bob", "bob2"] },
                            i64::from(no_answer_ms),
                        ),
                    ),
                    _ => unreachable!("outer match arm"),
                };
                let bob2 = env
                    .bob2
                    .ok_or_else(|| StepError::UnexpectedKind {
                        who: self.id.to_string(),
                        detail: "bound without a bob2 leg".to_string(),
                    })?
                    .clone();
                let mut plan = plan;
                if winner_reliable {
                    plan = plan.with_supported_100rel();
                }
                b.caller_goals.push(Goal::new(
                    Barrier::None,
                    GoalStep::Invite { callee: "bob", plan: Some(plan) },
                ));
                b.caller_feed.on_answer_rx = Feed::new(Some("time_to_200"), None);
                if winner_reliable {
                    b.caller_feed.on_prack_ok =
                        Feed::new(Some("time_to_prack_200"), Some("pracked"));
                }
                b.callees.push(ActorSpec {
                    role: "bob",
                    agent: env.bob.clone(),
                    disposition: primary,
                    media: MediaState::none(),
                    goals: vec![],
                    invite_targets: vec![],
                    via: None,
                    feed: CtxFeed::default(),

                    cseq: None,
                    delayed: vec![],
                    claim: None,
                });
                b.callees.push(ActorSpec {
                    role: "bob2",
                    agent: bob2,
                    disposition: if winner_reliable {
                        Disposition::ReliableAnswer
                    } else {
                        Disposition::RingThenAnswer { ring: env.ring_delay }
                    },
                    media: MediaState::answer(ANSWER_SDP),
                    goals: vec![],
                    invite_targets: vec![],
                    via: None,
                    feed: CtxFeed {
                        on_invite_rx: Feed::new(None, Some("rerouted")),
                        on_ack_rx: if self.stamp_connected {
                            Feed::new(None, Some("connected"))
                        } else {
                            Feed::default()
                        },
                        ..CtxFeed::default()
                    },

                    cseq: None,
                    delayed: vec![],
                    claim: None,
                });
                b.winner = "bob2";
                let est = established_pred("bob2");
                b.phases.push(phase("established", est.clone()));
                b.gate = Barrier::pred("established", est);
            }
            Establishment::Forked { tags, winner, reliable, loser_late_200 } => {
                let mut plan = self.binder.invite_plan(env, RouteIntent::Direct { target: "bob" });
                if reliable {
                    plan = plan.with_supported_100rel();
                }
                b.caller_goals.push(Goal::new(
                    Barrier::None,
                    GoalStep::Invite { callee: "bob", plan: Some(plan) },
                ));
                b.caller_feed.on_answer_rx = Feed::new(Some("time_to_200"), None);
                if reliable {
                    b.caller_feed.on_prack_ok =
                        Feed::new(Some("time_to_prack_200"), Some("pracked"));
                }
                // bob is the forking UAS: one 18x per distinct To-tag on its one
                // INVITE server txn, then 200 under `winner` (a fresh tag when
                // unrung) (+ optional
                // losing-tag late 200). The fork-aware caller reactor (C1b) tracks
                // the early-dialog set and ACK+BYEs a loser's late 200 itself.
                b.callees.push(ActorSpec {
                    role: "bob",
                    agent: env.bob.clone(),
                    disposition: Disposition::ForkingRing {
                        tags,
                        winner,
                        ring: env.ring_delay,
                        reliable,
                        loser_late_200,
                    },
                    media: MediaState::answer(ANSWER_SDP),
                    goals: vec![],
                    invite_targets: vec![],
                    via: None,
                    feed: connected_feed(self.stamp_connected),

                    cseq: None,
                    delayed: vec![],
                    claim: None,
                });
                let est = established_pred("bob");
                b.phases.push(phase("established", est.clone()));
                b.gate = Barrier::pred("established", est);
            }
            Establishment::Held { forks, reliable } => {
                let mut plan = self.binder.invite_plan(env, RouteIntent::Direct { target: "bob" });
                if reliable {
                    plan = plan.with_supported_100rel();
                }
                b.caller_goals.push(Goal::new(
                    Barrier::None,
                    GoalStep::Invite { callee: "bob", plan: Some(plan) },
                ));
                b.caller_feed.on_answer_rx = Feed::new(Some("time_to_200"), None);
                if reliable {
                    b.caller_feed.on_prack_ok =
                        Feed::new(Some("time_to_prack_200"), Some("pracked"));
                }
                let ring_after = match self.teardown {
                    Teardown::Cancel(at) => crate::held::ring_after(at),
                    _ => Duration::ZERO,
                };
                b.callees.push(crate::held::held_callee(
                    env.bob.clone(),
                    forks,
                    reliable,
                    ring_after,
                    connected_feed(self.stamp_connected),
                ));
                b.gate = crate::held::early_gate(forks, reliable);
                b.held_answer = Some(match forks {
                    Forks::Sole => None,
                    Forks::Forked { winner, .. } => Some(winner),
                });
            }
            Establishment::RejectTerminal { code } => {
                let plan = self.binder.invite_plan(env, RouteIntent::Direct { target: "bob" });
                b.caller_goals.push(Goal::new(
                    Barrier::None,
                    GoalStep::Invite { callee: "bob", plan: Some(plan) },
                ));
                b.callees.push(ActorSpec {
                    role: "bob",
                    agent: env.bob.clone(),
                    disposition: Disposition::Reject(code),
                    media: MediaState::none(),
                    goals: vec![],
                    invite_targets: vec![],
                    via: None,
                    feed: CtxFeed::default(),

                    cseq: None,
                    delayed: vec![],
                    claim: None,
                });
                b.phases.push(phase("rejected", |s| s.leg_at_least("bob", LegPhase::Terminated)));
                b.expect = Expect::one(Branch::rejected(code));
            }
            Establishment::AbandonAfterRinging => {
                let plan = self.binder.invite_plan(env, RouteIntent::Direct { target: "bob" });
                let ringing = |s: &StateInner| s.leg_rang("alice");
                b.caller_goals.push(Goal::new(
                    Barrier::None,
                    GoalStep::Invite { callee: "bob", plan: Some(plan) },
                ));
                b.caller_goals.push(Goal::new(
                    Barrier::pred("ringing", ringing),
                    GoalStep::Cancel { stated: Vec::new() },
                ));
                // A 200 that crossed the CANCEL confirmed the call: hang up
                // (RFC 3261 §15). After a 487 the accepted final drops it.
                b.caller_goals.push(Goal::new(
                    Barrier::pred("answered", |s: &StateInner| {
                        s.leg_at_least("alice", LegPhase::Confirmed)
                    }),
                    GoalStep::ByeIfConfirmed,
                ));
                b.caller_feed.on_provisional = Feed::new(Some("time_to_180"), None);
                b.callees.push(ActorSpec {
                    role: "bob",
                    agent: env.bob.clone(),
                    disposition: Disposition::RingThenAnswer { ring: env.ring_delay },
                    media: MediaState::answer(ANSWER_SDP),
                    goals: vec![],
                    invite_targets: vec![],
                    via: None,
                    feed: CtxFeed::default(),

                    cseq: None,
                    delayed: vec![],
                    claim: None,
                });
                b.phases.push(phase("ringing", ringing));
                const ABANDON_ENDINGS: &[Branch] = &[
                    Branch::ABANDONED,
                    Branch::ANSWERED_DESPITE_CANCEL,
                    Branch::ANSWERED_BEFORE_CANCEL,
                ];
                b.expect = Expect::of(ABANDON_ENDINGS);
            }
            Establishment::CancelAnswerCrossing => {
                // C2/E5: alice INVITEs then CANCELs (gated on ringing, after a
                // dwell timed near bob's answer), so the CANCEL and the 200
                // cross. The outcome is a branch-aware race oracle; the load
                // lane accepts whichever legal branch occurred. The
                // branch-conditional teardown rides bob (ByeIfConfirmed), so a
                // 487 never trips alice's incidental-failure path — she keeps
                // only [Invite, Cancel].
                const E5_BRANCHES: &[Branch] = &[
                    Branch::ANSWERED_DESPITE_CANCEL,
                    Branch::ANSWERED_BEFORE_CANCEL,
                    Branch::ABANDONED,
                ];
                let plan = self.binder.invite_plan(env, RouteIntent::Direct { target: "bob" });
                let ringing = |s: &StateInner| s.leg_rang("alice");
                b.caller_goals.push(Goal::new(
                    Barrier::None,
                    GoalStep::Invite { callee: "bob", plan: Some(plan) },
                ));
                b.caller_goals.push(
                    Goal::new(
                        Barrier::pred("ringing", ringing),
                        GoalStep::Cancel { stated: Vec::new() },
                    )
                    .after(env.ring_delay),
                );
                b.caller_feed.on_provisional = Feed::new(Some("time_to_180"), None);
                b.callees.push(ActorSpec {
                    role: "bob",
                    agent: env.bob.clone(),
                    disposition: Disposition::RingThenAnswer { ring: env.ring_delay },
                    media: MediaState::answer(ANSWER_SDP),
                    // The branch-conditional teardown: the guard holds once bob
                    // has RESOLVED (Confirmed, or the monotone Terminated after
                    // a 487); ByeIfConfirmed BYEs the winning dialog or no-ops.
                    goals: vec![Goal::new(
                        Barrier::pred("bob_resolved", |s| {
                            s.leg_at_least("bob", LegPhase::Confirmed)
                        }),
                        GoalStep::ByeIfConfirmed,
                    )],
                    invite_targets: vec![],
                    via: None,
                    feed: CtxFeed::default(),

                    cseq: None,
                    delayed: vec![],
                    claim: None,
                });
                b.phases.push(phase("ringing", ringing));
                b.expect = Expect::of(E5_BRANCHES);
            }
        }
        Ok(())
    }

    fn compile_script(&self, env: &CallEnv<'_>, b: &mut Build, script: &Script, by: Party) {
        let role = b.role(by);
        let caller = by == Party::Caller;
        match *script {
            Script::Reinvite { n } => {
                // C6: serialize N delayed-offer re-INVITE cycles. Cycle 0 fires
                // on the incoming gate; cycle `i` (i>=1) waits until the leg has
                // COMPLETED i prior renegotiations, counted on from the ones it
                // completed before this script (its early UPDATEs included), so
                // no two re-INVITEs are ever in flight (that would glare into a
                // 491). `reneg_count` is the cardinality of the leg's grow-only
                // completed-reneg set (state.rs `RenegCompleted`).
                let base = b.add_renegs(role, 0);
                for i in 0..n {
                    let guard = if i == 0 {
                        b.gate.clone()
                    } else {
                        renegs_reached("reinvited", role, base + i)
                    };
                    let goal = Goal::new(guard, GoalStep::Reinvite).after(env.reinvite_gap);
                    b.goals_mut(role).push(goal);
                }
                if caller {
                    b.caller_feed.on_reinvite_ok =
                        Feed::new(Some("time_to_reinvite_200"), Some("reinvited"));
                }
                b.gate = renegs_reached("reinvited", role, b.add_renegs(role, n));
            }
            Script::UpdatePostConnect => {
                let goal = Goal::new(b.gate.clone(), GoalStep::Update).after(env.reinvite_gap);
                b.goals_mut(role).push(goal);
                if caller {
                    b.caller_feed.on_update_ok =
                        Feed::new(Some("time_to_update_200"), Some("updated"));
                }
                b.gate = renegs_reached("updated", role, b.add_renegs(role, 1));
            }
            // C5 (RFC 3311 §5.1): the caller UPDATEs the reliable EARLY dialog
            // BEFORE the final 200 (validate() guarantees a caller script on
            // Establishment::Reliable — bob is a ReliableAnswer callee we upgrade
            // to hold its INVITE 200 until the UPDATE completes). The UPDATE
            // fires once the caller is Early (the 183 is in + PRACKed); the gate
            // stays `established`.
            Script::UpdateEarly => {
                let early = |s: &StateInner| s.leg("alice").subflow(SUBFLOW_EARLY).is_some();
                b.caller_goals
                    .push(Goal::new(Barrier::pred("early", early), GoalStep::UpdateEarly));
                b.caller_feed.on_update_ok = Feed::new(Some("time_to_update_200"), Some("updated"));
                b.callee_mut("bob").disposition = Disposition::ReliableAnswerEarlyUpdate;
                b.add_renegs("alice", 1);
            }
            Script::KeepaliveOnce => {
                let goal = Goal::new(b.gate.clone(), GoalStep::Options);
                b.goals_mut(role).push(goal);
                if caller {
                    b.caller_feed.on_options_ok =
                        Feed::new(Some("time_to_options_200"), Some("keepalive_ack"));
                }
            }
            Script::KeepaliveLoop => {
                let step =
                    GoalStep::EveryOptions { cadence: env.options_cadence, hold: env.options_hold };
                let goal = Goal::new(b.gate.clone(), step);
                b.goals_mut(role).push(goal);
                if caller {
                    b.caller_feed.on_options_ok =
                        Feed::new(Some("time_to_options_200"), Some("keepalive_ack"));
                }
            }
            Script::Ping => {
                let step = GoalStep::InDialog {
                    method: InDialogMethod::Options,
                    content_type: None,
                    body: None,
                    headers: Vec::new(),
                };
                let goal = Goal::new(b.gate.clone(), step).after(env.reinvite_gap);
                b.goals_mut(role).push(goal);
                b.gate = answered("pinged", role, "OPTIONS", b.add_ping(role));
            }
        }
    }

    fn compile_transfer(
        &self,
        env: &CallEnv<'_>,
        b: &mut Build,
        transfer: &Transfer,
    ) -> Result<(), StepError> {
        if env.charlie.is_none() {
            return Err(StepError::UnexpectedKind {
                who: self.id.to_string(),
                detail: "bound without a charlie leg".to_string(),
            });
        }
        let refer_to = env.refer_to().ok_or_else(|| StepError::UnexpectedKind {
            who: self.id.to_string(),
            detail: "no charlie for Refer-To".to_string(),
        })?;
        let (refer_key, happy, decline_code) = match transfer {
            Transfer::Blind { refer_key } => (refer_key, true, 0),
            Transfer::BlindDeclined { refer_key, code } => (refer_key, false, *code),
        };
        let authorization = self.binder.refer_authorization(env, refer_key);

        // The current dialog's remote end (bob) drives the REFER once both
        // ends are established, after a realistic talk dwell.
        let bob = b.callee_mut("bob");
        bob.goals.push(
            Goal::new(
                Barrier::AllConfirmed(&["alice", "bob"]),
                GoalStep::Refer { refer_to, authorization },
            )
            .after(env.reinvite_gap),
        );
        if happy {
            bob.feed.on_refer_accepted = Feed::new(Some("time_to_202"), Some("referred"));
        }

        b.callees.push(ActorSpec {
            role: "charlie",
            agent: env.callee_agent("charlie").clone().transfer_target(),
            disposition: if happy {
                Disposition::RingThenAnswer { ring: Duration::ZERO }
            } else {
                Disposition::Reject(decline_code)
            },
            media: MediaState::answer(ANSWER_SDP),
            goals: vec![],
            invite_targets: vec![],
            via: None,
            feed: if happy {
                CtxFeed {
                    on_answer_sent: Feed::new(Some("time_to_charlie_200"), Some("transferred")),
                    ..CtxFeed::default()
                }
            } else {
                CtxFeed::default()
            },

            cseq: None,
            delayed: vec![],
            claim: None,
        });

        if happy {
            b.phases.push(phase("referred", |s| {
                s.leg("bob").subflow(SUBFLOW_REFER).is_some_and(|f| f >= SubflowState::Answered)
            }));
            b.phases.push(phase("transferred", |s| s.leg_at_least("charlie", LegPhase::Confirmed)));
            b.phases.push(phase("merged", merged));
            b.gate = Barrier::pred("merged", merged);
        } else {
            b.phases.push(phase("declined", declined));
            b.gate = Barrier::pred("declined", declined);
            b.expect = Expect::one(TRANSFER_DECLINED);
        }
        Ok(())
    }
}

impl ActorScenario for ShapePlan {
    fn id(&self) -> ScenarioId {
        self.id
    }

    fn build(&self, env: &CallEnv<'_>) -> Result<ActorCall, StepError> {
        self.compile(env)
    }
}

#[cfg(test)]
mod tests;
