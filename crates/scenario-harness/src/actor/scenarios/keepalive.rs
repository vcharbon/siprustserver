//! The keepalive scenarios: a call held alive by in-dialog OPTIONS pings.

use super::established;
use crate::actor::ending::Expect;
use crate::actor::endpoint::{CtxFeed, Disposition, Feed, MediaState};
use crate::actor::goals::{Barrier, Goal, GoalStep};
use crate::actor::spec::{ActorCall, ActorScenario};
use crate::actor::state::StateInner;
use crate::actor::{phase, ActorSpec, SettleBarrier};
use crate::realcall::{CallEnv, ScenarioId};
use crate::{StepError, ANSWER_SDP, OFFER_SDP};

/// Both legs established and the caller's OPTIONS ping answered with a final:
/// the `long_call` hold starts there, so its BYE never overtakes a ping her
/// Timer E is still resending (RFC 3261 §17.1.2.2). A 481 or 408 has ended the
/// dialog by then (§12.2.1.2); any other final leaves it to the BYE.
fn pinged(s: &StateInner) -> bool {
    established(s)
        && s.leg_ref("alice").is_some_and(|l| {
            l.responses().iter().any(|f| f.status >= 200 && f.cseq_method == "OPTIONS")
        })
}

/// OPTIONS-keepalive long hold, actor-declared — the port of
/// `OptionsHold`. Establish, then keep the dialog
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

                cseq: None,
                delayed: vec![],
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

/// Long recorded call, actor-declared — the port of
/// `LongCall`. Establish, send exactly ONE in-dialog OPTIONS keepalive ping,
/// and once it is answered simply SURVIVE for `long_hold` — the reactors
/// answer the SUT's own in-dialog keepalives on BOTH legs concurrently — then
/// BYE tolerantly.
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
                    // Hold for `long_hold` once the ping is answered — the goal-arm
                    // dwell is NON-blocking to the reactor, so alice keeps answering
                    // SUT keepalives on her leg throughout (bob's reactor does the same).
                    Goal::new(Barrier::pred("pinged", pinged), GoalStep::Bye).after(env.long_hold),
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

                cseq: None,
                delayed: vec![],
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
