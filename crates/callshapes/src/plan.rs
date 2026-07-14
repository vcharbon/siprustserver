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

use scenario_harness::actor::{
    phase, ActorCall, ActorScenario, ActorSpec, Barrier, BarrierPhase, CtxFeed, Disposition,
    Expect, Feed, Goal, GoalStep, LegPhase, MediaState, SettleBarrier, StateInner, SubflowState,
    SUBFLOW_REALIGN, SUBFLOW_REFER, SUBFLOW_RENEG,
};
use scenario_harness::realcall::{CallEnv, ScenarioId};
use scenario_harness::{StepError, ANSWER_SDP, OFFER_SDP};

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
    /// TERMINAL: the callee rejects with `code` and that IS the call — no
    /// dialog, no teardown ([`Expect::Reject`]).
    RejectTerminal { code: u16 },
    /// TERMINAL: the caller CANCELs once ringing (RFC 3261 §9.1) — no dialog,
    /// no teardown ([`Expect::AbandonedEarly`]).
    AbandonAfterRinging,
}

/// An in-dialog script or a dialog-changing transfer — the chainable stages
/// after establishment.
#[derive(Debug, Clone)]
pub enum Stage {
    Script(Script),
    Transfer(Transfer),
}

/// A deliberate in-dialog sequence the caller drives on the CURRENT dialog.
#[derive(Debug, Clone, Copy)]
pub enum Script {
    /// `n` sequential delayed-offer re-INVITE renegotiations (each guarded on
    /// the previous one completing; each dwells `reinvite_gap`). Replaces the
    /// gate with `reinvited`.
    Reinvite { n: u32 },
    /// One in-dialog UPDATE (RFC 3311) renegotiation (its 200 completes it —
    /// no ACK). Replaces the gate with `updated`.
    UpdatePostConnect,
    /// ONE in-dialog OPTIONS keepalive ping, its 200 read inline. Gate
    /// unchanged.
    KeepaliveOnce,
    /// OPTIONS keepalive pings every `options_cadence` until `options_hold`
    /// elapses (both env knobs). Gate unchanged; the goal cursor advances only
    /// after the hold, so a following teardown is naturally held back.
    KeepaliveLoop,
}

/// A blind transfer (REFER) — moves the current dialog to the transfer target.
#[derive(Debug, Clone)]
pub enum Transfer {
    /// The target answers; the B2BUA's post-transfer media merge re-INVITEs
    /// the caller and the target as parallel sub-flows. Replaces the gate with
    /// `merged` (the conjunction of both realigns).
    Blind { refer_key: String },
    /// The target DECLINES with `code` (603); the transfer fails, the original
    /// dialog stays up. Replaces the gate with `declined`;
    /// [`Expect::TransferDeclined`].
    BlindDeclined { refer_key: String, code: u16 },
}

/// A dwell knob resolved against the per-call env (the realistic-timing
/// sleeps every dwell in the historic bodies came from).
#[derive(Debug, Clone, Copy)]
pub enum DwellKnob {
    None,
    TalkTime,
    ReinviteGap,
    LongHold,
}

