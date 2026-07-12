//! The actor-declared scenario bodies — the [`ActorScenario`] ports of the
//! linear [`crate::realcall::scenarios`] bodies (P1 ships the exemplar,
//! `Refer`; P3 collapses the rest).

use std::time::Duration;

use super::actor::{
    CtxFeed, Disposition, Feed, MediaState, SUBFLOW_REALIGN, SUBFLOW_RENEG, SUBFLOW_REFER,
};
use super::goals::{Barrier, Goal, GoalStep};
use super::spec::{ActorCall, ActorScenario, Expect};
use super::state::{LegPhase, StateInner, SubflowState};
use super::{phase, ActorSpec, SettleBarrier};
use crate::realcall::{CallEnv, ScenarioId};
use crate::{StepError, ANSWER_SDP, OFFER_SDP};

/// The caller's in-dialog renegotiation (a re-INVITE's answered-and-ACKed 2xx,
/// or an UPDATE's 200) is complete — the `reinvite` / `prack_update` teardown
/// barrier, so the BYE never races the renegotiation.
fn reneg_done(s: &StateInner) -> bool {
    s.leg("alice").subflow(SUBFLOW_RENEG).is_some_and(|f| f >= SubflowState::Confirmed)
}

/// Both legs established (confirmed) — the shared `established` controller gate.
fn established(s: &StateInner) -> bool {
    s.leg_at_least("alice", LegPhase::Confirmed) && s.leg_at_least("bob", LegPhase::Confirmed)
}

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

/// Rerouting + a RELIABLE provisional on the winning leg, actor-declared — the
/// port of [`crate::realcall::scenarios::ReroutingPrack`] (the LOAD body of the
/// dual-body `rerouting_prack` shape). Alice INVITEs with a `[bob, bob2]`
/// candidate list; bob `486`s, the SUT fails over to bob2, which answers
/// RELIABLY (RFC 3262: `183`/PRACK/`200`/ACK) on the winning leg. Each endpoint
/// reacts independently: bob rejects, bob2 runs the reliable-answer state
/// machine ([`Disposition::ReliableAnswer`]), alice PRACKs the reliable 183 in
/// her reactor and BYEs once established.
///
/// Downstream contract (`docs/todos/actor-harness-p1-contract-table.md` §5.7):
/// phases `rerouted` → `pracked` → `connected` → `bye_200`; checkpoints
/// `time_to_prack_200` / `time_to_200` / `time_to_bye_200`; `mark_ringing(true)`
/// on bob2's reliable 183; anchors `PRACK_ANCHORS` (with `initialInvite` on BOTH
/// bob's rejected and bob2's winning INVITE).
pub struct ReroutingPrack;

impl ActorScenario for ReroutingPrack {
    fn id(&self) -> ScenarioId {
        "rerouting_prack"
    }

