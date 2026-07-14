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
    Expect, ExpectBranch, Feed, Goal, GoalStep, LegPhase, MediaState, SettleBarrier, StateInner,
    SubflowState, SUBFLOW_EARLY, SUBFLOW_REALIGN, SUBFLOW_REFER, SUBFLOW_RENEG,
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
    /// NO-ANSWER-triggered failover (newkahneed-047): the primary callee rings
    /// (`180`) then NEVER answers ([`Disposition::RingThenSilent`]); the SUT's
    /// own no-answer timer fires, CANCELs the primary (whose `487` settles the
    /// leg cleanly) and fails over to `bob2`, which answers reliably
    /// (`winner_reliable`) or plainly. Routing rides the SAME
    /// [`RouteIntent::FailoverOnReject`](crate::binder::RouteIntent) seam as
    /// [`RerouteOnReject`](Self::RerouteOnReject), so a downstream binder maps
    /// it unchanged; `no_answer_sec` is the SUT ring-timer knob a binder MAY
    /// arm client-side ([`RouteBinder::invite_plan_no_answer`]) — a platform
    /// whose SUT owns its own ring timer ignores it. The primary's dwell is
    /// bounded by the SUT timer, never by the callee.
    RerouteOnNoAnswer { no_answer_sec: u32, winner_reliable: bool },
    /// TRUE forking (RFC 3261 §12.1.2): the callee `bob` emits one 18x per
    /// DISTINCT explicit To-tag in `tags` on its ONE INVITE server transaction
    /// (as if a downstream proxy had forked), then answers `200` under
    /// `winner`; `loser_late_200` optionally emits a late `200` under a losing
    /// tag (§13.2.2.4 — the caller ACKs then BYEs it). `reliable` makes each
    /// fork's 18x a `183` (`Require:100rel`) the caller PRACKs per early dialog.
    /// **Only valid under the SUT's transparent CORE relay**
    /// (`relay_first_18x_to_180 = None`, the default `B2buaSut`): any
    /// `relayFirst18x` masking mode collapses the forks to one To-tag and the
    /// distinct-early-dialog behavior cannot be exercised.
    Forked {
        tags: &'static [&'static str],
        winner: &'static str,
        reliable: bool,
        loser_late_200: Option<&'static str>,
    },
    /// TERMINAL: the callee rejects with `code` and that IS the call — no
    /// dialog, no teardown ([`Expect::Reject`]).
    RejectTerminal { code: u16 },
    /// TERMINAL: the caller CANCELs once ringing (RFC 3261 §9.1) — no dialog,
    /// no teardown ([`Expect::AbandonedEarly`]).
    AbandonAfterRinging,
    /// CANCEL×200 CROSSING (C2/E5): the caller CANCELs while the callee answers,
    /// so the terminal outcome is EITHER confirmed (the 200 crossed the CANCEL,
    /// §9.2 — the call tears down) OR cancelled (the CANCEL won — 487). A
    /// branch-aware race oracle ([`Expect::EitherOf`]); the load lane accepts
    /// whichever legal branch occurred. Terminal-style (no stages, no separate
    /// teardown): the branch-conditional teardown rides the callee.
    CancelAnswerCrossing,
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
    /// CROSSING BYE (C3/S3, RFC 3261 §15.1.2): the caller AND the winning callee
    /// both BYE on the final gate at the same instant, so each BYE is in flight
    /// when the peer's arrives. Each reactor 200s the inbound BYE while its own
    /// is outstanding; both legs terminate. The caller stamps the same
    /// checkpoint/phase feed as a plain `CallerBye`.
    CrossingBye { after: DwellKnob },
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
    /// The winning callee role (`"bob"`, or `"bob2"` after a reroute) — the leg
    /// a crossing-BYE teardown gives its own BYE goal to.
    winner: &'static str,
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
            if tags.len() < 2 {
                return Err(err("Forked needs >= 2 distinct fork tags"));
            }
            if !tags.contains(&winner) {
                return Err(err("Forked winner must be one of the declared tags"));
            }
            if let Some(loser) = loser_late_200 {
                if loser == winner || !tags.contains(&loser) {
                    return Err(err("Forked loser_late_200 must be a declared tag != winner"));
                }
            }
        }
        for stage in &self.stages {
            if let Stage::Script(Script::Reinvite { n }) = stage {
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
            winner: "bob",
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
                    Establishment::RerouteOnNoAnswer { no_answer_sec, winner_reliable } => (
                        winner_reliable,
                        // 047: the primary rings then stays silent; the SUT's
                        // no-answer timer CANCELs it (→ 487, a clean settle) and
                        // walks the plan to bob2.
                        Disposition::RingThenSilent,
                        self.binder.invite_plan_no_answer(
                            env,
                            RouteIntent::FailoverOnReject { targets: &["bob", "bob2"] },
                            i64::from(no_answer_sec),
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
                // INVITE server txn, then 200 under `winner` (+ optional
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
                });
                let est = established_pred("bob");
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
            Establishment::CancelAnswerCrossing => {
                // C2/E5: alice INVITEs then CANCELs (gated on ringing, after a
                // dwell timed near bob's answer), so the CANCEL and the 200
                // cross. The outcome is a branch-aware race oracle; the load
                // lane accepts whichever legal branch occurred. The
                // branch-conditional teardown rides bob (ByeIfConfirmed), so a
                // 487 never trips alice's incidental-failure path — she keeps
                // only [Invite, Cancel].
                const E5_BRANCHES: &[ExpectBranch] =
                    &[ExpectBranch::Answered, ExpectBranch::Cancelled { code: 487 }];
                let plan = self.binder.invite_plan(env, RouteIntent::Direct { target: "bob" });
                let ringing = |s: &StateInner| s.leg_at_least("alice", LegPhase::Early);
                b.caller_goals.push(Goal::new(
                    Barrier::None,
                    GoalStep::Invite { callee: "bob", plan: Some(plan) },
                ));
                b.caller_goals.push(
                    Goal::new(Barrier::pred("ringing", ringing), GoalStep::Cancel)
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
                });
                b.phases.push(phase("ringing", ringing));
                b.expect = Expect::EitherOf(E5_BRANCHES);
            }
        }
        Ok(())
    }

    fn compile_script(&self, env: &CallEnv<'_>, b: &mut Build, script: &Script) {
        match *script {
            Script::Reinvite { n } => {
                // C6: serialize N delayed-offer re-INVITE cycles. Cycle 0 fires
                // on the incoming gate; cycle `i` (i>=1) waits until the leg has
                // COMPLETED i prior cycles (`reneg_count() >= i`), so no two
                // re-INVITEs are ever in flight (that would glare into a 491).
                // `reneg_count` is the cardinality of the caller's grow-only
                // completed-reneg set (state.rs `RenegCompleted`) — a real
                // per-cycle counter, unlike the monotone SUBFLOW_RENEG latch.
                // For n=1 this is byte-for-byte the old `reneg_done` gate (the
                // count hits 1 the same instant the latch confirms).
                for i in 0..n {
                    let guard = if i == 0 {
                        b.gate.clone()
                    } else {
                        Barrier::pred("reinvited", move |s| s.leg("alice").reneg_count() >= i)
                    };
                    b.caller_goals
                        .push(Goal::new(guard, GoalStep::Reinvite).after(env.reinvite_gap));
                }
                b.caller_feed.on_reinvite_ok =
                    Feed::new(Some("time_to_reinvite_200"), Some("reinvited"));
                b.caller_needs_answer_sdp = true;
                b.gate = Barrier::pred("reinvited", move |s| s.leg("alice").reneg_count() >= n);
            }
            Script::UpdatePostConnect => {
                b.caller_goals
                    .push(Goal::new(b.gate.clone(), GoalStep::Update).after(env.reinvite_gap));
                b.caller_feed.on_update_ok =
                    Feed::new(Some("time_to_update_200"), Some("updated"));
                b.gate = Barrier::pred("updated", reneg_done);
            }
            // C5 (RFC 3311 §5.1): the caller UPDATEs the reliable EARLY dialog
            // BEFORE the final 200 (validate() guarantees Establishment::Reliable
            // — bob is a ReliableAnswer callee we upgrade to hold its INVITE 200
            // until the UPDATE completes). The UPDATE fires once the caller is
            // Early (the 183 is in + PRACKed); the gate stays `established`.
            Script::UpdateEarly => {
                let early = |s: &StateInner| s.leg("alice").subflow(SUBFLOW_EARLY).is_some();
                b.caller_goals
                    .push(Goal::new(Barrier::pred("early", early), GoalStep::UpdateEarly));
                b.caller_feed.on_update_ok =
                    Feed::new(Some("time_to_update_200"), Some("updated"));
                b.callee_mut("bob").disposition = Disposition::ReliableAnswerEarlyUpdate;
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
            shapes::reinvite_n(b(), "reinvite10", 10),
            shapes::crossing_bye(b()),
            shapes::forked(b()),
            shapes::forked_loser_late_200(b()),
            shapes::forked_reliable(b()),
            shapes::prack_update(b()),
            shapes::rerouting_prack(b()),
            shapes::rerouting_noanswer(b()),
            shapes::options_hold(b()),
            shapes::long_call(b()),
            shapes::refer(b(), "k"),
            shapes::refer_charlie_reject(b(), "k"),
            shapes::invite_reject(b()),
            shapes::abandon_ringing(b()),
            shapes::cancel_answer_crossing(b()),
            shapes::prack_update_early(b()),
        ] {
            plan.validate().unwrap_or_else(|e| panic!("{} invalid: {e:?}", plan.id));
        }
    }

    /// C6: an N-cycle re-INVITE script is now valid for any `n >= 1` (the
    /// per-cycle reneg counter serializes it — the `n != 1` guardrail is gone);
    /// only `n == 0` (a no-op script) is rejected.
    #[test]
    fn reinvite_n_validates_and_rejects_zero() {
        shapes::reinvite_n(shapes::default_binder(), "reinvite10", 10)
            .validate()
            .expect("n=10 is valid now the per-cycle barrier exists");

        let mut plan = shapes::reinvite_n(shapes::default_binder(), "reinvite0", 1);
        plan.stages = vec![Stage::Script(Script::Reinvite { n: 0 })];
        assert_eq!(detail(plan.validate().unwrap_err()), "Reinvite n must be >= 1");
    }

    /// C5: an early UPDATE is valid only on a reliable establishment (it needs
    /// the PRACKed reliable early dialog to attach to, RFC 3311 §5.1).
    #[test]
    fn update_early_requires_a_reliable_establishment() {
        // Reliable: valid.
        shapes::prack_update_early(shapes::default_binder())
            .validate()
            .expect("UpdateEarly on a Reliable establishment is valid");

        // Transparent (no reliable early dialog): rejected.
        let mut plan = shapes::prack_update_early(shapes::default_binder());
        plan.establish = Establishment::Transparent;
        assert_eq!(
            detail(plan.validate().unwrap_err()),
            "UpdateEarly requires a reliable establishment (early dialog)"
        );
    }

    /// C1/E3: Forked validation — winner and loser must be declared tags,
    /// loser distinct from winner, ≥2 tags.
    #[test]
    fn forked_validates_tags() {
        let mk = |tags, winner, loser| ShapePlan {
            id: "fk",
            binder: shapes::default_binder(),
            establish: Establishment::Forked { tags, winner, reliable: false, loser_late_200: loser },
            stages: vec![],
            teardown: Teardown::CallerBye { after: DwellKnob::None, feed: ByeFeed::CheckpointAndPhase },
            ringing_gate: true,
            stamp_connected: true,
        };
        mk(&["a", "b"], "a", None).validate().expect("valid fork");
        assert_eq!(
            detail(mk(&["a"], "a", None).validate().unwrap_err()),
            "Forked needs >= 2 distinct fork tags"
        );
        assert_eq!(
            detail(mk(&["a", "b"], "c", None).validate().unwrap_err()),
            "Forked winner must be one of the declared tags"
        );
        assert_eq!(
            detail(mk(&["a", "b"], "a", Some("a")).validate().unwrap_err()),
            "Forked loser_late_200 must be a declared tag != winner"
        );
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