impl DwellKnob {
    fn resolve(self, env: &CallEnv<'_>) -> Duration {
        match self {
            DwellKnob::None => Duration::ZERO,
            DwellKnob::TalkTime => env.talk_time,
            DwellKnob::ReinviteGap => env.reinvite_gap,
            DwellKnob::LongHold => env.long_hold,
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
    /// No teardown goal — only legal for terminal establishments.
    None,
}

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
struct Build {
    caller_goals: Vec<Goal>,
    caller_feed: CtxFeed,
    /// Upgraded to `full` (answer SDP) when a stage answers renegotiations
    /// reactively (re-INVITE scripts, the post-transfer realign).
    caller_needs_answer_sdp: bool,
    callees: Vec<ActorSpec>,
    phases: Vec<BarrierPhase>,
    expect: Expect,
    /// The barrier gating the next deliberate action on the current dialog.
    gate: Barrier,
}

impl Build {
    fn callee_mut(&mut self, role: &'static str) -> &mut ActorSpec {
        self.callees
            .iter_mut()
            .find(|a| a.role == role)
            .expect("stage references an undeclared callee role")
    }
}

/// `alice` + `winner` both confirmed — the `established` gate/phase, with the
/// winning callee role captured (bob, or bob2 after a reroute).
fn established_pred(winner: &'static str) -> impl Fn(&StateInner) -> bool + Send + Sync + Clone {
    move |s: &StateInner| {
        s.leg_at_least("alice", LegPhase::Confirmed) && s.leg_at_least(winner, LegPhase::Confirmed)
    }
}

/// The caller's renegotiation (re-INVITE 2xx ACKed / UPDATE 200) completed.
fn reneg_done(s: &StateInner) -> bool {
    s.leg("alice").subflow(SUBFLOW_RENEG).is_some_and(|f| f >= SubflowState::Confirmed)
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

impl ShapePlan {
    /// Whether the establishment terminates the call by itself (no dialog to
    /// script or tear down).
    fn is_terminal(&self) -> bool {
        matches!(
            self.establish,
            Establishment::RejectTerminal { .. } | Establishment::AbandonAfterRinging
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
        Ok(())
    }

    fn compile(&self, env: &CallEnv<'_>) -> Result<ActorCall, StepError> {
        self.validate()?;
        let mut b = Build {
            caller_goals: Vec::new(),
            caller_feed: CtxFeed { ringing_gate: self.ringing_gate, ..CtxFeed::default() },
            caller_needs_answer_sdp: false,
            callees: Vec::new(),
            phases: Vec::new(),
            expect: Expect::HappyBye,
            gate: Barrier::None,
        };

        self.compile_establishment(env, &mut b)?;

        for stage in &self.stages {
            match stage {
                Stage::Script(s) => self.compile_script(env, &mut b, s),
                Stage::Transfer(t) => self.compile_transfer(env, &mut b, t)?,
            }
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
            Teardown::None => {} // legal only for terminals — validate() gates
        }

        let media = if b.caller_needs_answer_sdp {
            MediaState::full(OFFER_SDP, ANSWER_SDP)
        } else {
            MediaState::offer(OFFER_SDP)
        };
        let mut actors = vec![ActorSpec {
            role: "alice",
            agent: env.alice.clone(),
            disposition: Disposition::Caller,
            media,
            goals: b.caller_goals,
            invite_targets: vec![("bob", env.bob.clone())],
            via: None,
            feed: b.caller_feed,
        }];
        actors.extend(b.callees);

        Ok(ActorCall {
            actors,
            plan: b.phases,
            settle: SettleBarrier::default_ceiling(),
            expect: b.expect,
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
                });
                let est = established_pred("bob");
                b.phases.push(phase("established", est.clone()));
                b.gate = Barrier::pred("established", est);
            }
            Establishment::RerouteOnReject { reject, winner_reliable } => {
                let bob2 = env
                    .bob2
                    .ok_or_else(|| StepError::UnexpectedKind {
                        who: self.id.to_string(),
                        detail: "bound without a bob2 leg".to_string(),
                    })?
                    .clone();
                let mut plan = self.binder.invite_plan(
                    env,
                    RouteIntent::FailoverOnReject { targets: &["bob", "bob2"] },
                );
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
                // The primary callee rejects (its reject-ACK absorbed without
                // confirming), triggering the SUT's failover to bob2.
                b.callees.push(ActorSpec {
                    role: "bob",
                    agent: env.bob.clone(),
                    disposition: Disposition::Reject(reject),
                    media: MediaState::none(),
                    goals: vec![],
                    invite_targets: vec![],
                    via: None,
                    feed: CtxFeed::default(),
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
                });
                let est = established_pred("bob2");
                b.phases.push(phase("established", est.clone()));
                b.gate = Barrier::pred("established", est);
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
                });
                b.phases
                    .push(phase("rejected", |s| s.leg_at_least("bob", LegPhase::Terminated)));
                b.expect = Expect::Reject(code);
            }
            Establishment::AbandonAfterRinging => {
                let plan = self.binder.invite_plan(env, RouteIntent::Direct { target: "bob" });
                let ringing = |s: &StateInner| s.leg_at_least("alice", LegPhase::Early);
                b.caller_goals.push(Goal::new(
                    Barrier::None,
                    GoalStep::Invite { callee: "bob", plan: Some(plan) },
                ));
                b.caller_goals.push(Goal::new(Barrier::pred("ringing", ringing), GoalStep::Cancel));
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
                });
                b.phases.push(phase("ringing", ringing));
                b.expect = Expect::AbandonedEarly;
            }
        }
        Ok(())
    }

    fn compile_script(&self, env: &CallEnv<'_>, b: &mut Build, script: &Script) {
        match *script {
            Script::Reinvite { n } => {
                for i in 0..n {
                    // The first re-INVITE waits on the incoming gate; each
                    // subsequent one on the previous renegotiation completing
                    // (origination resets the reneg sub-flow, so `reinvited`
                    // re-arms per cycle).
                    let guard = if i == 0 {
                        b.gate.clone()
                    } else {
                        Barrier::pred("reinvited", reneg_done)
                    };
                    b.caller_goals
                        .push(Goal::new(guard, GoalStep::Reinvite).after(env.reinvite_gap));
                }
                b.caller_feed.on_reinvite_ok =
                    Feed::new(Some("time_to_reinvite_200"), Some("reinvited"));
                b.caller_needs_answer_sdp = true;
                b.gate = Barrier::pred("reinvited", reneg_done);
            }
            Script::UpdatePostConnect => {
                b.caller_goals
                    .push(Goal::new(b.gate.clone(), GoalStep::Update).after(env.reinvite_gap));
                b.caller_feed.on_update_ok =
                    Feed::new(Some("time_to_update_200"), Some("updated"));
                b.gate = Barrier::pred("updated", reneg_done);
            }
            Script::KeepaliveOnce => {
                b.caller_goals.push(Goal::new(b.gate.clone(), GoalStep::Options));
                b.caller_feed.on_options_ok =
                    Feed::new(Some("time_to_options_200"), Some("keepalive_ack"));
            }
            Script::KeepaliveLoop => {
                b.caller_goals.push(Goal::new(
                    b.gate.clone(),
                    GoalStep::EveryOptions { cadence: env.options_cadence, hold: env.options_hold },
                ));
                b.caller_feed.on_options_ok =
                    Feed::new(Some("time_to_options_200"), Some("keepalive_ack"));
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
            agent: env.callee_agent("charlie").clone(),
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
        });

        // The caller answers the post-transfer realign re-INVITE reactively
        // (answer SDP on every realign 200 — RFC 3264 §5).
        b.caller_needs_answer_sdp = true;

        if happy {
            b.phases.push(phase("referred", |s| {
                s.leg("bob").subflow(SUBFLOW_REFER).is_some_and(|f| f >= SubflowState::Answered)
            }));
            b.phases
                .push(phase("transferred", |s| s.leg_at_least("charlie", LegPhase::Confirmed)));
            b.phases.push(phase("merged", merged));
            b.gate = Barrier::pred("merged", merged);
        } else {
            b.phases.push(phase("declined", declined));
            b.gate = Barrier::pred("declined", declined);
            b.expect = Expect::TransferDeclined;
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
mod tests {
    use super::*;
    use crate::shapes;

    fn detail(err: StepError) -> String {
        match err {
            StepError::UnexpectedKind { detail, .. } => detail,
            other => panic!("expected UnexpectedKind, got {other:?}"),
        }
    }

    /// Every shipped composition is structurally valid.
    #[test]
    fn shipped_catalog_validates() {
        let b = shapes::default_binder;
        for plan in [
            shapes::basic_call(b()),
            shapes::reinvite(b()),
            shapes::prack_update(b()),
            shapes::rerouting_prack(b()),
            shapes::options_hold(b()),
            shapes::long_call(b()),
            shapes::refer(b(), "k"),
            shapes::refer_charlie_reject(b(), "k"),
            shapes::invite_reject(b()),
            shapes::abandon_ringing(b()),
        ] {
            plan.validate().unwrap_or_else(|e| panic!("{} invalid: {e:?}", plan.id));
        }
    }

    /// A terminal establishment admits neither stages nor a teardown, and a
    /// non-terminal chain MUST tear down (the "properly terminated" rule).
    #[test]
    fn validate_rejects_miscomposed_chains() {
        let mut term = shapes::invite_reject(shapes::default_binder());
        term.stages = vec![Stage::Script(Script::Reinvite { n: 1 })];
        assert_eq!(detail(term.validate().unwrap_err()), "stage chained after a terminal establishment");

        let mut term = shapes::abandon_ringing(shapes::default_binder());
        term.teardown =
            Teardown::CallerBye { after: DwellKnob::None, feed: ByeFeed::CheckpointAndPhase };
        assert_eq!(
            detail(term.validate().unwrap_err()),
            "teardown declared on a terminal establishment"
        );

        let mut leaky = shapes::basic_call(shapes::default_binder());
        leaky.teardown = Teardown::None;
        assert_eq!(
            detail(leaky.validate().unwrap_err()),
            "non-terminal shape with no teardown (calls must terminate)"
        );
    }
}