    fn build(&self, env: &CallEnv<'_>) -> Result<ActorCall, StepError> {
        // The linear body's guard, byte-for-byte (`rerouting_prack.rs:44` — the
        // `who` is the fixed sample-key, contract table §5.7 / §8.3).
        let bob2 = env.bob2.ok_or_else(|| guard("rerouting_prack", "bound without a bob2 leg"))?;

        let actors = vec![
            // Alice INVITEs through the SUT advertising 100rel over the [bob,
            // bob2] candidate list, PRACKs the winning leg's reliable 183 in her
            // reactor, then BYEs the (rerouted) winning leg.
            ActorSpec {
                role: "alice",
                agent: env.alice.clone(),
                disposition: Disposition::Caller,
                media: MediaState::offer(OFFER_SDP),
                goals: vec![
                    Goal::new(
                        Barrier::None,
                        GoalStep::Invite {
                            callee: "bob",
                            // The candidate list realizes as the SUT's failover
                            // plan ([bob, bob2] → the X-Api-Call routes walked on
                            // bob's rejection); alice adds `Supported: 100rel`.
                            plan: Some(env.invite_plan(&["bob", "bob2"]).with_supported_100rel()),
                        },
                    ),
                    Goal::new(Barrier::AllConfirmed(&["alice", "bob2"]), GoalStep::Bye)
                        .after(env.talk_time),
                ],
                invite_targets: vec![("bob", env.bob.clone())],
                via: None,
                feed: CtxFeed {
                    // bob2's reliable 183 is guaranteed-delivery, so it counts
                    // toward the cross-call 18x gate (contract table §5.7).
                    ringing_gate: true,
                    on_answer_rx: Feed::new(Some("time_to_200"), None),
                    on_prack_ok: Feed::new(Some("time_to_prack_200"), Some("pracked")),
                    on_bye_ok: Feed::new(Some("time_to_bye_200"), Some("bye_200")),
                    ..CtxFeed::default()
                },
            },
            // The primary callee REJECTS its b-leg (486), triggering the SUT's
            // failover to bob2. Its reject-ACK is absorbed without confirming.
            ActorSpec {
                role: "bob",
                agent: env.bob.clone(),
                disposition: Disposition::Reject(486),
                media: MediaState::none(),
                goals: vec![],
                invite_targets: vec![],
                via: None,
                feed: CtxFeed::default(),
            },
            // The rerouted winning leg answers RELIABLY (183/PRACK/200/ACK).
            ActorSpec {
                role: "bob2",
                agent: bob2.clone(),
                disposition: Disposition::ReliableAnswer,
                media: MediaState::answer(ANSWER_SDP),
                goals: vec![],
                invite_targets: vec![],
                via: None,
                feed: CtxFeed {
                    // `rerouted` on receiving the winning INVITE; `connected` on
                    // its ACK.
                    on_invite_rx: Feed::new(None, Some("rerouted")),
                    on_ack_rx: Feed::new(None, Some("connected")),
                    ..CtxFeed::default()
                },
            },
        ];

        // Internal controller gating only (no `ctx.phase` stamps here — those
        // ride the reactor feeds above): established (winning leg confirmed).
        let plan = vec![phase("established", |s| {
            s.leg_at_least("alice", LegPhase::Confirmed)
                && s.leg_at_least("bob2", LegPhase::Confirmed)
        })];

        Ok(ActorCall {
            actors,
            plan,
            settle: SettleBarrier::default_ceiling(),
            expect: Expect::HappyBye,
        })
    }
}

/// RFC 3262 reliable-provisional establishment followed by an RFC 3311 in-dialog
/// UPDATE renegotiation, actor-declared — the port of
/// [`crate::realcall::scenarios::PrackUpdate`]. Bob answers RELIABLY
/// ([`Disposition::ReliableAnswer`]: `183`/PRACK/`200`/ACK), alice PRACKs the
/// reliable 183 in her reactor, and — once established — sends an in-dialog
/// UPDATE (offer) whose 200 completes the exchange (no ACK), then BYEs. The
/// reactor reuses the 100rel machinery the rerouting-prack port built.
///
/// Downstream contract (`docs/todos/actor-harness-p1-contract-table.md` §5.6):
/// phases `pracked` → `connected` → `updated` → `bye_200`; checkpoints
/// `time_to_prack_200` / `time_to_200` / `time_to_update_200` / `time_to_bye_200`;
/// `mark_ringing(true)` on the reliable 183; anchors `PRACK_ANCHORS` (UPDATE NOT
/// anchored).
pub struct PrackUpdate;

impl ActorScenario for PrackUpdate {
    fn id(&self) -> ScenarioId {
        "prack_update"
    }

    fn build(&self, env: &CallEnv<'_>) -> Result<ActorCall, StepError> {
        let actors = vec![
            // Alice INVITEs advertising 100rel, PRACKs bob's reliable 183 in her
            // reactor, then UPDATEs the confirmed dialog and BYEs.
            ActorSpec {
                role: "alice",
                agent: env.alice.clone(),
                disposition: Disposition::Caller,
                media: MediaState::offer(OFFER_SDP),
                goals: vec![
                    Goal::new(
                        Barrier::None,
                        GoalStep::Invite {
                            callee: "bob",
                            plan: Some(env.invite_plan(&["bob"]).with_supported_100rel()),
                        },
                    ),
                    Goal::new(Barrier::pred("established", established), GoalStep::Update)
                        .after(env.reinvite_gap),
                    Goal::new(Barrier::pred("updated", reneg_done), GoalStep::Bye)
                        .after(env.reinvite_gap),
                ],
                invite_targets: vec![("bob", env.bob.clone())],
                via: None,
                feed: CtxFeed {
                    // bob's reliable 183 is guaranteed-delivery, so it counts
                    // toward the cross-call 18x gate (contract table §5.6).
                    ringing_gate: true,
                    on_answer_rx: Feed::new(Some("time_to_200"), None),
                    on_prack_ok: Feed::new(Some("time_to_prack_200"), Some("pracked")),
                    on_update_ok: Feed::new(Some("time_to_update_200"), Some("updated")),
                    on_bye_ok: Feed::new(Some("time_to_bye_200"), Some("bye_200")),
                    ..CtxFeed::default()
                },
            },
            // Bob answers RELIABLY (183/PRACK/200/ACK) and reacts to the UPDATE
            // (200 + SDP) reactively; his ACK receipt stamps `connected`.
            ActorSpec {
                role: "bob",
                agent: env.bob.clone(),
                disposition: Disposition::ReliableAnswer,
                media: MediaState::answer(ANSWER_SDP),
                goals: vec![],
                invite_targets: vec![],
                via: None,
                feed: CtxFeed {
                    on_ack_rx: Feed::new(None, Some("connected")),
                    ..CtxFeed::default()
                },
            },
        ];

        // Internal controller gating only (no `ctx.phase` stamps here — those
        // ride the reactor feeds above): established → updated.
        let plan = vec![phase("established", established), phase("updated", reneg_done)];

        Ok(ActorCall {
            actors,
            plan,
            settle: SettleBarrier::default_ceiling(),
            expect: Expect::HappyBye,
        })
    }
}

