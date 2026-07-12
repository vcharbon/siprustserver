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

/// Alice's leg reached (at least) an early dialog — she received the 180 (the
/// abandon body's CANCEL trigger).
fn alice_early(s: &StateInner) -> bool {
    s.leg_at_least("alice", LegPhase::Early)
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

/// OPTIONS-keepalive long hold, actor-declared — the port of
/// [`crate::realcall::scenarios::OptionsHold`]. Establish, then keep the dialog
/// alive with periodic in-dialog OPTIONS pings for `options_hold` (each 200 read
/// inline), then BYE. The first ping stamps `keepalive_ack`.
///
/// Downstream contract (`docs/todos/actor-harness-p1-contract-table.md` §5.4):
/// phases `connected` → `keepalive_ack` (first ping only) → `bye_200`;
/// checkpoints `time_to_200` / `time_to_options_200` (first) / `time_to_bye_200`;
/// `mark_ringing` on the 180; anchors `LOAD_CALL_ANCHORS` (OPTIONS NOT anchored).
pub struct OptionsHold;

impl ActorScenario for OptionsHold {
    fn id(&self) -> ScenarioId {
        "options_hold"
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
                    Goal::new(
                        Barrier::pred("established", established),
                        GoalStep::EveryOptions {
                            cadence: env.options_cadence,
                            hold: env.options_hold,
                        },
                    ),
                    Goal::new(Barrier::pred("established", established), GoalStep::Bye),
                ],
                invite_targets: vec![("bob", env.bob.clone())],
                via: None,
                feed: CtxFeed {
                    ringing_gate: true,
                    on_answer_rx: Feed::new(Some("time_to_200"), None),
                    on_options_ok: Feed::new(Some("time_to_options_200"), Some("keepalive_ack")),
                    on_bye_ok: Feed::new(Some("time_to_bye_200"), Some("bye_200")),
                    ..CtxFeed::default()
                },
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
            },
        ];

        let plan = vec![phase("established", established)];

        Ok(ActorCall {
            actors,
            plan,
            settle: SettleBarrier::default_ceiling(),
            expect: Expect::HappyBye,
        })
    }
}

/// Long recorded call, actor-declared — the port of
/// [`crate::realcall::scenarios::LongCall`]. Establish, send exactly ONE
/// in-dialog OPTIONS keepalive ping, then simply SURVIVE for `long_hold` — the
/// reactors answer the SUT's own in-dialog keepalives on BOTH legs concurrently
/// (that is what the linear body's `quiesce` did) — then BYE tolerantly.
///
/// Downstream contract (`docs/todos/actor-harness-p1-contract-table.md` §5.5):
/// phases `connected` → `keepalive_ack` (terminal — **no `bye_200`**);
/// checkpoints `time_to_200` / `time_to_options_200` / `time_to_bye_200`;
/// `mark_ringing` on the 180; anchors `LOAD_ESTABLISH_ANCHORS` (no bye anchor).
pub struct LongCall;

impl ActorScenario for LongCall {
    fn id(&self) -> ScenarioId {
        "long_call"
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
                    Goal::new(Barrier::pred("established", established), GoalStep::Options),
                    // Hold for `long_hold` before teardown — the goal-arm dwell is
                    // NON-blocking to the reactor, so alice keeps answering SUT
                    // keepalives on her leg throughout (bob's reactor does the same).
                    Goal::new(Barrier::pred("established", established), GoalStep::Bye)
                        .after(env.long_hold),
                ],
                invite_targets: vec![("bob", env.bob.clone())],
                via: None,
                feed: CtxFeed {
                    ringing_gate: true,
                    on_answer_rx: Feed::new(Some("time_to_200"), None),
                    on_options_ok: Feed::new(Some("time_to_options_200"), Some("keepalive_ack")),
                    // The BYE carries the `time_to_bye_200` checkpoint but NO phase
                    // — `long_call`'s terminal phase stays `keepalive_ack`
                    // (contract table §5.5 / §8.5).
                    on_bye_ok: Feed::new(Some("time_to_bye_200"), None),
                    ..CtxFeed::default()
                },
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
            },
        ];

        let plan = vec![phase("established", established)];

        Ok(ActorCall {
            actors,
            plan,
            settle: SettleBarrier::default_ceiling(),
            expect: Expect::HappyBye,
        })
    }
}

