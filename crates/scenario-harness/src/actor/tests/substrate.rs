use std::time::Duration;

use crate::actor::*;
use crate::{Harness, ANSWER_SDP, OFFER_SDP};

/// The substrate exit gate: two actors (alice caller + bob `RingThenAnswer`) reach
/// `torn_down` and settle OK under a paused clock — the reactor answers, the
/// goal cursor originates + hangs up, the controller drives the barriers, and
/// the RFC hard gate at `finish()` confirms the wire stayed compliant.
#[tokio::test(start_paused = true)]
async fn two_actor_toy_call_reaches_torn_down() {
    let h = Harness::new("actor-toy-call").describe(
        "Substrate proof: alice originates, bob rings-then-answers, the \
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
            
                cseq: None,
                delayed: None,
                claim: None,
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
            
                cseq: None,
                delayed: None,
                claim: None,
            },
        ],
        plan: vec![phase("established", |s| {
            s.leg_at_least("alice", LegPhase::Confirmed)
                && s.leg_at_least("bob", LegPhase::Confirmed)
        })],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
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
            
                cseq: None,
                delayed: None,
                claim: None,
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
            
                cseq: None,
                delayed: None,
                claim: None,
            },
        ],
        plan: vec![phase("established", |s| {
            s.leg_at_least("alice", LegPhase::Confirmed)
                && s.leg_at_least("bob", LegPhase::Confirmed)
        })],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };

    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the crossing-BYE call must settle OK, got {verdict:?}");

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
