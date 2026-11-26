//! The actor-declared scenario bodies — the load/functional call flows
//! expressed as per-endpoint reactive actors ([`ActorScenario`]). The only
//! executor: each drives one full call through the SUT, and the load driver +
//! the in-process functional leak gate both run them.

use std::time::Duration;

use super::ending::{Branch, Ending, Expect};
use super::endpoint::{
    CtxFeed, Disposition, Feed, MediaState, SUBFLOW_REALIGN, SUBFLOW_REFER, SUBFLOW_RENEG,
};
use super::goals::{Barrier, Goal, GoalStep};
use super::spec::{ActorCall, ActorScenario};
use super::state::{LegPhase, StateInner, SubflowState};
use super::{phase, ActorSpec, SettleBarrier};
use crate::realcall::{CallEnv, ScenarioId};
use crate::{StepError, ANSWER_SDP, OFFER_SDP};

mod keepalive;

pub use keepalive::{LongCall, OptionsHold};

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

/// Alice rang, or a lost provisional of the call stands for it once she heard
/// one: the abandon body's CANCEL trigger (RFC 3261 §9.1).
fn alice_ringing(s: &StateInner) -> bool {
    s.rang_or_ring_lost("alice") && s.leg("alice").heard_provisional()
}

/// Guard [`StepError`] under a fixed synthetic `who` (a bounded sample-key) —
/// the actor twin of the linear bodies' `who`-tagged build guards. `detail` is
/// free-form (never keyed).
fn guard(who: &'static str, detail: &'static str) -> StepError {
    StepError::UnexpectedKind { who: who.to_string(), detail: detail.to_string() }
}

/// REFER blind transfer, actor-declared. The B2BUA's post-transfer media merge re-INVITEs
/// charlie (the c-realign) and alice (the a-realign) as parallel sub-flows: each leg answers
/// whatever arrives whenever it arrives, the `merged` barrier is the conjunction of both
/// realigns, and the settle barrier holds the verdict until every in-dialog request (each
/// REFER-progress NOTIFY, each realign ACK) is acknowledged, a dropped datagram recovered by
/// re-emission, never excused.
///
/// Downstream contract: phases `referred` → `transferred` ONLY; checkpoints
/// `time_to_200` / `time_to_202` / `time_to_charlie_200`; NO `mark_ringing`;
/// guard errors under `who: "refer"`, a fixed string that keys a bounded sample.
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
            // re-INVITE reactively (her own session on every realign 200 —
            // RFC 3264 §5, §8), and BYEs once the merge completes.
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

                cseq: None,
                delayed: vec![],
                answers: Default::default(),
                claim: None,
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

                cseq: None,
                delayed: vec![],
                answers: Default::default(),
                claim: None,
            },
            // Charlie answers the transfer INVITE (180 then an immediate 200,
            // the linear shape) and the c-realign re-INVITE reactively.
            ActorSpec {
                role: "charlie",
                agent: env.callee_agent("charlie").clone().transfer_target(),
                disposition: Disposition::RingThenAnswer { ring: Duration::ZERO },
                media: MediaState::answer(ANSWER_SDP),
                goals: vec![],
                invite_targets: vec![],
                via: None,
                feed: CtxFeed {
                    on_answer_sent: Feed::new(Some("time_to_charlie_200"), Some("transferred")),
                    ..CtxFeed::default()
                },

                cseq: None,
                delayed: vec![],
                answers: Default::default(),
                claim: None,
            },
        ];

        let plan = vec![
            phase("established", |s| {
                s.leg_at_least("alice", LegPhase::Confirmed)
                    && s.leg_at_least("bob", LegPhase::Confirmed)
            }),
            phase("referred", |s| {
                s.leg("bob").subflow(SUBFLOW_REFER).is_some_and(|f| f >= SubflowState::Answered)
            }),
            phase("transferred", |s| s.leg_at_least("charlie", LegPhase::Confirmed)),
            phase("merged", merged),
        ];

        Ok(ActorCall {
            actors,
            plan,
            settle: SettleBarrier::default_ceiling(),
            expect: Expect::answered(),

            waivers: Vec::new(),
            automatics: Default::default(),
            ceiling: None,
            delta_policy: None,
            reception_observer: None,
        })
    }
}

