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
mod settle;
mod state;

use std::sync::Arc;
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};

use crate::realcall::{CallCtx, CallScope};
use crate::StepError;

pub use actor::{run_actor, ActorSpec, ActorState, Disposition, MediaState};
pub use goals::{Barrier, Goal, GoalCursor, GoalStep};
pub use ledger::{ObligationKey, ObligationKind, ObligationLedger};
pub use settle::{SettleBarrier, SettleVerdict, T1};
pub use state::{
    await_pred, LegObservation, LegPhase, Observation, ObservedState, StateInner, SubflowState,
};

/// One phase barrier in the controller's plan — a named predicate over the
/// observed state that must hold (bounded by `recv_timeout`) before the call
/// proceeds. The name is a bounded label for the timeout `StepError`.
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
}

/// The controller: owns the shared observed state, the barrier plan, the settle
/// barrier, and each actor's teardown scope. Drives the phase barriers → the
/// `torn_down` barrier → the settle barrier to a verdict, with the actor
/// reactors running concurrently throughout.
struct CallController {
    obs: ObservedState,
    plan: Vec<BarrierPhase>,
    settle: SettleBarrier,
    recv_timeout: Duration,
    scopes: Vec<Arc<CallScope>>,
}

impl CallController {
    /// Drive the plan barriers, then wait for teardown, then run the settle
    /// barrier — the reactors stay alive throughout (this future runs in the
    /// same `select!` as [`drive_actors`], so a re-emitted request is consumed
    /// and acked DURING settle).
    async fn drive_to_verdict(&self) -> CallVerdict {
        for phase in &self.plan {
            let deadline = tokio::time::Instant::now() + self.recv_timeout;
            let held = await_pred(&self.obs, phase.name, |s| (phase.pred)(s), deadline).await;
            if let Err(e) = held {
                return CallVerdict::Failed(e);
            }
        }
        // Teardown: every leg terminated. Bounded separately (teardown can take
        // the whole flow after the last phase barrier).
        let deadline = tokio::time::Instant::now() + self.recv_timeout;
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

/// Run one declarative [`CallPlan`] to a verdict. Builds the shared observed
/// state, one teardown scope per actor, and the actor futures; races the
/// controller's verdict against the joined actors; then tears down every scope
/// (a no-op on a clean call, best-effort CANCEL/BYE on an aborted one).
pub async fn run_call(call: CallPlan, recv_timeout: Duration) -> CallVerdict {
    let obs = ObservedState::new();
    let ctx = Arc::new(CallCtx::new());

    let mut scopes = Vec::with_capacity(call.actors.len());
    let mut states = Vec::with_capacity(call.actors.len());
    for spec in call.actors {
        let scope = Arc::new(CallScope::new());
        states.push(ActorState::from_spec(
            spec,
            obs.clone(),
            scope.clone(),
            ctx.clone(),
            recv_timeout,
        ));
        scopes.push(scope);
    }

    let controller = CallController {
        obs: obs.clone(),
        plan: call.plan,
        settle: call.settle,
        recv_timeout,
        scopes: scopes.clone(),
    };

    let actors: FuturesUnordered<_> = states.into_iter().map(run_actor).collect();

    let verdict = tokio::select! {
        v = controller.drive_to_verdict() => v,
        e = drive_actors(actors) => CallVerdict::Failed(e),
    };

    // The loser future is dropped; teardown acts on whatever each scope last
    // registered (Terminated → no-op on the happy path).
    for scope in &controller.scopes {
        scope.teardown().await;
    }
    verdict
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
                        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob" }),
                        Goal::new(
                            Barrier::AllConfirmed(&["alice", "bob"]),
                            GoalStep::Bye,
                        ),
                    ],
                    invite_targets: vec![("bob", bob.clone())],
                    via: None,
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
                },
            ],
            plan: vec![phase("established", |s| {
                s.leg_at_least("alice", LegPhase::Confirmed)
                    && s.leg_at_least("bob", LegPhase::Confirmed)
            })],
            settle: SettleBarrier::default_ceiling(),
        };

        let verdict = run_call(call, Duration::from_secs(5)).await;
        assert!(verdict.is_ok(), "the toy call must settle OK, got {verdict:?}");

        h.finish().await;
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
                Observation::InDialogRequest { leg: "bob", call_id: "c1".to_string(), cseq: 2 },
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
                Observation::InDialogRequest { leg: "bob", call_id: "c1".to_string(), cseq: 3 },
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
}
