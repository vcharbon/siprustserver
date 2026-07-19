//! Per-endpoint **actor** test harness — an alternative executor for the
//! portable multi-party real-call scenarios ([`crate::realcall`]) that replaces
//! linear choreography with N autonomous per-endpoint UA actors coordinated by
//! a controller through observed-state barriers, with the verdict gated by an
//! acknowledgement ledger behind a 32 s settle barrier.
//!
//! Two endurance failures motivated it (see the design artifact / the
//! `loadgen-loss-model-resilience` memory): a dropped datagram cascading into a
//! total-call failure because the recv-sequence WAS the control flow, and a lost
//! NOTIFY leaving a permanent CSeq gap because the verdict raced the protocol's
//! own re-emission. Both dissolve here: the reactor stays reactive so a
//! late/reordered/retransmitted datagram is always consumed, and the settle
//! barrier holds the verdict until the ledger closes (every in-dialog request
//! acknowledged) or the 32 s ceiling elapses.
//!
//! # Concurrency model — joined futures on ONE task (no spawn)
//!
//! Actors are concurrent *futures* joined within the one per-call task via a
//! [`FuturesUnordered`], NOT `tokio::spawn`ed — for determinism under the paused
//! clock and no `'static` gymnastics. [`drive_actors`] `?`s the first fatal
//! actor error and otherwise parks (an actor reaching its exit cleanly must NOT
//! collapse the join — the B1 correction). Everything is `Send`; both lanes use
//! plain `tokio::time` (no `SettleDriver` — B2/B4).
//!
//! P0 ships the substrate only; nothing outside this module references it yet
//! (the [`crate::realcall`] bodies keep their linear form until P1's adapter).

mod actor;
mod goals;
mod ledger;
pub mod scenarios;
mod settle;
mod spec;
mod state;

use std::sync::Arc;
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use futures::FutureExt;

use crate::realcall::{CallCtx, CallScope, ChallengeResponder};
use crate::StepError;

pub use actor::{
    run_actor, ActorSpec, ActorState, Automatics, CtxFeed, Disposition, Feed, MediaState,
    SUBFLOW_EARLY, SUBFLOW_REALIGN, SUBFLOW_RENEG, SUBFLOW_REFER,
};
pub use goals::{Barrier, BodyExpect, EarlyId, FinalAssert, Goal, GoalCursor, GoalStep, RequestKind};
pub use ledger::{ObligationKey, ObligationKind, ObligationLedger};
pub use settle::{SettleBarrier, SettleVerdict, T1};
pub use spec::{
    into_result, originating_role, run_actor_scenario, ActorCall, ActorScenario, Expect,
    ExpectBranch, STEP_TIMEOUT,
};
pub use state::{
    await_pred, LegObservation, LegPhase, Observation, ObservedState, RecordedFinal, ReplayEntry,
    ResponseFact, StateInner, SubflowState,
};

/// One phase barrier in the controller's plan — a named predicate over the
/// observed state that must hold (bounded by the controller's `step_timeout`)
/// before the call proceeds. The name is a bounded label for the timeout
/// `StepError`.
pub struct BarrierPhase {
    name: &'static str,
    pred: Box<dyn Fn(&StateInner) -> bool + Send + Sync>,
}

/// Build a [`BarrierPhase`] from a name + predicate.
pub fn phase(
    name: &'static str,
    pred: impl Fn(&StateInner) -> bool + Send + Sync + 'static,
) -> BarrierPhase {
    BarrierPhase { name, pred: Box::new(pred) }
}

/// The verdict of one actor-driven call.
#[derive(Debug)]
pub enum CallVerdict {
    /// Every phase barrier held, the call tore down, and the ledger settled.
    Ok,
    /// A fatal step / barrier-timeout aborted the call (an actor error or an
    /// unmet phase barrier).
    Failed(StepError),
    /// The call tore down but the settle barrier's ceiling elapsed with
    /// obligations still open — names each one.
    Settle(Vec<String>),
}

impl CallVerdict {
    /// Whether the call reached the fully-settled happy path.
    pub fn is_ok(&self) -> bool {
        matches!(self, CallVerdict::Ok)
    }
}

/// A declarative multi-party call: the actor specs + the barrier plan + the
/// settle barrier. The runner ([`run_call`]) turns it into joined actor futures
/// driven to a [`CallVerdict`].
pub struct CallPlan {
    pub actors: Vec<ActorSpec>,
    pub plan: Vec<BarrierPhase>,
    pub settle: SettleBarrier,
    /// The lane-chosen stack automatics for scripted endpoints — emitted
    /// identically on every lane (the cross-lane behavior contract).
    pub automatics: Automatics,
}

/// The controller: owns the shared observed state, the barrier plan, the settle
/// barrier, and each actor's teardown scope. Drives the phase barriers → the
/// `torn_down` barrier → the settle barrier to a verdict, with the actor
/// reactors running concurrently throughout.
struct CallController {
    obs: ObservedState,
    plan: Vec<BarrierPhase>,
    settle: SettleBarrier,
    /// The per-barrier wait bound (the same 32 s ceiling the actors' goal
    /// guards use — [`spec::STEP_TIMEOUT`]); a barrier that never holds fails
    /// the call rather than hanging.
    step_timeout: Duration,
}

impl CallController {
    /// Drive the plan barriers, then wait for teardown, then run the settle
    /// barrier — the reactors stay alive throughout (this future runs in the
    /// same `select!` as [`drive_actors`], so a re-emitted request is consumed
    /// and acked DURING settle).
    async fn drive_to_verdict(&self) -> CallVerdict {
        for phase in &self.plan {
            let deadline = tokio::time::Instant::now() + self.step_timeout;
            let held = await_pred(&self.obs, phase.name, |s| (phase.pred)(s), deadline).await;
            if let Err(e) = held {
                return CallVerdict::Failed(e);
            }
        }
        // Teardown: every leg terminated. Bounded separately (teardown can take
        // the whole flow after the last phase barrier).
        let deadline = tokio::time::Instant::now() + self.step_timeout;
        if let Err(e) =
            await_pred(&self.obs, "torn_down", |s| s.all_terminated(), deadline).await
        {
            return CallVerdict::Failed(e);
        }
        match self.settle.wait(&self.obs).await {
            SettleVerdict::Ok => CallVerdict::Ok,
            SettleVerdict::Fail(open) => CallVerdict::Settle(open),
        }
    }
}

/// Join the actor futures on ONE task. Returns the FIRST fatal actor error;
/// once every actor has resolved cleanly it parks forever (`pending`), so the
/// controller's verdict — not a cleanly-finished actor — decides the call (B1).
async fn drive_actors(
    mut actors: FuturesUnordered<impl std::future::Future<Output = Result<(), StepError>>>,
) -> StepError {
    while let Some(r) = actors.next().await {
        if let Err(e) = r {
            return e;
        }
    }
    std::future::pending().await
}

/// Run one declarative [`CallPlan`] to a verdict with its own fresh observed
/// state + per-call recorder — the self-contained form (the P0 toy call).
/// Adapter surfaces use [`run_call_with`] to share the driver's `CallCtx` and
/// read the observed state after the verdict.
pub async fn run_call(call: CallPlan, step_timeout: Duration) -> CallVerdict {
    let ctx = CallCtx::new();
    run_call_with(call, ObservedState::new(), &ctx, step_timeout, None).await
}