/// A blind transfer whose target declines (`603`), actor-declared. Alice and bob establish,
/// bob REFERs to charlie, charlie declines the transfer INVITE and the call stays up; the actor
/// runner owns teardown, so alice BYEs once the decline is observed, and the call succeeds
/// under its `transfer_declined` branch.
///
/// Downstream contract: NO phases, NO anchors, checkpoint `time_to_200` only,
/// NO `mark_ringing`; guard errors under `who: "refer_charlie_reject"`, a fixed
/// string that keys a bounded sample.
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

/// The declined transfer's accepted ending: the transfer target refused, and
/// the original call tore down.
const TRANSFER_DECLINED: Branch = Branch::new("transfer_declined", Ending::Observed(declined));

impl ActorScenario for ReferCharlieReject {
    fn id(&self) -> ScenarioId {
        "refer_charlie_reject"
    }

    fn build(&self, env: &CallEnv<'_>) -> Result<ActorCall, StepError> {
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
                media: MediaState::offer(OFFER_SDP),
                goals: vec![
                    Goal::new(
                        Barrier::None,
                        GoalStep::Invite { callee: "bob", plan: Some(env.invite_plan(&["bob"])) },
                    ),
                    Goal::new(Barrier::pred("declined", declined), GoalStep::Bye),
                ],
                invite_targets: vec![("bob", env.bob.clone())],
                via: None,
                feed: CtxFeed {
                    on_answer_rx: Feed::new(Some("time_to_200"), None),
                    ..CtxFeed::default()
                },

                cseq: None,
                delayed: vec![],
                answers: Default::default(),
                claim: None,
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

                cseq: None,
                delayed: vec![],
                answers: Default::default(),
                claim: None,
            },
            // Charlie DECLINES the transfer INVITE with 603 (the contract's
            // TransferDeclined outcome).
            ActorSpec {
                role: "charlie",
                agent: env.callee_agent("charlie").clone().transfer_target(),
                disposition: Disposition::Reject(603),
                media: MediaState::answer(ANSWER_SDP),
                goals: vec![],
                invite_targets: vec![],
                via: None,
                feed: CtxFeed::default(),

                cseq: None,
                delayed: vec![],
                answers: Default::default(),
                claim: None,
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
            expect: Expect::one(TRANSFER_DECLINED),

            waivers: Vec::new(),
            automatics: Default::default(),
            ceiling: None,
            delta_policy: None,
            reception_observer: None,
        })
    }
}

/// Rerouting and a reliable provisional on the winning leg, actor-declared (the load body of
/// the anchored `rerouting_prack` shape). Alice INVITEs with a `[bob, bob2]` candidate list;
/// bob answers `486`, the SUT fails over to bob2, which answers reliably (RFC 3262: 183, PRACK,
/// 200, ACK; [`Disposition::ReliableAnswer`]). Alice PRACKs the 183 in her reactor and BYEs once
/// established.
///
/// Downstream contract: phases `rerouted` → `pracked` → `connected` → `bye_200`; checkpoints
/// `time_to_prack_200` / `time_to_200` / `time_to_bye_200`; `mark_ringing(true)`
/// on bob2's reliable 183, guaranteed-delivery, so it counts toward the cross-call
/// 18x gate; anchors `PRACK_ANCHORS` (with `initialInvite` on BOTH bob's rejected
/// and bob2's winning INVITE); guard errors under `who: "rerouting_prack"`, a
/// fixed string that keys a bounded sample.
pub struct ReroutingPrack;

impl ActorScenario for ReroutingPrack {
    fn id(&self) -> ScenarioId {
        "rerouting_prack"
    }

    fn build(&self, env: &CallEnv<'_>) -> Result<ActorCall, StepError> {
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
                    ringing_gate: true,
                    on_answer_rx: Feed::new(Some("time_to_200"), None),
                    on_prack_ok: Feed::new(Some("time_to_prack_200"), Some("pracked")),
                    on_bye_ok: Feed::new(Some("time_to_bye_200"), Some("bye_200")),
                    ..CtxFeed::default()
                },

                cseq: None,
                delayed: vec![],
                answers: Default::default(),
                claim: None,
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

                cseq: None,
                delayed: vec![],
                answers: Default::default(),
                claim: None,
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

                cseq: None,
                delayed: vec![],
                answers: Default::default(),
                claim: None,
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
            expect: Expect::answered(),

