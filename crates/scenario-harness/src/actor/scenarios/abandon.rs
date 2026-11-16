//! The abandoned call: the caller CANCELs once ringing.

use crate::actor::ending::{Branch, Expect};
use crate::actor::endpoint::{CtxFeed, Disposition, Feed, MediaState};
use crate::actor::goals::{Barrier, Goal, GoalStep};
use crate::actor::spec::{ActorCall, ActorScenario};
use crate::actor::state::{LegPhase, StateInner};
use crate::actor::{phase, ActorSpec, SettleBarrier};
use crate::realcall::{CallEnv, ScenarioId};
use crate::{StepError, ANSWER_SDP, OFFER_SDP};

/// Alice received the 180 (the abandon body's CANCEL trigger): the callee
/// holds the INVITE, so her CANCEL reaches a ringing leg (RFC 3261 §9.1). A
/// provisional of the call the loss model dropped reads as her ringing once
/// she heard any provisional, as §9.1 asks before a CANCEL.
fn alice_ringing(s: &StateInner) -> bool {
    s.rang_or_ring_lost("alice") && s.leg("alice").heard_provisional()
}

/// The caller ABANDONS after ringing (CANCEL), actor-declared. Alice INVITEs,
/// sees the 180, then CANCELs the still-pending INVITE (RFC 3261 §9.1); the SUT
/// relays the CANCEL to bob, who 200s it and 487s his held INVITE, and both legs
/// reap. The torn-down, settled call succeeds under [`Branch::ABANDONED`]. When
/// bob's 200 crosses the CANCEL, alice ACKs it and BYEs, and the call succeeds
/// under [`Branch::ANSWERED_DESPITE_CANCEL`]; when it reaches her before her
/// CANCEL leaves, under [`Branch::ANSWERED_BEFORE_CANCEL`].
///
/// Downstream contract (`docs/todos/actor-harness-p1-contract-table.md` §5.9):
/// NO phases, NO anchors, checkpoint `time_to_180` only, NO `mark_ringing`.
pub struct AbandonRinging;

/// The abandon's accepted endings: the CANCEL won (487), the 200 crossed it,
/// or the 200 reached alice before her CANCEL left.
const ABANDON_ENDINGS: &[Branch] =
    &[Branch::ABANDONED, Branch::ANSWERED_DESPITE_CANCEL, Branch::ANSWERED_BEFORE_CANCEL];

/// Alice's call confirmed (a 2xx answered her INVITE).
fn alice_confirmed(s: &StateInner) -> bool {
    s.leg_at_least("alice", LegPhase::Confirmed)
}

impl ActorScenario for AbandonRinging {
    fn id(&self) -> ScenarioId {
        "abandon_ringing"
    }

    fn build(&self, env: &CallEnv<'_>) -> Result<ActorCall, StepError> {
        let actors = vec![
            // Alice originates, then CANCELs once she has seen the 180 (her leg
            // Early). She keeps her pending INVITE so its 487 still routes to it.
            ActorSpec {
                role: "alice",
                agent: env.alice.clone(),
                disposition: Disposition::Caller,
                media: MediaState::offer(OFFER_SDP),
                goals: vec![
                    Goal::new(
                        Barrier::None,
                        GoalStep::Invite { callee: "bob", plan: Some(env.invite_plan(&["bob"])) },
                    ),
                    Goal::new(
                        Barrier::pred("ringing", alice_ringing),
                        GoalStep::Cancel { stated: Vec::new() },
                    )
                    .unless(Barrier::pred("answered", alice_confirmed)),
                    // A 200 that crossed the CANCEL confirmed the call: hang
                    // up (§15). After a 487 the accepted final drops this goal.
                    Goal::new(Barrier::pred("answered", alice_confirmed), GoalStep::ByeIfConfirmed),
                ],
                invite_targets: vec![("bob", env.bob.clone())],
                via: None,
                feed: CtxFeed {
                    // `time_to_180` on the first provisional; NO phase, NO ringing
                    // gate (contract table §5.9).
                    on_provisional: Feed::new(Some("time_to_180"), None),
                    ..CtxFeed::default()
                },

                cseq: None,
                delayed: vec![],
                claim: None,
            },
            // Bob rings (180) then would answer — but the CANCEL arrives first, so
            // his CANCEL reactor 200s the CANCEL + 487s the held INVITE and reaps.
            ActorSpec {
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
            },
        ];

        // Internal controller gating only: alice rang (Early) → both torn down.
        let plan = vec![phase("ringing", alice_ringing)];

        Ok(ActorCall {
            actors,
            plan,
            settle: SettleBarrier::default_ceiling(),
            expect: Expect::of(ABANDON_ENDINGS),

            waivers: Vec::new(),
            automatics: Default::default(),
            ceiling: None,
            delta_policy: None,
            reception_observer: None,
        })
    }
}