/// Run one declarative [`CallPlan`] to a verdict over a caller-provided
/// observed state (readable afterwards — the `Expect::Reject` mapping) and
/// per-call recorder (the load driver's `CallCtx`). Builds one teardown scope
/// per actor and the joined actor futures; races the controller's verdict
/// against them; then tears down every scope (a no-op on a clean call,
/// best-effort CANCEL/BYE on an aborted one).
///
/// `challenge_responder` is the per-call deferred-auth adapter (RFC 3261 §22.2,
/// [`CallEnv::challenge_responder`](crate::realcall::CallEnv)) — wired onto each
/// actor so the caller's establishing INVITE honours a `401`/`407` challenge;
/// `None` (the default / the toy call) keeps a challenge classified as
/// `status_401/407` unchanged.
///
/// **Panic-safe:** the scopes are owned HERE, outside a `catch_unwind` around
/// the drive, so a panicking actor still gets its call torn down (no leaked
/// dialog on the SUT) before the panic resumes to the caller's own
/// `catch_unwind` (the load driver's per-call boundary, which classifies it).
pub async fn run_call_with(
    call: CallPlan,
    obs: ObservedState,
    ctx: &CallCtx,
    step_timeout: Duration,
    challenge_responder: Option<Arc<dyn ChallengeResponder>>,
) -> CallVerdict {
    let mut scopes = Vec::with_capacity(call.actors.len());
    let mut states = Vec::with_capacity(call.actors.len());
    let automatics = call.automatics;
    for spec in call.actors {
        let scope = Arc::new(CallScope::new());
        states.push(ActorState::from_spec(
            spec,
            obs.clone(),
            scope.clone(),
            ctx,
            step_timeout,
            challenge_responder.clone(),
            automatics,
        ));
        scopes.push(scope);
    }

    let controller = CallController {
        obs: obs.clone(),
        plan: call.plan,
        settle: call.settle,
        step_timeout,
    };

    let drive = async {
        let actors: FuturesUnordered<_> = states.into_iter().map(run_actor).collect();
        tokio::select! {
            v = controller.drive_to_verdict() => v,
            e = drive_actors(actors) => CallVerdict::Failed(e),
        }
    };
    let result = std::panic::AssertUnwindSafe(drive).catch_unwind().await;

    // The loser future is dropped; teardown acts on whatever each scope last
    // registered (Terminated → no-op on the happy path) — including after a
    // caught panic, so a panicking actor never leaks SUT state.
    for scope in &scopes {
        scope.teardown().await;
    }
    match result {
        Ok(verdict) => verdict,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::{Harness, ANSWER_SDP, OFFER_SDP};

    /// The P0 exit gate: two actors (alice caller + bob `RingThenAnswer`) reach
    /// `torn_down` and settle OK under a paused clock — the reactor answers, the
    /// goal cursor originates + hangs up, the controller drives the barriers, and
    /// the RFC hard gate at `finish()` confirms the wire stayed compliant.
    #[tokio::test(start_paused = true)]
    async fn two_actor_toy_call_reaches_torn_down() {
        let h = Harness::new("actor-toy-call").describe(
            "P0 substrate proof: alice originates, bob rings-then-answers, the \
             controller drives established → torn_down → settled entirely through \
             the reactive actor runner",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let call = CallPlan {
            actors: vec![
                ActorSpec {
                    role: "alice",
                    agent: alice.clone(),
                    disposition: Disposition::Caller,
                    media: MediaState::offer(OFFER_SDP),
                    goals: vec![
                        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                        Goal::new(
                            Barrier::AllConfirmed(&["alice", "bob"]),
                            GoalStep::Bye,
                        ),
                    ],
                    invite_targets: vec![("bob", bob.clone())],
                    via: None,
                    feed: CtxFeed::default(),
                },
                ActorSpec {
                    role: "bob",
                    agent: bob.clone(),
                    disposition: Disposition::RingThenAnswer {
                        ring: Duration::from_millis(500),
                    },
                    media: MediaState::answer(ANSWER_SDP),
                    goals: vec![],
                    invite_targets: vec![],
                    via: None,
                    feed: CtxFeed::default(),
                },
            ],
            plan: vec![phase("established", |s| {
                s.leg_at_least("alice", LegPhase::Confirmed)
                    && s.leg_at_least("bob", LegPhase::Confirmed)
            })],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        };

        let verdict = run_call(call, Duration::from_secs(5)).await;
        assert!(verdict.is_ok(), "the toy call must settle OK, got {verdict:?}");

        h.finish().await;
    }

    /// C3 crossing BYE (RFC 3261 §15.1.2): BOTH ends originate a BYE at the same
    /// paused-clock instant (both gated on `AllConfirmed`), so each BYE is in
    /// flight when the peer's BYE arrives. The reactor must 200 the inbound BYE
    /// even though its own BYE is still outstanding — each end terminates, the
    /// ledger closes (the own-BYE obligation is discharged when the peer's BYE
    /// tears the dialog down), and the RFC hard gate confirms both crossing BYEs
    /// rode the wire compliantly. Proves the reactor is order-independent here so
    /// the S3 shape (and its SUT path) can rely on it.
    #[tokio::test(start_paused = true)]
    async fn two_actor_crossing_bye_both_terminate() {
        let h = Harness::new("actor-crossing-bye").describe(
            "C3/S3: alice and bob BOTH BYE on the AllConfirmed gate (same instant); \
             the 1-transit crossing means each reactor 200s an inbound BYE while its \
             own BYE is in flight — both legs terminate, the ledger settles OK",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let both = Barrier::AllConfirmed(&["alice", "bob"]);
        let call = CallPlan {
            actors: vec![
                ActorSpec {
                    role: "alice",
                    agent: alice.clone(),
                    disposition: Disposition::Caller,
                    media: MediaState::offer(OFFER_SDP),
                    goals: vec![
                        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                        Goal::new(both.clone(), GoalStep::Bye),
                    ],
                    invite_targets: vec![("bob", bob.clone())],
                    via: None,
                    feed: CtxFeed::default(),
                },
                ActorSpec {
                    role: "bob",
                    agent: bob.clone(),
                    disposition: Disposition::RingThenAnswer { ring: Duration::from_millis(200) },
                    media: MediaState::answer(ANSWER_SDP),
                    // The callee ALSO hangs up on the same gate — the crossing.
                    goals: vec![Goal::new(both.clone(), GoalStep::Bye)],
                    invite_targets: vec![],
                    via: None,
                    feed: CtxFeed::default(),
                },
            ],
            plan: vec![phase("established", |s| {
                s.leg_at_least("alice", LegPhase::Confirmed)
                    && s.leg_at_least("bob", LegPhase::Confirmed)
            })],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        };

        let verdict = run_call(call, Duration::from_secs(5)).await;
        assert!(verdict.is_ok(), "the crossing-BYE call must settle OK, got {verdict:?}");

        h.finish().await;
    }

    /// A bob-only [`CallPlan`] with the given forking disposition — the C1(a)
    /// machinery rig: alice is hand-rolled (the fork dance on the caller side is
    /// the C1(b) capability; here the CALLEE's emission is the subject).
    fn forking_bob_plan(bob: &crate::Agent, disposition: Disposition) -> CallPlan {
        CallPlan {
            actors: vec![ActorSpec {
                role: "bob",
                agent: bob.clone(),
                disposition,
                media: MediaState::answer(ANSWER_SDP),
                goals: vec![],
                invite_targets: vec![],
                via: None,
                feed: CtxFeed::default(),
            }],
            plan: vec![phase("established", |s| s.leg_at_least("bob", LegPhase::Confirmed))],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        }
    }

    /// C1(a): a forking callee emits DISTINCT-tag 18x on the ONE INVITE server
    /// transaction and answers 200 under the WINNING tag. The hand-rolled caller
    /// sees two 180s with distinct To-tags (two early dialogs, RFC 3261
    /// §12.1.2), the 200 carries the declared winner's tag, and the confirmed
    /// dialog (ACK + BYE) rides that tag. The RFC hard gate at `finish()`
    /// confirms the forked wire stayed compliant.
    #[tokio::test(start_paused = true)]
    async fn forking_ring_emits_distinct_tag_18x_and_answers_winner() {
        let h = Harness::new("actor-forking-ring").describe(
            "C1(a): bob (ForkingRing) emits 180(f1) + 180(f2) — distinct explicit \
             To-tags on one INVITE server txn — then 200 under the winner f2; the \
             hand-rolled alice confirms and BYEs the winning dialog",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let alice_task = {
            let alice = alice.clone();
            let bob = bob.clone();
            tokio::spawn(async move {
                let mut call = alice.invite(&bob).with_sdp(OFFER_SDP).send().await;
                let p1 = call.expect(180).await;
                let t1 = p1.to.tag.clone().expect("fork1 tag");
                let p2 = call.expect(180).await;
                let t2 = p2.to.tag.clone().expect("fork2 tag");
                assert_ne!(t1, t2, "each fork's 18x carries a DISTINCT To-tag");
                assert_eq!(t1, "f1");
                assert_eq!(t2, "f2");
                let ok = call.expect(200).await;
                assert_eq!(ok.to.tag.as_deref(), Some("f2"), "the 200 is under the winner tag");
                let mut dialog = call.ack().await;
                let mut bye = dialog.bye().await;
                bye.expect(200).await;
            })
        };

        let call = forking_bob_plan(
            &bob,
            Disposition::ForkingRing {
                tags: &["f1", "f2"],
                winner: "f2",
                ring: Duration::from_millis(200),
                reliable: false,
                loser_late_200: None,
            },
        );
        let verdict = run_call(call, Duration::from_secs(5)).await;
        assert!(verdict.is_ok(), "the forked call must settle OK, got {verdict:?}");

        alice_task.await.unwrap();
        h.finish().await;
    }

    /// C1(a) loser-late-200 variant: after the winner's 200 (tag f2) the callee
    /// emits a LATE 200 under the losing tag f1. The caller ACKs the loser's 200
    /// on ITS OWN fork dialog and BYEs it (RFC 3261 §13.2.2.4) — the callee's
    /// reactor 200s that BYE WITHOUT terminating its leg (the winning dialog
    /// lives on and is BYE'd normally afterwards).
    #[tokio::test(start_paused = true)]
    async fn forking_ring_loser_late_200_is_acked_and_byed() {
        let h = Harness::new("actor-forking-loser-200").describe(
            "C1(a): bob answers 200 under winner f2 THEN emits a late 200 under \
             loser f1; alice ACKs+BYEs the loser fork (bob's leg survives), then \
             tears down the winning dialog",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let alice_task = {
            let alice = alice.clone();
            let bob = bob.clone();
            tokio::spawn(async move {
                let mut call = alice.invite(&bob).with_sdp(OFFER_SDP).send().await;
                call.expect(180).await;
                call.expect(180).await;
                // The winner's 200 (f2): ACK, keep the confirmed dialog.
                let ok = call.expect(200).await;
                assert_eq!(ok.to.tag.as_deref(), Some("f2"));
                let mut winner = call.ack().await;
                // The loser's LATE 200 (f1): §13.2.2.4 — ACK it on its own fork
                // dialog, then BYE that fork. (`expect(200)` re-points the
                // ClientInvite's dialog at the latest 2xx's tag, so `ack()` here
                // addresses the LOSER fork.)
                let late = call.expect(200).await;
                assert_eq!(late.to.tag.as_deref(), Some("f1"), "the late 200 is the loser's");
                let mut loser = call.ack().await;
                let mut loser_bye = loser.bye().await;
                loser_bye.expect(200).await;
                // The winning dialog is unaffected — tear it down normally.
                let mut bye = winner.bye().await;
                bye.expect(200).await;
            })
        };

        let call = forking_bob_plan(
            &bob,
            Disposition::ForkingRing {
                tags: &["f1", "f2"],
                winner: "f2",
                ring: Duration::from_millis(200),
                reliable: false,
                loser_late_200: Some("f1"),
            },
        );
        let verdict = run_call(call, Duration::from_secs(5)).await;
        assert!(
            verdict.is_ok(),
            "the loser-late-200 call must settle OK (bob's leg must survive the \
             loser-fork BYE), got {verdict:?}",
        );

        alice_task.await.unwrap();
        h.finish().await;
    }

    /// C1(a) reliable variant: each fork's 18x is a reliable 183 (Require:100rel,
    /// RSeq:1, SDP) and the 200 waits for the WINNER fork's PRACK (MUST-014). A
    /// losing fork's PRACK is 200'd but does NOT release the answer.
    #[tokio::test(start_paused = true)]
    async fn forking_ring_reliable_answers_on_winner_prack_only() {
        use sip_message::generators::InDialogMethod;

        let h = Harness::new("actor-forking-reliable").describe(
            "C1(a): bob (ForkingRing reliable) emits reliable 183(f1)+183(f2); \
             alice PRACKs each fork on its own early dialog; only the winner \
             (f2) fork's PRACK releases the 200",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let alice_task = {
            let alice = alice.clone();
            let bob = bob.clone();
            tokio::spawn(async move {
                let mut call = alice
                    .invite(&bob)
                    .with_sdp(OFFER_SDP)
                    .with_header("Supported", "100rel")
                    .send()
                    .await;
                let p1 = call.expect(183).await;
                assert_eq!(p1.to.tag.as_deref(), Some("f1"));
                let p2 = call.expect(183).await;
                assert_eq!(p2.to.tag.as_deref(), Some("f2"));
                // PRACK the LOSER fork first — the answer must NOT be released.
                let mut prack1 = call
                    .send_request(InDialogMethod::Prack)
                    .with_to_tag("f1")
                    .with_rack("1 1 INVITE")
                    .send()
                    .await;
                prack1.expect(200).await;
                // PRACK the WINNER fork — this releases the 200 (under f2).
                let mut prack2 = call
                    .send_request(InDialogMethod::Prack)
                    .with_to_tag("f2")
                    .with_rack("1 1 INVITE")
                    .send()
                    .await;
                prack2.expect(200).await;
                let ok = call.expect(200).await;
                assert_eq!(ok.to.tag.as_deref(), Some("f2"), "answered under the winner tag");
                let mut dialog = call.ack().await;
                let mut bye = dialog.bye().await;
                bye.expect(200).await;
            })
        };

        let call = forking_bob_plan(
            &bob,
            Disposition::ForkingRing {
                tags: &["f1", "f2"],
                winner: "f2",
                ring: Duration::ZERO,
                reliable: true,
                loser_late_200: None,
            },
        );
        let verdict = run_call(call, Duration::from_secs(5)).await;
        assert!(verdict.is_ok(), "the reliable forked call must settle OK, got {verdict:?}");

        alice_task.await.unwrap();
        h.finish().await;
    }

    /// A two-actor plan pairing the ACTOR caller with a forking callee — the
    /// C1(b) rig: alice is the reactive actor (the fork dance on the caller
    /// side is the subject), bob the C1(a) `ForkingRing` UAS.
    fn forked_pair_plan(
        alice: &crate::Agent,
        bob: &crate::Agent,
        disposition: Disposition,
        plan: Option<crate::realcall::InvitePlan>,
    ) -> CallPlan {
        CallPlan {
            actors: vec![
                ActorSpec {
                    role: "alice",
                    agent: alice.clone(),
                    disposition: Disposition::Caller,
                    media: MediaState::offer(OFFER_SDP),
                    goals: vec![
                        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan }),
                        Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), GoalStep::Bye),
                    ],
                    invite_targets: vec![("bob", bob.clone())],
                    via: None,
                    feed: CtxFeed::default(),
                },
                ActorSpec {
                    role: "bob",
                    agent: bob.clone(),
                    disposition,
                    media: MediaState::answer(ANSWER_SDP),
                    goals: vec![],
                    invite_targets: vec![],
                    via: None,
                    feed: CtxFeed::default(),
                },
            ],
            plan: vec![phase("established", |s| {
                s.leg_at_least("alice", LegPhase::Confirmed)
                    && s.leg_at_least("bob", LegPhase::Confirmed)
            })],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        }
    }

    /// C1(b): the ACTOR caller absorbs a forked establishment — two distinct-tag
    /// 180s (two early dialogs, §12.1.2), the 2xx's tag picks the winner
    /// (§13.2.2.4), the confirmed dialog rides it, and the teardown BYE
    /// addresses the winning fork. Verdict Ok + the RFC hard gate.
    #[tokio::test(start_paused = true)]
    async fn actor_caller_confirms_forked_winner() {
        let h = Harness::new("actor-caller-forked-winner").describe(
            "C1(b): the actor caller sees 180(f1)+180(f2) then 200(f2); the 2xx \
             tag is the winner — the call confirms, tears down and settles OK",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let call = forked_pair_plan(
            &alice,
            &bob,
            Disposition::ForkingRing {
                tags: &["f1", "f2"],
                winner: "f2",
                ring: Duration::from_millis(200),
                reliable: false,
                loser_late_200: None,
            },
            None,
        );
        let verdict = run_call(call, Duration::from_secs(5)).await;
        assert!(verdict.is_ok(), "the forked call must settle OK, got {verdict:?}");
        h.finish().await;
    }

    /// C1(b) loser-late-200: the actor caller confirms the winner (f2), then the
    /// LOSING fork's late 200 (f1) arrives — the reactor ACKs it on the loser's
    /// OWN fork dialog and BYEs that fork (§13.2.2.4), the fork BYE's 200
    /// (recognised by its tag mismatch) closes the `ForkBye` obligation WITHOUT
    /// terminating alice's leg, and the winning dialog tears down normally.
    /// The fork-aware `unackedInvite2xxByed` audit rule gates the wire at
    /// `finish()` — an unACKed or unBYEd loser 200 would fail there.
    #[tokio::test(start_paused = true)]
    async fn actor_caller_acks_and_byes_losing_fork_late_200() {
        let h = Harness::new("actor-caller-loser-late-200").describe(
            "C1(b): a losing fork's late 200 (f1) after the winner's (f2) is \
             ACKed on its own fork dialog then BYE'd; the winning dialog and \
             the verdict are unaffected",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let call = forked_pair_plan(
            &alice,
            &bob,
            Disposition::ForkingRing {
                tags: &["f1", "f2"],
                winner: "f2",
                ring: Duration::from_millis(200),
                reliable: false,
                loser_late_200: Some("f1"),
            },
            None,
        );
        let verdict = run_call(call, Duration::from_secs(5)).await;
        assert!(
            verdict.is_ok(),
            "the loser-late-200 call must settle OK (fork ACK+BYE + ForkBye close), got {verdict:?}",
        );
        h.finish().await;
    }

    /// C1(b) reliable forks: each fork's reliable 183 (distinct tag, RSeq:1) is
    /// PRACKed on its OWN early dialog — the `(to_tag, rseq)` dedup re-key: an
    /// RSeq-only dedup would swallow the second fork's PRACK (both forks start
    /// at RSeq 1) and the callee (which answers only on the WINNER's PRACK)
    /// would never answer; the `prackOnReliable1xx` audit rule would also flag
    /// the unPRACKed fork at `finish()`.
    #[tokio::test(start_paused = true)]
    async fn actor_caller_pracks_each_reliable_fork() {
        let h = Harness::new("actor-caller-forked-prack").describe(
            "C1(b): reliable 183(f1)+183(f2), both RSeq:1 — the actor caller \
             PRACKs EACH fork on its own early dialog ((tag,rseq) dedup); the \
             winner fork's PRACK releases the 200 and the call completes",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        // The caller must advertise 100rel for the UAS's reliable 183s to be
        // legal (RFC 3262 §3) — a hand-built direct-to-bob plan carries it.
        let plan = crate::realcall::InvitePlan {
            via: bob.addr(),
            from: None,
            to: None,
            ruri: None,
            headers: vec![("Supported".to_string(), "100rel".to_string())],
            rewrite: Default::default(),
        };

        let call = forked_pair_plan(
            &alice,
            &bob,
            Disposition::ForkingRing {
                tags: &["f1", "f2"],
                winner: "f2",
                ring: Duration::ZERO,
                reliable: true,
                loser_late_200: None,
            },
            Some(plan),
        );
        let verdict = run_call(call, Duration::from_secs(5)).await;
        assert!(verdict.is_ok(), "the reliable forked call must settle OK, got {verdict:?}");
        h.finish().await;
    }

    /// A CANCEL×200 crossing plan (C2/E5): alice INVITEs, CANCELs after
    /// `cancel_after` (gated on ringing), and BYEs-if-confirmed once the race
    /// resolves; bob rings then answers after `ring`. Varying `cancel_after`
    /// vs `ring` by one transit quantum pins each branch deterministically.
    fn cancel_crossing_plan(
        alice: &crate::Agent,
        bob: &crate::Agent,
        ring: Duration,
        cancel_after: Duration,
    ) -> CallPlan {
        let ringing = |s: &StateInner| s.leg_at_least("alice", LegPhase::Early);
        CallPlan {
            actors: vec![
                ActorSpec {
                    role: "alice",
                    agent: alice.clone(),
                    disposition: Disposition::Caller,
                    media: MediaState::offer(OFFER_SDP),
                    goals: vec![
                        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                        Goal::new(Barrier::pred("ringing", ringing), GoalStep::Cancel)
                            .after(cancel_after),
                    ],
                    invite_targets: vec![("bob", bob.clone())],
                    via: None,
                    feed: CtxFeed::default(),
                },
                ActorSpec {
                    role: "bob",
                    agent: bob.clone(),
                    disposition: Disposition::RingThenAnswer { ring },
                    media: MediaState::answer(ANSWER_SDP),
                    // The teardown rides BOB (branch-conditional): the guard
                    // holds once bob has RESOLVED (Confirmed, or the monotone
                    // Terminated after a 487); ByeIfConfirmed BYEs the winning
                    // dialog or no-ops on the cancelled one. Alice keeps only
                    // [Invite, Cancel] so a 487 never trips her incidental-
                    // failure WrongStatus path (it stays a clean terminal that
                    // the EitherOf oracle maps).
                    goals: vec![Goal::new(
                        Barrier::pred("bob_resolved", |s| s.leg_at_least("bob", LegPhase::Confirmed)),
                        GoalStep::ByeIfConfirmed,
                    )],
                    invite_targets: vec![],
                    via: None,
                    feed: CtxFeed::default(),
                },
            ],
            plan: vec![],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        }
    }

    /// C2/E5 branch — CANCEL WINS: alice CANCELs well before bob's answer timer
    /// fires (cancel_after < ring − 2·transit), so bob 487s the held INVITE. The
    /// call reaches a clean terminal (both legs terminated, alice ACKs the 487)
    /// and the EitherOf oracle maps it to the abandoned `Timeout`.
    #[tokio::test(start_paused = true)]
    async fn cancel_answer_crossing_cancel_wins() {
        let h = Harness::new("actor-cancel-wins").describe(
            "C2/E5: alice CANCELs at 2ms, bob answers at 20ms — the CANCEL wins, \
             bob 487s the INVITE; the branch oracle maps it to the abandoned \
             terminal",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let call = cancel_crossing_plan(
            &alice,
            &bob,
            Duration::from_millis(20),
            Duration::from_millis(2),
        );
        let verdict = run_call(call, Duration::from_secs(5)).await;
        assert!(verdict.is_ok(), "the CANCEL-wins call must reach a clean terminal, got {verdict:?}");
        // The observed 487 → the abandoned branch: pinned in the oracle unit
        // test `either_of_oracle_maps_each_branch`.
        h.finish().await;
    }

    /// C2/E5 branch — 200 WINS (the true crossing): alice CANCELs just AFTER
    /// bob's answer timer fires (cancel_after > ring), so bob has already sent
    /// the 200 when the CANCEL arrives. Per §9.2 bob 200s the CANCEL and ignores
    /// it (the confirmed dialog survives); alice ACKs the crossed 200, bob BYEs
    /// the winning dialog, and the call tears down OK.
    #[tokio::test(start_paused = true)]
    async fn cancel_answer_crossing_answer_wins() {
        let h = Harness::new("actor-answer-wins").describe(
            "C2/E5: alice CANCELs at 20ms, bob answers at 5ms — the 200 crossed \
             the CANCEL; bob ignores the late CANCEL (§9.2), alice ACKs the 200, \
             the winning dialog tears down OK",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let call = cancel_crossing_plan(
            &alice,
            &bob,
            Duration::from_millis(5),
            Duration::from_millis(20),
        );
        let verdict = run_call(call, Duration::from_secs(5)).await;
        assert!(verdict.is_ok(), "the 200-wins call must confirm and tear down OK, got {verdict:?}");

        h.finish().await;
    }

    /// 047 machinery: a `RingThenSilent` callee emits its 180 then holds the
    /// INVITE server txn with NO answer ever scheduled — the peer's CANCEL (in
    /// production, the SUT's no-answer timer firing) releases the held txn as a
    /// `487`, the leg terminates, and the reject-final obligation closes on the
    /// hop-ACK: a clean settle, no leaked server txn. SUT-less pin of the
    /// silent primary a no-answer-triggered failover composes with.
    #[tokio::test(start_paused = true)]
    async fn ring_then_silent_487s_on_cancel_and_settles() {
        let h = Harness::new("actor-ring-then-silent").describe(
            "047: bob rings then stays silent (held INVITE txn, no answer ever); \
             alice CANCELs — standing in for the SUT's no-answer timer — bob \
             487s the held txn and both legs settle cleanly",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let ringing = |s: &StateInner| s.leg_at_least("alice", LegPhase::Early);
        let call = CallPlan {
            actors: vec![
                ActorSpec {
                    role: "alice",
                    agent: alice.clone(),
                    disposition: Disposition::Caller,
                    media: MediaState::offer(OFFER_SDP),
                    goals: vec![
                        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                        // The stand-in for the SUT's no-answer timer: CANCEL a
                        // while after the ring (bob would ring forever).
                        Goal::new(Barrier::pred("ringing", ringing), GoalStep::Cancel)
                            .after(Duration::from_millis(500)),
                    ],
                    invite_targets: vec![("bob", bob.clone())],
                    via: None,
                    feed: CtxFeed::default(),
                },
                ActorSpec {
                    role: "bob",
                    agent: bob.clone(),
                    disposition: Disposition::RingThenSilent,
                    media: MediaState::none(),
                    goals: vec![],
                    invite_targets: vec![],
                    via: None,
                    feed: CtxFeed::default(),
                },
            ],
            plan: vec![phase("ringing", ringing)],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        };

        let verdict = run_call(call, Duration::from_secs(5)).await;
        assert!(
            verdict.is_ok(),
            "the CANCELled silent ring must settle cleanly (487 + hop-ACK), got {verdict:?}"
        );
        h.finish().await;
    }

    /// The `into_result` branch oracle maps each observed race outcome to its
    /// bounded downstream class: a caller that saw the 487 → the abandoned
    /// `Timeout` (the `timeout` load class); a caller that confirmed (no non-2xx
    /// final) → `Ok`.
    #[tokio::test]
    async fn either_of_oracle_maps_each_branch() {
        use crate::actor::ExpectBranch;
        let branches: &[ExpectBranch] =
            &[ExpectBranch::Answered, ExpectBranch::Cancelled { code: 487 }];

        // Cancel-wins: alice saw a 487 final.
        let obs = ObservedState::new();
        let now = tokio::time::Instant::now();
        obs.record(Observation::LegEarly { leg: "alice" }, now);
        obs.record(
            Observation::LegFinal { leg: "alice", status: 487, reason: "Request Terminated".into() },
            now,
        );
        obs.record(Observation::LegTerminated { leg: "alice" }, now);
        match into_result(Expect::EitherOf(branches), CallVerdict::Ok, &obs, "alice") {
            Err(StepError::Timeout { who }) => {
                assert_eq!(who, "alice-abandoned-after-ringing")
            }
            other => panic!("cancel-wins must map to the abandoned Timeout, got {other:?}"),
        }

        // Answer-wins: alice confirmed, no non-2xx final.
        let obs = ObservedState::new();
        obs.record(Observation::LegConfirmed { leg: "alice" }, now);
        obs.record(Observation::LegTerminated { leg: "alice" }, now);
        assert!(
            into_result(Expect::EitherOf(branches), CallVerdict::Ok, &obs, "alice").is_ok(),
            "answer-wins (no 487) must map to Ok",
        );
    }

    /// C4/S5 re-INVITE glare (RFC 3261 §14.1): alice and bob BOTH originate a
    /// re-INVITE at the same paused-clock instant (gated on `AllConfirmed`), so
    /// each arrives while the peer's own re-INVITE is outstanding — BOTH get
    /// `491 Request Pending`. Each end hop-ACKs the 491, closes its obligation,
    /// and retries after the §14.1 back-off (owner alice: 2.5s; non-owner bob:
    /// 1.0s), so bob's retry lands first (alice's re-INVITE no longer pending →
    /// 200), then alice's. Both rounds complete; the RFC hard gate confirms both
    /// 491s were ACKed and no obligation leaked.
    #[tokio::test(start_paused = true)]
    async fn reinvite_glare_491_both_ways_then_retry_resolves() {
        let h = Harness::new("actor-reinvite-glare").describe(
            "C4/S5: alice+bob re-INVITE at once → 491 both ways → §14.1 owner/\
             non-owner back-off retries (bob 1s, alice 2.5s) → both rounds \
             complete → BYE",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let both_confirmed = Barrier::AllConfirmed(&["alice", "bob"]);
        let both_reneged = Barrier::pred("glare_resolved", |s| {
            s.leg("alice").reneg_count() >= 1 && s.leg("bob").reneg_count() >= 1
        });
        let call = CallPlan {
            actors: vec![
                ActorSpec {
                    role: "alice",
                    agent: alice.clone(),
                    // Alice answers bob's realign re-INVITE with SDP (full media).
                    media: MediaState::full(OFFER_SDP, ANSWER_SDP),
                    disposition: Disposition::Caller,
                    goals: vec![
                        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                        Goal::new(both_confirmed.clone(), GoalStep::Reinvite),
                        Goal::new(both_reneged.clone(), GoalStep::Bye),
                    ],
                    invite_targets: vec![("bob", bob.clone())],
                    via: None,
                    feed: CtxFeed::default(),
                },
                ActorSpec {
                    role: "bob",
                    agent: bob.clone(),
                    disposition: Disposition::RingThenAnswer { ring: Duration::from_millis(100) },
                    media: MediaState::full(ANSWER_SDP, ANSWER_SDP),
                    // Bob ALSO re-INVITEs on the same gate — the glare.
                    goals: vec![Goal::new(both_confirmed.clone(), GoalStep::Reinvite)],
                    invite_targets: vec![],
                    via: None,
                    feed: CtxFeed::default(),
                },
            ],
            plan: vec![
                phase("established", |s| {
                    s.leg_at_least("alice", LegPhase::Confirmed)
                        && s.leg_at_least("bob", LegPhase::Confirmed)
                }),
                phase("glare_resolved", |s| {
                    s.leg("alice").reneg_count() >= 1 && s.leg("bob").reneg_count() >= 1
                }),
            ],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        };

        let verdict = run_call(call, Duration::from_secs(10)).await;
        assert!(verdict.is_ok(), "the glare must resolve via §14.1 retry, got {verdict:?}");

        h.finish().await;
    }

    /// C4/S6 UPDATE-vs-re-INVITE collision (RFC 3311 §5.2): alice sends a
    /// re-INVITE and bob an UPDATE at the same instant, both carrying offers.
    /// Each has an outstanding offer when the peer's offer-bearing request
    /// arrives, so BOTH are rejected 491 (the re-INVITE 491 is hop-ACKed, the
    /// UPDATE 491 takes no ACK). Each retries after the back-off (owner alice
    /// 2.5s, non-owner bob 1.0s): bob's UPDATE retry lands first (alice has no
    /// pending offer → 200), then alice's re-INVITE (bob's UPDATE done → 200).
    /// Both renegotiations complete; the RFC hard gate confirms the wire.
    #[tokio::test(start_paused = true)]
    async fn update_vs_reinvite_collision_491_then_retry_resolves() {
        let h = Harness::new("actor-update-reinvite-glare").describe(
            "C4/S6: alice re-INVITE × bob UPDATE at once → 491 both ways → \
             back-off retries (bob UPDATE 1s, alice re-INVITE 2.5s) → both \
             offers complete → BYE",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let both_confirmed = Barrier::AllConfirmed(&["alice", "bob"]);
        let both_reneged = Barrier::pred("collision_resolved", |s| {
            s.leg("alice").reneg_count() >= 1 && s.leg("bob").reneg_count() >= 1
        });
        let call = CallPlan {
            actors: vec![
                ActorSpec {
                    role: "alice",
                    agent: alice.clone(),
                    media: MediaState::full(OFFER_SDP, ANSWER_SDP),
                    disposition: Disposition::Caller,
                    goals: vec![
                        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                        Goal::new(both_confirmed.clone(), GoalStep::Reinvite),
                        Goal::new(both_reneged.clone(), GoalStep::Bye),
                    ],
                    invite_targets: vec![("bob", bob.clone())],
                    via: None,
                    feed: CtxFeed::default(),
                },
                ActorSpec {
                    role: "bob",
                    agent: bob.clone(),
                    disposition: Disposition::RingThenAnswer { ring: Duration::from_millis(100) },
                    media: MediaState::full(ANSWER_SDP, ANSWER_SDP),
                    // Bob collides with an UPDATE on the same gate.
                    goals: vec![Goal::new(both_confirmed.clone(), GoalStep::Update)],
                    invite_targets: vec![],
                    via: None,
                    feed: CtxFeed::default(),
                },
            ],
            plan: vec![
                phase("established", |s| {
                    s.leg_at_least("alice", LegPhase::Confirmed)
                        && s.leg_at_least("bob", LegPhase::Confirmed)
                }),
                phase("collision_resolved", |s| {
                    s.leg("alice").reneg_count() >= 1 && s.leg("bob").reneg_count() >= 1
                }),
            ],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        };

        let verdict = run_call(call, Duration::from_secs(10)).await;
        assert!(verdict.is_ok(), "the UPDATE×re-INVITE collision must resolve, got {verdict:?}");

        h.finish().await;
    }

    /// C5 early UPDATE (RFC 3311 §5.1): alice INVITEs with 100rel; bob answers
    /// reliably (183) and HOLDS the INVITE. Alice PRACKs, then — while still in
    /// the EARLY dialog, before the final 200 — sends an UPDATE renegotiating
    /// media. Bob 200s the UPDATE and only THEN answers the INVITE 200. Alice
    /// ACKs and the call tears down. The RFC hard gate confirms the pre-answer
    /// UPDATE rode the early dialog compliantly.
    #[tokio::test(start_paused = true)]
    async fn early_update_on_the_reliable_early_dialog() {
        let h = Harness::new("actor-early-update").describe(
            "C5: 100rel INVITE → reliable 183 → PRACK → EARLY UPDATE (200) → \
             final 200 INVITE → ACK → BYE; bob holds the INVITE until the early \
             UPDATE completes (RFC 3311 §5.1)",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        // The caller advertises 100rel via a direct-to-bob plan.
        let plan = crate::realcall::InvitePlan {
            via: bob.addr(),
            from: None,
            to: None,
            ruri: None,
            headers: vec![("Supported".to_string(), "100rel".to_string())],
            rewrite: Default::default(),
        };
        let alice_confirmed =
            Barrier::pred("confirmed", |s| s.leg_at_least("alice", LegPhase::Confirmed));
        let call = CallPlan {
            actors: vec![
                ActorSpec {
                    role: "alice",
                    agent: alice.clone(),
                    media: MediaState::full(OFFER_SDP, ANSWER_SDP),
                    disposition: Disposition::Caller,
                    goals: vec![
                        Goal::new(
                            Barrier::None,
                            GoalStep::Invite { callee: "bob", plan: Some(plan) },
                        ),
                        // The early UPDATE fires once alice has PRACKed the
                        // reliable 183 (SUBFLOW_EARLY — a real post-183 signal,
                        // NOT LegPhase::Early which holds the instant she
                        // originates), before the final 200 (RFC 3311 §5.1).
                        Goal::new(
                            Barrier::pred("early", |s| {
                                s.leg("alice").subflow(SUBFLOW_EARLY).is_some()
                            }),
                            GoalStep::UpdateEarly,
                        ),
                        Goal::new(alice_confirmed.clone(), GoalStep::Bye),
                    ],
                    invite_targets: vec![("bob", bob.clone())],
                    via: None,
                    feed: CtxFeed::default(),
                },
                ActorSpec {
                    role: "bob",
                    agent: bob.clone(),
                    disposition: Disposition::ReliableAnswerEarlyUpdate,
                    media: MediaState::answer(ANSWER_SDP),
                    goals: vec![],
                    invite_targets: vec![],
                    via: None,
                    feed: CtxFeed::default(),
                },
            ],
            plan: vec![phase("confirmed", |s| s.leg_at_least("alice", LegPhase::Confirmed))],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        };

        let verdict = run_call(call, Duration::from_secs(5)).await;
        assert!(verdict.is_ok(), "the early-UPDATE call must settle OK, got {verdict:?}");

        h.finish().await;
    }

    /// The generic in-dialog origination primitive (`GoalStep::InDialog`): alice
    /// establishes, sends an INFO carrying a typed body + an extra header on the
    /// confirmed dialog, and hangs up ONLY once the observed state shows bob
    /// received that INFO (`Barrier::received` — the ordering gate that replaces a
    /// timed dwell). The INFO opens an `InDialog` obligation the settle barrier
    /// holds on until its 2xx closes it; the RFC hard gate at `finish()` confirms
    /// the INFO rode the wire compliantly.
    #[tokio::test(start_paused = true)]
    async fn actor_originates_in_dialog_info() {
        use sip_message::generators::InDialogMethod;

        let h = Harness::new("actor-in-dialog-info").describe(
            "044 primitive: alice originates a plain in-dialog INFO (typed body) on \
             the confirmed dialog; bob 200s it reactively; alice BYEs gated on the \
             observed fact that bob received the INFO (Barrier::received), and the \
             settle barrier holds until the INFO's 2xx closes its InDialog obligation",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let call = CallPlan {
            actors: vec![
                ActorSpec {
                    role: "alice",
                    agent: alice.clone(),
                    disposition: Disposition::Caller,
                    media: MediaState::offer(OFFER_SDP),
                    goals: vec![
                        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                        // Send the INFO once the call is up.
                        Goal::new(
                            Barrier::AllConfirmed(&["alice", "bob"]),
                            GoalStep::InDialog {
                                method: InDialogMethod::Info,
                                content_type: Some("application/x-info-test".to_string()),
                                body: Some(b"<info>eof</info>".to_vec()),
                                headers: vec![("X-Info-Kind".to_string(), "eof".to_string())],
                            },
                        ),
                        // Hang up only once bob has OBSERVABLY received the INFO —
                        // the ordering barrier, no timed dwell.
                        Goal::new(Barrier::received("info_seen", "bob", "INFO"), GoalStep::Bye),
                    ],
                    invite_targets: vec![("bob", bob.clone())],
                    via: None,
                    feed: CtxFeed::default(),
                },
                ActorSpec {
                    role: "bob",
                    agent: bob.clone(),
                    disposition: Disposition::RingThenAnswer {
                        ring: Duration::from_millis(200),
                    },
                    media: MediaState::answer(ANSWER_SDP),
                    goals: vec![],
                    invite_targets: vec![],
                    via: None,
                    feed: CtxFeed::default(),
                },
            ],
            plan: vec![phase("established", |s| {
                s.leg_at_least("alice", LegPhase::Confirmed)
                    && s.leg_at_least("bob", LegPhase::Confirmed)
            })],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        };

        let verdict = run_call(call, Duration::from_secs(5)).await;
        assert!(verdict.is_ok(), "the INFO call must settle OK, got {verdict:?}");

        h.finish().await;
    }

    /// `Barrier::received` (and its `leg_received_method` backing) hold exactly
    /// when the named leg has folded an inbound request of the named method —
    /// method-specific and leg-scoped, so the MRF-EOF ordering gates on the right
    /// observed fact and never a sibling's traffic.
    #[tokio::test]
    async fn barrier_received_is_method_and_leg_scoped() {
        let now = tokio::time::Instant::now();
        let obs = ObservedState::new();
        let barrier = Barrier::received("info_seen", "bob", "INFO");

        assert!(!obs.with_snapshot(|s| barrier.holds(s)), "no INFO observed yet");

        // A different method on the right leg does NOT satisfy it.
        obs.record(
            Observation::InDialogRequest {
                leg: "bob",
                call_id: "c1".to_string(),
                cseq: 2,
                method: "OPTIONS".to_string(),
            },
            now,
        );
        assert!(!obs.with_snapshot(|s| barrier.holds(s)), "OPTIONS is not INFO");

        // The INFO on the RIGHT leg satisfies it...
        obs.record(
            Observation::InDialogRequest {
                leg: "bob",
                call_id: "c1".to_string(),
                cseq: 3,
                method: "INFO".to_string(),
            },
            now,
        );
        assert!(obs.with_snapshot(|s| barrier.holds(s)), "bob received INFO");
        assert!(obs.with_snapshot(|s| s.leg_received_method("bob", "INFO")));
        // ...but the fact is leg-scoped: alice has not received an INFO.
        assert!(!obs.with_snapshot(|s| s.leg_received_method("alice", "INFO")));
    }

    /// Fold-order determinism: the SAME set of observations folded in forward
    /// and reverse order yields the SAME barrier verdict + ledger state — the
    /// grow-only, commutative fold the N-reactor reconciliation depends on.
    #[tokio::test]
    async fn fold_order_is_deterministic() {
        let now = tokio::time::Instant::now();
        // A complete torn-down call: both legs confirmed then terminated, alice's
        // BYE obligation opened + closed, bob's dialog CSeq stream seeded (1) then
        // filled by the received BYE (2).
        let facts = || {
            vec![
                Observation::LegEarly { leg: "alice" },
                Observation::SeedDialog { leg: "bob", call_id: "c1".to_string(), cseq: 1 },
                Observation::LegConfirmed { leg: "alice" },
                Observation::LegConfirmed { leg: "bob" },
                Observation::RequestSent {
                    key: ObligationKey::new("alice", ObligationKind::Bye, 2),
                    detail: "hangup".to_string(),
                },
                Observation::InDialogRequest {
                    leg: "bob",
                    call_id: "c1".to_string(),
                    cseq: 2,
                    method: "BYE".to_string(),
                },
                Observation::ResponseObserved {
                    key: ObligationKey::new("alice", ObligationKind::Bye, 2),
                },
                Observation::LegTerminated { leg: "alice" },
                Observation::LegTerminated { leg: "bob" },
            ]
        };

        let fold = |ordered: Vec<Observation>| {
            let obs = ObservedState::new();
            for o in ordered {
                obs.record(o, now);
            }
            let established = obs.with_snapshot(|s| {
                s.leg_at_least("alice", LegPhase::Confirmed)
                    && s.leg_at_least("bob", LegPhase::Confirmed)
            });
            (obs.all_terminated(), obs.ledger_closed(), established, obs.describe_open())
        };

        let forward = fold(facts());
        let mut rev = facts();
        rev.reverse();
        let backward = fold(rev);

        assert_eq!(forward, backward, "fold order must not change the verdict");
        // And the complete set is a fully-settled, torn-down call.
        assert_eq!(forward, (true, true, true, Vec::new()));
    }

    /// A permuted fold that leaves an obligation open is ALSO order-independent —
    /// the close-before-open reconciliation and the CSeq gap both hold whichever
    /// way the facts arrive.
    #[tokio::test]
    async fn fold_order_is_deterministic_when_open() {
        let now = tokio::time::Instant::now();
        let facts = || {
            vec![
                // A NOTIFY gap: cseq 1 seeded, 3 seen, 2 dropped.
                Observation::SeedDialog { leg: "bob", call_id: "c1".to_string(), cseq: 1 },
                Observation::InDialogRequest {
                    leg: "bob",
                    call_id: "c1".to_string(),
                    cseq: 3,
                    method: "NOTIFY".to_string(),
                },
                // An obligation closed before it is opened (permutation hazard).
                Observation::ResponseObserved {
                    key: ObligationKey::new("bob", ObligationKind::Notify, 3),
                },
                Observation::RequestSent {
                    key: ObligationKey::new("bob", ObligationKind::Notify, 3),
                    detail: "progress".to_string(),
                },
            ]
        };
        let fold = |ordered: Vec<Observation>| {
            let obs = ObservedState::new();
            for o in ordered {
                obs.record(o, now);
            }
            (obs.ledger_closed(), obs.describe_open())
        };
        let forward = fold(facts());
        let mut rev = facts();
        rev.reverse();
        let backward = fold(rev);
        assert_eq!(forward, backward, "open-obligation fold must be order-independent");
        // The obligation reconciles (grow-only), but the cseq-2 gap keeps it open.
        assert!(!forward.0, "the dropped cseq-2 leaves the ledger open");
        assert!(
            forward.1.iter().any(|s| s.contains("cseq=2")),
            "the open detail names the gap: {:?}",
            forward.1
        );
    }

    /// A static-credential [`ChallengeResponder`] (the deferred seam's simplest
    /// implementation — real digest would hash the challenge). Returns a fixed
    /// credential for any challenge; records what it was asked so the auth-path
    /// inputs can be asserted.
    struct FakeResponder {
        credential: String,
        seen: std::sync::Mutex<Vec<(u16, String, String)>>,
    }
    impl FakeResponder {
        fn new(credential: &str) -> Self {
            Self { credential: credential.to_string(), seen: std::sync::Mutex::new(Vec::new()) }
        }
    }
    impl crate::realcall::ChallengeResponder for FakeResponder {
        fn respond(
            &self,
            challenge: &crate::realcall::Challenge,
            method: &str,
            ruri: &str,
        ) -> Option<String> {
            self.seen
                .lock()
                .unwrap()
                .push((challenge.status, method.to_string(), ruri.to_string()));
            Some(self.credential.clone())
        }
    }

    /// The actor caller wires the deferred-auth adapter onto its establishing
    /// INVITE (RFC 3261 §22.2): a challenging UAS `401`s the first INVITE, alice's
    /// reactor ACKs it, adds the responder's credential, resends ONCE (bumped
    /// CSeq, fresh branch), and the authenticated INVITE is admitted — the retry
    /// is invisible above the reactor (she reaches `Confirmed` and BYEs). This
    /// recovers the deleted linear `admitted_uas_retries_through_a_challenging_
    /// middlebox` intent against the actor runner.
    #[tokio::test(start_paused = true)]
    async fn actor_caller_retries_through_a_401_challenge() {
        let h = Harness::new("actor-auth-retry").describe(
            "The actor caller honours env.challenge_responder: a UAS 401s alice's \
             INVITE, her reactor ACKs + credentials + resends once (§22.2), the \
             authed INVITE is admitted, and the call establishes and tears down OK",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let responder = Arc::new(FakeResponder::new(
            "Digest username=\"alice\", realm=\"sip\", nonce=\"n1\", response=\"deadbeef\"",
        ));
        let seen = responder.clone();

        fn alice_confirmed(s: &StateInner) -> bool {
            s.leg_at_least("alice", LegPhase::Confirmed)
        }

        // The challenging UAS: 401 the first INVITE, read its ACK, then admit the
        // authenticated resend (180/200), read its ACK, and answer the BYE.
        let bob_srv = bob.clone();
        let server = tokio::spawn(async move {
            use sip_message::message_helpers::get_header;
            let bob = bob_srv;
            let mut c = bob.try_receive("INVITE").await.unwrap();
            assert_eq!(c.request().cseq.seq, 1, "the first INVITE is CSeq 1");
            assert!(
                get_header(&c.request().headers, "authorization").is_none(),
                "the first INVITE carries no credential",
            );
            c.respond(401, "Unauthorized")
                .with_header("WWW-Authenticate", "Digest realm=\"sip\", nonce=\"n1\"")
                .try_send()
                .await
                .unwrap();
            // The reactor auto-ACKs the 401 (§17.1.1.3) before the resend.
            let ack = bob.try_receive("ACK").await.unwrap();
            assert_eq!(ack.request().cseq.seq, 1, "the non-2xx ACK reuses the INVITE CSeq");
            // The authenticated resend: bumped CSeq + the responder's credential.
            let mut admit = bob.try_receive("INVITE").await.unwrap();
            assert_eq!(admit.request().cseq.seq, 2, "the retried INVITE bumps the CSeq (§22.2)");
            assert!(
                get_header(&admit.request().headers, "authorization")
                    .is_some_and(|v| v.starts_with("Digest ")),
                "the retried INVITE carries the responder's Authorization",
            );
            admit.respond(180, "Ringing").try_send().await.unwrap();
            admit.respond(200, "OK").with_sdp(ANSWER_SDP).try_send().await.unwrap();
            bob.try_receive("ACK").await.unwrap();
            // Alice hangs up once established.
            bob.try_receive("BYE").await.unwrap().respond(200, "OK").try_send().await.unwrap();
        });

        let call = CallPlan {
            actors: vec![ActorSpec {
                role: "alice",
                agent: alice.clone(),
                disposition: Disposition::Caller,
                media: MediaState::offer(OFFER_SDP),
                goals: vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                    Goal::new(Barrier::pred("confirmed", alice_confirmed), GoalStep::Bye),
                ],
                invite_targets: vec![("bob", bob.clone())],
                via: None,
                feed: CtxFeed::default(),
            }],
            plan: vec![phase("confirmed", alice_confirmed)],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        };

        let ctx = CallCtx::new();
        let verdict = run_call_with(
            call,
            ObservedState::new(),
            &ctx,
            Duration::from_secs(5),
            Some(responder),
        )
        .await;
        assert!(
            verdict.is_ok(),
            "the challenged call must retry, admit, and settle OK, got {verdict:?}",
        );

        // The responder saw the challenge status + the request-line inputs.
        {
            let seen = seen.seen.lock().unwrap();
            assert_eq!(seen.len(), 1, "exactly one retry (§22.2 caps at one)");
            assert_eq!(seen[0].0, 401);
            assert_eq!(seen[0].1, "INVITE");
            assert!(seen[0].2.starts_with("sip:bob@"), "ruri passed through: {}", seen[0].2);
        }

        server.await.unwrap();
        h.finish().await;
    }

    /// WITHOUT a responder (the default) a `401`/`407` to the actor caller's
    /// establishing INVITE classifies UNCHANGED: the reactor records the shed
    /// final and surfaces `WrongStatus { who: "alice", expected: 180, got: 401 }`
    /// (→ `status_401`, `who: "alice"` — the B7 contract), marking the leg/scope
    /// terminated with nothing to CANCEL (the challenged INVITE transaction is
    /// complete). Recovers the deleted linear `admitted_uas_without_responder_
    /// classifies_401_unchanged` intent against the actor runner.
    #[tokio::test(start_paused = true)]
    async fn actor_caller_without_responder_classifies_401_unchanged() {
        let h = Harness::new("actor-auth-no-responder").describe(
            "No responder (the default): a 401 to alice's INVITE surfaces as \
             WrongStatus{who:alice, expected:180, got:401} — the challenge is a \
             counted deviation, unchanged, with nothing to CANCEL",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        // The challenger 401s and — since no retry follows — only reads the ACK
        // the reactor auto-sends (§17.1.1.3), so the reject transaction completes
        // on the recorded trace (rfc3261.unackedInviteNon2xxFinal gates).
        let bob_srv = bob.clone();
        let server = tokio::spawn(async move {
            let bob = bob_srv;
            let mut c = bob.try_receive("INVITE").await.unwrap();
            c.respond(401, "Unauthorized")
                .with_header("WWW-Authenticate", "Digest realm=\"sip\", nonce=\"n1\"")
                .try_send()
                .await
                .unwrap();
            bob.try_receive("ACK").await.unwrap();
        });

        fn alice_confirmed(s: &StateInner) -> bool {
            s.leg_at_least("alice", LegPhase::Confirmed)
        }
        let call = CallPlan {
            actors: vec![ActorSpec {
                role: "alice",
                agent: alice.clone(),
                disposition: Disposition::Caller,
                media: MediaState::offer(OFFER_SDP),
                // A pending goal (the would-be BYE) makes the shed an INCIDENTAL
                // establishment failure → the linear `establish` WrongStatus, not
                // an intended terminal.
                goals: vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                    Goal::new(Barrier::pred("confirmed", alice_confirmed), GoalStep::Bye),
                ],
                invite_targets: vec![("bob", bob.clone())],
                via: None,
                feed: CtxFeed::default(),
            }],
            plan: vec![phase("confirmed", alice_confirmed)],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        };

        let ctx = CallCtx::new();
        let verdict = run_call_with(
            call,
            ObservedState::new(),
            &ctx,
            Duration::from_secs(5),
            None,
        )
        .await;
        match verdict {
            CallVerdict::Failed(StepError::WrongStatus { who, expected, got, .. }) => {
                assert_eq!((who.as_str(), expected, got), ("alice", 180, 401));
            }
            other => panic!("expected WrongStatus{{who:alice, 180/401}}, got {other:?}"),
        }

        server.await.unwrap();
        h.finish().await;
    }

    // -----------------------------------------------------------------------
    // Capture-replay verbs: template emission, Scripted park-or-react,
    // reception goals, lane automatics.
    // -----------------------------------------------------------------------

    use sip_message::{EmitOpts, MessageTemplate, Method, TemplateHeader};

    /// A captured-INVITE stand-in: frozen extra headers + an SDP offer body.
    fn invite_template(extra: &[(&str, &str)]) -> MessageTemplate {
        let mut headers = vec![TemplateHeader::frozen("Content-Type", "application/sdp")];
        for (n, v) in extra {
            headers.push(TemplateHeader::frozen(*n, *v));
        }
        MessageTemplate::request(Method::Invite, headers, OFFER_SDP.as_bytes().to_vec())
    }

    /// A captured-response stand-in — optionally carrying the answer SDP.
    fn response_template(status: u16, reason: &str, sdp: bool) -> MessageTemplate {
        if sdp {
            MessageTemplate::response(
                status,
                reason,
                vec![TemplateHeader::frozen("Content-Type", "application/sdp")],
                ANSWER_SDP.as_bytes().to_vec(),
            )
        } else {
            MessageTemplate::response(status, reason, vec![], Vec::new())
        }
    }

    /// A plain caller spec with the given goals (offer media, no plan/via).
    fn caller_spec(role: &'static str, agent: &crate::Agent, callee: (&'static str, crate::Agent), goals: Vec<Goal>) -> ActorSpec {
        ActorSpec {
            role,
            agent: agent.clone(),
            disposition: Disposition::Caller,
            media: MediaState::offer(OFFER_SDP),
            goals,
            invite_targets: vec![callee],
            via: None,
            feed: CtxFeed::default(),
        }
    }

    /// A Scripted callee spec with the given goals (answer media).
    fn scripted_spec(role: &'static str, agent: &crate::Agent, goals: Vec<Goal>) -> ActorSpec {
        ActorSpec {
            role,
            agent: agent.clone(),
            disposition: Disposition::Scripted,
            media: MediaState::answer(ANSWER_SDP),
            goals,
            invite_targets: vec![],
            via: None,
            feed: CtxFeed::default(),
        }
    }

    fn established_phase() -> BarrierPhase {
        phase("established", |s| {
            s.leg_at_least("alice", LegPhase::Confirmed)
                && s.leg_at_least("bob", LegPhase::Confirmed)
        })
    }

    /// Scripted end-to-end template replay: a templated INVITE, a scripted 180
    /// (provisional, binding NOT consumed) then 200 via `RespondTemplate` on
    /// the ONE parked server transaction, the reactor's auto-ACK, and a BYE
    /// teardown — verdict Ok, settle clean, RFC hard gate green.
    #[tokio::test(start_paused = true)]
    async fn scripted_template_replay_end_to_end() {
        let h = Harness::new("actor-scripted-template-replay").describe(
            "InviteTemplate → scripted RespondTemplate 180 (non-consuming) + 200 \
             on the parked INVITE txn → auto-ACK → BYE; settles clean",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let call = CallPlan {
            actors: vec![
                caller_spec(
                    "alice",
                    &alice,
                    ("bob", bob.clone()),
                    vec![
                        Goal::new(
                            Barrier::None,
                            GoalStep::InviteTemplate {
                                callee: "bob",
                                plan: None,
                                template: invite_template(&[("X-Replay", "cap-1")]),
                                opts: EmitOpts::default(),
                            },
                        ),
                        Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), GoalStep::Bye),
                    ],
                ),
                scripted_spec(
                    "bob",
                    &bob,
                    vec![
                        Goal::new(
                            Barrier::None,
                            GoalStep::RespondTemplate {
                                template: response_template(180, "Ringing", false),
                                opts: EmitOpts::default(),
                                early: None,
                            },
                        ),
                        Goal::new(
                            Barrier::None,
                            GoalStep::RespondTemplate {
                                template: response_template(200, "OK", true),
                                opts: EmitOpts::default(),
                                early: None,
                            },
                        ),
                    ],
                ),
            ],
            plan: vec![established_phase()],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        };

        let verdict = run_call(call, Duration::from_secs(5)).await;
        assert!(verdict.is_ok(), "the scripted template replay must settle OK, got {verdict:?}");
        h.finish().await;
    }

    /// ObserveFinal divergence: the plan expected a 400 but the peer answers
    /// 200 — the verdict stays Ok, the replay record disagrees, and the
    /// follow-up ACK rides the OBSERVED 2xx (the RFC gate proves the ACK).
    #[tokio::test(start_paused = true)]
    async fn observe_final_records_divergence_and_acks_observed_2xx() {
        let h = Harness::new("actor-observe-final-divergence").describe(
            "ObserveFinal{expected 400} observes a 200: verdict Ok, \
             RecordedFinal disagrees, the ACK follows the observed 2xx",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let call = CallPlan {
            actors: vec![
                caller_spec(
                    "alice",
                    &alice,
                    ("bob", bob.clone()),
                    vec![
                        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                        Goal::new(Barrier::None, GoalStep::ObserveFinal { key: 7, expected: Some(400) }),
                        Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), GoalStep::Bye),
                    ],
                ),
                ActorSpec {
                    role: "bob",
                    agent: bob.clone(),
                    disposition: Disposition::RingThenAnswer { ring: Duration::from_millis(100) },
                    media: MediaState::answer(ANSWER_SDP),
                    goals: vec![],
                    invite_targets: vec![],
                    via: None,
                    feed: CtxFeed::default(),
                },
            ],
            plan: vec![established_phase()],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        };

        let ctx = CallCtx::new();
        let obs = ObservedState::new();
        let verdict =
            run_call_with(call, obs.clone(), &ctx, Duration::from_secs(5), None).await;
        assert!(verdict.is_ok(), "divergence is data, never a failure — got {verdict:?}");
        let replay = obs.replay_record();
        assert!(
            replay.contains(&ReplayEntry::Final(RecordedFinal {
                key: 7,
                expected: Some(400),
                observed: 200,
            })),
            "the replay record carries the disagreement: {replay:?}",
        );
        h.finish().await;
    }

    /// Truncated flow: scripted goals up to the anchor, `ExpectFinal` with a
    /// class assert, then policy `Respond` + BYE completion — the whole
    /// truncated-variant lowering pattern on the engine's verbs.
    #[tokio::test(start_paused = true)]
    async fn truncated_flow_completes_after_class_assert() {
        let h = Harness::new("actor-truncated-completes").describe(
            "scripted Respond 180+200, ExpectFinal{Class(2)} at the anchor, \
             then standard BYE completion — full post-call verification",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let call = CallPlan {
            actors: vec![
                caller_spec(
                    "alice",
                    &alice,
                    ("bob", bob.clone()),
                    vec![
                        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                        Goal::new(Barrier::None, GoalStep::ExpectFinal { assert: FinalAssert::Class(2) }),
                        Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), GoalStep::Bye),
                    ],
                ),
                scripted_spec(
                    "bob",
                    &bob,
                    vec![
                        Goal::new(Barrier::None, GoalStep::Respond { status: 180 }),
                        Goal::new(Barrier::None, GoalStep::Respond { status: 200 }),
                    ],
                ),
            ],
            plan: vec![established_phase()],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        };

        let verdict = run_call(call, Duration::from_secs(5)).await;
        assert!(verdict.is_ok(), "the truncated completion must settle OK, got {verdict:?}");
        h.finish().await;
    }

    /// The truncated anchor's NEGATIVE: the fixed final never arrives (a 486
    /// where a 2xx-class was asserted) — the goal fails fast with the assert's
    /// own expectation (`expected: 200`), not the reactor's incidental shed
    /// (`expected: 180`), and never by barrier timeout.
    #[tokio::test(start_paused = true)]
    async fn truncated_class_assert_fails_fast_on_wrong_class() {
        let h = Harness::new("actor-truncated-assert-fails").describe(
            "ExpectFinal{Class(2)} observes a scripted 486: fail-fast \
             WrongStatus{expected 200, got 486} owned by the goal",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let call = CallPlan {
            actors: vec![
                caller_spec(
                    "alice",
                    &alice,
                    ("bob", bob.clone()),
                    vec![
                        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                        Goal::new(Barrier::None, GoalStep::ExpectFinal { assert: FinalAssert::Class(2) }),
                    ],
                ),
                scripted_spec(
                    "bob",
                    &bob,
                    vec![Goal::new(Barrier::None, GoalStep::Respond { status: 486 })],
                ),
            ],
            plan: vec![],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        };

        let verdict = run_call(call, Duration::from_secs(5)).await;
        match verdict {
            CallVerdict::Failed(StepError::WrongStatus { who, expected, got, .. }) => {
                assert_eq!(
                    (who.as_str(), expected, got),
                    ("alice", 200, 486),
                    "the assert (not the shed) owns the failure",
                );
            }
            other => panic!("expected the class-assert WrongStatus, got {other:?}"),
        }
        h.finish().await;
    }

    /// Incidental-failure suppression: a non-2xx final on the establishing
    /// INVITE with a RECEPTION goal next is the goal's to judge — verdict Ok,
    /// never the reactor's `WrongStatus{expected: 180}` shed.
    #[tokio::test(start_paused = true)]
    async fn reception_goal_suppresses_incidental_shed() {
        let h = Harness::new("actor-reception-suppresses-shed").describe(
            "bob rejects 486 while alice's next goal is ObserveFinal: the goal \
             owns the final (recorded), no incidental WrongStatus shed",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let call = CallPlan {
            actors: vec![
                caller_spec(
                    "alice",
                    &alice,
                    ("bob", bob.clone()),
                    vec![
                        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                        Goal::new(Barrier::None, GoalStep::ObserveFinal { key: 1, expected: Some(486) }),
                    ],
                ),
                ActorSpec {
                    role: "bob",
                    agent: bob.clone(),
                    disposition: Disposition::Reject(486),
                    media: MediaState::none(),
                    goals: vec![],
                    invite_targets: vec![],
                    via: None,
                    feed: CtxFeed::default(),
                },
            ],
            plan: vec![],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        };

        let ctx = CallCtx::new();
        let obs = ObservedState::new();
        let verdict =
            run_call_with(call, obs.clone(), &ctx, Duration::from_secs(5), None).await;
        assert!(verdict.is_ok(), "the reception goal owns the 486, got {verdict:?}");
        assert!(
            obs.replay_record().contains(&ReplayEntry::Final(RecordedFinal {
                key: 1,
                expected: Some(486),
                observed: 486,
            })),
            "the observed final is recorded: {:?}",
            obs.replay_record(),
        );
        h.finish().await;
    }

    /// Parked-CANCEL fail-fast: the CANCEL automatic consumes the parked
    /// initial INVITE (200 + 487); the later `RespondTemplate` bound to it
    /// fails immediately with the bounded StepError naming the automatic —
    /// never a 32 s goal timeout.
    #[tokio::test(start_paused = true)]
    async fn cancel_consumed_parked_invite_fails_respond_fast() {
        let h = Harness::new("actor-parked-cancel-fail-fast").describe(
            "alice CANCELs the parked INVITE (CANCEL→200+487 automatic); bob's \
             later RespondTemplate fails fast with the bounded tombstone error",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let call = CallPlan {
            actors: vec![
                caller_spec(
                    "alice",
                    &alice,
                    ("bob", bob.clone()),
                    vec![
                        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                        Goal::new(Barrier::None, GoalStep::Cancel).after(Duration::from_millis(200)),
                    ],
                ),
                scripted_spec(
                    "bob",
                    &bob,
                    // Gated PAST the cancellation, so the automatic has already
                    // consumed the parked INVITE when the script reaches it.
                    vec![Goal::new(
                        Barrier::pred("caller_gone", |s| {
                            s.leg_at_least("alice", LegPhase::Terminated)
                        }),
                        GoalStep::RespondTemplate {
                            template: response_template(200, "OK", true),
                            opts: EmitOpts::default(),
                            early: None,
                        },
                    )],
                ),
            ],
            plan: vec![],
            settle: SettleBarrier::default_ceiling(),
            // The §5 automatic: the parked INVITE draws an immediate 100, so
            // the CANCEL follows a provisional (RFC 3261 §9.1).
            automatics: Automatics { answer_100_trying: true },
        };

        let verdict = run_call(call, Duration::from_secs(5)).await;
        match verdict {
            CallVerdict::Failed(StepError::UnexpectedKind { who, detail }) => {
                assert_eq!(who, "bob");
                assert!(
                    detail.contains("consumed by an automatic"),
                    "the bounded error names the automatic: {detail}",
                );
            }
            other => panic!("expected the bounded tombstone StepError, got {other:?}"),
        }
        h.finish().await;
    }

    /// Requeue-on-advance: two INFOs park while one `ExpectRequest{Info}`
    /// remains; consuming the first advances the cursor, and the second — now
    /// matching no remaining goal — is auto-reacted (200) as a recorded stray
    /// instead of starving behind the script.
    #[tokio::test(start_paused = true)]
    async fn requeue_on_advance_auto_reacts_passed_parked_request() {
        use sip_message::generators::InDialogMethod;

        let h = Harness::new("actor-requeue-on-advance").describe(
            "two parked INFOs, one ExpectRequest{Info}: the consume advances \
             the cursor and the second INFO is auto-reacted as a stray",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let info = |body: &[u8]| GoalStep::InDialog {
            method: InDialogMethod::Info,
            content_type: Some("application/x-cap-test".to_string()),
            body: Some(body.to_vec()),
            headers: vec![],
        };
        let call = CallPlan {
            actors: vec![
                caller_spec(
                    "alice",
                    &alice,
                    ("bob", bob.clone()),
                    vec![
                        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                        Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), info(b"one")),
                        Goal::new(Barrier::None, info(b"two")),
                        Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), GoalStep::Bye)
                            .after(Duration::from_millis(400)),
                    ],
                ),
                scripted_spec(
                    "bob",
                    &bob,
                    vec![
                        Goal::new(
                            Barrier::None,
                            GoalStep::ExpectRequest {
                                kind: RequestKind::Initial,
                                body: BodyExpect::SdpPresent,
                                matcher: None,
                            },
                        ),
                        Goal::new(Barrier::None, GoalStep::Respond { status: 200 }),
                        // Dwell long enough for BOTH INFOs to park before the
                        // first is consumed — the requeue setup.
                        Goal::new(
                            Barrier::None,
                            GoalStep::ExpectRequest {
                                kind: RequestKind::InDialog(InDialogMethod::Info),
                                body: BodyExpect::Present,
                                matcher: None,
                            },
                        )
                        .after(Duration::from_millis(100)),
                        Goal::new(Barrier::None, GoalStep::Respond { status: 200 }),
                    ],
                ),
            ],
            plan: vec![established_phase()],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        };

        let ctx = CallCtx::new();
        let obs = ObservedState::new();
        let verdict =
            run_call_with(call, obs.clone(), &ctx, Duration::from_secs(5), None).await;
        assert!(verdict.is_ok(), "both INFOs must be answered, got {verdict:?}");
        let replay = obs.replay_record();
        assert!(
            replay.iter().any(|e| matches!(
                e,
                ReplayEntry::ServicedStray { leg: "bob", method, action }
                    if method == "INFO" && action.contains("advance")
            )),
            "the requeued INFO is a recorded stray: {replay:?}",
        );
        h.finish().await;
    }

    /// ExpectResponse provisional strictness: expecting a 183 but the FINAL
    /// (a 486) arrives first — fail-fast with the goal's own expectation.
    #[tokio::test(start_paused = true)]
    async fn expect_response_fails_fast_when_final_precedes_provisional() {
        let h = Harness::new("actor-expect-provisional-strict").describe(
            "ExpectResponse{183} sees the 486 final first: fail-fast \
             WrongStatus{expected 183, got 486}, owned by the goal",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let call = CallPlan {
            actors: vec![
                caller_spec(
                    "alice",
                    &alice,
                    ("bob", bob.clone()),
                    vec![
                        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                        Goal::new(
                            Barrier::None,
                            GoalStep::ExpectResponse {
                                status: 183,
                                body: BodyExpect::Any,
                                early: None,
                                ack_body: None,
                                matcher: None,
                            },
                        ),
                    ],
                ),
                ActorSpec {
                    role: "bob",
                    agent: bob.clone(),
                    disposition: Disposition::Reject(486),
                    media: MediaState::none(),
                    goals: vec![],
                    invite_targets: vec![],
                    via: None,
                    feed: CtxFeed::default(),
                },
            ],
            plan: vec![],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        };

        let verdict = run_call(call, Duration::from_secs(5)).await;
        match verdict {
            CallVerdict::Failed(StepError::WrongStatus { who, expected, got, .. }) => {
                assert_eq!(
                    (who.as_str(), expected, got),
                    ("alice", 183, 486),
                    "the goal's expectation (183), never the shed's (180)",
                );
            }
            other => panic!("expected the strict provisional WrongStatus, got {other:?}"),
        }
        h.finish().await;
    }

    /// Forked RespondTemplate early ids: two distinct fork ids emit
    /// distinct-tag 180s on the ONE parked INVITE transaction; the final's id
    /// names the winner (its tag becomes the dialog tag) and the loser fork
    /// settles with no final — the caller's reception goals bind per fork.
    #[tokio::test(start_paused = true)]
    async fn forked_respond_template_early_ids_name_winner() {
        let h = Harness::new("actor-scripted-forked-early-ids").describe(
            "RespondTemplate early f1/f2 → 180(f1)+180(f2) on one txn, 200 \
             under winner f2; alice's ExpectResponse binds each fork by id",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let expect_180 = |id: EarlyId| GoalStep::ExpectResponse {
            status: 180,
            body: BodyExpect::Any,
            early: Some(id),
            ack_body: None,
            matcher: None,
        };
        let respond_180 = |id: EarlyId| GoalStep::RespondTemplate {
            template: response_template(180, "Ringing", false),
            opts: EmitOpts::default(),
            early: Some(id),
        };
        let call = CallPlan {
            actors: vec![
                caller_spec(
                    "alice",
                    &alice,
                    ("bob", bob.clone()),
                    vec![
                        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                        Goal::new(Barrier::None, expect_180("f1")),
                        Goal::new(Barrier::None, expect_180("f2")),
                        Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), GoalStep::Bye),
                    ],
                ),
                scripted_spec(
                    "bob",
                    &bob,
                    vec![
                        Goal::new(Barrier::None, respond_180("f1")),
                        Goal::new(Barrier::None, respond_180("f2")),
                        Goal::new(
                            Barrier::None,
                            GoalStep::RespondTemplate {
                                template: response_template(200, "OK", true),
                                opts: EmitOpts::default(),
                                early: Some("f2"),
                            },
                        ),
                    ],
                ),
            ],
            plan: vec![established_phase()],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        };

        let verdict = run_call(call, Duration::from_secs(5)).await;
        assert!(verdict.is_ok(), "the forked scripted answer must settle OK, got {verdict:?}");
        h.finish().await;
    }

    /// Scripted originator attribution: the caller is the actor whose FIRST
    /// goal originates the dialog (`InviteTemplate`), not `Disposition::Caller`
    /// and not the `"alice"` fallback — an `Expect::Reject` terminal carries
    /// that actor's role.
    #[tokio::test]
    async fn scripted_originator_attribution_keys_on_first_goal() {
        let h = Harness::new("actor-scripted-attribution").describe(
            "originating_role keys on the first Invite/InviteTemplate goal: a \
             Scripted carol originator is the attributed caller, not alice",
        );
        let carol = h.agent("carol", "127.0.0.1:5061").await;
        let bob = h.agent("bob", "127.0.0.1:5071").await;

        let actors = vec![
            ActorSpec {
                role: "carol",
                agent: carol.clone(),
                disposition: Disposition::Scripted,
                media: MediaState::none(),
                goals: vec![Goal::new(
                    Barrier::None,
                    GoalStep::InviteTemplate {
                        callee: "bob",
                        plan: None,
                        template: invite_template(&[]),
                        opts: EmitOpts::default(),
                    },
                )],
                invite_targets: vec![("bob", bob.clone())],
                via: None,
                feed: CtxFeed::default(),
            },
            scripted_spec("bob", &bob, vec![]),
        ];
        assert_eq!(originating_role(&actors), "carol");

        // The Reject terminal is attributed to carol, never "alice".
        let obs = ObservedState::new();
        obs.record(
            Observation::LegFinal { leg: "carol", status: 486, reason: "Busy Here".into() },
            tokio::time::Instant::now(),
        );
        match into_result(Expect::Reject(486), CallVerdict::Ok, &obs, originating_role(&actors)) {
            Err(StepError::WrongStatus { who, expected, got, .. }) => {
                assert_eq!((who.as_str(), expected, got), ("carol", 200, 486));
            }
            other => panic!("expected the Reject terminal under carol, got {other:?}"),
        }
        h.finish().await;
    }

    /// One `answer_100_trying` run: alice invites, Scripted bob answers by
    /// policy, BYE teardown — returns whether alice observed a `100`.
    async fn run_100_trying_case(name: &'static str, on: bool) -> bool {
        let h = Harness::new(name).describe(
            "Automatics{answer_100_trying}: the parked INVITE draws (or not) an \
             immediate 100 Trying; the call settles clean either way",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let call = CallPlan {
            actors: vec![
                caller_spec(
                    "alice",
                    &alice,
                    ("bob", bob.clone()),
                    vec![
                        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                        Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), GoalStep::Bye),
                    ],
                ),
                scripted_spec(
                    "bob",
                    &bob,
                    vec![
                        Goal::new(Barrier::None, GoalStep::Respond { status: 180 }),
                        Goal::new(Barrier::None, GoalStep::Respond { status: 200 }),
                    ],
                ),
            ],
            plan: vec![established_phase()],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics { answer_100_trying: on },
        };

        let ctx = CallCtx::new();
        let obs = ObservedState::new();
        let verdict =
            run_call_with(call, obs.clone(), &ctx, Duration::from_secs(5), None).await;
        assert!(verdict.is_ok(), "[{name}] must settle clean, got {verdict:?}");
        h.finish().await;
        obs.with_snapshot(|s| s.leg("alice").saw_status(100))
    }

    /// `Automatics{answer_100_trying}`: ON emits the immediate 100 on the
    /// parked INVITE; OFF does not — both settle clean on the fake lane.
    #[tokio::test(start_paused = true)]
    async fn answer_100_trying_automatic_toggles_emission() {
        assert!(
            run_100_trying_case("actor-100-trying-on", true).await,
            "with the automatic ON, alice observes the 100",
        );
        assert!(
            !run_100_trying_case("actor-100-trying-off", false).await,
            "with the automatic OFF, no 100 is emitted",
        );
    }

    /// One initial-INVITE matcher run: alice's `InviteTemplate` carries a
    /// frozen `X-Cap: v1`; bob's `ExpectRequest{Initial}` matcher pins
    /// `X-Cap: expect_value` plus CAPTURE-time tier-1 rows (Call-ID/Via/CSeq —
    /// regenerated live, never value-compared). Returns the verdict.
    async fn run_initial_matcher_case(name: &'static str, expect_value: &str) -> CallVerdict {
        let h = Harness::new(name).describe(
            "ExpectRequest{matcher} on the initial INVITE: frozen X-Cap \
             compared; captured tier-1 values are structural-only",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let matcher = MessageTemplate::request(
            Method::Invite,
            vec![
                TemplateHeader::classified("Via", "SIP/2.0/UDP 9.9.9.9:9;branch=z9hG4bK-cap"),
                TemplateHeader::classified("Call-ID", "captured@9.9.9.9"),
                TemplateHeader::classified("CSeq", "7 INVITE"),
                TemplateHeader::frozen("X-Cap", expect_value),
                TemplateHeader::frozen("Content-Type", "application/sdp"),
            ],
            OFFER_SDP.as_bytes().to_vec(),
        );
        let call = CallPlan {
            actors: vec![
                caller_spec(
                    "alice",
                    &alice,
                    ("bob", bob.clone()),
                    vec![
                        Goal::new(
                            Barrier::None,
                            GoalStep::InviteTemplate {
                                callee: "bob",
                                plan: None,
                                template: invite_template(&[("X-Cap", "v1")]),
                                opts: EmitOpts::default(),
                            },
                        ),
                        Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), GoalStep::Bye),
                    ],
                ),
                scripted_spec(
                    "bob",
                    &bob,
                    vec![
                        Goal::new(
                            Barrier::None,
                            GoalStep::ExpectRequest {
                                kind: RequestKind::Initial,
                                body: BodyExpect::SdpPresent,
                                matcher: Some(matcher),
                            },
                        ),
                        Goal::new(Barrier::None, GoalStep::Respond { status: 200 }),
                    ],
                ),
            ],
            plan: vec![established_phase()],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        };

        let verdict = run_call(call, Duration::from_secs(5)).await;
        if verdict.is_ok() {
            h.finish().await;
        }
        verdict
    }

    /// Content matcher POSITIVE, paired with the sibling negative on the SAME
    /// goal shape: the identical configuration failing on a drifted value
    /// proves the matcher RUNS here, so the passing case demonstrably held
    /// (a silently-skipped matcher would make the sibling pass too).
    #[tokio::test(start_paused = true)]
    async fn content_matcher_holds_on_frozen_header() {
        let ok = run_initial_matcher_case("actor-content-matcher-holds", "v1").await;
        assert!(ok.is_ok(), "the matching value must hold, got {ok:?}");

        let bad = run_initial_matcher_case("actor-content-matcher-holds-neg", "v2").await;
        match bad {
            CallVerdict::Failed(StepError::UnexpectedKind { who, detail }) => {
                assert_eq!(who, "bob");
                assert!(
                    detail.contains("x-cap") && detail.contains("v2") && detail.contains("v1"),
                    "the same shape fails on a drifted value — the matcher runs: {detail}",
                );
            }
            other => panic!("the sibling negative must fail the matcher, got {other:?}"),
        }
    }

    /// A templated in-dialog re-INVITE (`RequestTemplate`, delayed offer):
    /// frozen header rides, the ReInvite obligation opens, the peer's 2xx is
    /// ACKed with the answer SDP, and the renegotiation completes — settle
    /// clean.
    #[tokio::test(start_paused = true)]
    async fn request_template_reinvite_completes_renegotiation() {
        let h = Harness::new("actor-request-template-reinvite").describe(
            "RequestTemplate re-INVITE (bodyless, frozen X header) on the \
             confirmed dialog: 2xx ACKed with the answer SDP, reneg completes",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let reinvite_tmpl = MessageTemplate::request(
            Method::Invite,
            vec![TemplateHeader::frozen("X-Renegotiate", "cap-1")],
            Vec::new(),
        );
        let call = CallPlan {
            actors: vec![
                ActorSpec {
                    role: "alice",
                    agent: alice.clone(),
                    disposition: Disposition::Caller,
                    media: MediaState::full(OFFER_SDP, ANSWER_SDP),
                    goals: vec![
                        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                        Goal::new(
                            Barrier::AllConfirmed(&["alice", "bob"]),
                            GoalStep::RequestTemplate {
                                template: reinvite_tmpl,
                                opts: EmitOpts::default(),
                                early: false,
                            },
                        ),
                        Goal::new(
                            Barrier::pred("reneg_done", |s| s.leg("alice").reneg_count() >= 1),
                            GoalStep::Bye,
                        ),
                    ],
                    invite_targets: vec![("bob", bob.clone())],
                    via: None,
                    feed: CtxFeed::default(),
                },
                ActorSpec {
                    role: "bob",
                    agent: bob.clone(),
                    disposition: Disposition::RingThenAnswer { ring: Duration::from_millis(100) },
                    media: MediaState::full(ANSWER_SDP, ANSWER_SDP),
                    goals: vec![],
                    invite_targets: vec![],
                    via: None,
                    feed: CtxFeed::default(),
                },
            ],
            plan: vec![established_phase()],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        };

        let verdict = run_call(call, Duration::from_secs(5)).await;
        assert!(verdict.is_ok(), "the templated re-INVITE must complete, got {verdict:?}");
        h.finish().await;
    }

    /// A templated BYE (`RequestTemplate`): the teardown discharge runs, the
    /// Bye obligation opens and its 200 terminates the leg — clean teardown.
    #[tokio::test(start_paused = true)]
    async fn request_template_bye_tears_down() {
        let h = Harness::new("actor-request-template-bye").describe(
            "RequestTemplate BYE (frozen X header) tears the call down with \
             the semantic Bye goal's bookkeeping",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let bye_tmpl = MessageTemplate::request(
            Method::Bye,
            vec![TemplateHeader::frozen("X-Hangup", "cap-1")],
            Vec::new(),
        );
        let call = CallPlan {
            actors: vec![
                caller_spec(
                    "alice",
                    &alice,
                    ("bob", bob.clone()),
                    vec![
                        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                        Goal::new(
                            Barrier::AllConfirmed(&["alice", "bob"]),
                            GoalStep::RequestTemplate {
                                template: bye_tmpl,
                                opts: EmitOpts::default(),
                                early: false,
                            },
                        ),
                    ],
                ),
                ActorSpec {
                    role: "bob",
                    agent: bob.clone(),
                    disposition: Disposition::RingThenAnswer { ring: Duration::from_millis(100) },
                    media: MediaState::answer(ANSWER_SDP),
                    goals: vec![],
                    invite_targets: vec![],
                    via: None,
                    feed: CtxFeed::default(),
                },
            ],
            plan: vec![established_phase()],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        };

        let verdict = run_call(call, Duration::from_secs(5)).await;
        assert!(verdict.is_ok(), "the templated BYE must tear down clean, got {verdict:?}");
        h.finish().await;
    }

    /// A templated EARLY UPDATE (`RequestTemplate{early: true}`, RFC 3311
    /// §5.1): rides the still-pending INVITE's early dialog after the PRACK,
    /// its 200 releases the callee's held INVITE 200 — the template twin of
    /// the `UpdateEarly` goal.
    #[tokio::test(start_paused = true)]
    async fn request_template_early_update_rides_early_dialog() {
        let h = Harness::new("actor-request-template-early-update").describe(
            "100rel INVITE → reliable 183 → PRACK → templated EARLY UPDATE \
             (200) → final 200 INVITE → ACK → BYE",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let plan = crate::realcall::InvitePlan {
            via: bob.addr(),
            from: None,
            to: None,
            ruri: None,
            headers: vec![("Supported".to_string(), "100rel".to_string())],
            rewrite: Default::default(),
        };
        let update_tmpl = MessageTemplate::request(
            Method::Update,
            vec![TemplateHeader::frozen("Content-Type", "application/sdp")],
            OFFER_SDP.as_bytes().to_vec(),
        );
        let call = CallPlan {
            actors: vec![
                ActorSpec {
                    role: "alice",
                    agent: alice.clone(),
                    media: MediaState::full(OFFER_SDP, ANSWER_SDP),
                    disposition: Disposition::Caller,
                    goals: vec![
                        Goal::new(
                            Barrier::None,
                            GoalStep::Invite { callee: "bob", plan: Some(plan) },
                        ),
                        Goal::new(
                            Barrier::pred("early", |s| {
                                s.leg("alice").subflow(SUBFLOW_EARLY).is_some()
                            }),
                            GoalStep::RequestTemplate {
                                template: update_tmpl,
                                opts: EmitOpts::default(),
                                early: true,
                            },
                        ),
                        Goal::new(
                            Barrier::pred("confirmed", |s| {
                                s.leg_at_least("alice", LegPhase::Confirmed)
                            }),
                            GoalStep::Bye,
                        ),
                    ],
                    invite_targets: vec![("bob", bob.clone())],
                    via: None,
                    feed: CtxFeed::default(),
                },
                ActorSpec {
                    role: "bob",
                    agent: bob.clone(),
                    disposition: Disposition::ReliableAnswerEarlyUpdate,
                    media: MediaState::answer(ANSWER_SDP),
                    goals: vec![],
                    invite_targets: vec![],
                    via: None,
                    feed: CtxFeed::default(),
                },
            ],
            plan: vec![phase("confirmed", |s| s.leg_at_least("alice", LegPhase::Confirmed))],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        };

        let verdict = run_call(call, Duration::from_secs(5)).await;
        assert!(verdict.is_ok(), "the templated early UPDATE must settle OK, got {verdict:?}");
        h.finish().await;
    }

    /// A TEMPLATED re-INVITE drawn into a §14.1 glare: both ends offer at
    /// once, both get 491 — the templated transaction's retained handle
    /// hop-ACKs the 491 (`sent_reinvite_txns`), the back-off retries resolve,
    /// and both rounds complete.
    #[tokio::test(start_paused = true)]
    async fn request_template_reinvite_glare_491_hop_acks_and_retries() {
        let h = Harness::new("actor-request-template-glare").describe(
            "templated re-INVITE × bob re-INVITE at once → 491 both ways \
             (templated txn hop-ACKs) → back-off retries → both complete → BYE",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let reinvite_tmpl = MessageTemplate::request(
            Method::Invite,
            vec![TemplateHeader::frozen("X-Renegotiate", "cap-1")],
            Vec::new(),
        );
        let both_confirmed = Barrier::AllConfirmed(&["alice", "bob"]);
        let both_reneged = Barrier::pred("glare_resolved", |s| {
            s.leg("alice").reneg_count() >= 1 && s.leg("bob").reneg_count() >= 1
        });
        let call = CallPlan {
            actors: vec![
                ActorSpec {
                    role: "alice",
                    agent: alice.clone(),
                    media: MediaState::full(OFFER_SDP, ANSWER_SDP),
                    disposition: Disposition::Caller,
                    goals: vec![
                        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                        Goal::new(
                            both_confirmed.clone(),
                            GoalStep::RequestTemplate {
                                template: reinvite_tmpl,
                                opts: EmitOpts::default(),
                                early: false,
                            },
                        ),
                        Goal::new(both_reneged.clone(), GoalStep::Bye),
                    ],
                    invite_targets: vec![("bob", bob.clone())],
                    via: None,
                    feed: CtxFeed::default(),
                },
                ActorSpec {
                    role: "bob",
                    agent: bob.clone(),
                    disposition: Disposition::RingThenAnswer { ring: Duration::from_millis(100) },
                    media: MediaState::full(ANSWER_SDP, ANSWER_SDP),
                    goals: vec![Goal::new(both_confirmed.clone(), GoalStep::Reinvite)],
                    invite_targets: vec![],
                    via: None,
                    feed: CtxFeed::default(),
                },
            ],
            plan: vec![
                established_phase(),
                phase("glare_resolved", |s| {
                    s.leg("alice").reneg_count() >= 1 && s.leg("bob").reneg_count() >= 1
                }),
            ],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        };

        let verdict = run_call(call, Duration::from_secs(10)).await;
        assert!(verdict.is_ok(), "the templated glare must resolve via §14.1, got {verdict:?}");
        h.finish().await;
    }

    /// A scripted 2xx answer to a RECEIVED re-INVITE (`ExpectRequest{InDialog
    /// (Invite)}` + `Respond{200}`): opens the answered-awaiting-ACK
    /// obligation — observed OPEN mid-call by a barrier probe — which the
    /// peer's ACK then closes, so settle held exactly until the ACK.
    #[tokio::test(start_paused = true)]
    async fn scripted_reinvite_answer_holds_settle_until_ack() {
        use sip_message::generators::InDialogMethod;
        use std::sync::atomic::{AtomicBool, Ordering};

        let h = Harness::new("actor-scripted-reinvite-answer").describe(
            "scripted 200 to a received re-INVITE: the realign obligation is \
             observed open until the peer's ACK closes it; settle clean",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let saw_open = Arc::new(AtomicBool::new(false));
        let probe = saw_open.clone();
        let call = CallPlan {
            actors: vec![
                ActorSpec {
                    role: "alice",
                    agent: alice.clone(),
                    disposition: Disposition::Caller,
                    media: MediaState::full(OFFER_SDP, ANSWER_SDP),
                    goals: vec![
                        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                        Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), GoalStep::Reinvite),
                        // The guard polls on every observed-state tick while
                        // pending: it witnesses the scripted answer's
                        // obligation OPEN (before alice's ACK closes it).
                        Goal::new(
                            Barrier::pred("reneg_done", move |s| {
                                if s.describe_open()
                                    .iter()
                                    .any(|o| o.contains("bob:re-INVITE") && o.contains("realign"))
                                {
                                    probe.store(true, Ordering::SeqCst);
                                }
                                s.leg("alice").reneg_count() >= 1
                            }),
                            GoalStep::Bye,
                        ),
                    ],
                    invite_targets: vec![("bob", bob.clone())],
                    via: None,
                    feed: CtxFeed::default(),
                },
                scripted_spec(
                    "bob",
                    &bob,
                    vec![
                        Goal::new(
                            Barrier::None,
                            GoalStep::ExpectRequest {
                                kind: RequestKind::Initial,
                                body: BodyExpect::SdpPresent,
                                matcher: None,
                            },
                        ),
                        Goal::new(Barrier::None, GoalStep::Respond { status: 200 }),
                        // The delayed-offer re-INVITE is bodyless.
                        Goal::new(
                            Barrier::None,
                            GoalStep::ExpectRequest {
                                kind: RequestKind::InDialog(InDialogMethod::Invite),
                                body: BodyExpect::Any,
                                matcher: None,
                            },
                        ),
                        Goal::new(Barrier::None, GoalStep::Respond { status: 200 }),
                    ],
                ),
            ],
            plan: vec![established_phase()],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        };

        let verdict = run_call(call, Duration::from_secs(5)).await;
        assert!(verdict.is_ok(), "the scripted realign must settle OK, got {verdict:?}");
        assert!(
            saw_open.load(Ordering::SeqCst),
            "the answered-awaiting-ACK obligation was observed OPEN mid-call — \
             it held settle until the peer's ACK closed it",
        );
        h.finish().await;
    }

    /// A scripted 200 answer to a RECEIVED BYE (`ExpectRequest{InDialog(Bye)}`
    /// + `Respond{200}`): the script — not the reactor — services the
    /// teardown, with the leg-termination bookkeeping and no stray entry.
    #[tokio::test(start_paused = true)]
    async fn scripted_bye_answer_tears_down_cleanly() {
        use sip_message::generators::InDialogMethod;

        let h = Harness::new("actor-scripted-bye-answer").describe(
            "scripted 200 to a received BYE: script-owned teardown, clean \
             settle, no serviced-stray entry for the BYE",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let call = CallPlan {
            actors: vec![
                caller_spec(
                    "alice",
                    &alice,
                    ("bob", bob.clone()),
                    vec![
                        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                        Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), GoalStep::Bye),
                    ],
                ),
                scripted_spec(
                    "bob",
                    &bob,
                    vec![
                        Goal::new(
                            Barrier::None,
                            GoalStep::ExpectRequest {
                                kind: RequestKind::Initial,
                                body: BodyExpect::SdpPresent,
                                matcher: None,
                            },
                        ),
                        Goal::new(Barrier::None, GoalStep::Respond { status: 200 }),
                        Goal::new(
                            Barrier::None,
                            GoalStep::ExpectRequest {
                                kind: RequestKind::InDialog(InDialogMethod::Bye),
                                body: BodyExpect::Any,
                                matcher: None,
                            },
                        ),
                        Goal::new(Barrier::None, GoalStep::Respond { status: 200 }),
                    ],
                ),
            ],
            plan: vec![established_phase()],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        };

        let ctx = CallCtx::new();
        let obs = ObservedState::new();
        let verdict =
            run_call_with(call, obs.clone(), &ctx, Duration::from_secs(5), None).await;
        assert!(verdict.is_ok(), "the scripted BYE answer must settle OK, got {verdict:?}");
        assert!(
            !obs.replay_record().iter().any(|e| matches!(
                e,
                ReplayEntry::ServicedStray { method, .. } if method == "BYE"
            )),
            "the SCRIPT serviced the BYE — no stray entry: {:?}",
            obs.replay_record(),
        );
        h.finish().await;
    }

    /// The `ack_body` override end-to-end: the ACK to a delayed-offer
    /// re-INVITE 2xx carries the `ExpectResponse{ack_body}` bytes instead of
    /// the engine-built answer SDP. (A byte-identical retransmitted 2xx is
    /// absorbed below `recv_any` by the fake lane's receive-view dedup, so the
    /// re-surfacing idempotence is pinned by the unit test below.)
    #[tokio::test(start_paused = true)]
    async fn ack_body_override_rides_the_reinvite_ack() {
        const CUSTOM_ACK_SDP: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";

        let h = Harness::new("actor-ack-body-override").describe(
            "ExpectResponse{ack_body} on a delayed-offer re-INVITE 2xx: the \
             ACK carries the override bytes, not the engine answer SDP",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        // The hand-rolled peer answers, 2xx's the re-INVITE, and asserts the
        // ACK body is the override.
        let bob_srv = bob.clone();
        let server = tokio::spawn(async move {
            let bob = bob_srv;
            let mut inv = bob.try_receive("INVITE").await.unwrap();
            inv.respond(180, "Ringing").try_send().await.unwrap();
            inv.respond(200, "OK").with_sdp(ANSWER_SDP).try_send().await.unwrap();
            bob.try_receive("ACK").await.unwrap();
            let mut re = bob.try_receive("INVITE").await.unwrap();
            re.respond(200, "OK").with_sdp(ANSWER_SDP).try_send().await.unwrap();
            let ack = bob.try_receive("ACK").await.unwrap();
            assert_eq!(
                ack.request().body,
                CUSTOM_ACK_SDP.as_bytes(),
                "the ACK carries the ack_body override",
            );
            bob.try_receive("BYE").await.unwrap().respond(200, "OK").try_send().await.unwrap();
        });

        let alice_confirmed =
            Barrier::pred("alice_confirmed", |s| s.leg_at_least("alice", LegPhase::Confirmed));
        let expect = |status: u16, ack_body: Option<Vec<u8>>| GoalStep::ExpectResponse {
            status,
            body: BodyExpect::Any,
            early: None,
            ack_body,
            matcher: None,
        };
        let call = CallPlan {
            actors: vec![caller_spec(
                "alice",
                &alice,
                ("bob", bob.clone()),
                vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                    // Consume the establishment responses in order so the
                    // re-INVITE reception goal aligns on ITS final.
                    Goal::new(Barrier::None, expect(180, None)),
                    Goal::new(Barrier::None, expect(200, None)),
                    Goal::new(alice_confirmed, GoalStep::Reinvite),
                    Goal::new(
                        Barrier::None,
                        expect(200, Some(CUSTOM_ACK_SDP.as_bytes().to_vec())),
                    ),
                    Goal::new(Barrier::None, GoalStep::Bye).after(Duration::from_millis(100)),
                ],
            )],
            plan: vec![],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        };

        let verdict = run_call(call, Duration::from_secs(5)).await;
        assert!(verdict.is_ok(), "the override call must settle OK, got {verdict:?}");
        server.await.unwrap();
        h.finish().await;
    }

    /// The ACK-body resolution is resolved ONCE per CSeq and cached: a 2xx
    /// re-surfacing AFTER the goal cursor advanced past the override-carrying
    /// goal still draws the identical override bytes (RFC 3261 §13.2.2.4),
    /// and a different CSeq resolved later falls to the engine default.
    #[test]
    fn ack_body_resolution_is_cached_per_cseq() {
        use std::collections::HashMap;

        let mut cache: HashMap<u32, String> = HashMap::new();
        let override_goal = GoalStep::ExpectResponse {
            status: 200,
            body: BodyExpect::Any,
            early: None,
            ack_body: Some(b"custom-answer".to_vec()),
            matcher: None,
        };
        // First resolution: the pending override wins and is cached.
        assert_eq!(
            actor::resolve_ack_body(&mut cache, Some(&override_goal), "engine-sdp", 2),
            "custom-answer",
        );
        // The 2xx re-surfaces after the cursor advanced (next goal is Bye):
        // the CACHED bytes are re-emitted, never the engine default.
        assert_eq!(
            actor::resolve_ack_body(&mut cache, Some(&GoalStep::Bye), "engine-sdp", 2),
            "custom-answer",
        );
        // A different CSeq with no pending override takes the engine default.
        assert_eq!(
            actor::resolve_ack_body(&mut cache, Some(&GoalStep::Bye), "engine-sdp", 3),
            "engine-sdp",
        );
    }

    /// The per-goal deadline override bounds the guard wait tighter than the
    /// 32 s step timeout: a never-holding guard with `.deadline(2s)` fails the
    /// actor with the barrier's bounded Timeout well before the ceiling.
    #[tokio::test(start_paused = true)]
    async fn per_goal_deadline_bounds_the_guard_wait() {
        let h = Harness::new("actor-goal-deadline").describe(
            "a capture-declared tighter bound: .deadline(2s) on a never-holding \
             guard times the goal out long before the 32 s step timeout",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;

        let call = CallPlan {
            actors: vec![ActorSpec {
                role: "alice",
                agent: alice.clone(),
                disposition: Disposition::Caller,
                media: MediaState::none(),
                goals: vec![Goal::new(Barrier::pred("never", |_| false), GoalStep::Bye)
                    .deadline(Duration::from_secs(2))],
                invite_targets: vec![],
                via: None,
                feed: CtxFeed::default(),
            }],
            plan: vec![],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        };

        let started = tokio::time::Instant::now();
        let verdict = run_call(call, Duration::from_secs(32)).await;
        let elapsed = started.elapsed();
        match verdict {
            CallVerdict::Failed(StepError::Timeout { who }) => assert_eq!(who, "never"),
            other => panic!("expected the bounded guard Timeout, got {other:?}"),
        }
        assert!(
            elapsed < Duration::from_secs(10),
            "the 2 s per-goal deadline (not the 32 s ceiling) bounded the wait: {elapsed:?}",
        );
        h.finish().await;
    }

    /// Content matcher NEGATIVE: a changed frozen-header value fails fast at
    /// consume time with the match surface's detailed finding. Staged on an
    /// in-dialog INFO so the establishment (and the wire) stays clean.
    #[tokio::test(start_paused = true)]
    async fn content_matcher_fails_fast_on_changed_value() {
        use sip_message::generators::InDialogMethod;

        let h = Harness::new("actor-content-matcher-fails").describe(
            "ExpectRequest{matcher} on an INFO whose X-Cap drifted: fail-fast \
             with the template-match finding naming header and values",
        );
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;

        let matcher = MessageTemplate::request(
            Method::Info,
            vec![TemplateHeader::frozen("X-Cap", "v2")],
            b"payload".to_vec(),
        );
        let call = CallPlan {
            actors: vec![
                caller_spec(
                    "alice",
                    &alice,
                    ("bob", bob.clone()),
                    vec![
                        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                        Goal::new(
                            Barrier::AllConfirmed(&["alice", "bob"]),
                            GoalStep::InDialog {
                                method: InDialogMethod::Info,
                                content_type: Some("application/x-cap-test".to_string()),
                                body: Some(b"payload".to_vec()),
                                headers: vec![("X-Cap".to_string(), "v1".to_string())],
                            },
                        ),
                    ],
                ),
                scripted_spec(
                    "bob",
                    &bob,
                    vec![
                        Goal::new(
                            Barrier::None,
                            GoalStep::ExpectRequest {
                                kind: RequestKind::Initial,
                                body: BodyExpect::SdpPresent,
                                matcher: None,
                            },
                        ),
                        Goal::new(Barrier::None, GoalStep::Respond { status: 200 }),
                        Goal::new(
                            Barrier::None,
                            GoalStep::ExpectRequest {
                                kind: RequestKind::InDialog(InDialogMethod::Info),
                                body: BodyExpect::Present,
                                matcher: Some(matcher),
                            },
                        ),
                        Goal::new(Barrier::None, GoalStep::Respond { status: 200 }),
                    ],
                ),
            ],
            plan: vec![established_phase()],
            settle: SettleBarrier::default_ceiling(),
            automatics: Automatics::default(),
        };

        let verdict = run_call(call, Duration::from_secs(5)).await;
        match verdict {
            CallVerdict::Failed(StepError::UnexpectedKind { who, detail }) => {
                assert_eq!(who, "bob");
                assert!(
                    detail.contains("x-cap") && detail.contains("v2") && detail.contains("v1"),
                    "the match surface's finding names header and values: {detail}",
                );
            }
            other => panic!("expected the matcher's fail-fast finding, got {other:?}"),
        }
        h.finish().await;
    }
}