/// The callee REJECTS the INVITE with `486 Busy Here`, actor-declared — the port
/// of [`crate::realcall::scenarios::InviteReject`]. Alice INVITEs, bob 486s (the
/// final auto-ACKed on both legs), and the transaction completes with nothing to
/// CANCEL/BYE; the SUT must still reap the rejected call. Alice has ONLY the
/// INVITE goal (her INVITE is rejected — no dialog, no BYE); the clean torn-down
/// verdict is re-interpreted by [`Expect::Reject`] into the contract terminal.
///
/// Downstream contract (`docs/todos/actor-harness-p1-contract-table.md` §5.8):
/// NO phases, NO anchors, NO checkpoints, NO `mark_ringing`; terminal
/// `WrongStatus { who: "alice", expected: 200, got: 486, reason: "Busy Here" }`
/// → class `status_486`, case `alice@start`.
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
                // NO phases / checkpoints / ringing gate (contract table §5.8).
                feed: CtxFeed::default(),
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
            },
        ];

        // Internal controller gating only: bob rejected (both legs then terminate).
        let plan = vec![phase("rejected", |s| s.leg_at_least("bob", LegPhase::Terminated))];

        Ok(ActorCall {
            actors,
            plan,
            settle: SettleBarrier::default_ceiling(),
            expect: Expect::Reject(486),
        })
    }
}

/// The caller ABANDONS after ringing (CANCEL), actor-declared — the port of
/// [`crate::realcall::scenarios::AbandonRinging`]. Alice INVITEs, sees the 180,
/// then CANCELs the still-pending INVITE (RFC 3261 §9.1); the SUT relays the
/// CANCEL to bob, who 200s it and 487s his held INVITE, and both legs reap. The
/// clean torn-down verdict is re-interpreted by [`Expect::AbandonedEarly`] into
/// the synthetic terminal.
///
/// Downstream contract (`docs/todos/actor-harness-p1-contract-table.md` §5.9):
/// NO phases, NO anchors, checkpoint `time_to_180` only, NO `mark_ringing`;
/// terminal `Timeout { who: "alice-abandoned-after-ringing" }` → class `timeout`,
/// case `alice-abandoned-after-ringing@start`.
pub struct AbandonRinging;

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
                    Goal::new(Barrier::pred("ringing", alice_early), GoalStep::Cancel),
                ],
                invite_targets: vec![("bob", env.bob.clone())],
                via: None,
                feed: CtxFeed {
                    // `time_to_180` on the first provisional; NO phase, NO ringing
                    // gate (contract table §5.9).
                    on_provisional: Feed::new(Some("time_to_180"), None),
                    ..CtxFeed::default()
                },
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
            },
        ];

        // Internal controller gating only: alice rang (Early) → both torn down.
        let plan = vec![phase("ringing", alice_early)];

        Ok(ActorCall {
            actors,
            plan,
            settle: SettleBarrier::default_ceiling(),
            expect: Expect::AbandonedEarly,
        })
    }
}

/// The bread-and-butter happy call (INVITE/180/200/ACK, short talk, BYE),
/// actor-declared — the port of [`crate::realcall::scenarios::BasicCall`] and
/// the standard-path confirmation. Bob rings-then-answers; alice holds for
/// `talk_time` then BYEs. Emergency variant `basic_call_em` reuses this body
/// (the marker rides the INVITE plan).
///
/// Downstream contract (`docs/todos/actor-harness-p1-contract-table.md` §5.1):
/// phases `connected` → `bye_200`; checkpoints `time_to_200` / `time_to_bye_200`;
/// `mark_ringing` on the 180; anchors `LOAD_CALL_ANCHORS`.
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
            },
        ];

        let plan = vec![phase("established", established)];

        Ok(ActorCall {
            actors,
            plan,
            settle: SettleBarrier::default_ceiling(),
            expect: Expect::HappyBye,
        })
    }
}
