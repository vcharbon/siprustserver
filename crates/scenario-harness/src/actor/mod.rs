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
    run_actor, ActorSpec, ActorState, CtxFeed, Disposition, Feed, MediaState, SUBFLOW_REALIGN,
    SUBFLOW_RENEG, SUBFLOW_REFER,
};
pub use goals::{Barrier, Goal, GoalCursor, GoalStep};
pub use ledger::{ObligationKey, ObligationKind, ObligationLedger};
pub use settle::{SettleBarrier, SettleVerdict, T1};
pub use spec::{into_result, run_actor_scenario, ActorCall, ActorScenario, Expect, STEP_TIMEOUT};
pub use state::{
    await_pred, LegObservation, LegPhase, Observation, ObservedState, StateInner, SubflowState,
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
    for spec in call.actors {
        let scope = Arc::new(CallScope::new());
        states.push(ActorState::from_spec(
            spec,
            obs.clone(),
            scope.clone(),
            ctx,
            step_timeout,
            challenge_responder.clone(),
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
        };

        let verdict = run_call(call, Duration::from_secs(5)).await;
        assert!(verdict.is_ok(), "the crossing-BYE call must settle OK, got {verdict:?}");

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
}
