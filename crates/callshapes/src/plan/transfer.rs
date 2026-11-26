//! [`Stage::Transfer`](super::Stage::Transfer): the current dialog's callee
//! transfers the caller to charlie (a blind REFER, RFC 3515), and may hang up
//! its own leg while the transfer runs.
//!
//! The transferor's BYE ends the call while the target has not answered yet,
//! and only the transferor's leg once the target answered: the transfer then
//! goes on to merge the caller with the target (RFC 5589 §6.1).

use std::time::Duration;

use scenario_harness::actor::{
    phase, ActorSpec, Barrier, CtxFeed, Disposition, Expect, Feed, Goal, GoalStep, LegPhase,
    MediaState, SubflowState, SUBFLOW_REALIGN, SUBFLOW_REFER,
};
use scenario_harness::realcall::CallEnv;
use scenario_harness::{StepError, ANSWER_SDP};

use super::barriers::{declined, merged};
use super::{Build, ShapePlan, Transfer, TRANSFER_DECLINED};

/// When the transferor hangs up its own leg during a transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hangup {
    /// Once its REFER is accepted, while the target still rings: the call ends.
    BeforeTargetAnswer,
    /// Once the target answered, while the target's session is realigned.
    DuringTargetRealign,
    /// Once the target is realigned, while the caller's session is.
    DuringCallerRealign,
    /// At the merge, crossing the release the B2BUA sends it.
    AtMerge,
}

/// How long the target rings when the transferor hangs up before it answers:
/// past any dwell a perturbation adds to the transferor's BYE.
const TARGET_RING: Duration = Duration::from_secs(3);

/// The transferor's hangup goal: a BYE once `hangup`'s phase is reached.
fn hangup_goal(hangup: Hangup) -> Goal {
    let realigned = |s: &scenario_harness::actor::StateInner, leg: &str| {
        s.leg(leg).subflow(SUBFLOW_REALIGN).is_some_and(|f| f >= SubflowState::Confirmed)
    };
    let gate = match hangup {
        Hangup::BeforeTargetAnswer => Barrier::pred("referred", |s| {
            s.leg("bob").subflow(SUBFLOW_REFER).is_some_and(|f| f >= SubflowState::Answered)
        }),
        Hangup::DuringTargetRealign => {
            Barrier::pred("transferred", |s| s.leg_at_least("charlie", LegPhase::Confirmed))
        }
        Hangup::DuringCallerRealign => {
            Barrier::pred("target_realigned", move |s| realigned(s, "charlie"))
        }
        Hangup::AtMerge => Barrier::pred("merged", merged),
    };
    Goal::new(gate, GoalStep::Bye)
}

impl ShapePlan {
    /// The current dialog's remote end (bob) REFERs once both ends are
    /// established and the stages before are done, after a talk dwell.
    ///
    /// When the transferor hangs up while the target rings, the call ends: the
    /// caller is BYEd, a ringing target CANCELed, one never reached left alone.
    /// A BYE landing after the target answered ends the transferor's leg alone
    /// and the transfer merges, so the next gate waits for either end. Once
    /// merged, the caller talks to the target, whose teardown a callee one is.
    pub(super) fn compile_transfer(
        &self,
        env: &CallEnv<'_>,
        b: &mut Build,
        transfer: &Transfer,
    ) -> Result<(), StepError> {
        if env.charlie.is_none() {
            return Err(StepError::UnexpectedKind {
                who: self.id.to_string(),
                detail: "bound without a charlie leg".to_string(),
            });
        }
        let refer_to = env.refer_to().ok_or_else(|| StepError::UnexpectedKind {
            who: self.id.to_string(),
            detail: "no charlie for Refer-To".to_string(),
        })?;
        let (refer_key, happy, decline_code, hangup) = match transfer {
            Transfer::Blind { refer_key, hangup } => (refer_key, true, 0, *hangup),
            Transfer::BlindDeclined { refer_key, code } => (refer_key, false, *code, None),
        };
        let early_hangup = hangup == Some(Hangup::BeforeTargetAnswer);
        let authorization = self.binder.refer_authorization(env, refer_key);

        let before = b.gate.clone();
        let both = Barrier::AllConfirmed(&["alice", "bob"]);
        let ready = Barrier::pred("transfer_ready", move |s| before.holds(s) && both.holds(s));
        let bob = b.callee_mut("bob");
        bob.goals.push(
            Goal::new(ready, GoalStep::Refer { refer_to, authorization }).after(env.reinvite_gap),
        );
        if let Some(h) = hangup {
            bob.goals.push(hangup_goal(h));
        }
        if happy {
            bob.feed.on_refer_accepted = Feed::new(Some("time_to_202"), Some("referred"));
        }

        let ring = if early_hangup { TARGET_RING } else { Duration::ZERO };
        b.callees.push(ActorSpec {
            role: "charlie",
            agent: env.callee_agent("charlie").clone().transfer_target(),
            disposition: if happy {
                Disposition::RingThenAnswer { ring }
            } else {
                Disposition::Reject(decline_code)
            },
            media: MediaState::answer(ANSWER_SDP),
            goals: vec![],
            invite_targets: vec![],
            via: None,
            feed: if happy {
                CtxFeed {
                    on_answer_sent: Feed::new(Some("time_to_charlie_200"), Some("transferred")),
                    ..CtxFeed::default()
                }
            } else {
                CtxFeed::default()
            },

            cseq: None,
            delayed: vec![],
            answers: Default::default(),
            claim: None,
        });

        if early_hangup {
            b.gate = Barrier::pred("transfer_abandoned_or_merged", |s| {
                let target_done = s.leg_at_least("charlie", LegPhase::Terminated)
                    || !s.leg_at_least("charlie", LegPhase::Early);
                merged(s) || (s.leg_at_least("alice", LegPhase::Terminated) && target_done)
            });
        } else if happy {
            b.phases.push(phase("referred", |s| {
                s.leg("bob").subflow(SUBFLOW_REFER).is_some_and(|f| f >= SubflowState::Answered)
            }));
            b.phases.push(phase("transferred", |s| s.leg_at_least("charlie", LegPhase::Confirmed)));
            b.phases.push(phase("merged", merged));
            b.gate = Barrier::pred("merged", merged);
            b.winner = "charlie";
        } else {
            b.phases.push(phase("declined", declined));
            b.gate = Barrier::pred("declined", declined);
            b.expect = Expect::one(TRANSFER_DECLINED);
        }
        Ok(())
    }
}
