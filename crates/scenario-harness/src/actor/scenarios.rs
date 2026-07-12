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

/// Guard [`StepError`] under a fixed synthetic `who` (a bounded sample-key) —
/// the actor twin of the linear bodies' `who`-tagged build guards. `detail` is
/// free-form (never keyed).
fn guard(who: &'static str, detail: &'static str) -> StepError {
    StepError::UnexpectedKind { who: who.to_string(), detail: detail.to_string() }
}

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

/// A blind transfer whose target DECLINES (`603`), actor-declared — the port of
/// [`crate::realcall::scenarios::ReferCharlieReject`]. A↔B establish, bob REFERs
/// to charlie, charlie 603-declines the transfer INVITE; the transfer fails and
/// A↔B stays up. The linear body returns its NOK terminal and leaves the scope
/// Confirmed so the *driver's* teardown BYEs A↔B; the actor runner OWNS teardown
/// (its own per-actor scopes), so here **alice BYEs A↔B herself** once the
/// decline is observed — the call reaches a clean torn-down + settled
/// [`CallVerdict::Ok`], which [`Expect::TransferDeclined`] then maps onto the
/// EXACT contract Err (`UnexpectedKind { who: "refer_charlie_reject", detail:
/// "transfer declined by charlie (603)" }`). Same net SUT effect (A↔B BYE'd,
/// fully reaped), same downstream `Result`.
///
/// Downstream contract (`docs/todos/actor-harness-p1-contract-table.md` §5.10):
/// NO phases, NO anchors, checkpoint `time_to_200` only, NO `mark_ringing`;
/// guard errors + the terminal all under `who: "refer_charlie_reject"`.
pub struct ReferCharlieReject {
    /// The `X-Api-Call.refer_key` the SUT's REFER backend authorizes.
    pub refer_key: String,
}

impl ReferCharlieReject {
    pub fn new(refer_key: impl Into<String>) -> Self {
        Self { refer_key: refer_key.into() }
    }
}

/// The transfer was declined: charlie's leg reached a terminal state (its
/// `603`), so no successful transfer INVITE was confirmed.
fn declined(s: &StateInner) -> bool {
    s.leg_at_least("charlie", LegPhase::Terminated)
}

impl ActorScenario for ReferCharlieReject {
    fn id(&self) -> ScenarioId {
        "refer_charlie_reject"
    }

    fn build(&self, env: &CallEnv<'_>) -> Result<ActorCall, StepError> {
        // The linear body's guards, byte-for-byte (`failures.rs:133,154` — the
        // `who` is the fixed sample-key, contract table §5.10 / §8.1).
        if env.charlie.is_none() {
            return Err(guard("refer_charlie_reject", "bound without a charlie leg"));
        }
        let refer_to = env
            .refer_to()
            .ok_or_else(|| guard("refer_charlie_reject", "no charlie for Refer-To"))?;
        let authorization = env.refer_authorization(&self.refer_key);

        let actors = vec![
            // Alice originates through the SUT and — once the decline is observed
            // — BYEs A↔B (the actor owns teardown; see the type doc). She answers
            // any post-decline realign reactively.
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
                    Goal::new(Barrier::pred("declined", declined), GoalStep::Bye),
                ],
                invite_targets: vec![("bob", env.bob.clone())],
                via: None,
                feed: CtxFeed {
                    // `time_to_200` on alice's answer; NO phase, NO ringing gate
                    // (contract table §5.10).
                    on_answer_rx: Feed::new(Some("time_to_200"), None),
                    ..CtxFeed::default()
                },
            },
            // Bob rings then answers, then REFERs the call to charlie; his
            // REFER-progress NOTIFYs (incl. the failure sipfrag) are answered
            // reactively and gap-checked by the ledger.
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
                feed: CtxFeed::default(),
            },
            // Charlie DECLINES the transfer INVITE with 603 (the contract's
            // TransferDeclined outcome).
            ActorSpec {
                role: "charlie",
                agent: env.callee_agent("charlie").clone(),
                disposition: Disposition::Reject(603),
                media: MediaState::answer(ANSWER_SDP),
                goals: vec![],
                invite_targets: vec![],
                via: None,
                feed: CtxFeed::default(),
            },
        ];

        // Internal controller gating only (these do NOT stamp `ctx.phase` — that
        // is the reactor's CtxFeed job, and this body declares none, so the
        // contract's "no phases" holds): established → declined → torn_down.
        let plan = vec![
            phase("established", |s| {
                s.leg_at_least("alice", LegPhase::Confirmed)
                    && s.leg_at_least("bob", LegPhase::Confirmed)
            }),
            phase("declined", declined),
        ];

        Ok(ActorCall {
            actors,
            plan,
            settle: SettleBarrier::default_ceiling(),
            expect: Expect::TransferDeclined,
        })
    }
}