            waivers: Vec::new(),
            automatics: Default::default(),
            ceiling: None,
            delta_policy: None,
            reception_observer: None,
        })
    }
}

/// RFC 3262 reliable-provisional establishment, then an RFC 3311 in-dialog UPDATE,
/// actor-declared. Bob answers reliably ([`Disposition::ReliableAnswer`]: 183, PRACK, 200,
/// ACK), alice PRACKs the 183 in her reactor and, once established, sends an UPDATE whose 200
/// completes the exchange, then BYEs.
///
/// Downstream contract: phases `pracked` → `connected` → `updated` → `bye_200`; checkpoints
/// `time_to_prack_200` / `time_to_200` / `time_to_update_200` / `time_to_bye_200`;
/// `mark_ringing(true)` on the reliable 183, guaranteed-delivery, so it counts
/// toward the cross-call 18x gate; anchors `PRACK_ANCHORS` (UPDATE NOT anchored).
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
                    ringing_gate: true,
                    on_answer_rx: Feed::new(Some("time_to_200"), None),
                    on_prack_ok: Feed::new(Some("time_to_prack_200"), Some("pracked")),
                    on_update_ok: Feed::new(Some("time_to_update_200"), Some("updated")),
                    on_bye_ok: Feed::new(Some("time_to_bye_200"), Some("bye_200")),
                    ..CtxFeed::default()
                },

                cseq: None,
                delayed: vec![],
                answers: Default::default(),
                claim: None,
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

                cseq: None,
                delayed: vec![],
                answers: Default::default(),
                claim: None,
            },
        ];

        // Internal controller gating only (no `ctx.phase` stamps here — those
        // ride the reactor feeds above): established → updated.
        let plan = vec![phase("established", established), phase("updated", reneg_done)];

        Ok(ActorCall {
            actors,
            plan,
            settle: SettleBarrier::default_ceiling(),
            expect: Expect::answered(),

            waivers: Vec::new(),
            automatics: Default::default(),
            ceiling: None,
            delta_policy: None,
            reception_observer: None,
        })
    }
}

/// Establish, a delayed-offer in-dialog re-INVITE, then BYE, actor-declared. Bob rings then
/// answers; alice sends a bodyless re-INVITE and ACKs its 2xx with the answer SDP (RFC 3264
/// §4), then BYEs. The emergency variant `reinvite_em` reuses this body (the marker rides the
/// INVITE plan).
///
/// Downstream contract: phases `connected` → `reinvited` → `bye_200`; checkpoints `time_to_200` /
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
            // own session reactively, and BYEs once the renegotiation completes.
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

                cseq: None,
                delayed: vec![],
                answers: Default::default(),
                claim: None,
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

                cseq: None,
                delayed: vec![],
                answers: Default::default(),
                claim: None,
            },
        ];

        let plan = vec![phase("established", established), phase("reinvited", reneg_done)];

        Ok(ActorCall {
            actors,
            plan,
            settle: SettleBarrier::default_ceiling(),
            expect: Expect::answered(),

            waivers: Vec::new(),
            automatics: Default::default(),
            ceiling: None,
            delta_policy: None,
            reception_observer: None,
        })
    }
}

/// The callee rejects the INVITE with `486 Busy Here`, actor-declared. Alice INVITEs, bob 486s
/// (the final ACKed on both legs), and nothing is left to CANCEL or BYE; the SUT still reaps
/// the call. Alice has only the INVITE goal; the settled call succeeds under `rejected`.
///
/// Downstream contract: NO phases, NO anchors, NO checkpoints, NO `mark_ringing`.
pub struct InviteReject;

impl ActorScenario for InviteReject {
    fn id(&self) -> ScenarioId {
        "invite_reject"
    }

