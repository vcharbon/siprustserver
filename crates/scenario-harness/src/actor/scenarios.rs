//! The actor-declared scenario bodies — the [`ActorScenario`] ports of the
//! linear [`crate::realcall::scenarios`] bodies (P1 ships the exemplar,
//! `Refer`; P3 collapses the rest).

use std::time::Duration;

use super::actor::{CtxFeed, Disposition, Feed, MediaState, SUBFLOW_REALIGN, SUBFLOW_REFER};
use super::goals::{Barrier, Goal, GoalStep};
use super::spec::{ActorCall, ActorScenario, Expect};
use super::state::{LegPhase, StateInner, SubflowState};
use super::{phase, ActorSpec, SettleBarrier};
use crate::realcall::{CallEnv, ScenarioId};
use crate::{StepError, ANSWER_SDP, OFFER_SDP};

/// REFER blind transfer, actor-declared — the port of
/// [`crate::realcall::scenarios::Refer`] and the redesign's exemplar: the
/// B2BUA's post-transfer media merge re-INVITEs charlie (the c-realign) and
/// alice (the a-realign) as logically parallel sub-flows, which the linear
/// body serialized (one stall froze the other two legs — endurance failure
/// `timeout/charlie@transferred`). Here each leg answers whatever arrives
/// whenever it arrives; the `merged` barrier is the CONJUNCTION of the two
/// realign sub-flows; and the settle barrier holds the verdict until every
/// in-dialog request (each REFER-progress NOTIFY, each realign ACK) is
/// actually acknowledged — a dropped datagram must be RECOVERED by
/// re-emission, never excused (endurance failure `rfc3261.cseqInDialogOrder`).
///
/// Downstream contract (`docs/todos/actor-harness-p1-contract-table.md` §5.3):
/// phases `referred` → `transferred` ONLY; checkpoints `time_to_200` /
/// `time_to_202` / `time_to_charlie_200`; NO `mark_ringing`; guard errors under
/// `who: "refer"` byte-for-byte.
pub struct Refer {
    /// The `X-Api-Call.refer_key` the SUT's REFER backend authorizes — per-run
    /// SUT auth data fed in at construction (the CLI's `--refer-key`), NOT
    /// topology (the transfer *target* resolves through the env's egress seam).
    pub refer_key: String,
}

impl Refer {
    pub fn new(refer_key: impl Into<String>) -> Self {
        Self { refer_key: refer_key.into() }
    }
}

/// The post-transfer media merge is complete: BOTH realign re-INVITEs (charlie
/// then alice, in the B2BUA's order — but observed here order-independently)
/// have been answered AND acknowledged.
fn merged(s: &StateInner) -> bool {
    let confirmed = |leg: &str| {
        s.leg(leg).subflow(SUBFLOW_REALIGN).is_some_and(|f| f >= SubflowState::Confirmed)
    };
    confirmed("alice") && confirmed("charlie")
}

impl ActorScenario for Refer {
    fn id(&self) -> ScenarioId {
        "refer"
    }

    fn build(&self, env: &CallEnv<'_>) -> Result<ActorCall, StepError> {
        // The linear body's guards, byte-for-byte (`refer.rs:39,80` — the `who`
        // is a bounded sample-key, see the contract table §8.2).
        if env.charlie.is_none() {
            return Err(StepError::UnexpectedKind {
                who: "refer".to_string(),
                detail: "REFER scenario bound without a charlie leg".to_string(),
            });
        }
        let refer_to = env.refer_to().ok_or_else(|| StepError::UnexpectedKind {
            who: "refer".to_string(),
            detail: "no charlie for Refer-To".to_string(),
        })?;
        let authorization = env.refer_authorization(&self.refer_key);

        let actors = vec![
            // Alice originates through the SUT, answers the a-realign
            // re-INVITE reactively (offer on her INVITE, answer SDP on every
            // realign 200 — RFC 3264 §5), and BYEs once the merge completes.
            ActorSpec {
                role: "alice",
                agent: env.alice.clone(),
                disposition: Disposition::Caller,
                media: MediaState::full(OFFER_SDP, ANSWER_SDP),
                goals: vec![
                    Goal::new(
                        Barrier::None,
                        GoalStep::Invite {
                            callee: "bob",
                            plan: Some(env.invite_plan(&["bob"])),
                        },
                    ),
                    Goal::new(Barrier::pred("merged", merged), GoalStep::Bye),
                ],
                invite_targets: vec![("bob", env.bob.clone())],
                via: None,
                feed: CtxFeed {
                    // `time_to_200` on alice's answer; NO phase (the linear
                    // refer never stamps `connected`) and NO ringing gate.
                    on_answer_rx: Feed::new(Some("time_to_200"), None),
                    ..CtxFeed::default()
                },
            },
            // Bob rings then answers, then — established + a realistic talk
            // dwell — REFERs the call to charlie. His REFER-progress NOTIFYs
            // are answered reactively and gap-checked by the ledger.
            ActorSpec {
                role: "bob",
                agent: env.bob.clone(),
                disposition: Disposition::RingThenAnswer { ring: env.ring_delay },
                media: MediaState::answer(ANSWER_SDP),
                goals: vec![Goal::new(
                    Barrier::AllConfirmed(&["alice", "bob"]),
                    GoalStep::Refer { refer_to, authorization },
                )
                .after(env.reinvite_gap)],
                invite_targets: vec![],
                via: None,
                feed: CtxFeed {
                    on_refer_accepted: Feed::new(Some("time_to_202"), Some("referred")),
                    ..CtxFeed::default()
                },
            },
            // Charlie answers the transfer INVITE (180 then an immediate 200,
            // the linear shape) and the c-realign re-INVITE reactively.
            ActorSpec {
                role: "charlie",
                agent: env.callee_agent("charlie").clone(),
                disposition: Disposition::RingThenAnswer { ring: Duration::ZERO },
                media: MediaState::answer(ANSWER_SDP),
                goals: vec![],
                invite_targets: vec![],
                via: None,
                feed: CtxFeed {
                    on_answer_sent: Feed::new(Some("time_to_charlie_200"), Some("transferred")),
                    ..CtxFeed::default()
                },
            },
        ];

        let plan = vec![
            phase("established", |s| {
                s.leg_at_least("alice", LegPhase::Confirmed)
                    && s.leg_at_least("bob", LegPhase::Confirmed)
            }),
            phase("referred", |s| {
                s.leg("bob")
                    .subflow(SUBFLOW_REFER)
                    .is_some_and(|f| f >= SubflowState::Answered)
            }),
            phase("transferred", |s| s.leg_at_least("charlie", LegPhase::Confirmed)),
            phase("merged", merged),
        ];

        Ok(ActorCall {
            actors,
            plan,
            settle: SettleBarrier::default_ceiling(),
            expect: Expect::HappyBye,
        })
    }
}