/// Establish then a delayed-offer in-dialog re-INVITE renegotiation then BYE,
/// actor-declared — the port of [`crate::realcall::scenarios::Reinvite`]. Bob
/// rings-then-answers; once established alice sends a bodyless (delayed-offer)
/// re-INVITE, whose 2xx she ACKs WITH the answer SDP (RFC 3264 §4) in her
/// reactor, then BYEs. Emergency variant `reinvite_em` reuses this body (the
/// marker rides the INVITE plan).
///
/// Downstream contract (`docs/todos/actor-harness-p1-contract-table.md` §5.2):
/// phases `connected` → `reinvited` → `bye_200`; checkpoints `time_to_200` /
/// `time_to_reinvite_200` / `time_to_bye_200`; `mark_ringing` on the 180;
/// anchors `LOAD_REINVITE_ANCHORS` (adds `reInvite`←bob rx).
pub struct Reinvite;

impl ActorScenario for Reinvite {
    fn id(&self) -> ScenarioId {
        "reinvite"
    }

    fn build(&self, env: &CallEnv<'_>) -> Result<ActorCall, StepError> {
        let actors = vec![
            // Alice originates, ACKs the delayed-offer re-INVITE's 2xx with her
            // answer SDP reactively, and BYEs once the renegotiation completes.
            ActorSpec {
                role: "alice",
                agent: env.alice.clone(),
                disposition: Disposition::Caller,
                media: MediaState::full(OFFER_SDP, ANSWER_SDP),
                goals: vec![
                    Goal::new(
                        Barrier::None,
                        GoalStep::Invite { callee: "bob", plan: Some(env.invite_plan(&["bob"])) },
                    ),
                    Goal::new(Barrier::pred("established", established), GoalStep::Reinvite)
                        .after(env.reinvite_gap),
                    Goal::new(Barrier::pred("reinvited", reneg_done), GoalStep::Bye)
                        .after(env.reinvite_gap),
                ],
                invite_targets: vec![("bob", env.bob.clone())],
                via: None,
                feed: CtxFeed {
                    ringing_gate: true,
                    on_answer_rx: Feed::new(Some("time_to_200"), None),
                    on_reinvite_ok: Feed::new(Some("time_to_reinvite_200"), Some("reinvited")),
                    on_bye_ok: Feed::new(Some("time_to_bye_200"), Some("bye_200")),
                    ..CtxFeed::default()
                },
            },
            // Bob rings then answers, then answers alice's re-INVITE (200 + SDP)
            // reactively; his ACK receipt stamps `connected`, and receiving the
            // re-INVITE stamps the `reInvite` anchor.
            ActorSpec {
                role: "bob",
                agent: env.bob.clone(),
                disposition: Disposition::RingThenAnswer { ring: env.ring_delay },
                media: MediaState::answer(ANSWER_SDP),
                goals: vec![],
                invite_targets: vec![],
                via: None,
                feed: CtxFeed {
                    on_ack_rx: Feed::new(None, Some("connected")),
                    ..CtxFeed::default()
                },
            },
        ];

        let plan = vec![phase("established", established), phase("reinvited", reneg_done)];

        Ok(ActorCall {
            actors,
            plan,
            settle: SettleBarrier::default_ceiling(),
            expect: Expect::HappyBye,
        })
    }
}