    fn build(&self, env: &CallEnv<'_>) -> Result<ActorCall, StepError> {
        let actors = vec![
            // Alice originates and — her INVITE rejected — reaches a terminal leg
            // with no dialog (the reactor records her 486 final + terminates).
            ActorSpec {
                role: "alice",
                agent: env.alice.clone(),
                disposition: Disposition::Caller,
                media: MediaState::offer(OFFER_SDP),
                goals: vec![Goal::new(
                    Barrier::None,
                    GoalStep::Invite { callee: "bob", plan: Some(env.invite_plan(&["bob"])) },
                )],
                invite_targets: vec![("bob", env.bob.clone())],
                via: None,
                feed: CtxFeed::default(),

                cseq: None,
                delayed: vec![],
                answers: Default::default(),
                claim: None,
            },
            // Bob rejects the initial INVITE with 486; its reject-ACK is absorbed
            // without confirming.
            ActorSpec {
                role: "bob",
                agent: env.bob.clone(),
                disposition: Disposition::Reject(486),
                media: MediaState::none(),
                goals: vec![],
                invite_targets: vec![],
                via: None,
                feed: CtxFeed::default(),

                cseq: None,
                delayed: vec![],
                answers: Default::default(),
                claim: None,
            },
        ];

        // Internal controller gating only: bob rejected (both legs then terminate).
        let plan = vec![phase("rejected", |s| s.leg_at_least("bob", LegPhase::Terminated))];

        Ok(ActorCall {
            actors,
            plan,
            settle: SettleBarrier::default_ceiling(),
            expect: Expect::one(Branch::rejected(486)),

            waivers: Vec::new(),
            automatics: Default::default(),
            ceiling: None,
            delta_policy: None,
            reception_observer: None,
        })
    }
}

/// The caller abandons after ringing, actor-declared. Alice INVITEs, sees the 180 and CANCELs
/// (RFC 3261 §9.1); bob 200s the relayed CANCEL and 487s his INVITE, and the call succeeds
/// under [`Branch::ABANDONED`]. A 200 crossing the CANCEL is ACKed and BYEd
/// ([`Branch::ANSWERED_DESPITE_CANCEL`]); one reaching alice before her CANCEL leaves succeeds
/// under [`Branch::ANSWERED_BEFORE_CANCEL`].
///
/// Downstream contract: NO phases, NO anchors, checkpoint `time_to_180` (on the
/// first provisional) only, NO `mark_ringing`.
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
                    ),
                    // A 200 that crossed the CANCEL confirmed the call: hang
                    // up (§15). After a 487 the accepted final drops this goal.
                    Goal::new(Barrier::pred("answered", alice_confirmed), GoalStep::ByeIfConfirmed),
                ],
                invite_targets: vec![("bob", env.bob.clone())],
                via: None,
                feed: CtxFeed {
                    on_provisional: Feed::new(Some("time_to_180"), None),
                    ..CtxFeed::default()
                },

                cseq: None,
                delayed: vec![],
                answers: Default::default(),
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
                answers: Default::default(),
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

/// The happy call (INVITE, 180, 200, ACK, a short talk, BYE), actor-declared. Bob rings then
/// answers; alice talks for `talk_time`, then BYEs. The emergency variant `basic_call_em`
/// reuses this body (the marker rides the INVITE plan).
///
/// Downstream contract: phases `connected` → `bye_200`; checkpoints `time_to_200` /
/// `time_to_bye_200`; `mark_ringing` on the 180; anchors `LOAD_CALL_ANCHORS`.
pub struct BasicCall;

impl ActorScenario for BasicCall {
    fn id(&self) -> ScenarioId {
        "basic_call"
    }

    fn build(&self, env: &CallEnv<'_>) -> Result<ActorCall, StepError> {
        let actors = vec![
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
                    // Realistic post-connect talk time before teardown.
                    Goal::new(Barrier::pred("established", established), GoalStep::Bye)
                        .after(env.talk_time),
                ],
                invite_targets: vec![("bob", env.bob.clone())],
                via: None,
                feed: CtxFeed {
                    ringing_gate: true,
                    on_answer_rx: Feed::new(Some("time_to_200"), None),
                    on_bye_ok: Feed::new(Some("time_to_bye_200"), Some("bye_200")),
                    ..CtxFeed::default()
                },

                cseq: None,
                delayed: vec![],
                answers: Default::default(),
                claim: None,
            },
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

                cseq: None,
                delayed: vec![],
                answers: Default::default(),
                claim: None,
            },
        ];

        let plan = vec![phase("established", established)];

        Ok(ActorCall {
            actors,
            plan,
            settle: SettleBarrier::default_ceiling(),
            expect: Expect::answered(),

            waivers: Vec::new(),
            automatics: Default::default(),
            ceiling: None,
            delta_policy: None,
            reception_observer: None,
        })
    }
}
